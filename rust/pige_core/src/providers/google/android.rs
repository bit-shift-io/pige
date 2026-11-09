//! Android Google authorization contract.
//!
//! S2 §9.4 freezes the Android flow to Google Identity Services
//! `AuthorizationClient` behind Play services. There is no redirect URI, OOB
//! code, or WebView: the host launches the platform consent UI and returns a
//! *validated* result, bound to one [`AuthAttemptId`]. The JNI bridge accepts
//! only the fields frozen here (`accessToken`, `expiresAtMs`, `scopes`,
//! identity, optional `serverAuthCode`), never arbitrary browser parameters.
//!
//! Live consent return, Activity recreation, silent renewal, and revocation are
//! BLOCKED on a registered Android OAuth client and a disposable account (S2
//! §9.7). The Rust contract and its fake host are exercised by `google_auth`.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use secrecy::SecretString;
use serde::Deserialize;
use url::Url;

use crate::account::AccountId;
use crate::providers::{AuthAttemptId, ProviderError};

use super::GMAIL_MODIFY_SCOPE;

/// Identity reported by the platform after consent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoogleIdentity {
    pub user_id: Option<String>,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

/// A validated on-device Google grant. The access token is short-lived; the
/// optional server auth code exists only when the user configured a server-side
/// offline exchange (S2 §9.6).
#[derive(Clone)]
pub struct GoogleAndroidResult {
    pub access_token: SecretString,
    pub expires_at_ms: i64,
    pub scopes: Vec<String>,
    pub identity: GoogleIdentity,
    pub server_auth_code: Option<SecretString>,
}

impl GoogleAndroidResult {
    /// Reject an incomplete or non-Gmail result before it can reach a secret
    /// store. `scopes` are the platform-reported *granted* scopes.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.expires_at_ms <= 0 {
            return Err(ProviderError::unsupported(
                "the Android sign-in returned no token expiry",
            ));
        }
        if self.scopes.is_empty() {
            return Err(ProviderError::forbidden(
                "the Android sign-in returned no granted scopes",
            ));
        }
        if !self.scopes.iter().any(|scope| scope == GMAIL_MODIFY_SCOPE) {
            return Err(ProviderError::forbidden(
                "Google mail access was not granted",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for GoogleAndroidResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleAndroidResult")
            .field("access_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scopes", &self.scopes)
            .field("identity", &self.identity)
            .field(
                "server_auth_code",
                &self.server_auth_code.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The JNI callback outcome. A `Failed` outcome carries no provider text; only
/// Rust-authored safe messages reach the UI.
#[derive(Clone)]
pub enum GoogleAuthorizationOutcome {
    Authorized(GoogleAndroidResult),
    Cancelled,
    Failed,
}
impl fmt::Debug for GoogleAuthorizationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authorized(result) => formatter.debug_tuple("Authorized").field(result).finish(),
            Self::Cancelled => formatter.write_str("Cancelled"),
            Self::Failed => formatter.write_str("Failed"),
        }
    }
}

/// The complete JNI payload: one attempt and its platform-validated result.
#[derive(Clone, Debug)]
pub struct GoogleAuthorizationResult {
    pub attempt: AuthAttemptId,
    pub result: GoogleAuthorizationOutcome,
}

impl GoogleAuthorizationResult {
    /// Parse the bounded JSON the Kotlin `GoogleAuthorization` helper sends.
    ///
    /// Unknown fields are rejected so a future ad-hoc redirect parameter cannot
    /// be smuggled through the bridge. Failure text from the host is ignored.
    pub fn from_json(payload: &str) -> Result<Self, ProviderError> {
        let wire: WireResult = serde_json::from_str(payload).map_err(|error| {
            ProviderError::transient("the Android Google result was invalid")
                .with_source_detail(error.to_string())
        })?;
        if wire.attempt.is_empty() || wire.attempt.len() > 128 {
            return Err(ProviderError::unsupported(
                "the Android Google result had no valid attempt",
            ));
        }
        if !wire.attempt.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        }) {
            return Err(ProviderError::unsupported(
                "the Android Google result had no valid attempt",
            ));
        }
        let attempt = AuthAttemptId::new(wire.attempt);
        let result = match wire.result {
            WireOutcome::Authorized {
                access_token,
                expires_at_ms,
                scopes,
                identity,
                server_auth_code,
            } => {
                let authorized = GoogleAndroidResult {
                    access_token: SecretString::from(access_token),
                    expires_at_ms,
                    scopes,
                    identity: GoogleIdentity {
                        user_id: identity.user_id,
                        email: identity.email,
                        display_name: identity.display_name,
                    },
                    server_auth_code: server_auth_code.map(SecretString::from),
                };
                authorized.validate()?;
                GoogleAuthorizationOutcome::Authorized(authorized)
            }
            WireOutcome::Cancelled => GoogleAuthorizationOutcome::Cancelled,
            WireOutcome::Failed => GoogleAuthorizationOutcome::Failed,
        };
        Ok(Self { attempt, result })
    }
}

/// The platform bridge that launches Google Identity Services and (for the
/// token source) performs a non-interactive acquisition while the grant lives.
#[async_trait]
pub trait GoogleAndroidHost: Send + Sync {
    /// Launch interactively. The result is delivered later through
    /// [`GoogleAuthentication::deliver_android_result`].
    async fn launch(&self, request: AndroidAuthorizationRequest) -> Result<(), ProviderError>;

    /// Cancel any platform UI bound to this attempt.
    fn cancel(&self, attempt: &AuthAttemptId) -> Result<(), ProviderError>;

    /// Obtain a fresh access token without an interactive prompt, if the grant
    /// is still intact. This is the Android on-demand model in S2 §9.6.
    async fn acquire(
        &self,
        account: &AccountId,
        scopes: &[String],
    ) -> Result<GoogleAndroidResult, ProviderError>;
}

/// What the host is asked to authorize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndroidAuthorizationRequest {
    pub attempt: AuthAttemptId,
    pub scopes: Vec<String>,
    pub request_offline_access: bool,
    /// The Web application client id required by
    /// `requestOfflineAccess(serverClientId)` when `request_offline_access` is
    /// true; `None` for the on-demand, no-device-refresh-token flow.
    pub server_client_id: Option<String>,
}

/// The server-side exchange configuration for `requestOfflineAccess` /
/// `serverAuthCode`.
///
/// `exchange_url` is a Pige-operated endpoint that holds the Google Web
/// client secret and exchanges the code server-side, per S2 §9.6. The Android
/// device never carries a Google client secret. If the endpoint returns a
/// refresh token it is persisted through F3; an endpoint that keeps the refresh
/// token itself can return only short-lived access tokens.
#[derive(Clone)]
pub struct AndroidOfflineExchange {
    pub server_client_id: String,
    pub exchange_url: Url,
}

impl fmt::Debug for AndroidOfflineExchange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AndroidOfflineExchange")
            .field("server_client_id", &"<configured>")
            .field("exchange_url", &self.exchange_url)
            .finish()
    }
}

/// Results delivered by the JNI bridge but not yet consumed by the active
/// authentication service. A bounded inbox keeps a process-wide callback safe
/// without retaining unlimited payloads from a misbehaving host.
static NATIVE_RESULTS: OnceLock<Mutex<VecDeque<GoogleAuthorizationResult>>> = OnceLock::new();

fn native_results() -> &'static Mutex<VecDeque<GoogleAuthorizationResult>> {
    NATIVE_RESULTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Validate and enqueue one JNI payload. Returns the bound attempt id.
pub fn deliver_native_result(payload: &str) -> Result<AuthAttemptId, ProviderError> {
    let result = GoogleAuthorizationResult::from_json(payload)?;
    let mut inbox = native_results()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if inbox.len() >= 32 {
        inbox.pop_front();
    }
    inbox.push_back(result.clone());
    Ok(result.attempt)
}

/// Take the queued result for one attempt, if any.
pub fn take_native_result(attempt: &AuthAttemptId) -> Option<GoogleAuthorizationResult> {
    let mut inbox = native_results()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    inbox
        .iter()
        .position(|result| &result.attempt == attempt)
        .and_then(|index| inbox.remove(index))
}

/// Whether a validated result for `attempt` is waiting to be consumed. The UI
/// polls this without taking the authentication service lock across a wait.
pub fn native_result_pending(attempt: &AuthAttemptId) -> bool {
    let inbox = native_results()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    inbox.iter().any(|result| &result.attempt == attempt)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireResult {
    attempt: String,
    result: WireOutcome,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum WireOutcome {
    Authorized {
        #[serde(rename = "accessToken")]
        access_token: String,
        #[serde(rename = "expiresAtMs")]
        expires_at_ms: i64,
        scopes: Vec<String>,
        #[serde(default)]
        identity: WireIdentity,
        #[serde(default, rename = "serverAuthCode")]
        server_auth_code: Option<String>,
    },
    Cancelled,
    Failed,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireIdentity {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

/// The production Android host: launches the Kotlin `GoogleAuthorization`
/// helper through `PigeBridge` and lets its validated result return through
/// [`deliver_native_result`].
///
/// The app does not configure a server-side offline exchange (the user has no
/// exchange server), so [`GoogleAndroidHost::acquire`] cannot mint a new token
/// silently. A cached access token is used until it expires; afterwards the user
/// signs in again on demand. That is the S2 §9.6 on-demand model.
#[cfg(target_os = "android")]
pub struct AndroidPlatformHost;

#[cfg(target_os = "android")]
impl AndroidPlatformHost {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "android")]
impl Default for AndroidPlatformHost {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "android")]
#[async_trait]
impl GoogleAndroidHost for AndroidPlatformHost {
    async fn launch(&self, request: AndroidAuthorizationRequest) -> Result<(), ProviderError> {
        let scopes = serde_json::to_string(&request.scopes).map_err(|error| {
            ProviderError::transient("could not prepare Google sign-in")
                .with_source_detail(error.to_string())
        })?;
        crate::platform::android::launch_google_authorization(
            request.attempt.as_str(),
            &scopes,
            request.server_client_id.as_deref(),
        )
        .map_err(|error| {
            ProviderError::transient("could not start Google sign-in").with_source_detail(error)
        })
    }

    fn cancel(&self, attempt: &AuthAttemptId) -> Result<(), ProviderError> {
        crate::platform::android::cancel_google_authorization(attempt.as_str()).map_err(|error| {
            ProviderError::transient("could not cancel Google sign-in").with_source_detail(error)
        })
    }

    async fn acquire(
        &self,
        _account: &AccountId,
        _scopes: &[String],
    ) -> Result<GoogleAndroidResult, ProviderError> {
        // No device refresh token is stored in the on-demand model, and a
        // non-interactive acquisition cannot launch consent. Reauthentication is
        // the correct, explicit outcome.
        Err(ProviderError::reauthenticate(
            "Google sign-in has expired; sign in again",
        ))
    }
}
