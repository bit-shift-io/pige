//! Google interactive authorization and token ownership.
//!
//! Desktop follows S2 §9.3 exactly: system browser, loopback redirect, PKCE
//! (S256), and an exact `state` check before the code is exchanged. The public
//! desktop client sends no client secret. Android follows S2 §9.4: Play services
//! returns a validated host result bound to one attempt; a device refresh token
//! is never invented (an optional server-side `serverAuthCode` exchange is the
//! only offline path).
//!
//! Refreshing is serialized, generation-aware, and saves a rotated refresh token
//! through F3 *before* returning the new access token. Errors are already
//! redacted; token endpoint bodies never reach `safe_message`, `Display`, or
//! `Debug`.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use crate::account::{Account, AccountId, MailBackend};
use crate::auth::{
    AccountSessionManager, AuthAnswer, AuthChallenge, AuthProgress, AuthRequest,
    AuthenticatedAccount, AuthenticationService, BrowserChallenge, CachedTokenSource,
    SecretEnvelope, SecretPersistence, SecretRepository, StagedSecretRef, TokenRefresher,
    TokenSource,
};
use crate::providers::{
    AuthAttemptId, GMAIL_PROVIDER, OperationContext, ProviderError, ProviderId,
};
use crate::store::{PendingAuthRecord, Store};

use super::GMAIL_MODIFY_SCOPE;
use super::android::{
    AndroidAuthorizationRequest, AndroidOfflineExchange, GoogleAndroidHost,
    GoogleAuthorizationOutcome, GoogleAuthorizationResult, GoogleIdentity, native_result_pending,
    take_native_result,
};
use super::desktop::{DesktopRedirect, DesktopRedirectReceiver};

pub const CONTACTS_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/contacts.readonly";
pub const CONTACTS_OTHER_READONLY_SCOPE: &str =
    "https://www.googleapis.com/auth/contacts.other.readonly";
const IDENTITY_SCOPES: [&str; 3] = ["openid", "email", "profile"];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The scopes Pige requests for mail, plus identity.
pub fn mail_scopes() -> Vec<String> {
    let mut scopes = vec![GMAIL_MODIFY_SCOPE.to_string()];
    scopes.extend(IDENTITY_SCOPES.iter().map(|scope| scope.to_string()));
    scopes
}

/// The scopes for one attempt. Contacts are requested only when the caller is
/// enabling that capability; mail and local history never depend on consent.
pub fn requested_scopes(enable_contacts: bool) -> Vec<String> {
    let mut scopes = mail_scopes();
    if enable_contacts {
        scopes.push(CONTACTS_READONLY_SCOPE.to_string());
        scopes.push(CONTACTS_OTHER_READONLY_SCOPE.to_string());
    }
    scopes
}

/// OAuth client configuration. `client_secret` is `None` for the desktop public
/// client and only present for the optional Android server-side exchange.
#[derive(Clone)]
pub struct GoogleOAuthConfig {
    pub client_id: String,
    pub client_secret: Option<SecretString>,
    pub authorization_url: Url,
    pub token_url: Url,
}

impl GoogleOAuthConfig {
    pub fn production(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            authorization_url: Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
                .expect("the Google authorization endpoint is a valid URL"),
            token_url: Url::parse("https://oauth2.googleapis.com/token")
                .expect("the Google token endpoint is a valid URL"),
        }
    }
}

impl fmt::Debug for GoogleOAuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleOAuthConfig")
            .field("client_id", &"<configured>")
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("authorization_url", &self.authorization_url)
            .field("token_url", &self.token_url)
            .finish()
    }
}

/// One token endpoint response. Never carries a provider error body.
struct GoogleTokenSet {
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    expires_at_ms: i64,
    scopes: Vec<String>,
    id_token: Option<SecretString>,
}

/// The minimal HTTPS client for Google's token endpoint.
pub struct GoogleTokenClient {
    http: reqwest::Client,
    oauth: GoogleOAuthConfig,
}

impl GoogleTokenClient {
    pub fn new(oauth: GoogleOAuthConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            oauth,
        }
    }

    async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<GoogleTokenSet, ProviderError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("client_id", &self.oauth.client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(secret) = &self.oauth.client_secret {
            form.push(("client_secret", secret.expose_secret()));
        }
        self.post(form).await
    }

    async fn refresh(&self, refresh_token: &SecretString) -> Result<GoogleTokenSet, ProviderError> {
        self.refresh_with(
            self.oauth.token_url.clone(),
            &self.oauth.client_id,
            self.oauth.client_secret.as_ref(),
            refresh_token,
        )
        .await
    }

    /// Refresh against an explicit endpoint/client. The Android offline exchange
    /// uses the Pige-operated endpoint and the Web client id; the server holds
    /// the client secret, so none is sent from the device.
    async fn refresh_with(
        &self,
        url: Url,
        client_id: &str,
        client_secret: Option<&SecretString>,
        refresh_token: &SecretString,
    ) -> Result<GoogleTokenSet, ProviderError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("client_id", client_id),
            ("refresh_token", refresh_token.expose_secret()),
            ("grant_type", "refresh_token"),
        ];
        if let Some(secret) = client_secret {
            form.push(("client_secret", secret.expose_secret()));
        }
        self.post_to(url, form).await
    }

    async fn exchange_server_auth_code(
        &self,
        code: &SecretString,
        exchange: &AndroidOfflineExchange,
    ) -> Result<GoogleTokenSet, ProviderError> {
        // The server (not this device) holds the Google Web client secret.
        let form: Vec<(&str, &str)> = vec![
            ("client_id", &exchange.server_client_id),
            ("code", code.expose_secret()),
            ("grant_type", "authorization_code"),
        ];
        self.post_to(exchange.exchange_url.clone(), form).await
    }

    async fn post(&self, form: Vec<(&str, &str)>) -> Result<GoogleTokenSet, ProviderError> {
        self.post_to(self.oauth.token_url.clone(), form).await
    }

    async fn post_to(
        &self,
        url: Url,
        form: Vec<(&str, &str)>,
    ) -> Result<GoogleTokenSet, ProviderError> {
        let response = self
            .http
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .form(&form)
            .send()
            .await
            .map_err(map_transport_error)?;

        let status = response.status();
        let body = response.text().await.map_err(map_transport_error)?;
        if !status.is_success() {
            return Err(map_token_error(status, &body));
        }
        parse_token_response(&body)
    }
}

fn parse_token_response(body: &str) -> Result<GoogleTokenSet, ProviderError> {
    let wire: WireTokenResponse = serde_json::from_str(body).map_err(|error| {
        ProviderError::transient("the Google token response could not be read")
            .with_source_detail(error.to_string())
    })?;
    let access_token = SecretString::from(wire.access_token);
    let refresh_token = wire.refresh_token.map(SecretString::from);
    let expires_in = wire.expires_in.unwrap_or(3600).max(0);
    Ok(GoogleTokenSet {
        access_token,
        refresh_token,
        expires_at_ms: now_ms().saturating_add(expires_in.saturating_mul(1000)),
        scopes: wire
            .scope
            .map(|scope| scope.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        id_token: wire.id_token.map(SecretString::from),
    })
}

fn map_transport_error(error: reqwest::Error) -> ProviderError {
    // Never echo a request body or header. The token endpoint URL has no secret
    // query parameters, so the reqwest classification alone is safe and useful.
    if error.is_timeout() {
        ProviderError::transient("the Google sign-in request timed out")
    } else if error.is_connect() {
        ProviderError::offline("could not reach Google sign-in")
    } else {
        ProviderError::transient("the Google sign-in request failed")
    }
}

fn map_token_error(status: reqwest::StatusCode, body: &str) -> ProviderError {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string)
        });
    let error = match code.as_deref() {
        Some("invalid_grant" | "invalid_client" | "unauthorized_client" | "access_denied") => {
            ProviderError::reauthenticate("Google sign-in has expired; sign in again")
        }
        Some("invalid_request") => {
            ProviderError::reauthenticate("Google sign-in could not be completed; sign in again")
        }
        _ if status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
            ProviderError::rate_limited(Duration::from_secs(60))
        }
        _ if status.is_server_error() => {
            ProviderError::transient("Google is temporarily unavailable")
        }
        _ => ProviderError::transient("the Google sign-in request was rejected"),
    };
    // The status code is safe; the response body may contain tokens and is never
    // attached, even to the adapter-only source detail.
    error.with_source_detail(format!("token endpoint returned HTTP {status}"))
}

/// Refreshes one account's Google access token.
///
/// The refresh mutex serializes callers even when several independent
/// `CachedTokenSource`s exist for the account. The rotated refresh token is
/// persisted through the generation-gated F3 repository before this method
/// returns the new access token, so it can never be reused from stale state.
pub struct GoogleTokenRefresher {
    secrets: Arc<dyn SecretRepository>,
    tokens: GoogleTokenClient,
    refresh_endpoint: Option<RefreshEndpoint>,
    serial: AsyncMutex<()>,
}

/// The refresh destination when it differs from [`GoogleOAuthConfig::token_url`].
///
/// Android's offline exchange keeps the refresh token server-side; the device
/// sends only `client_id`, `grant_type`, and the refresh token to the Pige
/// endpoint, which holds the Google Web client secret.
#[derive(Clone)]
struct RefreshEndpoint {
    token_url: Url,
    client_id: String,
}

impl GoogleTokenRefresher {
    pub fn new(secrets: Arc<dyn SecretRepository>, oauth: GoogleOAuthConfig) -> Self {
        Self {
            secrets,
            tokens: GoogleTokenClient::new(oauth),
            refresh_endpoint: None,
            serial: AsyncMutex::new(()),
        }
    }

    /// Refreshes through an explicit endpoint instead of the OAuth token URL.
    /// Used by the Android offline exchange, where the server owns the secret.
    pub fn with_refresh_endpoint(
        secrets: Arc<dyn SecretRepository>,
        oauth: GoogleOAuthConfig,
        token_url: Url,
        client_id: impl Into<String>,
    ) -> Self {
        Self {
            secrets,
            tokens: GoogleTokenClient::new(oauth),
            refresh_endpoint: Some(RefreshEndpoint {
                token_url,
                client_id: client_id.into(),
            }),
            serial: AsyncMutex::new(()),
        }
    }
}

#[async_trait]
impl TokenRefresher for GoogleTokenRefresher {
    async fn refresh(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        ctx.ensure_active()?;
        let _serial = self.serial.lock().await;
        ctx.ensure_active()?;
        let envelope = self.secrets.load(&ctx.account)?.ok_or_else(|| {
            ProviderError::reauthenticate("Google sign-in has expired; sign in again")
        })?;
        let secret = match envelope.credentials {
            crate::auth::StoredCredentials::Google(secret) => secret,
            _ => {
                return Err(ProviderError::unsupported(
                    "this account does not use Google sign-in",
                ));
            }
        };

        let still_valid = secret
            .access_token
            .as_ref()
            .zip(secret.expires_at_ms)
            .is_some_and(|(_, expires_at)| expires_at > now_ms() + 60_000);
        if still_valid {
            return Ok(secret
                .access_token
                .expect("a still-valid access token was checked above"));
        }

        let refresh_token = secret.refresh_token.ok_or_else(|| {
            ProviderError::reauthenticate("Google sign-in has expired; sign in again")
        })?;
        let fresh = match &self.refresh_endpoint {
            Some(endpoint) => {
                self.tokens
                    .refresh_with(
                        endpoint.token_url.clone(),
                        &endpoint.client_id,
                        None,
                        &refresh_token,
                    )
                    .await?
            }
            None => self.tokens.refresh(&refresh_token).await?,
        };
        ctx.ensure_active()?;

        let scopes = if fresh.scopes.is_empty() {
            secret.scopes
        } else {
            fresh.scopes
        };
        let rotated = SecretEnvelope::google(crate::auth::GoogleSessionSecret {
            access_token: Some(fresh.access_token.clone()),
            refresh_token: fresh.refresh_token.or(Some(refresh_token)),
            expires_at_ms: Some(fresh.expires_at_ms),
            scopes,
        });
        // Persist first. If the write is fenced by an invalidation, the caller
        // gets an error and never receives a token derived from a secret this
        // account may no longer own.
        self.secrets.save(ctx, &rotated)?;
        Ok(fresh.access_token)
    }
}

/// Android's provider token source.
///
/// When the optional offline exchange persisted a refresh token, that token is
/// preferred and refreshed through the configured endpoint, so a restart keeps
/// working without the platform host. Only accounts with no stored refresh token
/// fall back to the platform-owned on-demand `acquire` model.
pub struct GoogleAndroidTokenSource {
    host: Arc<dyn GoogleAndroidHost>,
    secrets: Arc<dyn SecretRepository>,
    refresher: Arc<GoogleTokenRefresher>,
    account: AccountId,
    scopes: Vec<String>,
    cached: AsyncMutex<Option<(SecretString, i64)>>,
}

impl GoogleAndroidTokenSource {
    pub fn new(
        host: Arc<dyn GoogleAndroidHost>,
        secrets: Arc<dyn SecretRepository>,
        refresher: Arc<GoogleTokenRefresher>,
        account: AccountId,
        scopes: Vec<String>,
    ) -> Self {
        Self {
            host,
            secrets,
            refresher,
            account,
            scopes,
            cached: AsyncMutex::new(None),
        }
    }

    fn has_stored_refresh_token(&self) -> Result<bool, ProviderError> {
        let Some(envelope) = self.secrets.load(&self.account)? else {
            return Ok(false);
        };
        Ok(matches!(
            envelope.credentials,
            crate::auth::StoredCredentials::Google(secret) if secret.refresh_token.is_some()
        ))
    }
}

#[async_trait]
impl TokenSource for GoogleAndroidTokenSource {
    async fn access_token(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        ctx.ensure_active()?;
        // The offline exchange exists so refresh works after a restart. Prefer
        // the persisted refresh token; the host acquire path is only for grants
        // that intentionally kept no device refresh token.
        if self.has_stored_refresh_token()? {
            return self.refresher.refresh(ctx).await;
        }
        let mut cached = self.cached.lock().await;
        ctx.ensure_active()?;
        if let Some((token, expires_at)) = cached.as_ref()
            && *expires_at > now_ms() + 60_000
        {
            return Ok(token.clone());
        }
        let result = self.host.acquire(&self.account, &self.scopes).await?;
        result.validate()?;
        ctx.ensure_active()?;
        let token = result.access_token.clone();
        *cached = Some((token.clone(), result.expires_at_ms));
        Ok(token)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptState {
    Pending,
    Resolved,
}

struct DesktopAttempt {
    verifier: SecretString,
    state: String,
    redirect_uri: String,
}

struct Attempt {
    state: AttemptState,
    account: Account,
    ctx: OperationContext,
    expires_at_ms: i64,
    scopes: Vec<String>,
    staged: Option<StagedSecretRef>,
    desktop: Option<DesktopAttempt>,
    receiver: Option<Box<dyn DesktopRedirectReceiver>>,
    android_request_offline: bool,
}

enum AuthPlatform {
    Desktop(Arc<dyn DesktopRedirect>),
    Android {
        host: Arc<dyn GoogleAndroidHost>,
        offline: Option<AndroidOfflineExchange>,
    },
}

/// Interactive Google authentication for one platform flow.
///
/// Desktop and Android are separate constructors because S2 freezes separate
/// client registrations and flow mechanics; there is no shared redirect URI and
/// no desktop-app impersonation on Android.
pub struct GoogleAuthentication {
    secrets: Arc<dyn SecretRepository>,
    sessions: Arc<AccountSessionManager>,
    store: Arc<Store>,
    oauth: GoogleOAuthConfig,
    platform: AuthPlatform,
    attempt_ttl: Duration,
    attempts: HashMap<AuthAttemptId, Attempt>,
}

impl GoogleAuthentication {
    pub fn desktop(
        secrets: Arc<dyn SecretRepository>,
        sessions: Arc<AccountSessionManager>,
        store: Arc<Store>,
        mut oauth: GoogleOAuthConfig,
        redirect: Arc<dyn DesktopRedirect>,
        attempt_ttl: Duration,
    ) -> Self {
        // S2 §9.3: the desktop registration is a public client. Never send a
        // client secret even if the console issued one.
        oauth.client_secret = None;
        Self {
            secrets,
            sessions,
            store,
            oauth,
            platform: AuthPlatform::Desktop(redirect),
            attempt_ttl,
            attempts: HashMap::new(),
        }
    }

    pub fn android(
        secrets: Arc<dyn SecretRepository>,
        sessions: Arc<AccountSessionManager>,
        store: Arc<Store>,
        oauth: GoogleOAuthConfig,
        host: Arc<dyn GoogleAndroidHost>,
        offline: Option<AndroidOfflineExchange>,
        attempt_ttl: Duration,
    ) -> Self {
        Self {
            secrets,
            sessions,
            store,
            oauth,
            platform: AuthPlatform::Android { host, offline },
            attempt_ttl,
            attempts: HashMap::new(),
        }
    }

    /// A per-account token source for the Gmail/People adapters. Desktop uses
    /// F3's single-flight cache. Android prefers a persisted offline refresh
    /// token (refreshed through the configured exchange), falling back to the
    /// platform host only when the account has no refresh token.
    pub fn provider_token_source(&self, account: &AccountId) -> Arc<dyn TokenSource> {
        match &self.platform {
            AuthPlatform::Desktop(_) => Arc::new(CachedTokenSource::new(
                Arc::new(GoogleTokenRefresher::new(
                    self.secrets.clone(),
                    self.oauth.clone(),
                )),
                Duration::from_secs(60),
            )),
            AuthPlatform::Android { host, offline } => {
                let refresher = match offline {
                    Some(exchange) => GoogleTokenRefresher::with_refresh_endpoint(
                        self.secrets.clone(),
                        self.oauth.clone(),
                        exchange.exchange_url.clone(),
                        exchange.server_client_id.clone(),
                    ),
                    None => GoogleTokenRefresher::new(self.secrets.clone(), self.oauth.clone()),
                };
                Arc::new(GoogleAndroidTokenSource::new(
                    host.clone(),
                    self.secrets.clone(),
                    Arc::new(refresher),
                    account.clone(),
                    mail_scopes(),
                ))
            }
        }
    }

    fn provisional_account(&self, id: AccountId, identity: &GoogleIdentity) -> Account {
        let email = identity.email.clone().unwrap_or_default();
        let display_name = identity.display_name.clone().unwrap_or_default();
        let label = if !display_name.trim().is_empty() {
            display_name.clone()
        } else if !email.trim().is_empty() {
            email.clone()
        } else {
            "Google account".to_string()
        };
        Account {
            id,
            label,
            email,
            display_name,
            backend: MailBackend::gmail(identity.user_id.clone().unwrap_or_default()),
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        }
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
            if let AuthPlatform::Android { host, .. } = &self.platform {
                let _ = host.cancel(attempt);
            }
        }
    }

    fn ensure_attempt_active(&self, attempt: &Attempt) -> Result<(), ProviderError> {
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
                "the account changed during Google sign-in",
            ));
        }
        Ok(())
    }

    async fn finish(
        &mut self,
        attempt_id: &AuthAttemptId,
        secret: crate::auth::GoogleSessionSecret,
        persistence: SecretPersistence,
        identity: GoogleIdentity,
    ) -> Result<AuthProgress, ProviderError> {
        let Some(attempt) = self.attempts.get(attempt_id) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        // Re-check after the network exchange: the account may have been
        // invalidated while Google was answering. A stale attempt must not wait
        // for its TTL to release the staged candidate.
        if let Err(error) = self.ensure_attempt_active(attempt) {
            self.discard_attempt(attempt_id);
            return Err(error);
        }
        if !secret
            .scopes
            .iter()
            .any(|scope| scope == GMAIL_MODIFY_SCOPE)
        {
            self.discard_attempt(attempt_id);
            return Err(ProviderError::forbidden(
                "Google mail access was not granted",
            ));
        }

        let staged = self
            .secrets
            .stage(attempt_id, &SecretEnvelope::google(secret))?;
        let now = now_ms();
        if let Err(error) = self.store.insert_pending_auth(&PendingAuthRecord {
            attempt: attempt_id.clone(),
            account: Some(attempt.account.id.clone()),
            provider: ProviderId::from(GMAIL_PROVIDER),
            credential_ref: staged.as_str().to_string(),
            generation: attempt.ctx.generation,
            created_at_ms: now,
            expires_at_ms: attempt.expires_at_ms,
        }) {
            let _ = self.secrets.discard_staged(&staged);
            self.discard_attempt(attempt_id);
            return Err(crate::auth::store_error_to_provider(error));
        }

        let account_id = attempt.account.id.clone();
        if let Some(state) = self.attempts.get_mut(attempt_id) {
            state.state = AttemptState::Resolved;
            state.staged = Some(staged.clone());
        }
        let account = self.provisional_account(account_id, &identity);
        Ok(AuthProgress::Complete(AuthenticatedAccount {
            account,
            persistence,
            credential_ref: staged,
        }))
    }
}

#[async_trait]
impl AuthenticationService for GoogleAuthentication {
    async fn begin(&mut self, request: AuthRequest) -> Result<AuthProgress, ProviderError> {
        if request.provider != ProviderId::from(GMAIL_PROVIDER) {
            return Err(ProviderError::unsupported(
                "Google authentication for this provider",
            ));
        }
        self.reap_expired();

        let attempt_id = AuthAttemptId::new(uuid::Uuid::new_v4().to_string());
        let account_id = request.account.clone().unwrap_or_default();
        // A new attempt for the same account supersedes any older pending one.
        // Activity recreation must reuse the restored attempt id; a shell that
        // starts over cannot leave a stale consent callback able to complete.
        let superseded: Vec<AuthAttemptId> = self
            .attempts
            .iter()
            .filter(|(_, state)| {
                state.account.id == account_id && state.state == AttemptState::Pending
            })
            .map(|(attempt, _)| attempt.clone())
            .collect();
        for attempt in superseded {
            self.discard_attempt(&attempt);
        }
        let scopes = requested_scopes(request.enable_contacts);
        let ctx = self.sessions.context(&account_id, self.attempt_ttl)?;
        let expires_at_ms = now_ms().saturating_add(self.attempt_ttl.as_millis() as i64);
        let account = self.provisional_account(account_id, &GoogleIdentity::default());

        match &self.platform {
            AuthPlatform::Desktop(redirect) => {
                let flow = redirect.prepare()?;
                let verifier = random_urlsafe(32);
                let challenge = pkce_challenge(&verifier);
                let state = random_urlsafe(32);
                let mut url = self.oauth.authorization_url.clone();
                {
                    let mut query = url.query_pairs_mut();
                    query
                        .append_pair("client_id", &self.oauth.client_id)
                        .append_pair("redirect_uri", &flow.redirect_uri)
                        .append_pair("response_type", "code")
                        .append_pair("scope", &scopes.join(" "))
                        .append_pair("code_challenge", &challenge)
                        .append_pair("code_challenge_method", "S256")
                        .append_pair("state", &state)
                        .append_pair("access_type", "offline")
                        .append_pair("prompt", "consent")
                        .append_pair("include_granted_scopes", "true");
                }
                self.attempts.insert(
                    attempt_id.clone(),
                    Attempt {
                        state: AttemptState::Pending,
                        account,
                        ctx,
                        expires_at_ms,
                        scopes,
                        staged: None,
                        desktop: Some(DesktopAttempt {
                            verifier: SecretString::from(verifier),
                            state,
                            redirect_uri: flow.redirect_uri,
                        }),
                        receiver: flow.receiver,
                        android_request_offline: false,
                    },
                );
                Ok(AuthProgress::Challenge {
                    attempt: attempt_id,
                    challenge: AuthChallenge::Browser(BrowserChallenge {
                        url: url.to_string(),
                        expires_at_ms,
                    }),
                })
            }
            AuthPlatform::Android { host, offline } => {
                let request_offline_access = offline.is_some();
                let server_client_id = offline
                    .as_ref()
                    .map(|exchange| exchange.server_client_id.clone());
                let launch = AndroidAuthorizationRequest {
                    attempt: attempt_id.clone(),
                    scopes: scopes.clone(),
                    request_offline_access,
                    server_client_id,
                };
                self.attempts.insert(
                    attempt_id.clone(),
                    Attempt {
                        state: AttemptState::Pending,
                        account,
                        ctx,
                        expires_at_ms,
                        scopes,
                        staged: None,
                        desktop: None,
                        receiver: None,
                        android_request_offline: request_offline_access,
                    },
                );
                if let Err(error) = host.launch(launch).await {
                    self.discard_attempt(&attempt_id);
                    return Err(error);
                }
                Ok(AuthProgress::Challenge {
                    attempt: attempt_id.clone(),
                    challenge: AuthChallenge::Browser(BrowserChallenge {
                        // The Android shell does not open this as a URL. It is an
                        // opaque handle that tells the UI which platform flow is
                        // active; the consent UI is Play services' own.
                        url: format!("pige-google-android:{attempt_id}"),
                        expires_at_ms,
                    }),
                })
            }
        }
    }

    async fn respond(
        &mut self,
        attempt: &AuthAttemptId,
        answer: AuthAnswer,
    ) -> Result<AuthProgress, ProviderError> {
        let AuthAnswer::Browser(answer) = answer else {
            return Err(ProviderError::unsupported(
                "this Google flow only accepts a browser redirect",
            ));
        };
        let Some(state) = self.attempts.get(attempt) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        if state.state != AttemptState::Pending {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        }
        if now_ms() >= state.expires_at_ms {
            self.discard_attempt(attempt);
            return Err(ProviderError::not_found(
                "the authentication attempt has expired",
            ));
        }
        let Some(desktop) = state.desktop.as_ref() else {
            return Err(ProviderError::unsupported(
                "this Google flow does not accept a browser redirect",
            ));
        };
        let verifier = desktop.verifier.clone();
        let expected_state = desktop.state.clone();
        let redirect_uri = desktop.redirect_uri.clone();

        let redirect = Url::parse(&answer.redirect_url)
            .map_err(|_| ProviderError::transient("the Google sign-in redirect was invalid"))?;
        let expected_redirect = Url::parse(&redirect_uri)
            .map_err(|_| ProviderError::transient("the Google sign-in redirect is unavailable"))?;
        let same_origin = redirect.scheme() == expected_redirect.scheme()
            && redirect.host_str() == expected_redirect.host_str()
            && redirect.port_or_known_default() == expected_redirect.port_or_known_default()
            && redirect.path() == expected_redirect.path();
        if !same_origin {
            self.discard_attempt(attempt);
            return Err(ProviderError::forbidden(
                "the Google sign-in response did not match this attempt",
            ));
        }
        let mut error_code = None;
        let mut code = None;
        let mut returned_state = None;
        for (name, value) in redirect.query_pairs() {
            match name.as_ref() {
                "error" => error_code = Some(value.into_owned()),
                "code" => code = Some(value.into_owned()),
                "state" => returned_state = Some(value.into_owned()),
                _ => {}
            }
        }
        // The state is the authenticating proof. Only a response that carries
        // our state may cancel this attempt, so an unsolicited loopback request
        // with `?error=access_denied` cannot kill a live sign-in.
        if returned_state.as_deref() != Some(expected_state.as_str()) {
            self.discard_attempt(attempt);
            return Err(ProviderError::forbidden(
                "the Google sign-in response did not match this attempt",
            ));
        }
        if let Some(error) = error_code {
            self.discard_attempt(attempt);
            return Err(if error == "access_denied" {
                ProviderError::cancelled("Google sign-in was cancelled")
            } else {
                ProviderError::reauthenticate("Google sign-in did not complete; sign in again")
            });
        }
        let Some(code) = code.filter(|code| !code.is_empty()) else {
            self.discard_attempt(attempt);
            return Err(ProviderError::transient(
                "the Google sign-in returned no code",
            ));
        };
        if code.len() > 4096 {
            self.discard_attempt(attempt);
            return Err(ProviderError::transient(
                "the Google sign-in code was invalid",
            ));
        }

        // Fence the single-use code: if the account was invalidated or the
        // attempt cancelled while the browser was open, no network exchange may
        // run (which would burn the code) and no secret may be staged.
        let activation = match self.attempts.get(attempt) {
            Some(state) => self.ensure_attempt_active(state),
            None => Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            )),
        };
        if let Err(error) = activation {
            self.discard_attempt(attempt);
            return Err(error);
        }

        let tokens = GoogleTokenClient::new(self.oauth.clone())
            .exchange_code(&code, verifier.expose_secret(), &redirect_uri)
            .await;
        let tokens = match tokens {
            Ok(tokens) => tokens,
            Err(error) => {
                // A code is single-use, so a failed exchange ends the attempt.
                self.discard_attempt(attempt);
                return Err(error);
            }
        };
        let identity = tokens
            .id_token
            .as_ref()
            .map(|token| decode_identity(token.expose_secret()))
            .unwrap_or_default();
        let scopes = if tokens.scopes.is_empty() {
            self.attempts
                .get(attempt)
                .map(|state| state.scopes.clone())
                .unwrap_or_else(mail_scopes)
        } else {
            tokens.scopes
        };
        let persistence = if tokens.refresh_token.is_some() {
            SecretPersistence::Persistent
        } else {
            SecretPersistence::SessionOnly
        };
        self.finish(
            attempt,
            crate::auth::GoogleSessionSecret {
                access_token: Some(tokens.access_token),
                refresh_token: tokens.refresh_token,
                expires_at_ms: Some(tokens.expires_at_ms),
                scopes,
            },
            persistence,
            identity,
        )
        .await
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
        let Some(envelope) = self.secrets.load(account)? else {
            return Err(ProviderError::reauthenticate(
                "there is no stored Google session for this account",
            ));
        };
        if !matches!(
            envelope.credentials,
            crate::auth::StoredCredentials::Google(_)
        ) {
            return Err(ProviderError::reauthenticate(
                "there is no stored Google session for this account",
            ));
        }
        let persistent = self.secrets.load_persistent(account)?.is_some();
        Ok(AuthProgress::Complete(AuthenticatedAccount {
            account: self.provisional_account(account.clone(), &GoogleIdentity::default()),
            persistence: if persistent {
                SecretPersistence::Persistent
            } else {
                SecretPersistence::SessionOnly
            },
            credential_ref: StagedSecretRef::for_account(account)?,
        }))
    }

    async fn revoke(&mut self, account: &AccountId) -> Result<(), ProviderError> {
        let attempts: Vec<AuthAttemptId> = self
            .attempts
            .iter()
            .filter(|(_, state)| {
                state.account.id == *account && state.state == AttemptState::Pending
            })
            .map(|(attempt, _)| attempt.clone())
            .collect();
        for attempt in attempts {
            self.discard_attempt(&attempt);
        }
        self.secrets.delete(account)?;
        Ok(())
    }

    async fn await_platform_completion(
        &mut self,
        attempt: &AuthAttemptId,
    ) -> Result<AuthProgress, ProviderError> {
        if matches!(&self.platform, AuthPlatform::Desktop(_)) {
            self.await_desktop_callback(attempt).await
        } else {
            // Android results arrive through the JNI inbox; holding this service
            // across a wait would block Cancel. The shell polls
            // `deliver_platform_result` instead.
            Err(ProviderError::unsupported(
                "a blocking Android consent callback",
            ))
        }
    }

    fn has_platform_result(&self, attempt: &AuthAttemptId) -> bool {
        native_result_pending(attempt)
    }

    async fn deliver_platform_result(
        &mut self,
        attempt: &AuthAttemptId,
    ) -> Result<Option<AuthProgress>, ProviderError> {
        let Some(result) = take_native_result(attempt) else {
            return Ok(None);
        };
        self.deliver_android_result(result).await.map(Some)
    }
}

impl GoogleAuthentication {
    /// Complete an attempt with the validated result delivered by the Android
    /// host. The `attempt` field is the binding: a late result for an abandoned
    /// attempt (for example after Activity recreation started a new one) is
    /// rejected and cannot stage a secret.
    pub async fn deliver_android_result(
        &mut self,
        result: GoogleAuthorizationResult,
    ) -> Result<AuthProgress, ProviderError> {
        let Some(state) = self.attempts.get(&result.attempt) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        if state.state != AttemptState::Pending {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        }
        if now_ms() >= state.expires_at_ms {
            self.discard_attempt(&result.attempt);
            return Err(ProviderError::not_found(
                "the authentication attempt has expired",
            ));
        }
        self.ensure_attempt_active(state)?;
        let request_offline = state.android_request_offline;
        let offline = match &self.platform {
            AuthPlatform::Android { offline, .. } => offline.clone(),
            AuthPlatform::Desktop(_) => {
                return Err(ProviderError::unsupported(
                    "this Google flow does not accept an Android result",
                ));
            }
        };

        match result.result {
            GoogleAuthorizationOutcome::Cancelled => {
                self.discard_attempt(&result.attempt);
                Ok(AuthProgress::Cancelled)
            }
            GoogleAuthorizationOutcome::Failed => {
                self.discard_attempt(&result.attempt);
                Err(ProviderError::reauthenticate(
                    "Google sign-in did not complete; sign in again",
                ))
            }
            GoogleAuthorizationOutcome::Authorized(authorized) => {
                authorized.validate()?;
                let identity = authorized.identity.clone();
                let (secret, persistence) = if let Some(offline) = offline {
                    if !request_offline {
                        return Err(ProviderError::unsupported(
                            "this Google attempt did not request offline access",
                        ));
                    }
                    let Some(code) = authorized.server_auth_code else {
                        self.discard_attempt(&result.attempt);
                        return Err(ProviderError::reauthenticate(
                            "Google sign-in did not return offline access; sign in again",
                        ));
                    };
                    let exchanged = GoogleTokenClient::new(self.oauth.clone())
                        .exchange_server_auth_code(&code, &offline)
                        .await;
                    let exchanged = match exchanged {
                        Ok(exchanged) => exchanged,
                        Err(error) => {
                            self.discard_attempt(&result.attempt);
                            return Err(error);
                        }
                    };
                    let refresh_token = exchanged.refresh_token.ok_or_else(|| {
                        ProviderError::reauthenticate(
                            "Google sign-in did not return offline access; sign in again",
                        )
                    })?;
                    let scopes = if exchanged.scopes.is_empty() {
                        authorized.scopes.clone()
                    } else {
                        exchanged.scopes.clone()
                    };
                    (
                        crate::auth::GoogleSessionSecret {
                            access_token: Some(exchanged.access_token),
                            refresh_token: Some(refresh_token),
                            expires_at_ms: Some(exchanged.expires_at_ms),
                            scopes,
                        },
                        SecretPersistence::Persistent,
                    )
                } else {
                    (
                        crate::auth::GoogleSessionSecret {
                            access_token: Some(authorized.access_token.clone()),
                            refresh_token: None,
                            expires_at_ms: Some(authorized.expires_at_ms),
                            scopes: authorized.scopes.clone(),
                        },
                        SecretPersistence::SessionOnly,
                    )
                };
                self.finish(&result.attempt, secret, persistence, identity)
                    .await
            }
        }
    }

    /// Wait for the loopback redirect and answer the desktop attempt. The shell
    /// opens the browser from the returned challenge and then calls this; a
    /// shell that manages its own listener can call [`AuthenticationService::respond`]
    /// directly with the captured URL.
    pub async fn await_desktop_callback(
        &mut self,
        attempt: &AuthAttemptId,
    ) -> Result<AuthProgress, ProviderError> {
        let receiver = self
            .attempts
            .get_mut(attempt)
            .and_then(|state| state.receiver.take())
            .ok_or_else(|| {
                ProviderError::unsupported("this Google attempt has no loopback listener")
            })?;
        let timeout = self.attempt_ttl;
        let redirect_url = receiver.receive(timeout).await?;
        self.respond(
            attempt,
            AuthAnswer::Browser(crate::auth::BrowserAnswer { redirect_url }),
        )
        .await
    }
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Read the identity claims from the ID token Google returned directly to this
/// process over TLS at the token endpoint. The token is not a bearer credential
/// for local resources, so it is parsed for display identity only; Pige never
/// accepts an ID token supplied by a caller or browser.
fn decode_identity(id_token: &str) -> GoogleIdentity {
    let Some(payload) = id_token.split('.').nth(1) else {
        return GoogleIdentity::default();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return GoogleIdentity::default();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return GoogleIdentity::default();
    };
    GoogleIdentity {
        user_id: value
            .get("sub")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        email: value
            .get("email")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        display_name: value
            .get("name")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    }
}

#[derive(serde::Deserialize)]
struct WireTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}
