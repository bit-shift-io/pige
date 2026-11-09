//! Native Proton interactive authentication.
//!
//! The whole state machine is Pige-owned. SRP, TOTP, human verification,
//! two-password accounts, and key unlock use the vendored library's lower-level
//! primitives; the library's file/keyring `session` module is never called.
//! Credentials leave this module only as a staged F3 reference.
//!
//! Attempt binding is structural: a challenge always names the attempt that
//! produced it, every answer is looked up by that id, and an answer for an
//! expired, cancelled, superseded, or unknown attempt is refused before any
//! network work or secret staging.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};

use crate::account::{Account, AccountId, MailBackend};
use crate::auth::{
    AccountSessionManager, AuthAnswer, AuthChallenge, AuthProgress, AuthRequest,
    AuthenticatedAccount, AuthenticationService, HumanChallenge, SecretPersistence,
    SecretRepository, StagedSecretRef, TokenRefresher, proton::proton_secret,
};
use crate::providers::{
    AuthAttemptId, OperationContext, PROTON_PROVIDER, ProviderError, ProviderId,
};
use crate::store::{PendingAuthRecord, Store};

use proton_core::HvChallenge;
use proton_core::auth::types::{AuthInfo, AuthResponse, SessionResp};
use proton_core::crypto;
use proton_core::crypto::keys::{Address, KeySalt, User};
use proton_core::transport::{Doer, HttpClient, Request};
use proton_srp::{RPGPVerifier, SRPAuth, SRPProofB64, SrpHashVersion};

use super::client::{PROTON_APP_VERSION, ProtonEndpoints, map_proton_error};
use super::keys::ProtonKeyStores;

/// How long one interactive Proton attempt may stay open.
pub const DEFAULT_ATTEMPT_TTL: Duration = Duration::from_secs(300);

/// How long a cold resume may spend refreshing and unlocking.
pub const DEFAULT_RESUME_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound on the best-effort server logout after local cleanup.
pub const DEFAULT_LOGOUT_TIMEOUT: Duration = Duration::from_secs(5);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(serde::Deserialize)]
struct SaltsResponse {
    #[serde(rename = "KeySalts", default)]
    key_salts: Vec<KeySalt>,
}

#[derive(serde::Deserialize)]
struct UserResponse {
    #[serde(rename = "User")]
    user: User,
}

#[derive(serde::Deserialize)]
struct AddressesResponse {
    #[serde(rename = "Addresses", default)]
    addresses: Vec<Address>,
}

/// Build a transport carrying one access token and deliberately no refresh
/// token.
///
/// The vendored transport refreshes a 401 in memory when a refresh token is
/// present, which would rotate (and lose) the server's refresh token because the
/// temporary client is dropped. Pige owns rotation through
/// [`crate::auth::proton::ProtonTokenRefresher`], which persists before returning.
async fn authenticated_http(
    endpoints: &ProtonEndpoints,
    uid: &str,
    access: SecretString,
) -> HttpClient {
    let http = HttpClient::new(endpoints.as_str(), PROTON_APP_VERSION);
    http.set_tokens(
        uid.to_string(),
        access,
        SecretString::from(String::new()),
    )
    .await;
    http
}

async fn fetch_salts(http: &HttpClient) -> Result<Vec<KeySalt>, ProviderError> {
    let response: SaltsResponse = http
        .decode(Request::get("/core/v4/keys/salts").no_refresh())
        .await
        .map_err(map_proton_error)?;
    Ok(response.key_salts)
}

async fn fetch_user_and_addresses(
    http: &HttpClient,
) -> Result<(User, Vec<Address>), ProviderError> {
    let user: UserResponse = http
        .decode(Request::get("/core/v4/users").no_refresh())
        .await
        .map_err(map_proton_error)?;
    let addresses: AddressesResponse = http
        .decode(Request::get("/core/v4/addresses").no_refresh())
        .await
        .map_err(map_proton_error)?;
    Ok((user.user, addresses.addresses))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptState {
    Pending,
    Resolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Password,
    Totp,
    MailboxPassword,
    HumanVerification,
}

#[derive(Clone)]
struct SessionTokens {
    uid: String,
    access: SecretString,
    refresh: SecretString,
}

#[derive(Clone)]
struct Login {
    tokens: SessionTokens,
    password_mode: u8,
}

struct Attempt {
    state: AttemptState,
    account: Account,
    ctx: OperationContext,
    expires_at_ms: i64,
    username: String,
    stage: Stage,
    password: Option<SecretString>,
    totp: Option<SecretString>,
    login: Option<Login>,
    /// Type of the pending human-verification token (for example `captcha`).
    hv_token_type: Option<String>,
    staged: Option<StagedSecretRef>,
}

enum LoginOutcome {
    Authenticated(Login),
    RequiresTotp(Login),
    FidoOnly,
    HumanVerification {
        challenge: HumanChallenge,
        token_type: String,
    },
}

/// Login-specific error mapping: a rejected username/password is a terminal
/// reauthentication result, while transport/server failures remain retryable.
fn map_login_error(error: proton_core::Error) -> ProviderError {
    if let proton_core::Error::Api(api) = &error {
        if api.http_status == 429 {
            return map_proton_error(error);
        }
        if api.http_status >= 500 {
            return map_proton_error(error);
        }
        return ProviderError::reauthenticate(
            "Proton rejected the account sign-in; check the address and password",
        )
        .with_source_detail(error.to_string());
    }
    match &error {
        proton_core::Error::Srp(_) => ProviderError::reauthenticate(
            "Proton could not verify the server's SRP challenge; sign in again",
        )
        .with_source_detail(error.to_string()),
        _ => map_proton_error(error),
    }
}

fn human_outcome(challenge: HvChallenge, expires_at_ms: i64) -> LoginOutcome {
    let token_type = challenge
        .methods
        .first()
        .cloned()
        .unwrap_or_else(|| "captcha".to_string());
    let instructions = if challenge.web_url.is_empty() {
        "Complete Proton's human verification, then provide the verification token.".to_string()
    } else {
        challenge.web_url
    };
    LoginOutcome::HumanVerification {
        challenge: HumanChallenge {
            instructions,
            expires_at_ms,
        },
        token_type,
    }
}

/// Run the SRP login flow.
///
/// `hv` supplies an already-obtained human-verification token; when present,
/// the transport answers a 9001 challenge and retries the *same* request
/// automatically. Without it, a 9001 is returned to the state machine as a
/// typed challenge bound to the caller's attempt.
async fn run_login(
    endpoints: &ProtonEndpoints,
    username: &str,
    password: &SecretString,
    totp: Option<&SecretString>,
    hv: Option<(String, String)>,
    expires_at_ms: i64,
) -> Result<LoginOutcome, ProviderError> {
    let mut http = HttpClient::new(endpoints.as_str(), PROTON_APP_VERSION);
    if let Some(token) = hv {
        http.set_hv_resolver(Arc::new(move |_challenge| {
            let token = token.clone();
            Box::pin(async move { Ok(token) })
        }));
    }

    macro_rules! step {
        ($expression:expr) => {
            match $expression {
                Ok(value) => value,
                Err(proton_core::Error::HumanVerification(challenge)) => {
                    return Ok(human_outcome(challenge, expires_at_ms));
                }
                Err(error) => return Err(map_login_error(error)),
            }
        };
    }

    let session: SessionResp = step!(
        http.decode(
            Request::post("/auth/v4/sessions")
                .json(serde_json::json!({}))
                .enforce_unauth()
                .no_refresh(),
        )
        .await
    );
    http.set_tokens(
        session.uid.clone(),
        SecretString::from(session.access_token.clone()),
        SecretString::from(session.refresh_token.clone()),
    )
    .await;

    let info: AuthInfo = step!(
        http.decode(
            Request::post("/core/v4/auth/info")
                .json(serde_json::json!({ "Username": username }))
                .no_refresh(),
        )
        .await
    );

    let version = SrpHashVersion::try_from(info.version).map_err(|_| {
        ProviderError::reauthenticate(
            "Proton requested an unsupported SRP version; update the app and try again",
        )
    })?;
    // SRP proof generation and modulus verification are CPU-heavy; keep them
    // off the async/UI executor. Only the proof (no secret) crosses back.
    let username_owned = username.to_string();
    let password_owned = password.clone();
    let salt = info.salt.clone();
    let modulus = info.modulus.clone();
    let server_ephemeral = info.server_ephemeral.clone();
    let proof = tokio::task::spawn_blocking(move || {
        let verifier = RPGPVerifier::default();
        let srp = SRPAuth::new(
            &verifier,
            Some(&username_owned),
            password_owned.expose_secret(),
            version,
            &salt,
            &modulus,
            &server_ephemeral,
        )
        .map_err(|_| {
            ProviderError::reauthenticate(
                "Proton's SRP modulus could not be verified; sign in again",
            )
        })?;
        let proof = srp.generate_proofs().map_err(|_| {
            ProviderError::reauthenticate("Proton's SRP challenge could not be answered")
        })?;
        Ok::<SRPProofB64, ProviderError>(proof.into())
    })
    .await
    .map_err(|_| ProviderError::transient("the Proton SRP worker stopped"))??;

    let response: AuthResponse = step!(
        http.decode(
            Request::post("/core/v4/auth")
                .json(serde_json::json!({
                    "Username": username,
                    "ClientProof": proof.client_proof.clone(),
                    "ClientEphemeral": proof.client_ephemeral.clone(),
                    "SRPSession": info.srp_session,
                }))
                .no_refresh(),
        )
        .await
    );

    if !proof.compare_server_proof(&response.server_proof) {
        return Err(ProviderError::reauthenticate(
            "Proton could not verify the server's response; sign in again",
        ));
    }

    let two_fa = response.two_fa.enabled;
    let password_mode = response.password_mode;
    let tokens = SessionTokens {
        uid: response.uid.clone(),
        access: SecretString::from(response.access_token.clone()),
        refresh: SecretString::from(response.refresh_token.clone()),
    };
    http.set_tokens(
        response.uid.clone(),
        SecretString::from(response.access_token.clone()),
        SecretString::from(response.refresh_token.clone()),
    )
    .await;

    // Bit 0 is TOTP, bit 1 is FIDO2/WebAuthn. A FIDO-only account is refused
    // explicitly: there is no supported bypass.
    if two_fa & 1 == 0 && two_fa & 2 != 0 {
        return Ok(LoginOutcome::FidoOnly);
    }

    let login = Login {
        tokens,
        password_mode,
    };

    if two_fa & 1 != 0 {
        let Some(code) = totp else {
            return Ok(LoginOutcome::RequiresTotp(login));
        };
        let _: serde_json::Value = step!(
            http.decode(
                Request::post("/core/v4/auth/2fa")
                    .json(serde_json::json!({ "TwoFactorCode": code.expose_secret() }))
                    .no_refresh(),
            )
            .await
        );
    }

    Ok(LoginOutcome::Authenticated(login))
}

/// Interactive Proton authentication for one account.
pub struct ProtonAuthentication {
    secrets: Arc<dyn SecretRepository>,
    sessions: Arc<AccountSessionManager>,
    store: Arc<Store>,
    endpoints: ProtonEndpoints,
    keys: Arc<ProtonKeyStores>,
    attempt_ttl: Duration,
    resume_timeout: Duration,
    logout_timeout: Duration,
    refresher: Arc<crate::auth::proton::ProtonTokenRefresher>,
    attempts: HashMap<AuthAttemptId, Attempt>,
    /// Test-only seam that runs after key unlock/activity checks and before the
    /// fenced key install. Tests use it to land a revocation in that window
    /// deterministically; product builds do not compile it.
    #[cfg(any(test, feature = "provider-proton-test-support"))]
    install_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ProtonAuthentication {
    pub fn new(
        secrets: Arc<dyn SecretRepository>,
        sessions: Arc<AccountSessionManager>,
        store: Arc<Store>,
        endpoints: ProtonEndpoints,
        keys: Arc<ProtonKeyStores>,
    ) -> Self {
        let refresher = Arc::new(crate::auth::proton::ProtonTokenRefresher::new(
            secrets.clone(),
            endpoints.clone(),
        ));
        Self {
            secrets,
            sessions,
            store,
            endpoints,
            keys,
            attempt_ttl: DEFAULT_ATTEMPT_TTL,
            resume_timeout: DEFAULT_RESUME_TIMEOUT,
            logout_timeout: DEFAULT_LOGOUT_TIMEOUT,
            refresher,
            attempts: HashMap::new(),
            #[cfg(any(test, feature = "provider-proton-test-support"))]
            install_hook: None,
        }
    }

    #[cfg(any(test, feature = "provider-proton-test-support"))]
    #[doc(hidden)]
    pub fn set_install_hook(&mut self, hook: impl Fn() + Send + Sync + 'static) {
        self.install_hook = Some(Arc::new(hook));
    }

    /// Replace the token refresher, for callers that must share one instance
    /// with the [`super::ProtonProviderFactory`].
    ///
    /// One [`crate::auth::proton::ProtonTokenRefresher`] has one serial mutex;
    /// two independently constructed instances can race to rotate the same
    /// refresh token. [`super::proton_registration`] wires the factory and auth
    /// service to the same instance.
    #[must_use]
    pub fn with_refresher(
        mut self,
        refresher: Arc<crate::auth::proton::ProtonTokenRefresher>,
    ) -> Self {
        self.refresher = refresher;
        self
    }

    #[must_use]
    pub fn refresher(&self) -> Arc<crate::auth::proton::ProtonTokenRefresher> {
        self.refresher.clone()
    }

    #[must_use]
    pub fn with_attempt_ttl(mut self, ttl: Duration) -> Self {
        self.attempt_ttl = ttl;
        self
    }

    #[must_use]
    pub fn with_resume_timeout(mut self, timeout: Duration) -> Self {
        self.resume_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_logout_timeout(mut self, timeout: Duration) -> Self {
        self.logout_timeout = timeout;
        self
    }

    /// Start an attempt for an explicit account email. The F3 `AuthRequest`
    /// carries no email, so new-account setup uses this entry point; an existing
    /// account can use [`AuthenticationService::begin`], which resolves the
    /// email from the local account mirror.
    pub async fn begin_with_username(
        &mut self,
        username: &str,
        account: Option<AccountId>,
        enable_contacts: bool,
    ) -> Result<AuthProgress, ProviderError> {
        self.begin_inner(username, account, enable_contacts)
    }

    fn username_for(&self, account: &AccountId) -> Result<String, ProviderError> {
        let accounts = self
            .store
            .accounts()
            .map_err(crate::auth::store_error_to_provider)?;
        accounts
            .into_iter()
            .find(|candidate| candidate.id == *account)
            .map(|candidate| candidate.email)
            .filter(|email| !email.trim().is_empty())
            .ok_or_else(|| {
                ProviderError::reauthenticate(
                    "the Proton account email is missing; sign in with the account address",
                )
            })
    }

    fn provisional_account(id: AccountId, email: &str) -> Account {
        Account {
            id,
            label: email.to_string(),
            email: email.to_string(),
            display_name: String::new(),
            // The numeric Proton user id is not required for a session; the
            // optional field stays unset rather than being fabricated.
            backend: MailBackend::proton(None),
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        }
    }

    fn begin_inner(
        &mut self,
        username: &str,
        account: Option<AccountId>,
        _enable_contacts: bool,
    ) -> Result<AuthProgress, ProviderError> {
        if username.trim().is_empty() {
            return Err(ProviderError::unsupported(
                "Proton sign-in without the account email; supply the account address",
            ));
        }
        self.reap_expired();

        let attempt_id = AuthAttemptId::new(uuid::Uuid::new_v4().to_string());
        let account_id = account.unwrap_or_default();
        let superseded: Vec<AuthAttemptId> = self
            .attempts
            .iter()
            .filter(|(_, attempt)| {
                attempt.account.id == account_id && attempt.state == AttemptState::Pending
            })
            .map(|(attempt, _)| attempt.clone())
            .collect();
        for attempt in superseded {
            self.discard_attempt(&attempt);
        }

        let ctx = self.sessions.context(&account_id, self.attempt_ttl)?;
        let expires_at_ms = now_ms().saturating_add(self.attempt_ttl.as_millis() as i64);
        let account = Self::provisional_account(account_id, username);
        self.attempts.insert(
            attempt_id.clone(),
            Attempt {
                state: AttemptState::Pending,
                account,
                ctx,
                expires_at_ms,
                username: username.to_string(),
                stage: Stage::Password,
                password: None,
                totp: None,
                login: None,
                hv_token_type: None,
                staged: None,
            },
        );

        Ok(AuthProgress::Challenge {
            attempt: attempt_id,
            challenge: AuthChallenge::Password {
                username: Some(username.to_string()),
            },
        })
    }

    fn reap_expired(&mut self) {
        let now = now_ms();
        let expired: Vec<AuthAttemptId> = self
            .attempts
            .iter()
            .filter(|(_, attempt)| {
                attempt.state == AttemptState::Pending && now >= attempt.expires_at_ms
            })
            .map(|(attempt, _)| attempt.clone())
            .collect();
        for attempt in expired {
            self.discard_attempt(&attempt);
        }
        let _ = crate::auth::sweep_expired_pending_auth(&self.store, self.secrets.as_ref());
    }

    fn discard_attempt(&mut self, attempt: &AuthAttemptId) {
        if let Some(state) = self.attempts.remove(attempt) {
            if let Some(staged) = state.staged {
                let _ = self.secrets.discard_staged(&staged);
            }
            let _ = self.store.remove_pending_auth(attempt);
        }
    }

    fn ensure_active(&self, attempt: &Attempt) -> Result<(), ProviderError> {
        if attempt.state != AttemptState::Pending {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        }
        if now_ms() >= attempt.expires_at_ms {
            return Err(ProviderError::not_found(
                "the authentication attempt has expired",
            ));
        }
        attempt.ctx.ensure_active()?;
        if self.sessions.generation(&attempt.account.id)? != attempt.ctx.generation {
            return Err(ProviderError::conflict(
                "the account changed during Proton sign-in",
            ));
        }
        Ok(())
    }

    /// Install freshly unlocked keys under the account gate shared with
    /// [`AccountSessionManager::invalidate`] and account removal.
    ///
    /// Both `resume` and `finish_with_password` use this helper. Checking
    /// activity before the install is not enough: a revocation can land between
    /// that check and the install. Inside the gate the durable generation is
    /// re-read, so a stale sign-in/resume wipes its material and installs
    /// nothing instead of resurrecting unlocked keys after logout.
    fn install_keys_fenced(
        &self,
        account: &AccountId,
        ctx: &OperationContext,
        uid: String,
        keys: proton_core::crypto::keys::KeyStore,
    ) -> Result<Arc<super::keys::UnlockedKeyStore>, ProviderError> {
        self.store.with_account_gate(account, || {
            let fenced = (|| {
                ctx.ensure_active()?;
                if self.sessions.generation(account)? != ctx.generation {
                    return Err(ProviderError::conflict(
                        "the account changed while installing Proton keys",
                    ));
                }
                Ok(())
            })();
            match fenced {
                Ok(()) => {
                    let store = self.keys.get_or_create(account);
                    store.install(uid, keys);
                    Ok(store)
                }
                Err(error) => {
                    crate::providers::proton::keys::wipe_key_store(keys);
                    Err(error)
                }
            }
        })
    }

    async fn answer_password(
        &mut self,
        attempt_id: &AuthAttemptId,
        password: SecretString,
    ) -> Result<AuthProgress, ProviderError> {
        let Some(attempt) = self.attempts.get(attempt_id) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        let username = attempt.username.clone();
        let expires_at_ms = attempt.expires_at_ms;
        let outcome = run_login(
            &self.endpoints,
            &username,
            &password,
            None,
            None,
            expires_at_ms,
        )
        .await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                if error.kind == crate::providers::ErrorKind::Reauthenticate {
                    self.discard_attempt(attempt_id);
                }
                return Err(error);
            }
        };
        self.apply_login(attempt_id, password, None, outcome).await
    }

    async fn answer_totp(
        &mut self,
        attempt_id: &AuthAttemptId,
        code: SecretString,
    ) -> Result<AuthProgress, ProviderError> {
        let Some(attempt) = self.attempts.get(attempt_id) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        let username = attempt.username.clone();
        let expires_at_ms = attempt.expires_at_ms;
        let Some(password) = attempt.password.clone() else {
            self.discard_attempt(attempt_id);
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        let outcome = run_login(
            &self.endpoints,
            &username,
            &password,
            Some(&code),
            None,
            expires_at_ms,
        )
        .await;

        match outcome {
            Ok(outcome) => {
                self.apply_login(attempt_id, password, Some(code), outcome)
                    .await
            }
            Err(error) => {
                if error.kind == crate::providers::ErrorKind::Reauthenticate {
                    self.discard_attempt(attempt_id);
                } else if let Some(state) = self.attempts.get_mut(attempt_id) {
                    state.stage = Stage::Totp;
                }
                Err(error)
            }
        }
    }

    async fn apply_login(
        &mut self,
        attempt_id: &AuthAttemptId,
        password: SecretString,
        totp: Option<SecretString>,
        outcome: LoginOutcome,
    ) -> Result<AuthProgress, ProviderError> {
        match outcome {
            LoginOutcome::Authenticated(login) => {
                if login.password_mode == 2 {
                    if let Some(state) = self.attempts.get_mut(attempt_id) {
                        state.password = Some(password);
                        state.totp = totp;
                        state.login = Some(login);
                        state.stage = Stage::MailboxPassword;
                    }
                    Ok(AuthProgress::Challenge {
                        attempt: attempt_id.clone(),
                        challenge: AuthChallenge::MailboxPassword,
                    })
                } else {
                    self.finish_with_password(attempt_id, login, password).await
                }
            }
            LoginOutcome::RequiresTotp(login) => {
                if let Some(state) = self.attempts.get_mut(attempt_id) {
                    state.password = Some(password);
                    state.totp = None;
                    state.login = Some(login);
                    state.stage = Stage::Totp;
                }
                Ok(AuthProgress::Challenge {
                    attempt: attempt_id.clone(),
                    challenge: AuthChallenge::Totp,
                })
            }
            LoginOutcome::FidoOnly => {
                if let Some(state) = self.attempts.get_mut(attempt_id) {
                    state.state = AttemptState::Resolved;
                }
                Ok(AuthProgress::Challenge {
                    attempt: attempt_id.clone(),
                    challenge: AuthChallenge::Unsupported {
                        reason: "this Proton account requires a FIDO2 security key, which this \
                                 build does not support; enable a TOTP authenticator or use a \
                                 Proton app password"
                            .to_string(),
                    },
                })
            }
            LoginOutcome::HumanVerification {
                challenge,
                token_type,
            } => {
                if let Some(state) = self.attempts.get_mut(attempt_id) {
                    state.password = Some(password);
                    state.totp = totp;
                    state.hv_token_type = Some(token_type);
                    state.stage = Stage::HumanVerification;
                }
                Ok(AuthProgress::Challenge {
                    attempt: attempt_id.clone(),
                    challenge: AuthChallenge::HumanVerification(challenge),
                })
            }
        }
    }

    async fn answer_mailbox_password(
        &mut self,
        attempt_id: &AuthAttemptId,
        mailbox_password: SecretString,
    ) -> Result<AuthProgress, ProviderError> {
        let login = self
            .attempts
            .get(attempt_id)
            .and_then(|attempt| attempt.login.clone());
        let Some(login) = login else {
            self.discard_attempt(attempt_id);
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        self.finish_with_password(attempt_id, login, mailbox_password)
            .await
    }

    async fn answer_human_verification(
        &mut self,
        attempt_id: &AuthAttemptId,
        token: String,
    ) -> Result<AuthProgress, ProviderError> {
        if token.trim().is_empty() {
            return Err(ProviderError::transient(
                "the Proton verification token was empty",
            ));
        }
        let Some(attempt) = self.attempts.get(attempt_id) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        let username = attempt.username.clone();
        let expires_at_ms = attempt.expires_at_ms;
        let Some(password) = attempt.password.clone() else {
            self.discard_attempt(attempt_id);
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        let totp = attempt.totp.clone();
        let token_type = attempt
            .hv_token_type
            .clone()
            .unwrap_or_else(|| "captcha".to_string());

        let outcome = run_login(
            &self.endpoints,
            &username,
            &password,
            totp.as_ref(),
            Some((token, token_type)),
            expires_at_ms,
        )
        .await;

        match outcome {
            Ok(outcome) => self.apply_login(attempt_id, password, totp, outcome).await,
            Err(error) => {
                if error.kind == crate::providers::ErrorKind::Reauthenticate {
                    self.discard_attempt(attempt_id);
                } else if let Some(state) = self.attempts.get_mut(attempt_id) {
                    state.stage = Stage::HumanVerification;
                }
                Err(error)
            }
        }
    }

    async fn finish_with_password(
        &mut self,
        attempt_id: &AuthAttemptId,
        login: Login,
        mailbox_password: SecretString,
    ) -> Result<AuthProgress, ProviderError> {
        let Some(attempt) = self.attempts.get(attempt_id) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        let account_id = attempt.account.id.clone();
        let username = attempt.username.clone();
        let ctx = attempt.ctx.clone();
        let expires_at_ms = attempt.expires_at_ms;
        let uid = login.tokens.uid.clone();

        let http = authenticated_http(&self.endpoints, &uid, login.tokens.access.clone()).await;

        let salts = fetch_salts(&http).await?;
        let (user, addresses) = fetch_user_and_addresses(&http).await?;

        // Key unlock is CPU-heavy; run it on the blocking pool, not the async/UI
        // executor. The password moves into the worker and is dropped there.
        let unlocked = tokio::task::spawn_blocking(move || {
            let provider = crypto::provider();
            crypto::keys::unlock(&provider, &user, &addresses, &mailbox_password, &salts)
        })
        .await
        .map_err(|_| ProviderError::transient("the Proton key-unlock worker stopped"))?;
        let (keys, skp) = unlocked.map_err(map_proton_error)?;

        // A cancellation/invalidation that landed while the worker ran must not
        // leave unlocked material installed or stage a credential.
        if let Err(error) = ctx.ensure_active() {
            crate::providers::proton::keys::wipe_key_store(keys);
            self.discard_attempt(attempt_id);
            return Err(error);
        }

        // Test seam: a revocation in this window used to install keys after
        // logout had already wiped them.
        #[cfg(any(test, feature = "provider-proton-test-support"))]
        if let Some(hook) = &self.install_hook {
            hook();
        }

        // Install only after a successful unlock, under the same account gate as
        // `invalidate`/removal and with a durable generation recheck. If the
        // install is fenced out, the helper wipes the material.
        let key_store = match self.install_keys_fenced(&account_id, &ctx, uid.clone(), keys) {
            Ok(key_store) => key_store,
            Err(error) => {
                self.discard_attempt(attempt_id);
                return Err(error);
            }
        };

        let envelope = crate::auth::proton::proton_envelope(
            uid,
            login.tokens.access,
            login.tokens.refresh,
            skp,
        );
        let staged = match self.secrets.stage(attempt_id, &envelope) {
            Ok(staged) => staged,
            Err(error) => {
                key_store.lock();
                self.discard_attempt(attempt_id);
                return Err(error);
            }
        };
        if let Err(error) = self.store.insert_pending_auth(&PendingAuthRecord {
            attempt: attempt_id.clone(),
            account: Some(account_id.clone()),
            provider: ProviderId::from(PROTON_PROVIDER),
            credential_ref: staged.as_str().to_string(),
            generation: ctx.generation,
            created_at_ms: now_ms(),
            expires_at_ms,
        }) {
            let _ = self.secrets.discard_staged(&staged);
            key_store.lock();
            self.discard_attempt(attempt_id);
            return Err(crate::auth::store_error_to_provider(error));
        }

        if let Some(state) = self.attempts.get_mut(attempt_id) {
            state.state = AttemptState::Resolved;
            state.staged = Some(staged.clone());
            // The session tokens now live in the staged envelope; the attempt
            // no longer needs its transient copies of any secret input.
            state.login = None;
            state.password = None;
            state.totp = None;
        }
        Ok(AuthProgress::Complete(AuthenticatedAccount {
            account: Self::provisional_account(account_id, &username),
            persistence: SecretPersistence::Persistent,
            credential_ref: staged,
        }))
    }

    /// Resume with an explicit operation context.
    ///
    /// Normal callers use [`AuthenticationService::resume`], which derives a
    /// fresh context. This entry point exists so a caller that already owns the
    /// account gate can resume without silently creating a newer generation; the
    /// install step is fenced against the context's generation.
    pub async fn resume_with_context(
        &mut self,
        account: &AccountId,
        ctx: OperationContext,
    ) -> Result<AuthProgress, ProviderError> {
        ctx.ensure_active()?;
        if ctx.account != *account {
            return Err(ProviderError::conflict(
                "the Proton resume context belongs to another account",
            ));
        }
        let envelope = self.secrets.load(account)?.ok_or_else(|| {
            ProviderError::reauthenticate("there is no stored Proton session for this account")
        })?;
        let secret = proton_secret(envelope)?;
        let uid = secret.uid.clone().ok_or_else(|| {
            ProviderError::reauthenticate("the Proton session id is missing; sign in again")
        })?;
        let _refresh = secret.refresh_token.clone().ok_or_else(|| {
            ProviderError::reauthenticate("the Proton session has expired; sign in again")
        })?;
        let skp = secret.skp.clone().ok_or_else(|| {
            ProviderError::reauthenticate("unlock the Proton mailbox to continue")
        })?;

        // Use only the stored access token here. The temporary client gets no
        // refresh token, so the vendored transport cannot rotate the session
        // token in memory and drop the result. A 401 is retried through Pige's
        // refresher, which persists the rotated pair before returning.
        let mut access = secret.access_token.clone();
        let mut may_refresh = true;
        let (user, addresses) = loop {
            ctx.ensure_active()?;
            let token = match access.take() {
                Some(token) => token,
                None => self.refresher.refresh(&ctx).await?,
            };
            let http = authenticated_http(&self.endpoints, &uid, token).await;
            match fetch_user_and_addresses(&http).await {
                Ok(material) => break material,
                Err(error)
                    if error.kind == crate::providers::ErrorKind::Reauthenticate
                        && may_refresh =>
                {
                    may_refresh = false;
                    access = None;
                }
                Err(error) => return Err(error),
            }
        };

        let unlocked = tokio::task::spawn_blocking(move || {
            let provider = crypto::provider();
            crypto::keys::unlock_with_skp(&provider, &user, &addresses, &skp)
        })
        .await
        .map_err(|_| ProviderError::transient("the Proton key-unlock worker stopped"))?;
        let keys = unlocked.map_err(map_proton_error)?;

        // Installation shares the account gate with `invalidate`/removal. A
        // resume that lost the race must not recreate a key store after logout,
        // and the durable generation is checked inside the same gate as the
        // install.
        self.install_keys_fenced(account, &ctx, uid, keys)?;

        let email = self
            .store
            .accounts()
            .ok()
            .and_then(|accounts| {
                accounts
                    .into_iter()
                    .find(|candidate| candidate.id == *account)
                    .map(|candidate| candidate.email)
            })
            .unwrap_or_default();
        let persistent = self.secrets.load_persistent(account)?.is_some();
        Ok(AuthProgress::Complete(AuthenticatedAccount {
            account: Self::provisional_account(account.clone(), &email),
            persistence: if persistent {
                SecretPersistence::Persistent
            } else {
                SecretPersistence::SessionOnly
            },
            credential_ref: StagedSecretRef::for_account(account)?,
        }))
    }

    /// Best-effort server logout after local state is already protected.
    ///
    /// The request carries no refresh token and skips the vendor refresh path.
    /// It is bounded by `logout_timeout` and by the freshly derived context's
    /// cancellation token, so an unreachable or hostile endpoint cannot extend
    /// the logout or keep anything unlocked.
    async fn server_logout(&self, account: &AccountId, uid: String, access: SecretString) {
        let Ok(ctx) = self.sessions.context(account, self.logout_timeout) else {
            return;
        };
        let http = authenticated_http(&self.endpoints, &uid, access).await;
        let request = Request::delete("/core/v4/auth").no_refresh();
        let call = async {
            let _ = http.decode::<serde_json::Value>(request).await;
        };
        tokio::select! {
            _ = ctx.cancellation.cancelled() => {}
            _ = tokio::time::sleep_until(ctx.deadline) => {}
            _ = call => {}
        }
    }
}

#[async_trait]
impl AuthenticationService for ProtonAuthentication {
    async fn begin(&mut self, request: AuthRequest) -> Result<AuthProgress, ProviderError> {
        if request.provider != ProviderId::from(PROTON_PROVIDER) {
            return Err(ProviderError::unsupported(
                "Proton authentication for this provider",
            ));
        }
        let username = match &request.account {
            Some(account) => self.username_for(account)?,
            None => String::new(),
        };
        self.begin_inner(&username, request.account, request.enable_contacts)
    }

    async fn respond(
        &mut self,
        attempt: &AuthAttemptId,
        answer: AuthAnswer,
    ) -> Result<AuthProgress, ProviderError> {
        let (stage, active) = {
            let Some(state) = self.attempts.get(attempt) else {
                return Err(ProviderError::not_found(
                    "the authentication attempt is no longer active",
                ));
            };
            (state.stage, self.ensure_active(state))
        };
        if let Err(error) = active {
            self.discard_attempt(attempt);
            return Err(error);
        }

        match (stage, answer) {
            (Stage::Password, AuthAnswer::Password(password)) => {
                self.answer_password(attempt, password).await
            }
            (Stage::Totp, AuthAnswer::Totp(code)) => self.answer_totp(attempt, code).await,
            (Stage::MailboxPassword, AuthAnswer::MailboxPassword(password)) => {
                self.answer_mailbox_password(attempt, password).await
            }
            (Stage::HumanVerification, AuthAnswer::HumanVerification(answer)) => {
                self.answer_human_verification(attempt, answer.token).await
            }
            (Stage::Password, _) => Err(ProviderError::unsupported(
                "this Proton challenge expects the account password",
            )),
            (Stage::Totp, _) => Err(ProviderError::unsupported(
                "this Proton challenge expects a time-based one-time code",
            )),
            (Stage::MailboxPassword, _) => Err(ProviderError::unsupported(
                "this Proton challenge expects the mailbox password",
            )),
            (Stage::HumanVerification, _) => Err(ProviderError::unsupported(
                "this Proton challenge expects a human-verification token",
            )),
        }
    }

    async fn cancel(&mut self, attempt: &AuthAttemptId) -> Result<(), ProviderError> {
        if let Some(state) = self.attempts.get(attempt)
            && state.state == AttemptState::Pending
        {
            self.discard_attempt(attempt);
        }
        Ok(())
    }

    async fn resume(&mut self, account: &AccountId) -> Result<AuthProgress, ProviderError> {
        let ctx = self.sessions.context(account, self.resume_timeout)?;
        self.resume_with_context(account, ctx).await
    }

    async fn revoke(&mut self, account: &AccountId) -> Result<(), ProviderError> {
        // Discard every attempt for the account, including a completed one whose
        // staged credential has not yet been committed: logout must leave no
        // staged secret behind.
        let attempts: Vec<AuthAttemptId> = self
            .attempts
            .iter()
            .filter(|(_, state)| state.account.id == *account)
            .map(|(attempt, _)| attempt.clone())
            .collect();
        for attempt in attempts {
            self.discard_attempt(&attempt);
        }

        // Capture the session tokens before deleting the credential; local
        // protection must not depend on the network.
        let logout_tokens = match self.secrets.load(account) {
            Ok(Some(envelope)) => proton_secret(envelope).ok().and_then(|secret| {
                Some((secret.uid?, secret.access_token?))
            }),
            _ => None,
        };

        // Cancel live work, lock keys, and delete the credential first. Only then
        // is a bounded best-effort server logout attempted; an unreachable or
        // hostile endpoint cannot leave keys unlocked or contexts live.
        let invalidated = self.sessions.invalidate(account);
        self.keys.lock_and_remove(account);
        let deleted = self.secrets.delete(account);

        if let Some((uid, access)) = logout_tokens {
            self.server_logout(account, uid, access).await;
        }

        invalidated?;
        deleted?;
        Ok(())
    }
}

impl fmt::Debug for ProtonAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProtonAuthentication")
            .field("endpoints", &self.endpoints)
            .field("attempt_ttl", &self.attempt_ttl)
            .field("attempts", &self.attempts.len())
            .field("secrets", &"<protected credential repository>")
            .finish()
    }
}
