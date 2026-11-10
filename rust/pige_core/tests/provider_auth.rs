//! Protected auth envelopes, generation-safe account sessions, single-flight
//! token refresh, and the scripted fake authentication service.
//!
//! These tests are the task-F3 contract: a legacy raw password migrates without
//! loss, refresh runs exactly once for concurrent callers, a removed account
//! rejects rotated credentials, a session-only secret never reaches SQLite, a
//! credential-store failure is reported instead of silently dropped, a late or
//! cancelled auth answer is rejected, and every debug rendering redacts secrets.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};

use pige_core::account::AccountId;
use pige_core::auth::{
    AuthAnswer, AuthProgress, AuthRequest, AuthenticationService, CachedTokenSource,
    CredentialSecretRepository, FakeAuthScript, FakeAuthService, FakeTokenRefresher,
    GoogleSessionSecret, ProtonSessionSecret, SECRET_ENVELOPE_VERSION, SecretEnvelope,
    SecretPersistence, SecretRepository, StoredCredentials, TokenSource,
};
use pige_core::platform::{CredentialStore, fake::FakeCredentials};
use pige_core::providers::{
    AuthAttemptId, ErrorKind, IMAP_SMTP_PROVIDER, OperationContext, ProviderId,
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

    fn data_dir(&self) -> &Path {
        self.dir.path()
    }

    fn request(&self, account: Option<AccountId>) -> AuthRequest {
        AuthRequest {
            provider: ProviderId::from(IMAP_SMTP_PROVIDER),
            account,
            enable_contacts: false,
        }
    }

    fn stale_context(&self, account: &AccountId, generation: u64) -> OperationContext {
        OperationContext::new(
            account.clone(),
            generation,
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
    }
}

fn password_of(envelope: &SecretEnvelope) -> &str {
    match &envelope.credentials {
        StoredCredentials::Password(secret) => secret.expose_secret(),
        other => panic!("expected a password envelope, got {other:?}"),
    }
}

#[test]
fn legacy_password_migrates_without_loss() {
    let harness = Harness::new();
    let account = AccountId::new();

    // A password stored by an older build is a bare string, not an envelope.
    harness
        .credentials
        .set(&account, "hunter2-legacy")
        .expect("seed legacy password");

    let loaded = harness
        .secrets
        .load(&account)
        .expect("load legacy password")
        .expect("legacy password is present");
    assert_eq!(loaded.version, SECRET_ENVELOPE_VERSION);
    assert_eq!(password_of(&loaded), "hunter2-legacy");

    // Persisting the migrated envelope and reading it back must not lose it.
    let ctx = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");
    harness
        .secrets
        .save(&ctx, &loaded)
        .expect("persist envelope");

    let reloaded = harness
        .secrets
        .load(&account)
        .expect("reload")
        .expect("still present");
    assert_eq!(password_of(&reloaded), "hunter2-legacy");
    assert_eq!(reloaded.version, SECRET_ENVELOPE_VERSION);
}

#[tokio::test]
async fn refresh_is_single_flight() {
    let harness = Harness::new();
    let account = AccountId::new();
    let generation = harness.store.generation(&account).expect("generation");

    let refresher =
        Arc::new(FakeTokenRefresher::new("fresh-token").delay(Duration::from_millis(50)));
    let source = CachedTokenSource::new(refresher.clone(), Duration::from_secs(3600));

    let ctx = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");
    harness
        .secrets
        .save(&ctx, &SecretEnvelope::password("original"))
        .expect("seed active secret");

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let source = source.clone();
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move { source.access_token(&ctx).await }));
    }
    let mut tokens = Vec::new();
    for task in tasks {
        let token = task.await.expect("join").expect("token");
        tokens.push(token.expose_secret().to_string());
    }

    assert_eq!(refresher.calls(), 1, "refresh must run exactly once");
    assert!(tokens.iter().all(|token| token == "fresh-token"));

    // Invalidation both cancels the live context and fences its generation.
    harness.sessions.invalidate(&account).expect("invalidate");
    assert!(ctx.cancellation.is_cancelled());

    let rotated = SecretEnvelope::password("rotated");
    let stale = harness.stale_context(&account, generation);
    let error = harness
        .secrets
        .save(&stale, &rotated)
        .expect_err("a stale generation must not persist a rotated secret");
    assert_eq!(error.kind, ErrorKind::Conflict);
    let durable = harness
        .secrets
        .load(&account)
        .expect("load")
        .expect("present");
    assert_eq!(
        password_of(&durable),
        "original",
        "no durable write may happen after invalidation"
    );
}

#[tokio::test]
async fn cached_token_source_invalidate_forces_a_real_refresh() {
    let harness = Harness::new();
    let account = AccountId::new();
    let refresher = Arc::new(FakeTokenRefresher::new("fresh-token"));
    let source = CachedTokenSource::new(refresher.clone(), Duration::from_secs(3600));
    let ctx = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");

    let first = source.access_token(&ctx).await.expect("first token");
    assert_eq!(first.expose_secret(), "fresh-token");
    assert_eq!(refresher.calls(), 1);

    let cached = source.access_token(&ctx).await.expect("cached token");
    assert_eq!(cached.expose_secret(), "fresh-token");
    assert_eq!(refresher.calls(), 1, "an unexpired token is reused");

    source.invalidate().await;
    let refreshed = source.access_token(&ctx).await.expect("refreshed token");
    assert_eq!(refreshed.expose_secret(), "fresh-token");
    assert_eq!(
        refresher.calls(),
        2,
        "invalidate must drop the cache and force a real refresh"
    );
}

#[tokio::test]
async fn removed_account_rejects_rotated_secret() {
    let harness = Harness::new();
    let account = AccountId::new();
    let generation = harness.store.generation(&account).expect("generation");
    assert_eq!(generation, 1);

    let ctx = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");
    harness
        .secrets
        .save(&ctx, &SecretEnvelope::password("old-secret"))
        .expect("seed active secret");

    // Removing the account invalidates the session and durably increments the
    // generation, so a refresh that started earlier can never recreate it.
    harness.sessions.invalidate(&account).expect("invalidate");
    assert!(ctx.cancellation.is_cancelled());
    assert_eq!(harness.store.generation(&account).expect("generation"), 2);

    let stale = harness.stale_context(&account, generation);
    let error = harness
        .secrets
        .save(&stale, &SecretEnvelope::password("rotated"))
        .expect_err("stale generation must be rejected");
    assert_eq!(error.kind, ErrorKind::Conflict);

    let durable = harness
        .secrets
        .load(&account)
        .expect("load")
        .expect("present");
    assert_eq!(password_of(&durable), "old-secret");

    // A session at the new generation may still save (for example a re-auth).
    let fresh = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("fresh context");
    assert_eq!(fresh.generation, 2);
    harness
        .secrets
        .save(&fresh, &SecretEnvelope::password("after-reauth"))
        .expect("new generation saves");
}

#[tokio::test]
async fn stale_refresh_is_rejected_when_invalidate_lands_before_the_write() {
    let harness = Harness::new();
    let account = AccountId::new();
    let ctx = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");
    harness
        .secrets
        .save(&ctx, &SecretEnvelope::password("original"))
        .expect("seed active secret");

    // Reproduce the critical race: a durable invalidation lands after the
    // caller's context was minted (so a naive fence would already have passed)
    // but before the credential write. The generation check and the write are
    // now atomic under the account gate, so the write must observe the bump.
    let store = harness.store.clone();
    let invalidated = account.clone();
    harness.secrets.set_pre_write_hook(Arc::new(move || {
        store
            .invalidate_account(&invalidated)
            .expect("invalidate during save");
    }));

    let error = harness
        .secrets
        .save(&ctx, &SecretEnvelope::password("rotated"))
        .expect_err("a refresh fenced after invalidation must not persist");
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(harness.store.generation(&account).expect("generation"), 2);

    let durable = harness
        .secrets
        .load(&account)
        .expect("load")
        .expect("present");
    assert_eq!(
        password_of(&durable),
        "original",
        "a rotated credential must be impossible after invalidation"
    );
}

#[tokio::test]
async fn token_cache_is_scoped_to_generation() {
    let harness = Harness::new();
    let account = AccountId::new();
    let refresher = Arc::new(FakeTokenRefresher::new("gen-token"));
    let source = CachedTokenSource::new(refresher.clone(), Duration::from_secs(3600));

    let first = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("first context");
    assert_eq!(
        source
            .access_token(&first)
            .await
            .expect("token")
            .expose_secret(),
        "gen-token"
    );
    assert_eq!(refresher.calls(), 1);

    harness.sessions.invalidate(&account).expect("invalidate");
    assert!(first.cancellation.is_cancelled());
    // A cancelled context must not be served the cached token.
    assert!(source.access_token(&first).await.is_err());

    let second = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("second context");
    assert_eq!(second.generation, 2);
    // The cache was minted under generation 1; generation 2 must refresh rather
    // than be served the previous generation's token.
    assert_eq!(
        source
            .access_token(&second)
            .await
            .expect("token")
            .expose_secret(),
        "gen-token"
    );
    assert_eq!(refresher.calls(), 2);
}

#[tokio::test]
async fn context_advances_to_a_durable_generation_and_cancels_older_contexts() {
    let harness = Harness::new();
    let account = AccountId::new();
    let first = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("first context");
    assert_eq!(first.generation, 1);

    // Another process (or a restart) advances the durable generation.
    harness.store.bump_generation(&account).expect("bump");

    let second = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("second context");
    assert_eq!(second.generation, 2);
    assert!(
        first.cancellation.is_cancelled(),
        "a context older than the durable generation is cancelled"
    );
    assert!(
        !second.cancellation.is_cancelled(),
        "a context at the current generation survives"
    );
}

#[tokio::test]
async fn session_only_never_writes_sqlite() {
    let harness = Harness::new();
    let account = AccountId::new();

    let mut auth = FakeAuthService::new(
        harness.secrets.clone(),
        harness.sessions.clone(),
        harness.store.clone(),
    )
    .script(FakeAuthScript::password("session-secret").persistence(SecretPersistence::SessionOnly));

    let progress = auth
        .begin(harness.request(Some(account.clone())))
        .await
        .expect("begin");
    let AuthProgress::Challenge { attempt, .. } = progress else {
        panic!("expected a challenge");
    };
    let done = auth
        .respond(
            &attempt,
            AuthAnswer::Password(SecretString::from("session-secret")),
        )
        .await
        .expect("respond");
    let AuthProgress::Complete(authenticated) = done else {
        panic!("expected completion");
    };
    assert_eq!(authenticated.persistence, SecretPersistence::SessionOnly);

    // Committing a session-only credential keeps it in process memory only.
    let ctx = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");
    let staged = harness
        .secrets
        .load_staged(&authenticated.credential_ref)
        .expect("load staged")
        .expect("staged present");
    harness
        .secrets
        .save_session(&ctx, &staged)
        .expect("save session-only");
    harness
        .secrets
        .discard_staged(&authenticated.credential_ref)
        .expect("discard staged");

    assert!(
        harness.credentials.persistent_secret(&account).is_none(),
        "a session-only credential must not be persisted"
    );
    let loaded = harness
        .secrets
        .load(&account)
        .expect("load")
        .expect("session secret is readable in-process");
    assert_eq!(password_of(&loaded), "session-secret");

    assert_dir_lacks(harness.data_dir(), b"session-secret");
}

#[tokio::test]
async fn credential_store_failure_is_reported() {
    let harness = Harness::new();
    let account = AccountId::new();
    let ctx = harness
        .sessions
        .context(&account, Duration::from_secs(30))
        .expect("context");
    harness
        .secrets
        .save(&ctx, &SecretEnvelope::password("first"))
        .expect("seed");
    let persisted_before = harness
        .credentials
        .persistent_secret(&account)
        .expect("persisted value");

    // A single failed write is reported and cannot be mistaken for persisted.
    harness.credentials.fail_next_set();
    let error = harness
        .secrets
        .save(&ctx, &SecretEnvelope::password("second"))
        .expect_err("a failed write must be reported");
    assert!(!error.safe_message.is_empty());
    assert_eq!(
        harness.credentials.persistent_secret(&account).unwrap(),
        persisted_before,
        "the old value must survive a failed write"
    );

    // An unreachable store is a distinct, safe error and still writes nothing.
    harness.credentials.fail_unavailable();
    let error = harness
        .secrets
        .save(&ctx, &SecretEnvelope::password("third"))
        .expect_err("an unreachable store must be reported");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(
        harness.credentials.persistent_secret(&account).unwrap(),
        persisted_before
    );
}

#[tokio::test]
async fn late_attempt_answer_is_rejected() {
    let harness = Harness::new();
    let account = AccountId::new();
    let answer = || AuthAnswer::Password(SecretString::from("pw"));

    let mut auth = FakeAuthService::new(
        harness.secrets.clone(),
        harness.sessions.clone(),
        harness.store.clone(),
    )
    .script(FakeAuthScript::password("pw"));

    let AuthProgress::Challenge { attempt, .. } = auth
        .begin(harness.request(Some(account.clone())))
        .await
        .expect("begin")
    else {
        panic!("expected a challenge");
    };
    assert!(matches!(
        auth.respond(&attempt, answer()).await.expect("respond"),
        AuthProgress::Complete(_)
    ));
    assert!(
        auth.respond(&attempt, answer()).await.is_err(),
        "an answer to an already-resolved attempt is late"
    );

    // A cancelled attempt cannot be answered later.
    let AuthProgress::Challenge {
        attempt: cancelled, ..
    } = auth
        .begin(harness.request(Some(account.clone())))
        .await
        .expect("begin")
    else {
        panic!("expected a challenge");
    };
    auth.cancel(&cancelled).await.expect("cancel");
    assert!(
        auth.respond(&cancelled, answer()).await.is_err(),
        "a cancelled attempt must reject a late answer"
    );

    // An expired attempt is reaped and rejected.
    let mut expiring = FakeAuthService::new(
        harness.secrets.clone(),
        harness.sessions.clone(),
        harness.store.clone(),
    )
    .script(FakeAuthScript::password("pw").ttl(Duration::ZERO));
    let AuthProgress::Challenge {
        attempt: expired, ..
    } = expiring
        .begin(harness.request(Some(account)))
        .await
        .expect("begin")
    else {
        panic!("expected a challenge");
    };
    assert!(
        expiring.respond(&expired, answer()).await.is_err(),
        "an expired attempt must reject its answer"
    );
    assert!(
        harness
            .secrets
            .load_staged(&staged_ref_for(&expired))
            .expect("load staged")
            .is_none(),
        "an expired attempt must clean up its staged secret"
    );
}

#[tokio::test]
async fn terminal_attempts_sweep_their_staged_secret() {
    let harness = Harness::new();
    let account = AccountId::new();
    let mut auth = FakeAuthService::new(
        harness.secrets.clone(),
        harness.sessions.clone(),
        harness.store.clone(),
    )
    .script(FakeAuthScript::password("pw"));

    let AuthProgress::Challenge { attempt, .. } = auth
        .begin(harness.request(Some(account.clone())))
        .await
        .expect("begin")
    else {
        panic!("expected a challenge");
    };
    let AuthProgress::Complete(authenticated) = auth
        .respond(&attempt, AuthAnswer::Password(SecretString::from("pw")))
        .await
        .expect("respond")
    else {
        panic!("expected completion");
    };
    // F10 owns commit; until then the staged secret is still present.
    assert!(
        harness
            .secrets
            .load_staged(&authenticated.credential_ref)
            .expect("load staged")
            .is_some()
    );

    // A completed-but-uncommitted attempt must still be swept, or its staged
    // secret leaks forever.
    let reaped = auth.reap_expired().expect("reap");
    assert!(reaped.contains(&attempt));
    assert!(
        harness
            .secrets
            .load_staged(&authenticated.credential_ref)
            .expect("load staged")
            .is_none(),
        "a terminal attempt's staged secret is discarded"
    );
    assert!(
        harness
            .store
            .pending_auth(&attempt)
            .expect("pending")
            .is_none()
    );
}

#[tokio::test]
async fn begin_sweeps_orphaned_pending_auth_rows_and_their_staged_secrets() {
    let harness = Harness::new();
    let account = AccountId::new();
    let attempt = AuthAttemptId::new("22222222-2222-2222-2222-222222222222");
    let staged = pige_core::auth::StagedSecretRef::for_attempt(&attempt).expect("staged ref");

    // A pending row and staged secret left behind by a previous run.
    harness
        .store
        .insert_pending_auth(&pige_core::store::PendingAuthRecord {
            attempt: attempt.clone(),
            account: Some(account.clone()),
            provider: ProviderId::from(IMAP_SMTP_PROVIDER),
            credential_ref: staged.as_str().to_string(),
            generation: 1,
            created_at_ms: 0,
            expires_at_ms: 0,
        })
        .expect("seed orphan row");
    harness
        .credentials
        .set_session(&AccountId(staged.as_str().to_string()), "orphan-secret")
        .expect("seed orphan staged secret");

    let mut auth = FakeAuthService::new(
        harness.secrets.clone(),
        harness.sessions.clone(),
        harness.store.clone(),
    )
    .script(FakeAuthScript::password("pw"));
    auth.begin(harness.request(Some(account)))
        .await
        .expect("begin");

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
            .load_staged(&staged)
            .expect("load staged")
            .is_none(),
        "begin sweeps an orphaned staged secret"
    );
}

#[test]
fn cross_process_lease_fences_a_lost_writer() {
    let harness = Harness::new();
    let account = AccountId::new();
    // A second manager on the same store stands in for another process.
    let other = Arc::new(pige_core::auth::AccountSessionManager::new(
        harness.store.clone(),
    ));

    let lease = harness
        .sessions
        .acquire_lease(&account, Duration::from_secs(60))
        .expect("first lease");
    assert!(
        other
            .acquire_lease(&account, Duration::from_secs(60))
            .is_err(),
        "a live lease is exclusive across processes"
    );

    harness.sessions.release_lease(&lease).expect("release");
    let takeover = other
        .acquire_lease(&account, Duration::from_secs(60))
        .expect("takeover");
    assert!(takeover.fencing > lease.fencing);
    assert!(
        harness.sessions.ensure_lease(&lease).is_err(),
        "a superseded lease must not be usable for a durable write"
    );
    other.ensure_lease(&takeover).expect("live lease verifies");
}

#[test]
fn auth_debug_redacts_all_secret_variants() {
    let envelopes = [
        (
            SecretEnvelope::password("password-secret"),
            vec!["password-secret"],
        ),
        (
            SecretEnvelope::google(GoogleSessionSecret {
                access_token: Some(SecretString::from("google-access")),
                refresh_token: Some(SecretString::from("google-refresh")),
                expires_at_ms: Some(1_700_000_000),
                scopes: vec!["https://mail.google.com/".to_string()],
            }),
            vec!["google-access", "google-refresh"],
        ),
        (
            SecretEnvelope::proton(ProtonSessionSecret {
                uid: Some("proton-uid".to_string()),
                access_token: Some(SecretString::from("proton-access")),
                refresh_token: Some(SecretString::from("proton-refresh")),
                skp: Some(SecretString::from("proton-skp")),
            }),
            vec!["proton-access", "proton-refresh", "proton-skp"],
        ),
    ];

    for (envelope, secrets) in &envelopes {
        let rendered = format!("{envelope:?}");
        for secret in secrets {
            assert!(
                !rendered.contains(secret),
                "debug output leaked {secret:?}: {rendered}"
            );
        }
        assert!(
            rendered.to_ascii_lowercase().contains("redacted"),
            "debug output should mark redaction: {rendered}"
        );
    }

    let answers = [
        AuthAnswer::Password(SecretString::from("answer-password")),
        AuthAnswer::Totp(SecretString::from("answer-totp")),
        AuthAnswer::MailboxPassword(SecretString::from("answer-mailbox")),
    ];
    for (answer, secret) in answers
        .iter()
        .zip(["answer-password", "answer-totp", "answer-mailbox"])
    {
        let rendered = format!("{answer:?}");
        assert!(
            !rendered.contains(secret),
            "auth answer debug leaked {secret:?}: {rendered}"
        );
    }
}

/// The staged reference an attempt would have produced, reconstructed from the
/// attempt id so a test can prove its entry was cleaned up.
fn staged_ref_for(attempt: &AuthAttemptId) -> pige_core::auth::StagedSecretRef {
    pige_core::auth::StagedSecretRef::for_attempt(attempt).expect("valid attempt id")
}

fn assert_dir_lacks(dir: &Path, needle: &[u8]) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            assert_dir_lacks(&path, needle);
        } else if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !contains_bytes(&bytes, needle),
                "secret leaked into {}",
                path.display()
            );
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[allow(dead_code)]
fn db_path(harness: &Harness) -> PathBuf {
    harness.dir.path().join("pige.db")
}
