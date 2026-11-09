//! Task F10: account lifecycle, recoverable backend switching, and the
//! provider-boundary baseline.
//!
//! These are the RED tests for the lifecycle contract:
//!
//! - a backend switch is refused while any non-idempotent submission is pending
//!   (including ambiguous legacy failures);
//! - a switch preserves the `AccountId` and drafts but rebuilds the provider
//!   remote cache and checkpoints;
//! - a crash between activating the new secret and publishing the new config is
//!   recovered at startup so the config and the secret are always a pair;
//! - restore, removal, factory reset, and account edits cancel older
//!   generations so late work cannot resurrect an account or its secret;
//! - a contact-source replacement clears only that source's cache;
//! - an ordinary legacy IMAP account still sends after the upgrade.

use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret;
use tokio::sync::{Mutex, Semaphore};

use pige_core::account::{ContactSourceConfig, MailBackend, Security, ServerConfig};
use pige_core::api::{self, AccountInput, Config, DB_FILE_NAME};
use pige_core::auth::{
    AuthAnswer, AuthProgress, AuthRequest, AuthenticationService, CredentialSecretRepository,
    FakeAuthScript, FakeAuthService, SecretEnvelope, SecretPersistence, SecretRepository,
    StagedSecretRef, StoredCredentials,
};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::platform::{BackupStore, CredentialStore};
use pige_core::providers::{
    AuthAttemptId, ContactSourceId, IMAP_SMTP_PROVIDER, JobKey, JobKind, ProviderCursor,
    ProviderFactory, ProviderId, fake::FakeProviderFactory,
};
use pige_core::send::{
    OutgoingMessage, SubmissionState, outbox_entry, queue_submission, save_draft,
};
use pige_core::store::{PendingAuthRecord, Store};
use pige_core::sync::provider::account_job;
use pige_core::transport::{FakeTransport, FolderState, RemoteFolder};
use pige_core::{
    Account, AccountId, Address, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId,
    StoredMessage, ThreadId, Uid,
};
use tempfile::TempDir;

/// The process-wide API state is global, so every lifecycle test serializes on
/// this lock and re-initializes its own state.
static TEST_LOCK: Mutex<()> = Mutex::const_new(());

const CONTACTS_SOURCE_A: &str = "source-a";
const CONTACTS_SOURCE_B: &str = "source-b";

struct Harness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
    transport: Arc<FakeTransportFactory>,
    providers: Arc<dyn ProviderFactory>,
    backups: Arc<FakeBackupStore>,
    auth: Option<AuthenticationServiceHandle>,
}

type AuthenticationServiceHandle = Arc<Mutex<Box<dyn AuthenticationService>>>;

impl Harness {
    /// An IMAP-capable API backed by the scripted transport factory, so tests
    /// can drive the real provider sync/submission path.
    fn imap() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        let transport = Arc::new(FakeTransportFactory::default());
        let providers: Arc<dyn ProviderFactory> = Arc::new(
            pige_core::providers::imap::imap_registry(transport.clone(), credentials.clone()),
        );
        let auth = Some(build_auth(&dir, &credentials));
        let harness = Self {
            dir,
            credentials,
            transport,
            providers,
            backups: Arc::new(FakeBackupStore::new()),
            auth,
        };
        harness.init();
        harness
    }

    /// An API that dispatches every provider to the scripted fake. A backend
    /// switch to a native-shaped account can therefore be exercised without an
    /// actual native adapter.
    fn provider_neutral() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        let transport = Arc::new(FakeTransportFactory::default());
        let providers: Arc<dyn ProviderFactory> = Arc::new(FakeProviderFactory::new());
        let harness = Self {
            dir,
            credentials,
            transport,
            providers,
            backups: Arc::new(FakeBackupStore::new()),
            auth: None,
        };
        harness.init();
        harness
    }

    fn config(&self) -> Config {
        Config {
            data_dir: self.dir.path().join("data"),
            cache_dir: self.dir.path().join("cache"),
            config_dir: self.dir.path().join("config"),
            credentials: self.credentials.clone(),
            notifier: Arc::new(FakeNotifier::new()),
            opener: Arc::new(FakeOpener::new()),
            providers: self.providers.clone(),
            transport: self.transport.clone(),
            backups: self.backups.clone(),
            auth: self.auth.clone(),
        }
    }

    fn init(&self) {
        api::reset_for_tests();
        api::init(self.config()).expect("init api");
    }

    /// Simulate a process restart: drop the in-process API state while keeping
    /// the temp dirs and credential store, then init again.
    fn restart(&self) {
        api::reset_for_tests();
        api::init(self.config()).expect("reinit api");
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.dir.path().join("data").join(DB_FILE_NAME)
    }

    fn store(&self) -> Arc<Store> {
        let store = Arc::new(Store::open(self.db_path()).expect("open store"));
        store.migrate().expect("migrate store");
        store
    }

    fn secrets(&self) -> Arc<CredentialSecretRepository> {
        Arc::new(CredentialSecretRepository::with_gate(
            self.credentials.clone(),
            self.store(),
        ))
    }

    fn stage(&self, password: &str) -> StagedSecretRef {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let attempt = AuthAttemptId::new(format!("00000000-0000-0000-0000-{n:012}"));
        self.secrets()
            .stage(&attempt, &SecretEnvelope::password(password))
            .expect("stage secret")
    }

    fn stored_password(&self, account: &AccountId) -> Option<String> {
        let envelope = self.secrets().load(account).expect("load secret")?;
        match envelope.credentials {
            StoredCredentials::Password(password) => Some(password.expose_secret().to_string()),
            other => panic!("expected a password credential, got {other:?}"),
        }
    }

    async fn add_imap(&self, label: &str) -> AccountId {
        let input = AccountInput {
            label: label.to_string(),
            email: format!("{}@example.com", label.to_ascii_lowercase()),
            display_name: label.to_string(),
            username: format!("{}@example.com", label.to_ascii_lowercase()),
            password: format!("secret-{label}"),
            imap: server("imap.example.com", 993, Security::Tls),
            smtp: server("smtp.example.com", 465, Security::Tls),
        };
        let (id, _) = api::add_account_and_id(input).await.expect("add account");
        id
    }

    /// A replacement account with the same id but a native-shaped backend.
    fn gmail_target(existing: &Account) -> Account {
        let mut target = existing.clone();
        target.backend = MailBackend::gmail("google-uid-1");
        target.contact_sources = Vec::new();
        target
    }

    fn authenticated(
        target: Account,
        reference: StagedSecretRef,
    ) -> pige_core::auth::AuthenticatedAccount {
        Self::authenticated_with(target, reference, SecretPersistence::Persistent)
    }

    fn authenticated_with(
        target: Account,
        reference: StagedSecretRef,
        persistence: SecretPersistence,
    ) -> pige_core::auth::AuthenticatedAccount {
        pige_core::auth::AuthenticatedAccount {
            account: target,
            persistence,
            credential_ref: reference,
        }
    }
}

fn build_auth(dir: &TempDir, credentials: &Arc<FakeCredentials>) -> AuthenticationServiceHandle {
    let store =
        Arc::new(Store::open(dir.path().join("data").join(DB_FILE_NAME)).expect("auth store"));
    store.migrate().expect("migrate auth store");
    let secrets = Arc::new(CredentialSecretRepository::with_gate(
        credentials.clone(),
        store.clone(),
    ));
    let sessions = Arc::new(pige_core::auth::AccountSessionManager::new(store.clone()));
    let service = FakeAuthService::new(secrets, sessions, store)
        .script(FakeAuthScript::password("auth-password"));
    Arc::new(Mutex::new(
        Box::new(service) as Box<dyn AuthenticationService>
    ))
}

fn server(host: &str, port: u16, security: Security) -> ServerConfig {
    ServerConfig {
        host: host.to_string(),
        port,
        security,
    }
}

fn account_input_changed_host(label: &str) -> AccountInput {
    AccountInput {
        label: label.to_string(),
        email: format!("{}@example.com", label.to_ascii_lowercase()),
        display_name: label.to_string(),
        username: format!("{}@example.com", label.to_ascii_lowercase()),
        password: String::new(),
        imap: server("imap.changed.example", 993, Security::Tls),
        smtp: server("smtp.changed.example", 465, Security::Tls),
    }
}

fn outgoing(account: &AccountId, subject: &str) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account.clone(),
        to: vec![Address::new(None, "charles@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_string(),
        body: "Hello after the upgrade.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

fn folder(account: &AccountId, id: &str, name: &str, kind: FolderKind) -> Folder {
    Folder {
        id: FolderId(id.to_string()),
        account_id: account.clone(),
        name: name.to_string(),
        delimiter: Some("/".to_string()),
        kind,
        unread: 1,
        total: 1,
        uidvalidity: Some(1),
        uidnext: Some(2),
    }
}

fn message(account: &AccountId, folder: &Folder, uid: u32) -> StoredMessage {
    StoredMessage {
        id: MessageId(format!("{}-message-{uid}", folder.id.0)),
        account_id: account.clone(),
        folder_id: Some(folder.id.clone()),
        uid: Some(Uid(uid)),
        uidvalidity: Some(1),
        header: MessageHeader {
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: format!("Subject {uid}"),
            from: vec![Address::new(None, "sender@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            reply_to: Vec::new(),
            bcc: Vec::new(),
            date: i64::from(uid),
            size: 64,
        },
        flags: Flags::default(),
        thread_id: ThreadId(format!("thread-{uid}")),
        snippet: format!("snippet {uid}"),
        has_attachments: false,
    }
}

fn remote_folder(name: &str, kind: FolderKind) -> RemoteFolder {
    RemoteFolder {
        name: name.to_string(),
        delimiter: Some("/".to_string()),
        kind,
    }
}

fn fake_transport() -> FakeTransport {
    FakeTransport {
        folders: vec![
            remote_folder("INBOX", FolderKind::Inbox),
            remote_folder("Sent", FolderKind::Sent),
        ],
        states: [
            (
                "INBOX".to_string(),
                FolderState {
                    uidvalidity: 1,
                    uidnext: 2,
                    exists: 1,
                },
            ),
            (
                "Sent".to_string(),
                FolderState {
                    uidvalidity: 1,
                    uidnext: 1,
                    exists: 0,
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..FakeTransport::default()
    }
}

fn backup_document(accounts: Vec<Account>) -> Vec<u8> {
    let doc = pige_core::config::ConfigDocument {
        accounts,
        ..pige_core::config::ConfigDocument::default()
    };
    serde_json::to_vec_pretty(&doc).expect("serialize document")
}

fn assert_password(is: &str) -> impl Fn(&AccountId, &Harness) {
    let expected = is.to_string();
    move |account, harness| {
        assert_eq!(
            harness.stored_password(account).as_deref(),
            Some(expected.as_str()),
            "active credential must be {expected}"
        );
    }
}

#[tokio::test]
async fn switch_blocked_for_each_pending_state() {
    let _guard = TEST_LOCK.lock().await;

    for state in [
        SubmissionState::Queued,
        SubmissionState::Preparing,
        SubmissionState::Submitting,
        SubmissionState::Accepted,
        SubmissionState::Uncertain,
    ] {
        let harness = Harness::provider_neutral();
        let account = harness.add_imap("Personal").await;
        let store = harness.store();
        let id = queue_submission(&store, outgoing(&account, "Report"), Vec::new())
            .expect("queue submission");
        store
            .set_outbox_state(&id, state, Some("pending"))
            .expect("set state");

        let error = api::can_switch_backend(&account).expect_err("pending submission blocks");
        assert!(
            error.to_lowercase().contains("submission"),
            "unexpected error for {state:?}: {error}"
        );

        let existing = api::account(&account).expect("account");
        let target = Harness::gmail_target(&existing);
        let staged = harness.stage("new-secret");
        let error = api::switch_backend(&account, Harness::authenticated(target, staged))
            .await
            .expect_err("pending submission blocks the switch itself");
        assert!(error.to_lowercase().contains("submission"), "{error}");
        assert!(
            api::account(&account).unwrap().imap_smtp().is_some(),
            "the backend must not change while blocked"
        );
    }

    // Ambiguous legacy failures and pre-provider `sending` rows are blockers
    // even though they no longer decode to a current pending state name.
    for legacy in ["failed", "sending", "sent_local"] {
        let harness = Harness::provider_neutral();
        let account = harness.add_imap("Legacy").await;
        let store = harness.store();
        let id = queue_submission(&store, outgoing(&account, "Legacy"), Vec::new())
            .expect("queue submission");
        store
            .set_outbox_state(&id, SubmissionState::Queued, None)
            .unwrap();
        let connection = rusqlite::Connection::open(harness.db_path()).unwrap();
        connection
            .execute(
                "UPDATE outbox SET state = ?1 WHERE id = ?2",
                [legacy, id.0.as_str()],
            )
            .unwrap();
        drop(connection);
        let error = api::can_switch_backend(&account)
            .expect_err("an ambiguous legacy failure blocks the switch");
        assert!(
            error.to_lowercase().contains("submission"),
            "{legacy}: {error}"
        );
    }

    // Drafts, completed deliveries, and evidenced rejections are not blockers.
    for allowed in [
        SubmissionState::Draft,
        SubmissionState::Complete,
        SubmissionState::Rejected,
    ] {
        let harness = Harness::provider_neutral();
        let account = harness.add_imap("Allowed").await;
        let store = harness.store();
        let id = queue_submission(&store, outgoing(&account, "Allowed"), Vec::new())
            .expect("queue submission");
        store.set_outbox_state(&id, allowed, None).unwrap();
        api::can_switch_backend(&account)
            .unwrap_or_else(|error| panic!("{allowed:?} must not block a backend switch: {error}"));
    }
}

#[tokio::test]
async fn draft_survives_switch() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let draft =
        save_draft(&harness.store(), outgoing(&account, "Work in progress")).expect("draft");

    // Seed a representative provider-derived cache: a folder, a message, its
    // remote mapping/collection, and a sync checkpoint. The switch must rebuild
    // all of it.
    {
        let store = harness.store();
        let inbox = folder(&account, "inbox-1", "INBOX", FolderKind::Inbox);
        store.upsert_folder(&inbox).unwrap();
        let cached = message(&account, &inbox, 7);
        store.upsert_message(&cached).unwrap();
        let remote = pige_core::providers::RemoteMessageId::new("remote-7");
        store
            .set_remote_message(&account, IMAP_SMTP_PROVIDER, &remote, &cached.id)
            .unwrap();
        let remote_collection = pige_core::providers::RemoteCollectionId::new("INBOX");
        store
            .set_remote_collection(&account, IMAP_SMTP_PROVIDER, &remote_collection, &inbox.id)
            .unwrap();
        store
            .set_checkpoint(
                &account_job(&account),
                1,
                &ProviderCursor::new(1, "inbox-checkpoint"),
            )
            .unwrap();
    }

    let existing = api::account(&account).expect("account");
    let target = Harness::gmail_target(&existing);
    let staged = harness.stage("switched-secret");
    let committed_candidate = staged.clone();
    let snapshot = api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect("switch backend");

    assert_eq!(snapshot.accounts.len(), 1);
    let switched = api::account(&account).expect("account survives");
    assert_eq!(switched.id, existing.id, "AccountId is preserved");
    assert_eq!(switched.provider_id(), ProviderId::from("gmail"));
    assert!(switched.imap_smtp().is_none());

    // The draft keeps its identity, account, and body.
    let stored_draft = outbox_entry(&harness.store(), &draft)
        .expect("read draft")
        .expect("draft survives the switch");
    assert_eq!(stored_draft.message.account_id, account);
    assert_eq!(stored_draft.state, SubmissionState::Draft);
    assert_eq!(stored_draft.message.subject, "Work in progress");

    // Only provider-derived cache/checkpoints were cleared.
    let store = harness.store();
    assert!(store.folders(&account).unwrap().is_empty());
    assert!(store.messages_for_account(&account).unwrap().is_empty());
    assert!(
        store
            .job_checkpoint(&account_job(&account))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("switched-secret")
    );

    // The staged candidate was committed, not left behind.
    assert!(
        harness
            .secrets()
            .load_staged(&committed_candidate)
            .unwrap()
            .is_none(),
        "the staged authentication candidate is discarded after commit"
    );
}

#[tokio::test]
async fn switch_crash_recovers_config_and_secret_pair() {
    let _guard = TEST_LOCK.lock().await;

    // Crash after the new active secret was written but before the config was
    // published. Startup must restore the previous secret so the old config and
    // its credential are a pair again.
    {
        let harness = Harness::provider_neutral();
        let account = harness.add_imap("Personal").await;
        let existing = api::account(&account).unwrap();
        let target = Harness::gmail_target(&existing);
        let staged = harness.stage("new-backend-secret");
        api::inject_switch_crash_after_secret();
        let error = api::switch_backend(&account, Harness::authenticated(target, staged))
            .await
            .expect_err("the injected crash surfaces");
        assert!(error.contains("simulated"), "{error}");
        api::clear_switch_crash();

        // The process "dies" mid-switch: the intent survives, the config is old,
        // and the new secret is active.
        assert!(api::account(&account).unwrap().imap_smtp().is_some());
        harness.restart();

        assert!(
            api::account(&account).unwrap().imap_smtp().is_some(),
            "startup rolls the config back to the old backend"
        );
        let expected = assert_password("secret-Personal");
        expected(&account, &harness);
        api::can_switch_backend(&account).expect("recovery clears the switch intent");
    }

    // Crash after the config was published. The new secret and new backend must
    // both survive; startup completes the switch instead of reverting it.
    {
        let harness = Harness::provider_neutral();
        let account = harness.add_imap("Work").await;
        let existing = api::account(&account).unwrap();
        let target = Harness::gmail_target(&existing);
        let staged = harness.stage("committed-secret");
        api::inject_switch_crash_after_config();
        let error = api::switch_backend(&account, Harness::authenticated(target, staged))
            .await
            .expect_err("the injected crash surfaces");
        assert!(error.contains("simulated"), "{error}");
        api::clear_switch_crash();

        harness.restart();

        let recovered = api::account(&account).expect("account survives");
        assert_eq!(recovered.provider_id(), ProviderId::from("gmail"));
        let expected = assert_password("committed-secret");
        expected(&account, &harness);
        api::can_switch_backend(&account).expect("recovery clears the switch intent");
    }
}

#[tokio::test]
async fn restore_cancels_late_sync() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::imap();
    let account = harness.add_imap("Personal").await;

    let started = Arc::new(Semaphore::new(0));
    let pause = Arc::new(Semaphore::new(0));
    let mut transport = fake_transport();
    transport.fetch_headers_started = Some(started.clone());
    transport.fetch_headers_pause = Some(pause.clone());
    harness.transport.add_transport(account.clone(), transport);

    let account_config = api::account(&account).unwrap();
    harness
        .backups
        .write(
            "pige-backup-2026-10-21-140530.json",
            &backup_document(vec![account_config]),
        )
        .unwrap();

    let sync = tokio::spawn({
        let id = account.clone();
        async move { api::sync_account(&id).await }
    });
    let permit = tokio::time::timeout(Duration::from_secs(5), started.acquire())
        .await
        .expect("sync starts")
        .expect("permit");
    permit.forget();

    let before = harness.store().generation(&account).unwrap();
    api::restore_config("pige-backup-2026-10-21-140530.json".to_string())
        .await
        .expect("restore");
    let after = harness.store().generation(&account).unwrap();
    assert!(
        after > before,
        "restore must cancel the current generation before rebuilding the mirror"
    );

    pause.add_permits(1);
    let _ = sync.await.expect("sync task");

    let store = harness.store();
    assert!(
        store.folders(&account).unwrap().is_empty(),
        "a late sync must not populate a post-restore cache"
    );
    assert!(store.messages_for_account(&account).unwrap().is_empty());
    assert!(
        api::account(&account).is_ok(),
        "the restored account survives"
    );
}

#[tokio::test]
async fn remove_cancels_refresh_and_crawl() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::imap();
    let account = harness.add_imap("Personal").await;

    let started = Arc::new(Semaphore::new(0));
    let pause = Arc::new(Semaphore::new(0));
    let mut transport = fake_transport();
    transport.fetch_headers_started = Some(started.clone());
    transport.fetch_headers_pause = Some(pause.clone());
    harness.transport.add_transport(account.clone(), transport);

    let store = harness.store();
    let stale = pige_core::providers::OperationContext::with_timeout(
        account.clone(),
        store.generation(&account).unwrap(),
        Duration::from_secs(30),
    );

    let crawl = tokio::spawn({
        let id = account.clone();
        async move { api::sync_account(&id).await }
    });
    let permit = tokio::time::timeout(Duration::from_secs(5), started.acquire())
        .await
        .expect("sync starts")
        .expect("permit");
    permit.forget();

    api::remove_account(&account).await.expect("remove account");
    pause.add_permits(1);
    let _ = crawl.await.expect("crawl task");

    assert!(
        api::account(&account).is_err(),
        "a removed account stays gone"
    );
    assert_eq!(
        harness.credentials.secret(&account),
        None,
        "no credential may be recreated after removal"
    );
    assert!(
        harness.store().folders(&account).unwrap().is_empty(),
        "a late crawl cannot repopulate the removed account"
    );

    // A refresh that began before removal must not be able to persist a rotated
    // secret after the account is gone.
    let rotated = SecretEnvelope::password("rotated-after-removal");
    let error = harness
        .secrets()
        .save(&stale, &rotated)
        .expect_err("a stale refresh is fenced by the removal generation");
    assert!(
        matches!(error.kind, pige_core::providers::ErrorKind::Conflict),
        "{error:?}"
    );
    assert_eq!(harness.credentials.secret(&account), None);
}

#[tokio::test]
async fn same_account_edit_rejects_stale_generation() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::imap();
    let account = harness.add_imap("Personal").await;
    let store = harness.store();
    let generation = store.generation(&account).unwrap();
    let stale = pige_core::providers::OperationContext::with_timeout(
        account.clone(),
        generation,
        Duration::from_secs(30),
    );

    api::update_account(&account, account_input_changed_host("Personal"))
        .await
        .expect("edit account");

    let edited = api::account(&account).expect("edited account");
    let (_, imap, _, _) = edited.imap_smtp().expect("still IMAP");
    assert_eq!(imap.host, "imap.changed.example");
    assert!(
        harness.store().generation(&account).unwrap() > generation,
        "editing an account invalidates its old generation"
    );

    let error = harness
        .secrets()
        .save(&stale, &SecretEnvelope::password("stale-write"))
        .expect_err("an old generation cannot write after an account edit");
    assert!(
        matches!(error.kind, pige_core::providers::ErrorKind::Conflict),
        "{error:?}"
    );
}

#[tokio::test]
async fn factory_reset_leaves_no_job_or_secret() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let store = harness.store();

    // Seed every durable job/secret-adjacent record the lifecycle owns.
    let inbox = folder(&account, "inbox-reset", "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_message(&message(&account, &inbox, 1)).unwrap();
    store
        .set_checkpoint(&account_job(&account), 1, &ProviderCursor::new(1, "reset"))
        .unwrap();
    let attempt = AuthAttemptId::new("11111111-1111-1111-1111-111111111111");
    let staged = StagedSecretRef::for_attempt(&attempt).unwrap();
    store
        .insert_pending_auth(&PendingAuthRecord {
            attempt: attempt.clone(),
            account: Some(account.clone()),
            provider: ProviderId::from(IMAP_SMTP_PROVIDER),
            credential_ref: staged.as_str().to_string(),
            generation: 1,
            created_at_ms: 1,
            expires_at_ms: 10_000_000_000_000,
        })
        .unwrap();
    harness
        .credentials
        .set_session(&AccountId(staged.as_str().to_string()), "staged-secret")
        .unwrap();
    store
        .acquire_lease(&account, "lifecycle-test", Duration::from_secs(60))
        .unwrap();

    api::factory_reset().await.expect("factory reset");

    assert!(api::snapshot().accounts.is_empty());
    assert_eq!(harness.credentials.secret(&account), None);
    let connection = rusqlite::Connection::open(harness.db_path()).unwrap();
    for table in [
        "accounts",
        "folders",
        "messages",
        "outbox",
        "job_checkpoints",
        "pending_auth",
        "switch_intents",
        "account_lease",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|error| panic!("count {table}: {error}"));
        assert_eq!(count, 0, "{table} must be empty after factory reset");
    }
}

#[tokio::test]
async fn imap_account_still_sends_after_upgrade() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::imap();
    let account = harness.add_imap("Personal").await;

    // Simulate an account upgraded from an older build: the credential store
    // still holds a bare password, not an envelope.
    harness
        .credentials
        .set(&account, "legacy-bare-password")
        .expect("seed legacy password");
    harness
        .transport
        .add_transport(account.clone(), fake_transport());

    let outcome = api::send_mail(outgoing(&account, "Still sends"))
        .await
        .expect("send after upgrade");
    assert!(!outcome.queued, "the message must be delivered, not queued");

    let sent = harness.transport.sent_raw();
    assert_eq!(sent.len(), 1, "SMTP receives exactly one message");
    let raw = String::from_utf8_lossy(&sent[0]);
    assert!(raw.contains("Subject: Still sends"), "{raw}");
    assert!(raw.contains("Hello after the upgrade."), "{raw}");

    let store = harness.store();
    let entries = store.outbox_drafts(None).expect("outbox");
    assert!(
        entries.is_empty(),
        "the delivered submission is not a draft"
    );
    let connection = rusqlite::Connection::open(harness.db_path()).unwrap();
    let state: String = connection
        .query_row("SELECT state FROM outbox LIMIT 1", [], |row| row.get(0))
        .expect("outbox row");
    assert!(
        state == "complete" || state == "accepted",
        "delivery must complete after the upgrade, got {state}"
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("legacy-bare-password"),
        "the legacy password still reads back without loss"
    );
    assert_eq!(
        harness
            .transport
            .transport(&account)
            .await
            .unwrap()
            .append_calls
            .len(),
        1,
        "the Sent copy is appended exactly once"
    );
}

#[tokio::test]
async fn contact_source_change_clears_only_replaced_source_cache() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;

    // Install contact source A via a same-backend reconnect.
    let source_a = ContactSourceConfig {
        id: ContactSourceId::from(CONTACTS_SOURCE_A),
        provider: ProviderId::from(IMAP_SMTP_PROVIDER),
        kind: "personal".to_string(),
        enabled: true,
    };
    let mut target = api::account(&account).unwrap();
    target.contact_sources = vec![source_a.clone()];
    let staged = harness.stage("contact-a-secret");
    api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect("enable source A");

    // Mail cache and both kinds of checkpoint exist.
    let store = harness.store();
    let inbox = folder(&account, "inbox-contact", "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_message(&message(&account, &inbox, 1)).unwrap();
    let generation = store.generation(&account).unwrap();
    store
        .set_checkpoint(
            &account_job(&account),
            generation,
            &ProviderCursor::new(1, "mail"),
        )
        .unwrap();
    store
        .set_checkpoint(
            &JobKey {
                account: account.clone(),
                kind: JobKind::Contacts,
                source: CONTACTS_SOURCE_A.to_string(),
            },
            generation,
            &ProviderCursor::new(1, "contacts-a"),
        )
        .unwrap();

    // Replace A with B. Only A's contact checkpoint may disappear; mail and the
    // mail-sync checkpoint are preserved.
    let source_b = ContactSourceConfig {
        id: ContactSourceId::from(CONTACTS_SOURCE_B),
        provider: ProviderId::from(IMAP_SMTP_PROVIDER),
        kind: "personal".to_string(),
        enabled: true,
    };
    let mut target = api::account(&account).unwrap();
    target.contact_sources = vec![source_b.clone()];
    let staged = harness.stage("contact-b-secret");
    api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect("replace source A with B");

    let contacts_a = JobKey {
        account: account.clone(),
        kind: JobKind::Contacts,
        source: CONTACTS_SOURCE_A.to_string(),
    };
    assert!(
        store.job_checkpoint(&contacts_a).unwrap().is_none(),
        "the replaced source checkpoint is cleared"
    );
    assert!(
        store
            .job_checkpoint(&account_job(&account))
            .unwrap()
            .is_some(),
        "a contact-source change must not clear mail sync"
    );
    assert_eq!(
        store.messages_for_account(&account).unwrap().len(),
        1,
        "a contact-source change must not clear cached mail"
    );
    let restored = api::account(&account).unwrap();
    assert_eq!(restored.contact_sources, vec![source_b]);
}

#[tokio::test]
async fn finish_auth_commits_new_account_and_cancel_cleans_up() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::imap();

    let AuthProgress::Challenge { attempt, .. } = api::begin_auth(AuthRequest {
        provider: ProviderId::from(IMAP_SMTP_PROVIDER),
        account: None,
        enable_contacts: false,
    })
    .await
    .expect("begin auth") else {
        panic!("expected a challenge");
    };
    api::cancel_auth(&attempt).await.expect("cancel auth");
    assert!(
        harness.store().pending_auth(&attempt).unwrap().is_none(),
        "cancel removes the pending attempt"
    );

    let AuthProgress::Challenge { attempt, .. } = api::begin_auth(AuthRequest {
        provider: ProviderId::from(IMAP_SMTP_PROVIDER),
        account: None,
        enable_contacts: false,
    })
    .await
    .expect("begin auth") else {
        panic!("expected a challenge");
    };
    let AuthProgress::Complete(mut authenticated) = api::answer_auth(
        &attempt,
        AuthAnswer::Password(pige_core::auth::secrecy::SecretString::from(
            "auth-password",
        )),
    )
    .await
    .expect("answer auth") else {
        panic!("expected completion");
    };
    authenticated.account.label = "Authenticated".to_string();
    authenticated.account.email = "authenticated@example.com".to_string();
    authenticated.account.display_name = "Authenticated".to_string();
    authenticated.account.backend = MailBackend::imap_smtp(
        "authenticated@example.com",
        server("imap.example.com", 993, Security::Tls),
        server("smtp.example.com", 465, Security::Tls),
    );

    let (id, snapshot) = api::finish_auth(authenticated)
        .await
        .expect("commit account");
    assert!(snapshot.accounts.iter().any(|account| account.id == id));
    let committed = api::account(&id).expect("committed account");
    assert_eq!(committed.label, "Authenticated");
    assert_eq!(
        harness.stored_password(&id).as_deref(),
        Some("auth-password"),
        "the staged credential is committed to the new account"
    );
}

/// A failed rollback must never discard the only record that can repair the
/// old config/credential pair at startup.
#[tokio::test]
async fn rollback_failure_keeps_the_switch_recoverable() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let target = Harness::gmail_target(&api::account(&account).unwrap());
    let staged = harness.stage("new-backend-secret");

    // Fail both the new active write and the rollback restore. The old secret
    // therefore stays active, but the intent and its backup must survive.
    harness.credentials.fail_set_for(&account);
    let error = api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect_err("the credential write fails");
    assert!(
        error.contains("restore"),
        "the rollback failure is surfaced: {error}"
    );

    let store = harness.store();
    assert!(
        store.has_switch_intent(&account).unwrap(),
        "the intent must survive a failed rollback"
    );
    let backup = StagedSecretRef::from_stored_key(&format!("switch_backup_{}", account.0))
        .expect("backup reference");
    assert!(
        harness.secrets().load_staged(&backup).unwrap().is_some(),
        "the rollback credential must survive a failed rollback"
    );
    assert!(
        api::account(&account).unwrap().imap_smtp().is_some(),
        "the old configuration is still live"
    );
    drop(store);

    // Repair the store and restart: startup restores the old credential and
    // clears the intent.
    harness.credentials.allow_set_for(&account);
    harness.restart();
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal")
    );
    assert!(api::account(&account).unwrap().imap_smtp().is_some());
    assert!(!harness.store().has_switch_intent(&account).unwrap());
}

/// Restoring a pre-switch backup while a published switch intent is pending must
/// restore that backup's credential too, not leave the old backend paired with
/// the new backend's secret.
#[tokio::test]
async fn restore_reconciles_a_pending_published_switch() {
    let _guard = TEST_LOCK.lock().await;
    const BACKUP: &str = "pige-backup-2026-10-21-140530.json";
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let original = api::account(&account).unwrap();
    harness
        .backups
        .write(BACKUP, &backup_document(vec![original]))
        .unwrap();

    let target = Harness::gmail_target(&api::account(&account).unwrap());
    let staged = harness.stage("gmail-secret");
    api::inject_switch_crash_after_config();
    let error = api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect_err("the injected crash surfaces");
    assert!(error.contains("simulated"), "{error}");
    api::clear_switch_crash();

    // The crash left a phase-2 intent: new config and new secret are live.
    assert!(harness.store().has_switch_intent(&account).unwrap());
    assert_eq!(
        api::account(&account).unwrap().provider_id(),
        ProviderId::from("gmail")
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("gmail-secret")
    );

    let outcome = api::restore_config(BACKUP.to_string())
        .await
        .expect("restore");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    assert!(
        api::account(&account).unwrap().imap_smtp().is_some(),
        "the backup's backend is restored"
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal"),
        "the backup's credential is restored with its backend"
    );
    assert!(
        !harness.store().has_switch_intent(&account).unwrap(),
        "the reconciled intent is cleared"
    );
}

/// A session-only commit over a persistent credential must delete the old OS
/// entry, because `get` prefers the persistent half.
#[tokio::test]
async fn session_only_switch_removes_the_previous_persistent_secret() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    assert_eq!(
        harness.credentials.persistent_secret(&account).as_deref(),
        Some("secret-Personal")
    );

    let target = Harness::gmail_target(&api::account(&account).unwrap());
    let staged = harness.stage("session-only-secret");
    api::switch_backend(
        &account,
        Harness::authenticated_with(target, staged, SecretPersistence::SessionOnly),
    )
    .await
    .expect("session-only switch");

    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("session-only-secret"),
        "the new session credential is the active one"
    );
    assert_eq!(
        harness.credentials.persistent_secret(&account),
        None,
        "the previous OS secret is deleted"
    );
    assert!(
        harness
            .credentials
            .get(&account)
            .unwrap()
            .is_some_and(|stored| !stored.contains("secret-Personal")),
        "get must resolve the new credential, not the old OS entry"
    );

    // A real restart drops the session half; the old persistent secret must not
    // reappear under the new backend.
    harness.credentials.delete_session(&account).unwrap();
    assert_eq!(harness.stored_password(&account), None);
}

/// A crash after publishing a session-only switch rolls back to the previous
/// backend and its persistent credential, because the session secret cannot
/// survive a restart.
#[tokio::test]
async fn session_only_switch_crash_rolls_back_to_the_old_credential() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let target = Harness::gmail_target(&api::account(&account).unwrap());
    let staged = harness.stage("session-only-secret");
    api::inject_switch_crash_after_config();
    let error = api::switch_backend(
        &account,
        Harness::authenticated_with(target, staged, SecretPersistence::SessionOnly),
    )
    .await
    .expect_err("the injected crash surfaces");
    assert!(error.contains("simulated"), "{error}");
    api::clear_switch_crash();

    // A real process death drops the session credential but keeps the persistent
    // rollback backup.
    harness.credentials.delete_session(&account).unwrap();
    harness.restart();

    assert!(
        api::account(&account).unwrap().imap_smtp().is_some(),
        "the old backend is restored"
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal"),
        "the old persistent credential is restored"
    );
    assert!(!harness.store().has_switch_intent(&account).unwrap());
}

/// A rollback backup whose intent row is gone must be swept at startup, so a
/// factory reset or crash cannot leave an unreachable credential.
#[tokio::test]
async fn startup_sweeps_an_orphaned_switch_backup() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let backup = AccountId(format!("switch_backup_{}", account.0));
    harness
        .credentials
        .set(&backup, "orphan-rollback")
        .expect("seed orphan backup");
    assert_eq!(
        harness.credentials.get(&backup).unwrap().as_deref(),
        Some("orphan-rollback")
    );

    harness.restart();

    assert_eq!(
        harness.credentials.get(&backup).unwrap(),
        None,
        "an orphaned rollback credential is removed at startup"
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal"),
        "the active account credential is untouched"
    );
}

/// Forward recovery of a same-backend contact-source switch must clear the
/// replaced source checkpoint just like the live commit path.
#[tokio::test]
async fn same_backend_recovery_clears_the_replaced_contact_source() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;

    let source_a = ContactSourceConfig {
        id: ContactSourceId::from(CONTACTS_SOURCE_A),
        provider: ProviderId::from(IMAP_SMTP_PROVIDER),
        kind: "personal".to_string(),
        enabled: true,
    };
    let mut target = api::account(&account).unwrap();
    target.contact_sources = vec![source_a];
    let staged = harness.stage("contact-a-secret");
    api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect("enable source A");

    let store = harness.store();
    let generation = store.generation(&account).unwrap();
    let contacts_a = JobKey {
        account: account.clone(),
        kind: JobKind::Contacts,
        source: CONTACTS_SOURCE_A.to_string(),
    };
    store
        .set_checkpoint(
            &contacts_a,
            generation,
            &ProviderCursor::new(1, "contacts-a"),
        )
        .unwrap();
    store
        .set_checkpoint(
            &account_job(&account),
            generation,
            &ProviderCursor::new(1, "mail"),
        )
        .unwrap();
    drop(store);

    let source_b = ContactSourceConfig {
        id: ContactSourceId::from(CONTACTS_SOURCE_B),
        provider: ProviderId::from(IMAP_SMTP_PROVIDER),
        kind: "personal".to_string(),
        enabled: true,
    };
    let mut target = api::account(&account).unwrap();
    target.contact_sources = vec![source_b.clone()];
    let staged = harness.stage("contact-b-secret");
    api::inject_switch_crash_after_config();
    let error = api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect_err("the injected crash surfaces");
    assert!(error.contains("simulated"), "{error}");
    api::clear_switch_crash();

    // The crash left the target config published but never cleared the replaced
    // source. Startup's forward path must do it.
    harness.restart();

    let store = harness.store();
    assert!(
        store.job_checkpoint(&contacts_a).unwrap().is_none(),
        "recovery clears the replaced contact source cache"
    );
    assert!(
        store
            .job_checkpoint(&account_job(&account))
            .unwrap()
            .is_some(),
        "recovery keeps the mail sync checkpoint"
    );
    assert_eq!(
        api::account(&account).unwrap().contact_sources,
        vec![source_b]
    );
}
