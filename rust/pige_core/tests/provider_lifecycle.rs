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
#[cfg(feature = "provider-google")]
use pige_core::auth::AuthChallenge;
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
use pige_core::store::{PendingAuthRecord, Store, SwitchIntent};
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
        selectable: true,
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

/// Confirmed phases mirrored from `api::accounts`; the integration test cannot
/// import the crate-private constants.
const PHASE_INTENT: u8 = 0;
const PHASE_BACKED_UP: u8 = 1;
const PHASE_CREDENTIAL_WRITTEN: u8 = 2;
const PHASE_CONFIG_WRITTEN: u8 = 3;
const PHASE_COMPLETED: u8 = 4;

/// The durable state a crash could have left behind. Tests seed it directly so
/// every progression point is deterministic instead of relying on timing.
struct Seed {
    previous: Account,
    target: Account,
    phase: u8,
    session_only: bool,
    previous_present: bool,
    previous_persistent: bool,
    active_password: Option<&'static str>,
    backup_password: Option<&'static str>,
    /// The staged authentication candidate, when it would still be in process
    /// memory (a same-process crash). `None` models a real restart.
    staged_password: Option<&'static str>,
}

fn seed_switch(harness: &Harness, account: &AccountId, seed: Seed) -> Option<StagedSecretRef> {
    let store = harness.store();
    let secrets = harness.secrets();
    let backup_ref = StagedSecretRef::from_stored_key(&format!("switch_backup_{}", account.0))
        .expect("backup reference");
    if let Some(password) = seed.backup_password {
        secrets
            .write_reference(
                &backup_ref,
                &SecretEnvelope::password(password),
                seed.previous_persistent,
            )
            .expect("write rollback backup");
    }
    if let Some(password) = seed.active_password {
        harness
            .credentials
            .set(
                account,
                &SecretEnvelope::password(password)
                    .to_stored_string()
                    .expect("encode active secret"),
            )
            .expect("write active credential");
    }
    let staged = seed.staged_password.map(|password| harness.stage(password));
    let credential_ref = staged
        .as_ref()
        .map(|reference| reference.as_str().to_string())
        .unwrap_or_else(|| "pending_00000000-0000-0000-0000-000000000000".to_string());
    store
        .insert_switch_intent(&SwitchIntent {
            account: account.clone(),
            target: seed.target,
            previous: seed.previous,
            credential_ref,
            backup_ref: seed
                .previous_present
                .then(|| backup_ref.as_str().to_string()),
            session_only: seed.session_only,
            previous_present: seed.previous_present,
            previous_persistent: seed.previous_persistent,
            phase: seed.phase,
            created_at_ms: 1,
        })
        .expect("insert switch intent");
    staged
}

fn write_accounts(harness: &Harness, accounts: Vec<Account>) {
    let path = pige_core::config::document_path(&harness.dir.path().join("config"));
    let doc = pige_core::config::ConfigDocument {
        accounts,
        ..pige_core::config::ConfigDocument::default()
    };
    pige_core::config::save(&path, &doc).expect("write account document");
}

fn backup_ref(account: &AccountId) -> StagedSecretRef {
    StagedSecretRef::from_stored_key(&format!("switch_backup_{}", account.0))
        .expect("backup reference")
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

/// Critical 1: when the previous credential cannot be restored (for example the
/// OS store is unavailable), startup must keep the intent and its backup instead
/// of deleting the only repair path. A later start must finish the repair.
#[tokio::test]
async fn startup_restore_failure_keeps_the_intent_and_backup() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_CREDENTIAL_WRITTEN,
            session_only: false,
            previous_present: true,
            previous_persistent: true,
            active_password: Some("new-backend-secret"),
            backup_password: Some("secret-Personal"),
            staged_password: None,
        },
    );

    // The rollback restore cannot write the old credential right now.
    harness.credentials.fail_set_for(&account);
    harness.restart();

    assert!(
        harness.store().has_switch_intent(&account).unwrap(),
        "a failed restore must keep the recovery record"
    );
    assert!(
        harness
            .secrets()
            .load_staged(&backup_ref(&account))
            .unwrap()
            .is_some(),
        "the reachable rollback backup must survive a failed restore"
    );
    assert!(
        api::account(&account).unwrap().imap_smtp().is_some(),
        "the old backend configuration is still live"
    );

    harness.credentials.allow_set_for(&account);
    harness.restart();

    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal"),
        "the retried startup restores the old credential"
    );
    assert!(!harness.store().has_switch_intent(&account).unwrap());
    assert!(
        harness
            .secrets()
            .load_staged(&backup_ref(&account))
            .unwrap()
            .is_none(),
        "the backup is cleaned up only after the repair succeeds"
    );
}

/// Critical 1 (delete path): an account that had no previous credential must not
/// lose its intent while the possibly-new active secret cannot be deleted.
#[tokio::test]
async fn startup_delete_failure_keeps_the_intent_until_cleanup_succeeds() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_CREDENTIAL_WRITTEN,
            session_only: false,
            previous_present: false,
            previous_persistent: false,
            active_password: Some("new-backend-secret"),
            backup_password: None,
            staged_password: None,
        },
    );

    harness.credentials.fail_delete();
    harness.restart();

    assert!(
        harness.store().has_switch_intent(&account).unwrap(),
        "an unverified delete must not clear the intent"
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("new-backend-secret"),
        "the undeletable secret is still there"
    );

    harness.credentials.allow_delete();
    harness.restart();

    assert!(!harness.store().has_switch_intent(&account).unwrap());
    assert_eq!(
        harness.credentials.persistent_secret(&account),
        None,
        "the next start retries the durable delete"
    );
    assert!(api::account(&account).unwrap().imap_smtp().is_some());
}

/// Critical 2: a restored target backend must never keep the pre-switch
/// credential just because a write was attempted. Phase 1 only confirms the
/// backup, not the new credential.
#[tokio::test]
async fn restore_target_with_unconfirmed_write_drops_the_old_credential() {
    let _guard = TEST_LOCK.lock().await;
    const BACKUP: &str = "pige-backup-2026-10-21-140531.json";
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);
    harness
        .backups
        .write(BACKUP, &backup_document(vec![target.clone()]))
        .unwrap();

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_BACKED_UP,
            session_only: false,
            previous_present: true,
            previous_persistent: true,
            // The old credential is still active; the new write was not confirmed.
            active_password: Some("secret-Personal"),
            backup_password: Some("secret-Personal"),
            staged_password: None,
        },
    );

    let outcome = api::restore_config(BACKUP.to_string())
        .await
        .expect("restore target backend");

    assert_eq!(
        harness.stored_password(&account),
        None,
        "the old credential must not be paired with the restored target backend"
    );
    assert!(
        !harness.store().has_switch_intent(&account).unwrap(),
        "a successful mismatched-credential delete clears the intent"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("sign in again")),
        "{:?}",
        outcome.warnings
    );
}

/// A target restore over a still-available staged candidate activates it for
/// both pre-write phases (the old code only did this at phase 0).
#[tokio::test]
async fn restore_target_activates_the_staged_candidate_at_every_pre_write_state() {
    let _guard = TEST_LOCK.lock().await;
    for (index, phase) in [PHASE_INTENT, PHASE_BACKED_UP].into_iter().enumerate() {
        let harness = Harness::provider_neutral();
        let account = harness.add_imap("Personal").await;
        let previous = api::account(&account).unwrap();
        let target = Harness::gmail_target(&previous);
        let backup_name = format!("pige-backup-2026-10-21-14054{index}.json");
        harness
            .backups
            .write(&backup_name, &backup_document(vec![target.clone()]))
            .unwrap();
        seed_switch(
            &harness,
            &account,
            Seed {
                previous,
                target,
                phase,
                session_only: false,
                previous_present: true,
                previous_persistent: true,
                active_password: Some("secret-Personal"),
                backup_password: Some("secret-Personal"),
                staged_password: Some("staged-new-secret"),
            },
        );

        api::restore_config(backup_name)
            .await
            .expect("restore target backend");

        assert_eq!(
            harness.stored_password(&account).as_deref(),
            Some("staged-new-secret"),
            "phase {phase} must activate the staged candidate, not keep the old credential"
        );
        assert!(!harness.store().has_switch_intent(&account).unwrap());
    }
}

/// Important: a target restore whose new credential is confirmed must clear the
/// previous backend's provider cache exactly like the live commit/forward path.
#[tokio::test]
async fn restore_target_clears_the_previous_backend_cache() {
    let _guard = TEST_LOCK.lock().await;
    const BACKUP: &str = "pige-backup-2026-10-21-140532.json";
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);
    harness
        .backups
        .write(BACKUP, &backup_document(vec![target.clone()]))
        .unwrap();

    {
        let store = harness.store();
        let inbox = folder(
            &account,
            "inbox-restored-target",
            "INBOX",
            FolderKind::Inbox,
        );
        store.upsert_folder(&inbox).unwrap();
        store.upsert_message(&message(&account, &inbox, 3)).unwrap();
        store
            .set_checkpoint(
                &account_job(&account),
                1,
                &ProviderCursor::new(1, "old-backend"),
            )
            .unwrap();
    }

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_CONFIG_WRITTEN,
            session_only: false,
            previous_present: true,
            previous_persistent: true,
            active_password: Some("gmail-secret"),
            backup_password: Some("secret-Personal"),
            staged_password: None,
        },
    );

    let outcome = api::restore_config(BACKUP.to_string())
        .await
        .expect("restore target backend");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);

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
        Some("gmail-secret")
    );
    assert!(!store.has_switch_intent(&account).unwrap());
}

/// Important: a third-backend restore that cannot delete the mismatched
/// credential must keep the intent and retry, not finish with a stale secret.
#[tokio::test]
async fn restore_third_backend_keeps_the_intent_when_delete_fails() {
    let _guard = TEST_LOCK.lock().await;
    const BACKUP: &str = "pige-backup-2026-10-21-140533.json";
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);
    let mut third = previous.clone();
    third.backend = MailBackend::proton(None);
    third.contact_sources = Vec::new();
    harness
        .backups
        .write(BACKUP, &backup_document(vec![third]))
        .unwrap();

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_CONFIG_WRITTEN,
            session_only: false,
            previous_present: true,
            previous_persistent: true,
            active_password: Some("gmail-secret"),
            backup_password: Some("secret-Personal"),
            staged_password: None,
        },
    );

    harness.credentials.fail_delete();
    let outcome = api::restore_config(BACKUP.to_string())
        .await
        .expect("restore third backend");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("could not be cleared")),
        "{:?}",
        outcome.warnings
    );

    assert!(
        harness.store().has_switch_intent(&account).unwrap(),
        "a failed third-backend delete must keep the intent"
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("gmail-secret"),
        "the mismatched secret is still present until it can be cleared"
    );

    // The next start retries the delete and only then drops the intent.
    harness.credentials.allow_delete();
    harness.restart();
    assert!(!harness.store().has_switch_intent(&account).unwrap());
    assert_eq!(harness.credentials.persistent_secret(&account), None);
    assert_eq!(
        api::account(&account).unwrap().provider_id(),
        ProviderId::from("proton")
    );
}

/// Forward startup recovery of a backend change clears the previous provider
/// cache before completing the intent.
#[tokio::test]
async fn forward_recovery_clears_the_previous_backend_cache() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);
    write_accounts(&harness, vec![target.clone()]);

    {
        let store = harness.store();
        let inbox = folder(&account, "inbox-forward", "INBOX", FolderKind::Inbox);
        store.upsert_folder(&inbox).unwrap();
        store.upsert_message(&message(&account, &inbox, 4)).unwrap();
        store
            .set_checkpoint(
                &account_job(&account),
                1,
                &ProviderCursor::new(1, "old-backend"),
            )
            .unwrap();
    }

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_CONFIG_WRITTEN,
            session_only: false,
            previous_present: true,
            previous_persistent: true,
            active_password: Some("gmail-secret"),
            backup_password: Some("secret-Personal"),
            staged_password: None,
        },
    );

    harness.restart();

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
        api::account(&account).unwrap().provider_id(),
        ProviderId::from("gmail")
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("gmail-secret")
    );
    assert!(!store.has_switch_intent(&account).unwrap());
}

/// A crash after the confirmed credential write but before config publication
/// rolls back to the old credential (progression state 2).
#[tokio::test]
async fn startup_at_credential_written_restores_the_previous_credential() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_CREDENTIAL_WRITTEN,
            session_only: false,
            previous_present: true,
            previous_persistent: true,
            active_password: Some("new-backend-secret"),
            backup_password: Some("secret-Personal"),
            staged_password: None,
        },
    );

    harness.restart();

    assert!(api::account(&account).unwrap().imap_smtp().is_some());
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal")
    );
    assert!(!harness.store().has_switch_intent(&account).unwrap());
}

/// A crash after the cache was cleared but before the intent was removed
/// (progression state 4) completes forward without rolling back.
#[tokio::test]
async fn startup_at_completed_finishes_without_rolling_back() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let previous = api::account(&account).unwrap();
    let target = Harness::gmail_target(&previous);
    write_accounts(&harness, vec![target.clone()]);

    seed_switch(
        &harness,
        &account,
        Seed {
            previous,
            target,
            phase: PHASE_COMPLETED,
            session_only: false,
            previous_present: true,
            previous_persistent: true,
            active_password: Some("gmail-secret"),
            backup_password: Some("secret-Personal"),
            staged_password: None,
        },
    );

    harness.restart();

    assert_eq!(
        api::account(&account).unwrap().provider_id(),
        ProviderId::from("gmail")
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("gmail-secret")
    );
    assert!(!harness.store().has_switch_intent(&account).unwrap());
}

/// Live write failure after the backup is confirmed: the rollback succeeds, so
/// no intent or backup is left and the old pair is untouched.
/// A configuration naming a native provider this build cannot open must remain a
/// readable account, not be treated as a corrupt document and quarantined.
#[tokio::test]
async fn google_provider_disabled_is_a_readable_account_state() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::imap();
    let account = Account {
        id: AccountId::new(),
        label: "Work".to_string(),
        email: "person@example.com".to_string(),
        display_name: "Person".to_string(),
        backend: MailBackend::gmail("google-uid-restored"),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    };
    write_accounts(&harness, vec![account.clone()]);
    harness.restart();

    let snapshot = api::snapshot();
    let summary = snapshot
        .accounts
        .iter()
        .find(|candidate| candidate.id == account.id)
        .expect("the unavailable account is still listed");
    assert_eq!(summary.provider, pige_core::providers::GMAIL_PROVIDER);
    assert!(!summary.available);
    assert!(
        summary
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("not available")),
        "a readable reason must explain the missing provider: {:?}",
        summary.unavailable_reason
    );
    assert!(
        api::account(&account.id).is_ok(),
        "the account remains readable"
    );
    let corrupt = harness.dir.path().join("config").join("pige.json.corrupt");
    assert!(
        !corrupt.exists(),
        "the configuration must not be quarantined"
    );
}

// ---------------------------------------------------------------------------
// Task G5: native Gmail onboarding, explicit switching, and contact opt-in.
//
// These drive the same validated Android host-result path the JNI bridge uses
// (`deliver_native_result` -> `api::poll_platform_auth`) against a recording
// fake host. No test reaches Google.
// ---------------------------------------------------------------------------
#[cfg(feature = "provider-google")]
mod google_onboarding {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use secrecy::SecretString;

    use pige_core::auth::TokenSource;
    use pige_core::providers::google::{
        CONTACTS_OTHER_READONLY_SCOPE, CONTACTS_READONLY_SCOPE, GMAIL_MODIFY_SCOPE,
        GoogleAndroidHost, GoogleAndroidResult, GoogleAuthentication, GoogleEndpoints,
        GoogleOAuthConfig, GoogleProviderFactory, GoogleTokenSourceProvider, deliver_native_result,
    };
    use pige_core::providers::{
        GMAIL_PROVIDER, OperationContext, ProviderError, ProviderId, ProviderRegistry,
    };

    struct RecordingHost {
        launches: StdMutex<Vec<pige_core::providers::google::AndroidAuthorizationRequest>>,
        cancelled: StdMutex<Vec<AuthAttemptId>>,
    }

    impl RecordingHost {
        fn new() -> Self {
            Self {
                launches: StdMutex::new(Vec::new()),
                cancelled: StdMutex::new(Vec::new()),
            }
        }

        fn launches(&self) -> Vec<pige_core::providers::google::AndroidAuthorizationRequest> {
            self.launches.lock().expect("launches").clone()
        }

        fn cancelled(&self) -> Vec<AuthAttemptId> {
            self.cancelled.lock().expect("cancelled").clone()
        }
    }

    #[async_trait]
    impl GoogleAndroidHost for RecordingHost {
        async fn launch(
            &self,
            request: pige_core::providers::google::AndroidAuthorizationRequest,
        ) -> Result<(), ProviderError> {
            self.launches.lock().expect("launches").push(request);
            Ok(())
        }

        fn cancel(&self, attempt: &AuthAttemptId) -> Result<(), ProviderError> {
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
        ) -> Result<GoogleAndroidResult, ProviderError> {
            Err(ProviderError::reauthenticate(
                "no live Android grant in this test",
            ))
        }
    }

    /// An API backed by the real Google authentication service in its Android
    /// host-result shape, plus provider dispatch for Gmail and IMAP accounts.
    fn google_harness() -> (Harness, Arc<RecordingHost>) {
        let dir = TempDir::new().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("data")).expect("data dir");
        let credentials = Arc::new(FakeCredentials::default());
        let transport = Arc::new(FakeTransportFactory::default());
        let store = Arc::new(
            Store::open(dir.path().join("data").join(DB_FILE_NAME)).expect("google store"),
        );
        store.migrate().expect("migrate google store");
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials.clone(),
            store.clone(),
        ));
        let sessions = Arc::new(pige_core::auth::AccountSessionManager::new(store.clone()));
        let host = Arc::new(RecordingHost::new());
        let authentication = GoogleAuthentication::android(
            secrets,
            sessions,
            store,
            GoogleOAuthConfig::production("android-test-client"),
            host.clone(),
            None,
            Duration::from_secs(300),
        );

        let mut registry = ProviderRegistry::new();
        registry
            .register(
                ProviderId::from(GMAIL_PROVIDER),
                Arc::new(FakeProviderFactory::new()),
            )
            .expect("register gmail");
        registry
            .register(
                ProviderId::from(IMAP_SMTP_PROVIDER),
                Arc::new(FakeProviderFactory::new()),
            )
            .expect("register imap");
        let harness = Harness {
            dir,
            credentials,
            transport,
            providers: Arc::new(registry),
            backups: Arc::new(FakeBackupStore::new()),
            auth: Some(Arc::new(Mutex::new(
                Box::new(authentication) as Box<dyn AuthenticationService>
            ))),
        };
        harness.init();
        (harness, host)
    }

    async fn begin_google(account: Option<AccountId>, enable_contacts: bool) -> AuthAttemptId {
        let progress = api::begin_auth(AuthRequest {
            provider: ProviderId::from(GMAIL_PROVIDER),
            account,
            enable_contacts,
        })
        .await
        .expect("begin Google sign-in");
        let AuthProgress::Challenge { attempt, challenge } = progress else {
            panic!("expected a Google challenge");
        };
        let AuthChallenge::Browser(browser) = challenge else {
            panic!("expected a browser-shaped challenge");
        };
        assert!(
            browser.url.starts_with("pige-google-android:"),
            "the Android flow must be signalled as a platform challenge: {}",
            browser.url
        );
        attempt
    }

    fn deliver_android(attempt: &AuthAttemptId, scopes: &[&str], email: &str) -> AuthAttemptId {
        let payload = serde_json::json!({
            "attempt": attempt.as_str(),
            "result": {
                "kind": "authorized",
                "accessToken": "android-access-token",
                "expiresAtMs": now_ms() + 3_600_000,
                "scopes": scopes,
                "identity": {
                    "userId": "google-user-1",
                    "email": email,
                    "displayName": "Google Person"
                }
            }
        })
        .to_string();
        deliver_native_result(&payload).expect("queue the validated Android result")
    }

    async fn poll_complete(attempt: &AuthAttemptId) -> pige_core::auth::AuthenticatedAccount {
        for _ in 0..100 {
            if let Ok(Some(progress)) = api::poll_platform_auth(attempt).await {
                return match progress {
                    AuthProgress::Complete(authenticated) => authenticated,
                    other => panic!("expected a completed Google attempt, got {other:?}"),
                };
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the Android result never reached the Rust authentication service");
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0)
    }

    fn stored_google(harness: &Harness, account: &AccountId) -> bool {
        matches!(
            harness
                .secrets()
                .load(account)
                .expect("load credential")
                .map(|envelope| envelope.credentials),
            Some(StoredCredentials::Google(_))
        )
    }

    #[tokio::test]
    async fn new_google_account_has_no_password_or_server_fields() {
        let _guard = TEST_LOCK.lock().await;
        let (harness, host) = google_harness();
        let attempt = begin_google(None, false).await;
        let launch = host.launches().last().cloned().expect("host launch");
        assert_eq!(
            launch.server_client_id, None,
            "on-demand authorization forwards no server client id unless offline is configured"
        );
        deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "person@example.com");
        let authenticated = poll_complete(&attempt).await;
        assert!(matches!(
            authenticated.account.backend,
            MailBackend::Gmail { .. }
        ));
        assert!(authenticated.account.imap_smtp().is_none());

        let (id, snapshot) = api::finish_auth(authenticated)
            .await
            .expect("create account");
        let account = api::account(&id).expect("created account");
        assert!(matches!(account.backend, MailBackend::Gmail { .. }));
        assert!(account.imap_smtp().is_none());
        assert!(stored_google(&harness, &id));
        assert_eq!(snapshot.accounts.len(), 1);

        let document = std::fs::read_to_string(harness.dir.path().join("config").join("pige.json"))
            .expect("read configuration");
        assert!(
            !document.contains("\"password\""),
            "no password field: {document}"
        );
        assert!(
            !document.contains("\"imap\""),
            "no IMAP endpoints: {document}"
        );
        assert!(
            !document.contains("\"smtp\""),
            "no SMTP endpoints: {document}"
        );
        assert!(
            document.contains("\"kind\": \"gmail\""),
            "the native Gmail backend is persisted: {document}"
        );
    }

    #[tokio::test]
    async fn cancel_login_creates_no_account() {
        let _guard = TEST_LOCK.lock().await;
        let (harness, host) = google_harness();
        let attempt = begin_google(None, false).await;
        api::cancel_auth(&attempt).await.expect("cancel");
        assert!(host.cancelled().contains(&attempt));

        // A late platform result must not resurrect the abandoned attempt.
        deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "person@example.com");
        assert!(
            api::poll_platform_auth(&attempt).await.is_err(),
            "a cancelled attempt rejects its late result"
        );
        assert!(api::snapshot().accounts.is_empty());
        assert!(
            harness
                .store()
                .expired_pending_auth(i64::MAX)
                .expect("pending auth rows")
                .is_empty(),
            "no pending authentication survives cancellation"
        );
    }

    #[tokio::test]
    async fn contacts_optional_at_setup() {
        let _guard = TEST_LOCK.lock().await;
        let (harness, host) = google_harness();

        // Mail-only setup requests no contacts scope and commits no sources.
        let attempt = begin_google(None, false).await;
        let launch = host.launches().last().cloned().expect("mail launch");
        assert!(
            !launch
                .scopes
                .iter()
                .any(|scope| scope == CONTACTS_READONLY_SCOPE)
        );
        assert!(
            !launch
                .scopes
                .iter()
                .any(|scope| scope == CONTACTS_OTHER_READONLY_SCOPE)
        );
        deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "person@example.com");
        let authenticated = poll_complete(&attempt).await;
        let (id, _) = api::finish_auth_with_contacts(authenticated, false)
            .await
            .expect("create mail-only account");
        assert!(
            api::account(&id).unwrap().contact_sources.is_empty(),
            "contacts stay off unless the user opts in"
        );

        // Enabling contacts is a new authorization that requests both read scopes
        // and registers the R6 sources only after it completes.
        let launches_before = host.launches().len();
        let attempt = begin_google(Some(id.clone()), true).await;
        assert_eq!(host.launches().len(), launches_before + 1);
        let launch = host.launches().last().cloned().expect("contacts launch");
        assert!(
            launch
                .scopes
                .iter()
                .any(|scope| scope == CONTACTS_READONLY_SCOPE)
        );
        assert!(
            launch
                .scopes
                .iter()
                .any(|scope| scope == CONTACTS_OTHER_READONLY_SCOPE)
        );
        deliver_android(
            &attempt,
            &[
                GMAIL_MODIFY_SCOPE,
                CONTACTS_READONLY_SCOPE,
                CONTACTS_OTHER_READONLY_SCOPE,
            ],
            "person@example.com",
        );
        let authenticated = poll_complete(&attempt).await;
        let (same, _) = api::finish_auth_with_contacts(authenticated, true)
            .await
            .expect("grant contacts");
        assert_eq!(same, id);
        let account = api::account(&id).unwrap();
        let sources: Vec<&str> = account
            .contact_sources
            .iter()
            .map(|source| source.id.as_str())
            .collect();
        assert!(sources.contains(&"google.saved"), "{sources:?}");
        assert!(sources.contains(&"google.other"), "{sources:?}");
        assert!(account.contact_sources.iter().all(|source| source.enabled));
        let states = harness.store().contact_source_states(&id).expect("states");
        assert_eq!(states.len(), 2);
        assert!(states.iter().all(|(_, state)| state.enabled));
    }

    #[tokio::test]
    async fn switch_refuses_sent_copy_repair_or_uncertain_send() {
        let _guard = TEST_LOCK.lock().await;
        for state in [SubmissionState::Accepted, SubmissionState::Uncertain] {
            let (harness, _host) = google_harness();
            let account = harness.add_imap("LegacyGmail").await;
            let store = harness.store();
            let id = queue_submission(&store, outgoing(&account, "Report"), Vec::new())
                .expect("queue submission");
            store
                .set_outbox_state(&id, state, Some("pending"))
                .expect("set state");

            let blocked = api::can_switch_backend(&account).expect_err("switch is gated");
            assert!(blocked.to_lowercase().contains("submission"), "{blocked}");

            let attempt = begin_google(Some(account.clone()), false).await;
            deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "person@example.com");
            let authenticated = poll_complete(&attempt).await;
            let error = api::finish_auth(authenticated)
                .await
                .expect_err("an unfinished submission blocks the switch");
            assert!(error.to_lowercase().contains("submission"), "{error}");
            assert!(
                api::account(&account).unwrap().imap_smtp().is_some(),
                "{state:?} must not silently convert the IMAP account"
            );
        }
    }

    #[tokio::test]
    async fn native_delete_button_reflects_scope() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(Store::open(dir.path().join("pige.db")).expect("store"));
        store.migrate().expect("migrate");
        let sessions = Arc::new(pige_core::auth::AccountSessionManager::new(store));
        let factory = GoogleProviderFactory::new(sessions.clone(), GoogleEndpoints::production())
            .with_token_source_provider(Arc::new(FixedGoogleSources));
        let id = AccountId::new();
        let account = Account {
            id: id.clone(),
            label: "Gmail".to_string(),
            email: "person@example.com".to_string(),
            display_name: "Person".to_string(),
            backend: MailBackend::gmail("google-user-1"),
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        };
        let ctx = sessions
            .context(&id, Duration::from_secs(30))
            .expect("context");
        let session = provider_open(&factory, &account, &ctx).await;
        assert!(
            !session.capabilities.mutations.permanent_delete,
            "gmail.modify cannot permanently delete; the UI must not offer it"
        );
        assert!(
            session.capabilities.mutations.trash,
            "the native Gmail path must offer Trash instead"
        );
    }

    async fn provider_open(
        factory: &GoogleProviderFactory,
        account: &Account,
        ctx: &OperationContext,
    ) -> pige_core::providers::ProviderSession {
        use pige_core::providers::ProviderFactory;
        factory
            .open(account, ctx)
            .await
            .expect("open Gmail session")
    }

    struct FixedTokenSource;

    #[async_trait]
    impl TokenSource for FixedTokenSource {
        async fn access_token(
            &self,
            _ctx: &OperationContext,
        ) -> Result<SecretString, ProviderError> {
            Ok(SecretString::from("fixed-access"))
        }
    }

    struct FixedGoogleSources;

    impl GoogleTokenSourceProvider for FixedGoogleSources {
        fn token_source(
            &self,
            _account: &AccountId,
        ) -> Result<Arc<dyn TokenSource>, ProviderError> {
            Ok(Arc::new(FixedTokenSource))
        }
    }

    #[tokio::test]
    async fn restore_google_config_requires_reauth_not_embedded_tokens() {
        let _guard = TEST_LOCK.lock().await;
        let (harness, host) = google_harness();
        let account = Account {
            id: AccountId::new(),
            label: "Restored".to_string(),
            email: "restored@example.com".to_string(),
            display_name: "Restored".to_string(),
            backend: MailBackend::gmail("google-user-restored"),
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        };
        let backup = "pige-backup-2026-11-10-120000.json";
        harness
            .backups
            .write(backup, &backup_document(vec![account.clone()]))
            .expect("write Google backup");
        let outcome = api::restore_config(backup.to_string())
            .await
            .expect("restore native Gmail configuration");
        assert!(
            outcome
                .snapshot
                .accounts
                .iter()
                .any(|candidate| candidate.id == account.id)
        );

        // A configuration backup never carries credentials. With no credential
        // store entry for the restored id, the only valid next step is a new
        // authorization; no password or token is invented.
        assert!(
            harness
                .credentials
                .get(&account.id)
                .expect("read credential store")
                .is_none(),
            "restored Google config must not embed a token"
        );
        assert!(harness.secrets().load(&account.id).unwrap().is_none());
        let document = std::fs::read_to_string(harness.dir.path().join("config").join("pige.json"))
            .expect("read configuration");
        for marker in [
            "access_token",
            "refresh_token",
            "accessToken",
            "refreshToken",
            "client_secret",
        ] {
            assert!(
                !document.contains(marker),
                "the restored document must not carry {marker}: {document}"
            );
        }

        // Reauthentication is a fresh interactive attempt, not a silent resume.
        let attempt = begin_google(Some(account.id.clone()), false).await;
        assert!(
            host.launches()
                .iter()
                .any(|launch| launch.attempt == attempt)
        );
    }

    #[tokio::test]
    async fn reconnect_refuses_a_different_google_identity() {
        let _guard = TEST_LOCK.lock().await;
        let (harness, _host) = google_harness();
        let attempt = begin_google(None, false).await;
        deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "person@example.com");
        let authenticated = poll_complete(&attempt).await;
        let (id, _) = api::finish_auth(authenticated)
            .await
            .expect("create account");

        let attempt = begin_google(Some(id.clone()), false).await;
        deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "someone-else@example.com");
        let authenticated = poll_complete(&attempt).await;
        let error = api::finish_auth(authenticated)
            .await
            .expect_err("a different Google identity must not rewrite the account");
        assert!(error.contains("does not match"), "{error}");
        assert!(error.contains("person@example.com"), "{error}");

        let account = api::account(&id).expect("account survives");
        assert_eq!(account.email, "person@example.com");
        assert_eq!(account.provider_id().as_str(), GMAIL_PROVIDER);
        assert!(
            !harness.store().has_switch_intent(&id).unwrap(),
            "a refused identity change writes no switch intent"
        );
    }

    #[tokio::test]
    async fn mail_only_grant_registers_no_contact_sources() {
        let _guard = TEST_LOCK.lock().await;
        let (harness, host) = google_harness();

        // The attempt asks for contacts, but Google grants mail only.
        let attempt = begin_google(None, true).await;
        let launch = host.launches().last().cloned().expect("contacts launch");
        assert!(
            launch
                .scopes
                .iter()
                .any(|scope| scope == CONTACTS_READONLY_SCOPE)
        );
        deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "person@example.com");
        let authenticated = poll_complete(&attempt).await;
        let (id, _) = api::finish_auth_with_contacts(authenticated, true)
            .await
            .expect("mail-only grant still completes");

        let account = api::account(&id).expect("created account");
        assert!(
            account.contact_sources.is_empty(),
            "a mail-only grant must not register People sources: {:?}",
            account.contact_sources
        );
        let states = harness.store().contact_source_states(&id).expect("states");
        assert!(
            states.is_empty(),
            "a mail-only grant must not enable durable contact sources: {states:?}"
        );
    }

    #[tokio::test]
    async fn same_email_accounts_are_not_silently_merged() {
        let _guard = TEST_LOCK.lock().await;
        let (_harness, _host) = google_harness();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let attempt = begin_google(None, false).await;
            deliver_android(&attempt, &[GMAIL_MODIFY_SCOPE], "same@example.com");
            let authenticated = poll_complete(&attempt).await;
            let (id, _) = api::finish_auth(authenticated)
                .await
                .expect("create account");
            ids.push(id);
        }

        assert_ne!(
            ids[0], ids[1],
            "a second consent creates a distinct account"
        );
        let snapshot = api::snapshot();
        assert_eq!(snapshot.accounts.len(), 2);
        assert!(
            snapshot
                .accounts
                .iter()
                .all(|account| account.email == "same@example.com")
        );
    }
}

#[tokio::test]
async fn write_failure_after_backup_rolls_back_without_a_record() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let target = Harness::gmail_target(&api::account(&account).unwrap());

    // The backup (a different key) succeeds; only the active write fails, and
    // the rollback restore that follows succeeds.
    harness.credentials.fail_next_set_for(&account);
    let staged = harness.stage("new-backend-secret");
    let error = api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect_err("the active credential write fails");
    assert!(!error.is_empty());

    assert!(api::account(&account).unwrap().imap_smtp().is_some());
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal")
    );
    assert!(!harness.store().has_switch_intent(&account).unwrap());
    assert!(
        harness
            .secrets()
            .load_staged(&backup_ref(&account))
            .unwrap()
            .is_none(),
        "a completed rollback discards its backup"
    );
}

/// Live backup failure happens before any active credential change, so the
/// intent and its references are discarded.
#[tokio::test]
async fn backup_failure_leaves_no_recovery_record() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::provider_neutral();
    let account = harness.add_imap("Personal").await;
    let target = Harness::gmail_target(&api::account(&account).unwrap());

    // The first persistent set in a switch is the rollback backup.
    harness.credentials.fail_next_set();
    let staged = harness.stage("new-backend-secret");
    api::switch_backend(&account, Harness::authenticated(target, staged))
        .await
        .expect_err("the rollback backup write fails");

    assert!(api::account(&account).unwrap().imap_smtp().is_some());
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Personal")
    );
    assert!(!harness.store().has_switch_intent(&account).unwrap());
    assert!(
        harness
            .secrets()
            .load_staged(&backup_ref(&account))
            .unwrap()
            .is_none(),
        "the failed backup does not survive"
    );
}

/// Restoring a target-backend document over an intent at every confirmed phase
/// must leave the target paired with the new credential (never the old one).
#[tokio::test]
async fn restore_target_over_intent_is_consistent_at_every_phase() {
    let _guard = TEST_LOCK.lock().await;
    for phase in [
        PHASE_INTENT,
        PHASE_BACKED_UP,
        PHASE_CREDENTIAL_WRITTEN,
        PHASE_CONFIG_WRITTEN,
        PHASE_COMPLETED,
    ] {
        let harness = Harness::provider_neutral();
        let account = harness.add_imap("Personal").await;
        let previous = api::account(&account).unwrap();
        let target = Harness::gmail_target(&previous);
        let backup_name = format!("pige-backup-2026-10-21-1501{phase:02}.json");
        harness
            .backups
            .write(&backup_name, &backup_document(vec![target.clone()]))
            .unwrap();
        let pre_write = phase < PHASE_CREDENTIAL_WRITTEN;
        seed_switch(
            &harness,
            &account,
            Seed {
                previous,
                target,
                phase,
                session_only: false,
                previous_present: true,
                previous_persistent: true,
                active_password: Some(if pre_write {
                    "secret-Personal"
                } else {
                    "restored-target-secret"
                }),
                backup_password: Some("secret-Personal"),
                staged_password: pre_write.then_some("restored-target-secret"),
            },
        );

        api::restore_config(backup_name)
            .await
            .expect("restore target backend");

        assert_eq!(
            harness.stored_password(&account).as_deref(),
            Some("restored-target-secret"),
            "phase {phase} must pair the restored target backend with the new credential"
        );
        assert!(
            !harness.store().has_switch_intent(&account).unwrap(),
            "phase {phase} must not leave a resolved intent"
        );
    }
}
