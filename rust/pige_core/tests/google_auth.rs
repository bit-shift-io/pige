//! Task G1: Google authorization and token ownership.
//!
//! No test reaches Google. Desktop exchange and refresh are driven against a
//! local HTTP token endpoint; the Android flow is driven through the same
//! validated host-result contract the JNI bridge uses. The live Google cases
//! remain BLOCKED on registered OAuth clients and a disposable account (see
//! `docs/provider-feasibility.md` §9).

#![cfg(feature = "provider-google")]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;

use pige_core::account::AccountId;
use pige_core::auth::{
    AuthAnswer, AuthProgress, AuthRequest, AuthenticationService, BrowserAnswer,
    CredentialSecretRepository, GoogleSessionSecret, SecretEnvelope, SecretRepository,
    StoredCredentials, TokenRefresher,
};
use pige_core::platform::CredentialStore;
use pige_core::platform::fake::FakeCredentials;
use pige_core::providers::google::{
    AndroidAuthorizationRequest, AndroidOfflineExchange, CONTACTS_OTHER_READONLY_SCOPE,
    CONTACTS_READONLY_SCOPE, GMAIL_MODIFY_SCOPE, GoogleAndroidHost, GoogleAndroidResult,
    GoogleAuthentication, GoogleAuthorizationOutcome, GoogleAuthorizationResult, GoogleEndpoints,
    GoogleIdentity, GoogleOAuthConfig, GoogleProviderFactory, GoogleTokenRefresher,
    GoogleTokenSourceProvider, deliver_native_result, take_native_result,
};
use pige_core::providers::{
    AuthAttemptId, ErrorKind, GMAIL_PROVIDER, OperationContext, ProviderId,
};
use pige_core::store::Store;
use tempfile::TempDir;

struct Harness {
    dir: TempDir,
    store: Arc<Store>,
    credentials: Arc<FakeCredentials>,
    secrets: Arc<CredentialSecretRepository>,
    sessions: Arc<pige_core::auth::AccountSessionManager>,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(Store::open(dir.path().join("pige.db")).expect("open store"));
        store.migrate().expect("migrate store");
        let credentials = Arc::new(FakeCredentials::default());
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials.clone() as Arc<dyn CredentialStore>,
            store.clone(),
        ));
        let sessions = Arc::new(pige_core::auth::AccountSessionManager::new(store.clone()));
        Self {
            dir,
            store,
            credentials,
            secrets,
            sessions,
        }
    }

    fn request(&self, account: &AccountId, enable_contacts: bool) -> AuthRequest {
        AuthRequest {
            provider: ProviderId::from(GMAIL_PROVIDER),
            account: Some(account.clone()),
            enable_contacts,
        }
    }

    fn oauth(&self, token_url: Url) -> GoogleOAuthConfig {
        GoogleOAuthConfig {
            client_id: "desktop-test-client".to_string(),
            client_secret: None,
            authorization_url: Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
                .expect("authorization url"),
            token_url,
        }
    }

    fn desktop(&self, token_url: Url, ttl: Duration) -> GoogleAuthentication {
        GoogleAuthentication::desktop(
            self.secrets.clone(),
            self.sessions.clone(),
            self.store.clone(),
            self.oauth(token_url),
            Arc::new(FixedRedirect::new("http://127.0.0.1:45678")),
            ttl,
        )
    }

    fn android(
        &self,
        token_url: Url,
        host: Arc<RecordingAndroidHost>,
        ttl: Duration,
    ) -> GoogleAuthentication {
        GoogleAuthentication::android(
            self.secrets.clone(),
            self.sessions.clone(),
            self.store.clone(),
            self.oauth(token_url),
            host,
            None,
            ttl,
        )
    }

    fn android_offline(
        &self,
        token_url: Url,
        host: Arc<RecordingAndroidHost>,
        exchange_url: Url,
        ttl: Duration,
    ) -> GoogleAuthentication {
        GoogleAuthentication::android(
            self.secrets.clone(),
            self.sessions.clone(),
            self.store.clone(),
            self.oauth(token_url),
            host,
            Some(AndroidOfflineExchange {
                server_client_id: "web-client-id".to_string(),
                exchange_url,
            }),
            ttl,
        )
    }

    fn context(&self, account: &AccountId) -> OperationContext {
        self.sessions
            .context(account, Duration::from_secs(30))
            .expect("context")
    }
}

async fn begin_desktop(
    auth: &mut GoogleAuthentication,
    request: AuthRequest,
) -> (AuthAttemptId, Url) {
    let progress = auth.begin(request).await.expect("begin");
    let AuthProgress::Challenge { attempt, challenge } = progress else {
        panic!("expected a browser challenge");
    };
    let pige_core::auth::AuthChallenge::Browser(browser) = challenge else {
        panic!("expected a browser challenge");
    };
    let url = Url::parse(&browser.url).expect("valid authorization url");
    (attempt, url)
}

fn query_value(url: &Url, key: &str) -> String {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("missing query parameter {key}"))
}

fn redirect_with(redirect_uri: &str, code: &str, state: &str) -> String {
    format!("{redirect_uri}/?code={code}&state={state}")
}

fn staged_ref(attempt: &AuthAttemptId) -> pige_core::auth::StagedSecretRef {
    pige_core::auth::StagedSecretRef::for_attempt(attempt).expect("valid attempt")
}

fn google_secret(envelope: &SecretEnvelope) -> &GoogleSessionSecret {
    match &envelope.credentials {
        StoredCredentials::Google(secret) => secret,
        other => panic!("expected a Google envelope, got {other:?}"),
    }
}

struct FixedRedirect {
    uri: String,
}

impl FixedRedirect {
    fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }
}

impl pige_core::providers::google::DesktopRedirect for FixedRedirect {
    fn prepare(
        &self,
    ) -> Result<
        pige_core::providers::google::DesktopRedirectFlow,
        pige_core::providers::ProviderError,
    > {
        Ok(pige_core::providers::google::DesktopRedirectFlow {
            redirect_uri: self.uri.clone(),
            receiver: None,
        })
    }
}

struct RecordingAndroidHost {
    launches: Mutex<Vec<AndroidAuthorizationRequest>>,
    cancelled: Mutex<Vec<AuthAttemptId>>,
    acquires: Mutex<usize>,
}

impl RecordingAndroidHost {
    fn new() -> Self {
        Self {
            launches: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            acquires: Mutex::new(0),
        }
    }

    fn launches(&self) -> Vec<AndroidAuthorizationRequest> {
        self.launches.lock().expect("launches").clone()
    }

    fn acquires(&self) -> usize {
        *self.acquires.lock().expect("acquires")
    }
}

#[async_trait]
impl GoogleAndroidHost for RecordingAndroidHost {
    async fn launch(
        &self,
        request: AndroidAuthorizationRequest,
    ) -> Result<(), pige_core::providers::ProviderError> {
        self.launches.lock().expect("launches").push(request);
        Ok(())
    }

    fn cancel(&self, attempt: &AuthAttemptId) -> Result<(), pige_core::providers::ProviderError> {
        self.cancelled
            .lock()
            .expect("cancelled")
            .push(attempt.clone());
        Ok(())
    }

    async fn acquire(
        &self,
        _account: &AccountId,
        _scopes: &[String],
    ) -> Result<GoogleAndroidResult, pige_core::providers::ProviderError> {
        *self.acquires.lock().expect("acquires") += 1;
        Err(pige_core::providers::ProviderError::reauthenticate(
            "no live Android grant in this test",
        ))
    }
}

fn android_identity() -> GoogleIdentity {
    GoogleIdentity {
        user_id: Some("google-user-1".to_string()),
        email: Some("person@example.com".to_string()),
        display_name: Some("Person".to_string()),
    }
}

fn android_authorized() -> GoogleAndroidResult {
    GoogleAndroidResult {
        access_token: SecretString::from("android-access-token"),
        expires_at_ms: now_ms() + 3_600_000,
        scopes: vec![GMAIL_MODIFY_SCOPE.to_string()],
        identity: android_identity(),
        server_auth_code: None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[tokio::test]
async fn desktop_pkce_state_mismatch_rejected() {
    let mock = MockServer::start(vec![token_ok(
        "access-1",
        Some("refresh-1"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let mut auth = harness.desktop(mock.url(), Duration::from_secs(300));

    let (attempt, url) = begin_desktop(&mut auth, harness.request(&account, false)).await;
    assert_eq!(query_value(&url, "code_challenge_method"), "S256");
    assert!(!query_value(&url, "code_challenge").is_empty());
    assert!(!query_value(&url, "state").is_empty());

    let forged = redirect_with("http://127.0.0.1:45678", "auth-code", "not-the-state");
    let error = auth
        .respond(
            &attempt,
            AuthAnswer::Browser(BrowserAnswer {
                redirect_url: forged,
            }),
        )
        .await
        .expect_err("a mismatched state must be rejected");

    assert_eq!(error.kind, ErrorKind::Forbidden);
    assert!(mock.requests().is_empty(), "no token exchange may run");
    assert!(
        harness
            .store
            .pending_auth(&attempt)
            .expect("pending")
            .is_none()
    );
    assert!(
        harness
            .secrets
            .load_staged(&staged_ref(&attempt))
            .expect("load staged")
            .is_none()
    );
}

#[tokio::test]
async fn cancelled_attempt_cannot_store_secret() {
    let mock = MockServer::start(vec![token_ok(
        "access-1",
        Some("refresh-1"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let mut auth = harness.desktop(mock.url(), Duration::from_secs(300));

    let (attempt, url) = begin_desktop(&mut auth, harness.request(&account, false)).await;
    let state = query_value(&url, "state");
    auth.cancel(&attempt).await.expect("cancel");

    let error = auth
        .respond(
            &attempt,
            AuthAnswer::Browser(BrowserAnswer {
                redirect_url: redirect_with("http://127.0.0.1:45678", "auth-code", &state),
            }),
        )
        .await
        .expect_err("a cancelled attempt must not accept an answer");

    assert_eq!(error.kind, ErrorKind::NotFound);
    assert!(
        mock.requests().is_empty(),
        "cancellation must not exchange a code"
    );
    assert!(
        harness
            .secrets
            .load_staged(&staged_ref(&attempt))
            .expect("load staged")
            .is_none()
    );
    assert!(harness.secrets.load(&account).expect("load").is_none());
    assert!(
        harness
            .store
            .pending_auth(&attempt)
            .expect("pending")
            .is_none()
    );
}

#[tokio::test]
async fn stale_desktop_attempt_makes_no_exchange() {
    let mock = MockServer::start(vec![token_ok(
        "access-1",
        Some("refresh-1"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let mut auth = harness.desktop(mock.url(), Duration::from_secs(300));

    let (attempt, url) = begin_desktop(&mut auth, harness.request(&account, false)).await;
    let state = query_value(&url, "state");
    // The account is re-authenticated/invalidated while the browser is open.
    harness.sessions.invalidate(&account).expect("invalidate");

    let error = auth
        .respond(
            &attempt,
            AuthAnswer::Browser(BrowserAnswer {
                redirect_url: redirect_with("http://127.0.0.1:45678", "auth-code", &state),
            }),
        )
        .await
        .expect_err("a stale attempt must not exchange its single-use code");

    assert!(
        matches!(error.kind, ErrorKind::Cancelled | ErrorKind::Conflict),
        "unexpected error kind {:?}",
        error.kind
    );
    assert!(
        mock.requests().is_empty(),
        "a stale attempt must not burn the single-use code"
    );
    assert!(
        harness
            .store
            .pending_auth(&attempt)
            .expect("pending")
            .is_none(),
        "the stale attempt is discarded immediately"
    );
    assert!(
        harness
            .secrets
            .load_staged(&staged_ref(&attempt))
            .expect("load staged")
            .is_none()
    );
}

#[tokio::test]
async fn stale_finish_discards_the_attempt_instead_of_waiting_for_ttl() {
    let harness = Harness::new();
    let account = AccountId::new();
    // The token endpoint invalidates the account after the pre-exchange fence
    // but before the response returns, so `finish` observes a stale attempt.
    let sessions = harness.sessions.clone();
    let hook_account = account.clone();
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = sessions.invalidate(&hook_account);
    });
    let mock = MockServer::start_with_hook(
        vec![token_ok("access-1", Some("refresh-1"), GMAIL_MODIFY_SCOPE)],
        Some(hook),
    )
    .await;
    let mut auth = harness.desktop(mock.url(), Duration::from_secs(300));

    let (attempt, url) = begin_desktop(&mut auth, harness.request(&account, false)).await;
    let state = query_value(&url, "state");
    let error = auth
        .respond(
            &attempt,
            AuthAnswer::Browser(BrowserAnswer {
                redirect_url: redirect_with("http://127.0.0.1:45678", "auth-code", &state),
            }),
        )
        .await
        .expect_err("the invalidated attempt cannot finish");
    assert!(
        matches!(error.kind, ErrorKind::Cancelled | ErrorKind::Conflict),
        "unexpected error kind {:?}",
        error.kind
    );
    assert_eq!(mock.requests().len(), 1);
    assert!(
        harness
            .store
            .pending_auth(&attempt)
            .expect("pending")
            .is_none(),
        "finish failure must discard the pending attempt, not wait for the TTL"
    );
    assert!(
        harness
            .secrets
            .load_staged(&staged_ref(&attempt))
            .expect("load staged")
            .is_none()
    );

    // A recycled callback cannot resurrect the discarded attempt.
    let again = auth
        .respond(
            &attempt,
            AuthAnswer::Browser(BrowserAnswer {
                redirect_url: redirect_with("http://127.0.0.1:45678", "auth-code", &state),
            }),
        )
        .await
        .expect_err("a discarded attempt stays gone");
    assert_eq!(again.kind, ErrorKind::NotFound);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn android_result_after_activity_recreation_is_bound() {
    let mock = MockServer::start(vec![token_ok("unused", None, GMAIL_MODIFY_SCOPE)]).await;
    let harness = Harness::new();
    let account = AccountId::new();
    let host = Arc::new(RecordingAndroidHost::new());
    let mut auth = harness.android(mock.url(), host.clone(), Duration::from_secs(300));

    let AuthProgress::Challenge { attempt: first, .. } = auth
        .begin(harness.request(&account, false))
        .await
        .expect("first begin")
    else {
        panic!("expected a challenge");
    };

    // Activity recreation starts a second attempt before the first consent
    // result returns. The old result must not complete the new attempt.
    let AuthProgress::Challenge {
        attempt: second, ..
    } = auth
        .begin(harness.request(&account, false))
        .await
        .expect("second begin")
    else {
        panic!("expected a challenge");
    };
    assert_ne!(first, second);
    assert_eq!(host.launches().len(), 2);

    let stale = auth
        .deliver_android_result(GoogleAuthorizationResult {
            attempt: first.clone(),
            result: GoogleAuthorizationOutcome::Authorized(android_authorized()),
        })
        .await
        .expect_err("a result for an abandoned attempt must be rejected");
    assert_eq!(stale.kind, ErrorKind::NotFound);

    let completed = auth
        .deliver_android_result(GoogleAuthorizationResult {
            attempt: second.clone(),
            result: GoogleAuthorizationOutcome::Authorized(android_authorized()),
        })
        .await
        .expect("the live attempt completes");
    let AuthProgress::Complete(completed) = completed else {
        panic!("expected completion");
    };
    assert_eq!(completed.account.id, account);
    assert_eq!(completed.account.email, "person@example.com");
    assert_eq!(completed.account.label, "Person");
    let staged = harness
        .secrets
        .load_staged(&completed.credential_ref)
        .expect("staged")
        .expect("present");
    let secret = google_secret(&staged);
    assert_eq!(
        secret
            .access_token
            .as_ref()
            .map(ExposeSecret::expose_secret),
        Some("android-access-token")
    );
    assert!(
        harness
            .store
            .pending_auth(&second)
            .expect("pending")
            .is_some()
    );
}

#[tokio::test]
async fn android_offline_request_carries_server_client_id() {
    let mock = MockServer::start(vec![token_ok("unused", None, GMAIL_MODIFY_SCOPE)]).await;
    let harness = Harness::new();
    let account = AccountId::new();
    let host = Arc::new(RecordingAndroidHost::new());
    let mut auth = harness.android_offline(
        mock.url(),
        host.clone(),
        mock.url(),
        Duration::from_secs(300),
    );

    auth.begin(harness.request(&account, false))
        .await
        .expect("begin");
    let launches = host.launches();
    assert_eq!(launches.len(), 1);
    assert!(launches[0].request_offline_access);
    assert_eq!(
        launches[0].server_client_id.as_deref(),
        Some("web-client-id"),
        "Kotlin requestOfflineAccess needs the Web client id"
    );

    // Without the exchange, the host is told not to request offline access and
    // gets no server client id.
    let plain_host = Arc::new(RecordingAndroidHost::new());
    let mut plain = harness.android(mock.url(), plain_host.clone(), Duration::from_secs(300));
    plain
        .begin(harness.request(&AccountId::new(), false))
        .await
        .expect("begin without offline");
    let launches = plain_host.launches();
    assert!(!launches[0].request_offline_access);
    assert_eq!(launches[0].server_client_id, None);
}

#[tokio::test]
async fn android_offline_refresh_used_after_restart() {
    let mock = MockServer::start(vec![token_ok(
        "android-access-2",
        Some("android-refresh-2"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let oauth = harness.oauth(mock.url());
    let ctx = harness.context(&account);
    harness
        .secrets
        .save(
            &ctx,
            &SecretEnvelope::google(GoogleSessionSecret {
                access_token: Some(SecretString::from("expired-access")),
                refresh_token: Some(SecretString::from("android-refresh-1")),
                expires_at_ms: Some(1),
                scopes: vec![GMAIL_MODIFY_SCOPE.to_string()],
            }),
        )
        .expect("seed offline refresh token");

    // Restart: only the persisted credential and the platform credential store
    // survive; the in-process authentication/attempt state does not.
    let db_path = harness.dir.path().join("pige.db");
    drop(ctx);
    drop(harness.sessions);
    drop(harness.secrets);
    drop(harness.store);

    let restarted_store = Arc::new(Store::open(&db_path).expect("reopen store"));
    restarted_store.migrate().expect("migrate reopened store");
    let restarted_sessions = Arc::new(pige_core::auth::AccountSessionManager::new(
        restarted_store.clone(),
    ));
    let restarted_secrets = Arc::new(CredentialSecretRepository::with_gate(
        harness.credentials.clone() as Arc<dyn CredentialStore>,
        restarted_store.clone(),
    ));
    let host = Arc::new(RecordingAndroidHost::new());
    let auth = GoogleAuthentication::android(
        restarted_secrets.clone(),
        restarted_sessions.clone(),
        restarted_store,
        oauth,
        host.clone(),
        Some(AndroidOfflineExchange {
            server_client_id: "web-client-id".to_string(),
            exchange_url: mock.url(),
        }),
        Duration::from_secs(300),
    );

    let ctx = restarted_sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");
    let source = auth.provider_token_source(&account);
    let token = source
        .access_token(&ctx)
        .await
        .expect("the persisted refresh token must work after restart");
    assert_eq!(token.expose_secret(), "android-access-2");
    assert_eq!(
        host.acquires(),
        0,
        "a stored refresh token must take precedence over host acquire"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(form_value(&requests[0].body, "grant_type"), "refresh_token");
    assert_eq!(form_value(&requests[0].body, "client_id"), "web-client-id");
    assert_eq!(
        form_value(&requests[0].body, "refresh_token"),
        "android-refresh-1"
    );
    assert!(
        form_value(&requests[0].body, "client_secret").is_empty(),
        "the device never sends the server's Google secret"
    );

    let stored = restarted_secrets
        .load(&account)
        .expect("load")
        .expect("present");
    assert_eq!(
        google_secret(&stored)
            .refresh_token
            .as_ref()
            .map(ExposeSecret::expose_secret),
        Some("android-refresh-2"),
        "the exchange's rotated refresh token is persisted"
    );
}

#[tokio::test]
async fn android_without_refresh_token_falls_back_to_host_acquire() {
    let mock = MockServer::start(Vec::new()).await;
    let harness = Harness::new();
    let account = AccountId::new();
    let host = Arc::new(RecordingAndroidHost::new());
    let auth = harness.android_offline(
        mock.url(),
        host.clone(),
        mock.url(),
        Duration::from_secs(300),
    );

    let ctx = harness.context(&account);
    let source = auth.provider_token_source(&account);
    let error = source
        .access_token(&ctx)
        .await
        .expect_err("no refresh token and no live platform grant");
    assert_eq!(error.kind, ErrorKind::Reauthenticate);
    assert_eq!(host.acquires(), 1);
    assert!(
        mock.requests().is_empty(),
        "without a refresh token the exchange must not be called"
    );
}

#[tokio::test]
async fn desktop_public_client_never_sends_a_secret() {
    let mock = MockServer::start(vec![token_ok(
        "public-access",
        Some("public-refresh"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let mut oauth = harness.oauth(mock.url());
    // A Desktop app registration may display a secret, but S2 §9.3 freezes the
    // public-client flow: PKCE is sent, the secret is not.
    oauth.client_secret = Some(SecretString::from("should-not-be-sent"));
    let mut auth = GoogleAuthentication::desktop(
        harness.secrets.clone(),
        harness.sessions.clone(),
        harness.store.clone(),
        oauth,
        Arc::new(FixedRedirect::new("http://127.0.0.1:45678")),
        Duration::from_secs(300),
    );

    let (attempt, url) = begin_desktop(&mut auth, harness.request(&account, false)).await;
    let state = query_value(&url, "state");
    auth.respond(
        &attempt,
        AuthAnswer::Browser(BrowserAnswer {
            redirect_url: redirect_with("http://127.0.0.1:45678", "auth-code", &state),
        }),
    )
    .await
    .expect("public-client exchange");
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        form_value(&requests[0].body, "grant_type"),
        "authorization_code"
    );
    assert!(form_value(&requests[0].body, "client_secret").is_empty());
    assert!(!form_value(&requests[0].body, "code_verifier").is_empty());
}

#[tokio::test]
async fn google_tokens_survive_restart() {
    let mock = MockServer::start(vec![token_ok(
        "desktop-access",
        Some("desktop-refresh"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let mut auth = harness.desktop(mock.url(), Duration::from_secs(300));

    let (attempt, url) = begin_desktop(&mut auth, harness.request(&account, false)).await;
    let state = query_value(&url, "state");
    let AuthProgress::Complete(completed) = auth
        .respond(
            &attempt,
            AuthAnswer::Browser(BrowserAnswer {
                redirect_url: redirect_with("http://127.0.0.1:45678", "auth-code", &state),
            }),
        )
        .await
        .expect("respond")
    else {
        panic!("expected completion");
    };

    // F10 commits the staged candidate; G1 verifies it survives a process
    // restart as a persistent per-account envelope.
    let ctx = harness.context(&account);
    let staged = harness
        .secrets
        .load_staged(&completed.credential_ref)
        .expect("load staged")
        .expect("present");
    harness.secrets.save(&ctx, &staged).expect("persist token");
    harness
        .secrets
        .discard_staged(&completed.credential_ref)
        .expect("discard staged");

    let db_path = harness.dir.path().join("pige.db");
    drop(auth);
    drop(harness.sessions);
    drop(harness.secrets);
    drop(harness.store);

    let reopened = Arc::new(Store::open(&db_path).expect("reopen store"));
    reopened.migrate().expect("migrate reopened store");
    let restart_sessions = Arc::new(pige_core::auth::AccountSessionManager::new(
        reopened.clone(),
    ));
    let restart_secrets = CredentialSecretRepository::with_gate(
        harness.credentials.clone() as Arc<dyn CredentialStore>,
        reopened,
    );
    let loaded = restart_secrets
        .load(&account)
        .expect("reload token")
        .expect("token survives");
    let secret = google_secret(&loaded);
    assert_eq!(
        secret
            .access_token
            .as_ref()
            .map(ExposeSecret::expose_secret),
        Some("desktop-access")
    );
    assert_eq!(
        secret
            .refresh_token
            .as_ref()
            .map(ExposeSecret::expose_secret),
        Some("desktop-refresh")
    );
    let _ = restart_sessions.context(&account, Duration::from_secs(30));
    assert_dir_lacks(harness.dir.path(), b"desktop-access");
    assert_dir_lacks(harness.dir.path(), b"desktop-refresh");
}

#[tokio::test]
async fn refresh_rotation_saved_before_reuse() {
    let mock = MockServer::start(vec![
        token_ok("access-2", Some("refresh-2"), GMAIL_MODIFY_SCOPE),
        token_ok("access-3", Some("refresh-3"), GMAIL_MODIFY_SCOPE),
    ])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let ctx = harness.context(&account);
    harness
        .secrets
        .save(
            &ctx,
            &SecretEnvelope::google(GoogleSessionSecret {
                access_token: Some(SecretString::from("stale-access")),
                refresh_token: Some(SecretString::from("refresh-1")),
                expires_at_ms: Some(1),
                scopes: vec![GMAIL_MODIFY_SCOPE.to_string()],
            }),
        )
        .expect("seed token");

    let refresher = GoogleTokenRefresher::new(harness.secrets.clone(), harness.oauth(mock.url()));
    let first = refresher.refresh(&ctx).await.expect("first refresh");
    assert_eq!(first.expose_secret(), "access-2");

    // The rotation is durable before the fresh access token is returned: the
    // persisted credential already names refresh-2, so no later caller can
    // reuse refresh-1.
    let stored = harness
        .secrets
        .load(&account)
        .expect("load")
        .expect("present");
    let secret = google_secret(&stored);
    assert_eq!(
        secret
            .refresh_token
            .as_ref()
            .map(ExposeSecret::expose_secret),
        Some("refresh-2")
    );

    // Expire the freshly persisted access token so the next call must actually
    // refresh again; the point is that it presents refresh-2, not refresh-1.
    let mut stored = harness
        .secrets
        .load(&account)
        .expect("load")
        .expect("present");
    if let StoredCredentials::Google(secret) = &mut stored.credentials {
        secret.expires_at_ms = Some(1);
    }
    harness.secrets.save(&ctx, &stored).expect("expire access");

    let second = refresher.refresh(&ctx).await.expect("second refresh");
    assert_eq!(second.expose_secret(), "access-3");
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(form_value(&requests[0].body, "refresh_token"), "refresh-1");
    assert_eq!(form_value(&requests[1].body, "refresh_token"), "refresh-2");
}

#[tokio::test]
async fn refresh_token_is_not_returned_before_rotation_is_persisted() {
    let mock = MockServer::start(vec![token_ok(
        "rotated-access",
        Some("rotated-refresh"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let ctx = harness.context(&account);
    harness
        .secrets
        .save(
            &ctx,
            &SecretEnvelope::google(GoogleSessionSecret {
                access_token: Some(SecretString::from("expired-access")),
                refresh_token: Some(SecretString::from("refresh-1")),
                expires_at_ms: Some(1),
                scopes: vec![GMAIL_MODIFY_SCOPE.to_string()],
            }),
        )
        .expect("seed token");

    // Force the credential write fence to fail after the token endpoint has
    // already answered. Ordering proof: the fresh access token must not be
    // handed to the caller unless the rotated refresh token was persisted first.
    let sessions = harness.sessions.clone();
    let hook_account = account.clone();
    harness.secrets.set_pre_write_hook(Arc::new(move || {
        let _ = sessions.invalidate(&hook_account);
    }));

    let refresher = GoogleTokenRefresher::new(harness.secrets.clone(), harness.oauth(mock.url()));
    let error = refresher
        .refresh(&ctx)
        .await
        .expect_err("persistence is a precondition for returning the access token");
    assert!(
        matches!(error.kind, ErrorKind::Cancelled | ErrorKind::Conflict),
        "unexpected error kind {:?}",
        error.kind
    );
    assert_eq!(mock.requests().len(), 1);

    let stored = harness
        .secrets
        .load(&account)
        .expect("load")
        .expect("present");
    let secret = google_secret(&stored);
    assert_eq!(
        secret
            .refresh_token
            .as_ref()
            .map(ExposeSecret::expose_secret),
        Some("refresh-1"),
        "an unpersisted rotation is never observable"
    );
    assert_eq!(
        secret
            .access_token
            .as_ref()
            .map(ExposeSecret::expose_secret),
        Some("expired-access"),
        "the caller cannot reuse an access token whose rotation was not saved"
    );
}

#[tokio::test]
async fn refresh_failure_reauths_without_password_prompt() {
    let mock = MockServer::start(vec![MockResponse::json(
        400,
        r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let ctx = harness.context(&account);
    harness
        .secrets
        .save(
            &ctx,
            &SecretEnvelope::google(GoogleSessionSecret {
                access_token: Some(SecretString::from("expired-access")),
                refresh_token: Some(SecretString::from("revoked-refresh")),
                expires_at_ms: Some(1),
                scopes: vec![GMAIL_MODIFY_SCOPE.to_string()],
            }),
        )
        .expect("seed token");

    let refresher = GoogleTokenRefresher::new(harness.secrets.clone(), harness.oauth(mock.url()));
    let error = refresher
        .refresh(&ctx)
        .await
        .expect_err("a revoked grant must require reauthentication");
    assert_eq!(error.kind, ErrorKind::Reauthenticate);
    assert!(
        error.safe_message.to_ascii_lowercase().contains("sign in"),
        "the error should direct the user to sign in again: {}",
        error.safe_message
    );
}

#[tokio::test]
async fn denied_contacts_keeps_mail_scope() {
    let mock = MockServer::start(vec![token_ok(
        "mail-access",
        Some("mail-refresh"),
        GMAIL_MODIFY_SCOPE,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let mut auth = harness.desktop(mock.url(), Duration::from_secs(300));

    let (attempt, url) = begin_desktop(&mut auth, harness.request(&account, true)).await;
    let requested = query_value(&url, "scope");
    assert!(requested.contains(GMAIL_MODIFY_SCOPE));
    assert!(requested.contains(CONTACTS_READONLY_SCOPE));
    assert!(requested.contains(CONTACTS_OTHER_READONLY_SCOPE));

    let state = query_value(&url, "state");
    let AuthProgress::Complete(completed) = auth
        .respond(
            &attempt,
            AuthAnswer::Browser(BrowserAnswer {
                redirect_url: redirect_with("http://127.0.0.1:45678", "auth-code", &state),
            }),
        )
        .await
        .expect("mail still completes when contacts consent is denied")
    else {
        panic!("expected completion");
    };
    assert!(completed.account.contact_sources.is_empty());

    let staged = harness
        .secrets
        .load_staged(&completed.credential_ref)
        .expect("staged")
        .expect("present");
    let secret = google_secret(&staged);
    assert_eq!(secret.scopes, vec![GMAIL_MODIFY_SCOPE.to_string()]);
    assert!(!secret.scopes.iter().any(|scope| scope.contains("contacts")));
}

#[tokio::test]
async fn expired_attempt_callback_rejected() {
    let mock = MockServer::start(vec![token_ok("unused", None, GMAIL_MODIFY_SCOPE)]).await;
    let harness = Harness::new();
    let account = AccountId::new();
    let host = Arc::new(RecordingAndroidHost::new());
    let mut auth = harness.android(mock.url(), host, Duration::ZERO);

    let AuthProgress::Challenge { attempt, .. } = auth
        .begin(harness.request(&account, false))
        .await
        .expect("begin")
    else {
        panic!("expected a challenge");
    };

    let error = auth
        .deliver_android_result(GoogleAuthorizationResult {
            attempt: attempt.clone(),
            result: GoogleAuthorizationOutcome::Authorized(android_authorized()),
        })
        .await
        .expect_err("an expired attempt must reject its callback");
    assert_eq!(error.kind, ErrorKind::NotFound);
    assert!(
        harness
            .store
            .pending_auth(&attempt)
            .expect("pending")
            .is_none()
    );
    assert!(
        harness
            .secrets
            .load_staged(&staged_ref(&attempt))
            .expect("staged")
            .is_none()
    );
}

#[tokio::test]
async fn sensitive_http_errors_are_redacted() {
    let mock = MockServer::start(vec![MockResponse::json(
        400,
        r#"{"error":"invalid_grant","error_description":"secret refresh-secret-value and leaked-access-token","access_token":"leaked-access-token"}"#,
    )])
    .await;
    let harness = Harness::new();
    let account = AccountId::new();
    let ctx = harness.context(&account);
    harness
        .secrets
        .save(
            &ctx,
            &SecretEnvelope::google(GoogleSessionSecret {
                access_token: None,
                refresh_token: Some(SecretString::from("refresh-secret-value")),
                expires_at_ms: Some(1),
                scopes: vec![GMAIL_MODIFY_SCOPE.to_string()],
            }),
        )
        .expect("seed token");

    let refresher = GoogleTokenRefresher::new(harness.secrets.clone(), harness.oauth(mock.url()));
    let error = refresher
        .refresh(&ctx)
        .await
        .expect_err("the mock endpoint rejects the refresh");

    let display = format!("{error}");
    let debug = format!("{error:?}");
    let detail = error.source_detail().unwrap_or("");
    for rendered in [&display, &debug, detail, &error.safe_message] {
        for secret in [
            "refresh-secret-value",
            "leaked-access-token",
            "invalid_grant",
        ] {
            assert!(
                !rendered.contains(secret),
                "HTTP error leaked {secret:?} in {rendered:?}"
            );
        }
    }
}

#[test]
fn android_jni_payload_is_validated_before_queueing() {
    let attempt = AuthAttemptId::new("33333333-3333-3333-3333-333333333333");
    let payload = serde_json::json!({
        "attempt": attempt.as_str(),
        "result": {
            "kind": "authorized",
            "accessToken": "jni-access",
            "expiresAtMs": now_ms() + 3_600_000,
            "scopes": [GMAIL_MODIFY_SCOPE],
            "identity": { "email": "person@example.com" }
        }
    })
    .to_string();
    assert_eq!(
        deliver_native_result(&payload).expect("valid payload"),
        attempt
    );
    let queued = take_native_result(&attempt).expect("queued result");
    let GoogleAuthorizationOutcome::Authorized(queued) = queued.result else {
        panic!("expected an authorized result");
    };
    assert_eq!(queued.identity.email.as_deref(), Some("person@example.com"));

    // Arbitrary browser/redirect parameters are not part of the frozen bridge.
    let unexpected = serde_json::json!({
        "attempt": attempt.as_str(),
        "result": {
            "kind": "authorized",
            "accessToken": "jni-access",
            "expiresAtMs": now_ms() + 3_600_000,
            "scopes": [GMAIL_MODIFY_SCOPE],
            "redirectUrl": "http://127.0.0.1/forged"
        }
    })
    .to_string();
    assert!(deliver_native_result(&unexpected).is_err());

    // A grant without the mail scope cannot become a Gmail session.
    let no_mail = serde_json::json!({
        "attempt": attempt.as_str(),
        "result": {
            "kind": "authorized",
            "accessToken": "jni-access",
            "expiresAtMs": now_ms() + 3_600_000,
            "scopes": [CONTACTS_READONLY_SCOPE]
        }
    })
    .to_string();
    assert!(deliver_native_result(&no_mail).is_err());
    assert!(take_native_result(&attempt).is_none());
}

struct FixedSource;

#[async_trait]
impl pige_core::auth::TokenSource for FixedSource {
    async fn access_token(
        &self,
        _ctx: &OperationContext,
    ) -> Result<SecretString, pige_core::providers::ProviderError> {
        Ok(SecretString::from("fixed-access"))
    }
}

struct FixedTokenSources;

impl GoogleTokenSourceProvider for FixedTokenSources {
    fn token_source(
        &self,
        _account: &AccountId,
    ) -> Result<Arc<dyn pige_core::auth::TokenSource>, pige_core::providers::ProviderError> {
        Ok(Arc::new(FixedSource))
    }
}

#[test]
fn google_factory_shares_one_token_source_per_account() {
    let harness = Harness::new();
    let endpoints = GoogleEndpoints {
        gmail_base: Url::parse("http://127.0.0.1:9/gmail").unwrap(),
        people_base: Url::parse("http://127.0.0.1:9/people").unwrap(),
    };
    let factory = GoogleProviderFactory::new(harness.sessions.clone(), endpoints)
        .with_token_source_provider(Arc::new(FixedTokenSources));
    let first_account = AccountId::new();
    let second_account = AccountId::new();
    let first = factory.token_source(&first_account).expect("first source");
    let again = factory.token_source(&first_account).expect("cached source");
    let other = factory.token_source(&second_account).expect("other source");
    assert!(Arc::ptr_eq(&first, &again), "one source per account");
    assert!(
        !Arc::ptr_eq(&first, &other),
        "accounts do not share a cache"
    );
    assert!(factory.sessions().generation(&first_account).is_ok());
}

// --- Local token endpoint ---------------------------------------------------

#[derive(Clone, Debug)]
struct HttpRequest {
    body: String,
}

struct MockResponse {
    status: u16,
    body: String,
}

impl MockResponse {
    fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.to_string(),
        }
    }
}

struct MockServer {
    url: Url,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    _task: JoinHandle<()>,
}

impl MockServer {
    async fn start(responses: Vec<MockResponse>) -> Self {
        Self::start_with_hook(responses, None).await
    }

    async fn start_with_hook(
        responses: Vec<MockResponse>,
        hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock token endpoint");
        let address = listener.local_addr().expect("mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let recorded = recorded.clone();
                let queue = queue.clone();
                let hook = hook.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    recorded.lock().expect("record request").push(request);
                    if let Some(hook) = &hook {
                        hook();
                    }
                    let response = queue
                        .lock()
                        .expect("response queue")
                        .pop_front()
                        .unwrap_or(MockResponse::json(500, r#"{"error":"unexpected"}"#));
                    let head = format!(
                        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.status,
                        response.body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(response.body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            url: Url::parse(&format!("http://{address}/token")).expect("token url"),
            requests,
            _task: task,
        }
    }

    fn url(&self) -> Url {
        self.url.clone()
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<HttpRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < header_end + 4 + content_length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_start = header_end + 4;
    let body =
        String::from_utf8_lossy(&buffer[body_start..buffer.len().min(body_start + content_length)])
            .to_string();
    Some(HttpRequest { body })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn token_ok(access_token: &str, refresh_token: Option<&str>, scope: &str) -> MockResponse {
    let body = serde_json::json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expires_in": 3600,
        "scope": scope,
        "token_type": "Bearer",
    });
    MockResponse::json(200, &body.to_string())
}

fn form_value(body: &str, key: &str) -> String {
    url::form_urlencoded::parse(body.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

fn assert_dir_lacks(dir: &Path, needle: &[u8]) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read {dir:?}: {error}"));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            assert_dir_lacks(&path, needle);
        } else if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "secret leaked into {}",
                path.display()
            );
        }
    }
}
