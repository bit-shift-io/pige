//! Task H1: crash, migration, and cross-process recovery.
//!
//! These tests pin the release-recovery contract:
//!
//! - a v1 configuration and a v4 cache upgrade together without losing local
//!   ids, cached content, or outbox rows;
//! - an injected failure before the v7 table swap rolls the whole migration
//!   back, and a pre-upgrade consistent backup remains usable (migrations are
//!   forward-only; there is no older-binary rollback);
//! - an interrupted backend switch leaves a recoverable intent and startup
//!   restores the config/credential pair;
//! - a real subprocess killed after provider submission reopens without
//!   resending the message;
//! - two real processes cannot regress a job cursor while a durable lease and
//!   checkpoint CAS are in force;
//! - an unavailable provider feature does not hide cached mail from the reader;
//! - a backend swap preserves drafts but discards old remote/UID mappings;
//! - a hanging/failing revocation never blocks local removal or another
//!   account's cached reader.

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::Connection;
use secrecy::ExposeSecret;
use tempfile::TempDir;
use tokio::sync::Mutex;

use common::legacy::LegacyV4;
use pige_core::account::{Account, AccountId, MailBackend, Security, ServerConfig};
use pige_core::api::{self, AccountInput, Config, DB_FILE_NAME};
use pige_core::auth::{
    AuthAnswer, AuthProgress, AuthRequest, AuthenticationService, CredentialSecretRepository,
    SecretEnvelope, SecretRepository, StagedSecretRef, StoredCredentials,
};
use pige_core::mail::{
    Address, Flags, Folder, FolderId, FolderKind, MessageHeader, StoredMessage, ThreadId,
};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::providers::{
    AuthAttemptId, ChangePage, CollectionRecord, IMAP_SMTP_PROVIDER, MailChange, MessageRecord,
    OperationContext, PreparedSubmission, ProviderCursor, ProviderError, ProviderFactory,
    ProviderId, ReconcileOutcome, RemoteCollectionId, RemoteMessageId, SentCopyPolicy,
    SubmissionOutcome, SubmissionReceipt, SubmissionRequest, SubmissionService,
    fake::FakeProviderFactory,
};
use pige_core::send::{
    OutgoingMessage, SubmissionState, flush_provider_outbox, outbox_entry, queue_submission,
    save_draft,
};
use pige_core::store::{clear_migration_fault, inject_migration_fault};
use pige_core::sync::provider::account_job;
use pige_core::{SCHEMA_VERSION, Store, StoreError};

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

const WORKER_DB_ENV: &str = "PIGE_H1_WORKER_DB";
const LEASE_DB_ENV: &str = "PIGE_H1_LEASE_DB";
const WORKER_ACCOUNT: &str = "worker-account";
const LEASE_ACCOUNT: &str = "lease-account";

fn imap_account(id: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: format!("{id}@example.com"),
        display_name: id.to_string(),
        backend: MailBackend::imap_smtp(
            format!("{id}@example.com"),
            ServerConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                security: Security::Tls,
            },
            ServerConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                security: Security::Tls,
            },
        ),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn gmail_account(id: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: "Gmail".to_string(),
        email: "person@example.com".to_string(),
        display_name: "Person".to_string(),
        backend: MailBackend::gmail("google-user-recovery"),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn outgoing(account: &AccountId, subject: &str) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account.clone(),
        to: vec![Address::new(None, "charles@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_string(),
        body: "Recovery body.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

fn folder(account: &AccountId, id: &str) -> Folder {
    Folder {
        id: FolderId(id.to_string()),
        account_id: account.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 1,
        total: 1,
        uidvalidity: Some(1),
        uidnext: Some(2),
    }
}

fn cached_message(account: &AccountId, folder: &Folder, subject: &str) -> StoredMessage {
    StoredMessage {
        id: pige_core::MessageId(format!("{}-message", folder.id.0)),
        account_id: account.clone(),
        folder_id: Some(folder.id.clone()),
        uid: Some(pige_core::Uid(1)),
        uidvalidity: Some(1),
        header: MessageHeader {
            message_id: Some("<cached@example.com>".to_string()),
            in_reply_to: None,
            references: Vec::new(),
            subject: subject.to_string(),
            from: vec![Address::new(None, "sender@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            reply_to: Vec::new(),
            bcc: Vec::new(),
            date: 1,
            size: 64,
        },
        flags: Flags::default(),
        thread_id: ThreadId("cached-thread".to_string()),
        snippet: subject.to_string(),
        has_attachments: false,
    }
}

fn provider_record(remote: &str, collections: &[&str], snippet: String) -> MessageRecord {
    MessageRecord {
        remote_id: RemoteMessageId::new(remote),
        remote_thread: None,
        header: MessageHeader {
            message_id: Some(format!("<{remote}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: format!("Subject {remote}"),
            from: vec![Address::new(None, "sender@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            reply_to: Vec::new(),
            bcc: Vec::new(),
            date: 1,
            size: 64,
        },
        flags: Flags::default(),
        collections: collections
            .iter()
            .map(|remote| RemoteCollectionId::new(*remote))
            .collect(),
        snippet,
        has_attachments: false,
    }
}

fn inbox_collection() -> CollectionRecord {
    CollectionRecord {
        remote_id: RemoteCollectionId::new("INBOX"),
        name: "INBOX".to_string(),
        kind: FolderKind::Inbox,
        delimiter: Some("/".to_string()),
        total: None,
        unread: None,
    }
}

fn authenticated(
    target: Account,
    credential_ref: StagedSecretRef,
) -> pige_core::auth::AuthenticatedAccount {
    pige_core::auth::AuthenticatedAccount {
        account: target,
        persistence: pige_core::auth::SecretPersistence::Persistent,
        credential_ref,
    }
}

// ---------------------------------------------------------------------------
// API harness (process-wide state is serialized by TEST_LOCK)
// ---------------------------------------------------------------------------

struct RecoveryHarness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
    transport: Arc<FakeTransportFactory>,
    providers: Arc<dyn ProviderFactory>,
    backups: Arc<FakeBackupStore>,
    auth: Option<Arc<Mutex<Box<dyn AuthenticationService>>>>,
}

impl RecoveryHarness {
    fn provider_neutral() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        Self {
            dir,
            credentials,
            transport: Arc::new(FakeTransportFactory::default()),
            providers: Arc::new(FakeProviderFactory::new()),
            backups: Arc::new(FakeBackupStore::new()),
            auth: None,
        }
    }

    fn imap_only() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        let transport = Arc::new(FakeTransportFactory::default());
        let providers: Arc<dyn ProviderFactory> = Arc::new(
            pige_core::providers::imap::imap_registry(transport.clone(), credentials.clone()),
        );
        Self {
            dir,
            credentials,
            transport,
            providers,
            backups: Arc::new(FakeBackupStore::new()),
            auth: None,
        }
    }

    fn with_hanging_revoke() -> Self {
        let mut harness = Self::imap_only();
        harness.auth = Some(Arc::new(Mutex::new(
            Box::new(HangingRevoke) as Box<dyn AuthenticationService>
        )));
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

    fn restart(&self) {
        api::reset_for_tests();
        api::init(self.config()).expect("reinit api");
    }

    fn db_path(&self) -> PathBuf {
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
        let email = format!("{}@example.com", label.to_ascii_lowercase());
        let server = ServerConfig {
            host: "imap.example.com".to_string(),
            port: 993,
            security: Security::Tls,
        };
        let (id, _) = api::add_account_and_id(AccountInput {
            label: label.to_string(),
            email: email.clone(),
            display_name: label.to_string(),
            username: email,
            password: format!("secret-{label}"),
            imap: server.clone(),
            smtp: ServerConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                security: Security::Tls,
            },
        })
        .await
        .expect("add account");
        id
    }
}

struct HangingRevoke;

#[async_trait]
impl AuthenticationService for HangingRevoke {
    async fn begin(&mut self, _request: AuthRequest) -> Result<AuthProgress, ProviderError> {
        Err(ProviderError::unsupported("synthetic auth"))
    }

    async fn respond(
        &mut self,
        _attempt: &AuthAttemptId,
        _answer: AuthAnswer,
    ) -> Result<AuthProgress, ProviderError> {
        Err(ProviderError::unsupported("synthetic auth"))
    }

    async fn cancel(&mut self, _attempt: &AuthAttemptId) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn resume(&mut self, _account: &AccountId) -> Result<AuthProgress, ProviderError> {
        Err(ProviderError::unsupported("synthetic auth"))
    }

    async fn revoke(&mut self, _account: &AccountId) -> Result<(), ProviderError> {
        // Deliberately never completes. Local removal must not wait for a
        // provider revocation, and another account's cached reader must still
        // be usable while this task is stuck.
        std::future::pending::<()>().await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Migration recovery
// ---------------------------------------------------------------------------

const V1_CONFIG: &str = r##"{
  "version": 1,
  "accounts": [
    {
      "id": "acct-1",
      "label": "Personal",
      "email": "ada@example.com",
      "display_name": "Ada Lovelace",
      "username": "ada-user",
      "imap": { "host": "imap.custom.example", "port": 1993, "security": "StartTls" },
      "smtp": { "host": "smtp.custom.example", "port": 2587, "security": "Tls" },
      "auth": "Password",
      "sort_order": 4,
      "color": "#c2185b"
    }
  ],
  "settings": {},
  "blocked_senders": []
}"##;

#[test]
fn v1_to_latest_config_and_v4_cache_recover() {
    let dir = TempDir::new().expect("temp dir");
    let config_path = dir.path().join("pige.json");
    std::fs::write(&config_path, V1_CONFIG).expect("write v1 config");

    let database = dir.path().join(DB_FILE_NAME);
    let fixture = LegacyV4::create(&database);
    fixture
        .account("acct-1", "ada@example.com", 4)
        .folder("inbox", "acct-1", "INBOX", "inbox", 7)
        .message(
            "msg-1",
            "acct-1",
            "inbox",
            1,
            7,
            Some("<one@example.com>"),
            "Upgrade me",
            "thread-1",
            1,
        )
        .body("msg-1", "body survives the upgrade")
        .attachment("att-1", "msg-1", "2", "report.pdf")
        .outbox("out-1", "acct-1", "draft")
        .fts(
            "msg-1",
            "acct-1",
            "inbox",
            "Upgrade me",
            "body survives the upgrade",
        );

    let document = pige_core::config::load(&config_path)
        .expect("read v1 config")
        .expect("document");
    assert_eq!(document.version, pige_core::config::CURRENT_VERSION);
    let configured = &document.accounts[0];
    assert_eq!(configured.id, AccountId("acct-1".to_string()));
    let (_, imap, smtp, _) = configured.imap_smtp().expect("migrated to IMAP");
    assert_eq!(imap.host, "imap.custom.example");
    assert_eq!(smtp.port, 2587);

    let store = fixture.into_store();
    store.migrate().expect("v4 cache upgrades");
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    let mirrored = store
        .accounts()
        .expect("accounts")
        .into_iter()
        .find(|account| account.id == configured.id)
        .expect("the v4 account survives");
    assert_eq!(mirrored.provider_id(), ProviderId::from(IMAP_SMTP_PROVIDER));

    let message_id = pige_core::MessageId("msg-1".to_string());
    let stored = store
        .message(&message_id)
        .expect("read message")
        .expect("cached message survives");
    assert_eq!(stored.header.subject, "Upgrade me");
    assert_eq!(
        store
            .body(&message_id)
            .expect("read body")
            .expect("cached body survives")
            .plain_text
            .as_deref(),
        Some("body survives the upgrade")
    );
    assert_eq!(
        store.attachments(&message_id).expect("attachments").len(),
        1
    );
    assert_eq!(
        outbox_entry(&store, &pige_core::send::OutboxId("out-1".to_string()))
            .expect("read outbox")
            .expect("draft survives")
            .state,
        SubmissionState::Draft
    );

    let hits =
        pige_core::search::search(&store, "survives", &pige_core::SearchScope::AllAccounts, 10)
            .expect("search migrated cache");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].thread_id, ThreadId("thread-1".to_string()));
}

/// The separate tests above migrate the document and the cache in isolation.
/// This exercises the real startup sequence with both at once: a v1
/// configuration document and a v4 cache in the same data directory must come
/// up together, and a process reopen must find the same account, cache, and
/// cached body.
#[tokio::test]
async fn v1_config_and_v4_cache_reopen_together() {
    let _guard = TEST_LOCK.lock().await;
    let harness = RecoveryHarness::imap_only();

    let config_path = pige_core::config::document_path(&harness.dir.path().join("config"));
    std::fs::create_dir_all(config_path.parent().expect("config parent")).expect("config dir");
    std::fs::write(&config_path, V1_CONFIG).expect("write v1 config");

    let fixture = LegacyV4::create(&harness.db_path());
    fixture
        .account("acct-1", "ada@example.com", 4)
        .folder("inbox", "acct-1", "INBOX", "inbox", 7)
        .message(
            "msg-1",
            "acct-1",
            "inbox",
            1,
            7,
            Some("<one@example.com>"),
            "Startup survives",
            "thread-1",
            1,
        )
        .body("msg-1", "cached before the upgrade")
        .outbox("out-1", "acct-1", "draft");
    drop(fixture);

    harness.init();

    let account = api::account(&AccountId("acct-1".to_string())).expect("v1 account");
    let (username, imap, smtp, _) = account.imap_smtp().expect("migrated to IMAP");
    assert_eq!(username, "ada-user");
    assert_eq!(imap.host, "imap.custom.example");
    assert_eq!(imap.port, 1993);
    assert_eq!(smtp.host, "smtp.custom.example");
    assert_eq!(smtp.port, 2587);

    let snapshot = api::snapshot();
    assert!(
        snapshot
            .conversations
            .iter()
            .any(|conversation| conversation.thread_id == ThreadId("thread-1".to_string())),
        "the v4 cached conversation must appear after startup"
    );

    // Reopen exactly as the next process would: drop the API state without
    // touching the data or config directories, then init again.
    harness.restart();
    assert_eq!(
        api::account(&AccountId("acct-1".to_string()))
            .expect("account after reopen")
            .imap_smtp()
            .expect("still IMAP")
            .1
            .host,
        "imap.custom.example"
    );
    let opened = api::open_conversation(&ThreadId("thread-1".to_string()))
        .await
        .expect("cached reader after reopen");
    let detail = opened.selected_conversation.expect("selected conversation");
    assert_eq!(detail.messages.len(), 1);
    match detail.messages[0].body.clone().expect("cached body") {
        pige_core::api::MessageBody::Document(_) => {}
        other => panic!("expected the cached document, got {other:?}"),
    }
}

#[test]
fn every_outbox_state_survives_upgrade() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join(DB_FILE_NAME);
    let fixture = LegacyV4::create(&database);
    let states = [
        "draft",
        "queued",
        "preparing",
        "submitting",
        "accepted",
        "complete",
        "rejected",
        "uncertain",
        // Legacy v4 spellings.
        "sending",
        "sent_local",
        "sent",
        "failed",
    ];
    fixture.account("acct-1", "ada@example.com", 0);
    for (index, state) in states.iter().enumerate() {
        fixture.outbox(&format!("out-{index}"), "acct-1", state);
    }

    let store = fixture.into_store();
    store.migrate().expect("migrate outbox");
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    let account = AccountId("acct-1".to_string());
    let items = store.outbox_items(Some(&account)).expect("outbox rows");
    assert_eq!(items.len(), states.len(), "no outbox row may be dropped");

    let expected = [
        SubmissionState::Draft,
        SubmissionState::Queued,
        SubmissionState::Preparing,
        SubmissionState::Submitting,
        SubmissionState::Accepted,
        SubmissionState::Complete,
        SubmissionState::Rejected,
        SubmissionState::Uncertain,
        SubmissionState::Uncertain,
        SubmissionState::Accepted,
        SubmissionState::Complete,
        SubmissionState::Uncertain,
    ];
    for (index, expected) in expected.into_iter().enumerate() {
        let id = pige_core::send::OutboxId(format!("out-{index}"));
        let entry = items
            .iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("out-{index} must survive"));
        assert_eq!(
            entry.state, expected,
            "legacy state {:?} must decode conservatively",
            states[index]
        );
        assert!(
            !entry.operation_id.0.is_empty(),
            "every migrated submission needs a stable operation id"
        );
        assert_eq!(
            entry.backend.as_ref().map(ProviderId::as_str),
            Some(IMAP_SMTP_PROVIDER),
            "pre-provider rows belong to the IMAP/SMTP transport"
        );
    }

    // The raw catalog is canonical after v9, so a later process cannot
    // reinterpret a legacy name differently.
    let conn = Connection::open(&database).expect("raw connection");
    for (index, canonical) in [
        "draft",
        "queued",
        "preparing",
        "submitting",
        "accepted",
        "complete",
        "rejected",
        "uncertain",
        "uncertain",
        "accepted",
        "complete",
        "uncertain",
    ]
    .into_iter()
    .enumerate()
    {
        let raw: String = conn
            .query_row(
                "SELECT state FROM outbox WHERE id = ?1",
                [format!("out-{index}")],
                |row| row.get(0),
            )
            .expect("read canonical state");
        assert_eq!(raw, canonical, "out-{index}");
    }
}

#[test]
fn migration_fault_before_swap_rolls_back() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join(DB_FILE_NAME);
    let fixture = LegacyV4::create(&database);
    fixture
        .account("acct-1", "ada@example.com", 0)
        .folder("inbox", "acct-1", "INBOX", "inbox", 7)
        .message(
            "msg-1",
            "acct-1",
            "inbox",
            1,
            7,
            Some("<one@example.com>"),
            "Keep me",
            "thread-1",
            1,
        )
        .body("msg-1", "rollback body")
        .attachment("att-1", "msg-1", "2", "report.pdf")
        .outbox("out-1", "acct-1", "queued");

    // The documented rollback path is a pre-upgrade consistent backup, not an
    // older binary on the new schema. Prove that backup opens at v4 with the
    // user's rows intact.
    let backup = dir.path().join("pige-pre-upgrade.db");
    let backup_conn = Connection::open(&database).expect("backup connection");
    backup_conn
        .execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])
        .expect("consistent backup");
    drop(backup_conn);
    let backup_store = Store::open(&backup).expect("open backup");
    assert_eq!(backup_store.schema_version().unwrap(), 4);
    let backup_conn = Connection::open(&backup).expect("backup verify connection");
    assert_eq!(
        backup_conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
    drop(backup_conn);
    drop(backup_store);

    inject_migration_fault(1);
    let store = fixture.into_store();
    let result = store.migrate();
    clear_migration_fault();
    assert!(
        matches!(result, Err(StoreError::Corrupt { .. })),
        "the injected pre-swap fault must abort the migration, got {result:?}"
    );

    // The v4 schema and every dependent row are exactly as they were.
    assert_eq!(store.schema_version().unwrap(), 4);
    let conn = Connection::open(&database).expect("verify connection");
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'folder_id'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1,
        "the messages table must not have swapped"
    );
    for table in ["messages", "bodies", "attachments", "outbox"] {
        assert_eq!(
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1,
            "{table} must roll back intact"
        );
    }
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );

    // Clearing the fault lets the same database migrate cleanly; retrying is
    // always the in-place recovery, never running an older binary.
    store.migrate().expect("retry migration");
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(
        store
            .messages_for_account(&AccountId("acct-1".to_string()))
            .expect("messages")
            .len(),
        1
    );
}

/// A disk-full write must not be mistaken for cross-process contention and
/// must not delete or partially apply user state. The job stays on its last
/// committed checkpoint and the identical page commits once space is
/// available, exactly the retry a scheduled sync performs.
#[test]
fn database_full_preserves_state_and_recovers_after_limit_raised() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join(DB_FILE_NAME);
    let store = Store::open(&database).expect("open store");
    store.migrate().expect("migrate");
    let account = imap_account("db-full");
    store.upsert_account(&account).expect("account");
    let key = account_job(&account.id);
    let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));

    // Establish durable state and a committed checkpoint.
    let first = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(inbox_collection()),
            MailChange::UpsertMessage {
                message: provider_record("kept", &["INBOX"], "existing".to_string()),
                is_new_arrival: false,
            },
        ],
        checkpoint: ProviderCursor::new(1, "cp-1"),
        has_more: false,
    };
    store
        .apply_change_page(&ctx, &key, IMAP_SMTP_PROVIDER, None, &first, None)
        .expect("seed page");
    let kept = store
        .remote_message_for(
            &account.id,
            IMAP_SMTP_PROVIDER,
            &RemoteMessageId::new("kept"),
        )
        .expect("mapping")
        .expect("existing message mapped");

    // Cap the database at its current size. The next page cannot allocate even
    // one more page, so SQLite reports SQLITE_FULL on a genuine write.
    let cap = store
        .limit_database_size_for_test()
        .expect("lower page cap");
    assert!(cap > 0);

    let too_big = ChangePage {
        changes: vec![MailChange::UpsertMessage {
            message: provider_record("too-big", &["INBOX"], "x".repeat(8 * 1024 * 1024)),
            is_new_arrival: false,
        }],
        checkpoint: ProviderCursor::new(1, "cp-2"),
        has_more: false,
    };
    let error = store
        .apply_change_page(
            &ctx,
            &key,
            IMAP_SMTP_PROVIDER,
            Some(&ProviderCursor::new(1, "cp-1")),
            &too_big,
            None,
        )
        .expect_err("a full database must fail the write");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "a disk-full write must surface as a genuine error, not contention: {error:?}"
    );

    // Existing content, mappings, and the checkpoint are untouched.
    assert!(store.message(&kept).expect("read kept message").is_some());
    assert!(
        store
            .remote_message_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("too-big"),
            )
            .expect("lookup failed page")
            .is_none(),
        "a failed page must not leave a remote mapping"
    );
    assert_eq!(
        store.job_checkpoint(&key).expect("checkpoint"),
        Some(ProviderCursor::new(1, "cp-1")),
        "a failed page must not advance the cursor"
    );
    let conn = Connection::open(&database).expect("raw connection");
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    drop(conn);

    // Raise the limit: the same page now commits, proving the job was left
    // retryable rather than poisoned.
    let raised = store
        .set_max_page_count_for_test(u32::MAX)
        .expect("raise page cap");
    assert!(raised > cap);
    store
        .apply_change_page(
            &ctx,
            &key,
            IMAP_SMTP_PROVIDER,
            Some(&ProviderCursor::new(1, "cp-1")),
            &too_big,
            None,
        )
        .expect("retry after space is available");
    assert_eq!(
        store.job_checkpoint(&key).expect("checkpoint after retry"),
        Some(ProviderCursor::new(1, "cp-2"))
    );
    assert!(store.message(&kept).expect("kept after retry").is_some());
    assert!(
        store
            .remote_message_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("too-big"),
            )
            .expect("retried mapping")
            .is_some()
    );

    // A reopened process sees the recovered state too.
    drop(store);
    let reopened = Store::open(&database).expect("reopen");
    reopened.migrate().expect("reopen migrate");
    assert_eq!(
        reopened.job_checkpoint(&key).expect("reopened checkpoint"),
        Some(ProviderCursor::new(1, "cp-2"))
    );
    assert!(
        reopened
            .message(&kept)
            .expect("reopened existing message")
            .is_some()
    );
}

#[test]
fn newer_config_schema_and_secret_envelopes_stay_untouched() {
    // A configuration written by a newer build is rejected before any save and
    // is left byte-for-byte intact.
    let dir = TempDir::new().expect("temp dir");
    let config_path = dir.path().join("pige.json");
    let newer_config = format!(
        r#"{{"version": {}, "accounts": [], "settings": {{}}, "blocked_senders": []}}"#,
        pige_core::config::CURRENT_VERSION + 1
    );
    std::fs::write(&config_path, &newer_config).expect("write newer config");
    assert!(matches!(
        pige_core::config::load(&config_path),
        Err(pige_core::config::ConfigError::TooNew { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        newer_config,
        "a newer configuration must never be quarantined or overwritten"
    );

    // A newer database schema is rejected without modifying the database.
    let database = dir.path().join(DB_FILE_NAME);
    let store = Store::open(&database).expect("open store");
    store.migrate().expect("migrate store");
    {
        let conn = Connection::open(&database).expect("raw connection");
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            [(SCHEMA_VERSION + 1).to_string()],
        )
        .expect("mark newer schema");
    }
    assert!(matches!(
        store.migrate(),
        Err(StoreError::SchemaTooNew { .. })
    ));
    let conn = Connection::open(&database).expect("verify connection");
    let recorded: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("read schema version");
    assert_eq!(recorded, (SCHEMA_VERSION + 1).to_string());
    drop(conn);
    drop(store);

    // A secret envelope from a newer build is a hard error, never reinterpreted
    // as a legacy bare password (which the next save would re-persist).
    let newer_secret =
        r#"{"version":999,"credentials":{"kind":"password","password":"do-not-reinterpret"}}"#;
    let error = SecretEnvelope::from_stored_string(newer_secret)
        .expect_err("a newer secret envelope must be rejected");
    assert!(
        error.to_string().contains("newer than supported"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Config/credential switch recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn switch_fault_between_secret_and_config_recovers() {
    let _guard = TEST_LOCK.lock().await;
    let harness = RecoveryHarness::provider_neutral();
    harness.init();
    let account = harness.add_imap("Switch").await;
    let previous = api::account(&account).expect("account");
    let mut target = previous.clone();
    target.backend = MailBackend::gmail("google-user-switch");
    target.contact_sources = Vec::new();

    let staged = harness.stage("switched-secret");
    api::inject_switch_crash_after_secret();
    let error = api::switch_backend(&account, authenticated(target, staged))
        .await
        .expect_err("the injected crash surfaces");
    api::clear_switch_crash();
    assert!(error.contains("simulated"), "{error}");

    // The crash left the new secret active while the old config is still
    // published, plus a durable recovery intent.
    assert!(api::account(&account).unwrap().imap_smtp().is_some());
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("switched-secret")
    );
    assert!(harness.store().has_switch_intent(&account).unwrap());

    // Reopening the process restores the old config/credential pair.
    harness.restart();
    assert!(
        api::account(&account).unwrap().imap_smtp().is_some(),
        "startup must roll the config back"
    );
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("secret-Switch"),
        "the previous credential must be restored with its config"
    );
    assert!(
        !harness.store().has_switch_intent(&account).unwrap(),
        "a recovered switch leaves no intent"
    );
    api::can_switch_backend(&account).expect("a recovered account can switch again");
}

// ---------------------------------------------------------------------------
// Real subprocess kill/reopen
// ---------------------------------------------------------------------------

struct KillOnSubmit;

#[async_trait]
impl SubmissionService for KillOnSubmit {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        SentCopyPolicy::ClientAppend
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        Ok(PreparedSubmission {
            version: 1,
            backend: ProviderId::from(IMAP_SMTP_PROVIDER),
            raw: request.prepared.as_ref().and_then(|part| part.raw.clone()),
            reference: None,
        })
    }

    async fn submit(
        &mut self,
        _request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        // Simulate process death after the provider accepted the write but
        // before the acceptance was made durable.
        std::process::exit(42);
    }

    async fn reconcile(
        &mut self,
        _request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        Ok(ReconcileOutcome::Unknown)
    }

    async fn finalize_sent(
        &mut self,
        _request: &SubmissionRequest,
        _receipt: &SubmissionReceipt,
        _ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
}

struct NeverSubmit {
    submissions: usize,
    reconcile: ReconcileOutcome,
}

impl NeverSubmit {
    fn unknown() -> Self {
        Self {
            submissions: 0,
            reconcile: ReconcileOutcome::Unknown,
        }
    }
}

#[async_trait]
impl SubmissionService for NeverSubmit {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        SentCopyPolicy::ClientAppend
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        Ok(PreparedSubmission {
            version: 1,
            backend: ProviderId::from(IMAP_SMTP_PROVIDER),
            raw: request.prepared.as_ref().and_then(|part| part.raw.clone()),
            reference: None,
        })
    }

    async fn submit(
        &mut self,
        _request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        self.submissions += 1;
        Ok(SubmissionOutcome::Rejected(ProviderError::transient(
            "must not submit",
        )))
    }

    async fn reconcile(
        &mut self,
        _request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        Ok(self.reconcile.clone())
    }

    async fn finalize_sent(
        &mut self,
        _request: &SubmissionRequest,
        _receipt: &SubmissionReceipt,
        _ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
}

fn worker_account() -> Account {
    imap_account(WORKER_ACCOUNT)
}

/// Helper executed in a child process by
/// [`worker_kill_after_submission_never_resends`]. Ignored during ordinary test
/// runs.
#[test]
#[ignore = "helper executed in a killed child process by the worker recovery test"]
fn worker_kill_child() {
    let Some(database) = std::env::var_os(WORKER_DB_ENV) else {
        return;
    };
    let store = Store::open(database).expect("child store");
    store.migrate().expect("child migrate");
    let account = worker_account();
    store.upsert_account(&account).expect("child account");
    queue_submission(
        &store,
        outgoing(&account.id, "Killed after submission"),
        Vec::new(),
    )
    .expect("child queue");

    let runtime = tokio::runtime::Runtime::new().expect("child runtime");
    runtime.block_on(async {
        let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));
        let mut service = KillOnSubmit;
        let _ = flush_provider_outbox(&store, &account, &mut service, &ctx).await;
    });
    panic!("the child should have died during submit");
}

#[tokio::test]
async fn worker_kill_after_submission_never_resends() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join(DB_FILE_NAME);
    let executable = std::env::current_exe().expect("current test binary");
    let output = Command::new(executable)
        .args([
            "--ignored",
            "--exact",
            "worker_kill_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(WORKER_DB_ENV, &database)
        .output()
        .expect("spawn killed worker");

    assert_eq!(
        output.status.code(),
        Some(42),
        "the child must die during submit; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let account = worker_account();
    let store = Store::open(&database).expect("parent open");
    store.migrate().expect("parent migrate");
    let items = store
        .outbox_items(Some(&account.id))
        .expect("parent outbox");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].state,
        SubmissionState::Submitting,
        "the kill happened after the prepared submission became durable"
    );

    // Reconciliation cannot prove the outcome, so the row becomes Uncertain and
    // no second submission is attempted.
    let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));
    let mut service = NeverSubmit::unknown();
    let report = flush_provider_outbox(&store, &account, &mut service, &ctx)
        .await
        .expect("reconcile after kill");
    assert_eq!(
        service.submissions, 0,
        "a killed submission must not resend"
    );
    assert_eq!(report.uncertain, 1, "{report:?}");
    assert_eq!(
        outbox_entry(&store, &items[0].id).unwrap().unwrap().state,
        SubmissionState::Uncertain
    );

    // A reopened `Store` (same process, fresh connection and migrated state)
    // sees the same terminal state and still does not resend it. The genuine
    // kill above is the cross-process evidence; this reopen proves durability.
    drop(store);
    let reopened = Store::open(&database).expect("reopen");
    reopened.migrate().expect("reopen migrate");
    let mut service = NeverSubmit::unknown();
    let report = flush_provider_outbox(&reopened, &account, &mut service, &ctx)
        .await
        .expect("reopen reconcile");
    assert_eq!(service.submissions, 0, "reopen must not resend");
    assert_eq!(report.uncertain, 1, "{report:?}");
}

// ---------------------------------------------------------------------------
// Two real processes sharing one database and lease
// ---------------------------------------------------------------------------

fn lease_child_page() -> pige_core::providers::ChangePage {
    pige_core::providers::ChangePage {
        changes: vec![pige_core::providers::MailChange::UpsertCollection(
            pige_core::providers::CollectionRecord {
                remote_id: RemoteCollectionId::new("child-inbox"),
                name: "Child INBOX".to_string(),
                kind: FolderKind::Inbox,
                delimiter: Some("/".to_string()),
                total: None,
                unread: None,
            },
        )],
        checkpoint: ProviderCursor::new(1, "child-cursor"),
        has_more: false,
    }
}

/// Helper executed in a child process by
/// [`two_process_leases_do_not_regress_cursor`]. Ignored during ordinary runs.
#[test]
#[ignore = "helper executed as a second process by the lease recovery test"]
fn lease_child_process() {
    let Some(database) = std::env::var_os(LEASE_DB_ENV) else {
        return;
    };
    let store = Store::open(database).expect("child store");
    store.migrate().expect("child migrate");
    let account = imap_account(LEASE_ACCOUNT);
    store.upsert_account(&account).expect("child account");
    let lease = store
        .acquire_lease(&account.id, "child-process", Duration::from_secs(60))
        .expect("child lease");
    assert_eq!(lease.fencing, 1);
    let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));
    store
        .apply_change_page(
            &ctx,
            &account_job(&account.id),
            IMAP_SMTP_PROVIDER,
            None,
            &lease_child_page(),
            Some(&lease),
        )
        .expect("child page");
    // Exit without releasing the lease: this models a process that died while
    // holding the cross-process account gate.
}

#[tokio::test]
async fn two_process_leases_do_not_regress_cursor() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join(DB_FILE_NAME);
    let executable = std::env::current_exe().expect("current test binary");
    let output = Command::new(executable)
        .args([
            "--ignored",
            "--exact",
            "lease_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(LEASE_DB_ENV, &database)
        .output()
        .expect("spawn lease child");
    assert!(
        output.status.success(),
        "lease child failed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let account = imap_account(LEASE_ACCOUNT);
    let store = Store::open(&database).expect("parent open");
    store.migrate().expect("parent migrate");
    let key = account_job(&account.id);

    // The child's crash left its lease live, so the parent cannot take over.
    assert!(
        matches!(
            store.acquire_lease(&account.id, "parent-process", Duration::from_secs(60)),
            Err(StoreError::LeaseHeld)
        ),
        "a live crashed process's lease must still fence a second process"
    );
    assert_eq!(
        store.job_checkpoint(&key).expect("read checkpoint"),
        Some(ProviderCursor::new(1, "child-cursor"))
    );

    // The parent's page was read before the child committed, so its expected
    // previous cursor is `None`. The compare-and-swap must reject it instead of
    // moving the durable checkpoint backwards.
    let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));
    let stale = pige_core::providers::ChangePage {
        changes: Vec::new(),
        checkpoint: ProviderCursor::new(1, "parent-cursor"),
        has_more: false,
    };
    assert!(matches!(
        store.apply_change_page(&ctx, &key, IMAP_SMTP_PROVIDER, None, &stale, None),
        Err(StoreError::StalePage)
    ));
    assert_eq!(
        store
            .job_checkpoint(&key)
            .expect("checkpoint after stale page"),
        Some(ProviderCursor::new(1, "child-cursor")),
        "a stale second process must not regress the cursor"
    );

    // Releasing the crashed child's lease (what a later process does after the
    // TTL expires) still cannot make the already-stale page acceptable: the
    // durable checkpoint is the authority.
    let crashed = pige_core::store::AccountLease {
        account: account.id.clone(),
        holder: "child-process".to_string(),
        fencing: 1,
        expires_at_ms: 0,
    };
    store
        .release_lease(&crashed)
        .expect("release crashed lease");
    let takeover = store
        .acquire_lease(&account.id, "parent-process", Duration::from_secs(60))
        .expect("takeover after the crashed lease is released");
    assert_eq!(takeover.fencing, 2);
    assert!(matches!(
        store.apply_change_page(
            &ctx,
            &key,
            IMAP_SMTP_PROVIDER,
            None,
            &stale,
            Some(&takeover)
        ),
        Err(StoreError::StalePage)
    ));
    assert_eq!(
        store.job_checkpoint(&key).expect("final checkpoint"),
        Some(ProviderCursor::new(1, "child-cursor"))
    );
}

/// The checkpoint compare-and-swap alone does not stop a writer that lost the
/// cross-process account gate: its previous cursor can still match while
/// another process has already done remote work. The lease/fencing check must
/// reject the commit in the same transaction as the write.
#[test]
fn change_page_commit_rejects_a_lease_lost_before_the_write() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join(DB_FILE_NAME);
    let store = Store::open(&database).expect("open store");
    store.migrate().expect("migrate");
    let account = imap_account("lease-fence");
    store.upsert_account(&account).expect("account");
    let key = account_job(&account.id);
    let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));

    let first = ChangePage {
        changes: vec![MailChange::UpsertCollection(inbox_collection())],
        checkpoint: ProviderCursor::new(1, "cp-1"),
        has_more: false,
    };
    let owner = store
        .acquire_lease(&account.id, "writer-process", Duration::from_secs(60))
        .expect("writer lease");
    store
        .apply_change_page(&ctx, &key, IMAP_SMTP_PROVIDER, None, &first, Some(&owner))
        .expect("first page under a live lease");

    // The writer crashes/abandons its pass; a new process takes over and the
    // fencing token advances. The old handle must no longer be able to write.
    store.release_lease(&owner).expect("release writer lease");
    let takeover = store
        .acquire_lease(&account.id, "new-process", Duration::from_secs(60))
        .expect("takeover");
    assert_eq!(takeover.fencing, owner.fencing + 1);

    let second = ChangePage {
        changes: vec![MailChange::UpsertMessage {
            message: provider_record("fenced", &["INBOX"], "must not land".to_string()),
            is_new_arrival: false,
        }],
        checkpoint: ProviderCursor::new(1, "cp-2"),
        has_more: false,
    };
    let stale_lease = owner.clone();
    let error = store
        .apply_change_page(
            &ctx,
            &key,
            IMAP_SMTP_PROVIDER,
            Some(&ProviderCursor::new(1, "cp-1")),
            &second,
            Some(&stale_lease),
        )
        .expect_err("a stale lease must not commit");
    assert!(matches!(error, StoreError::LeaseLost), "{error:?}");
    assert_eq!(
        store.job_checkpoint(&key).expect("checkpoint after reject"),
        Some(ProviderCursor::new(1, "cp-1")),
        "the rejected page must not advance the cursor"
    );
    assert!(
        store
            .remote_message_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("fenced"),
            )
            .expect("lookup fenced message")
            .is_none(),
        "the rejected page must not persist content"
    );

    // The current holder can commit the same page.
    store
        .apply_change_page(
            &ctx,
            &key,
            IMAP_SMTP_PROVIDER,
            Some(&ProviderCursor::new(1, "cp-1")),
            &second,
            Some(&takeover),
        )
        .expect("current lease commits");
    assert_eq!(
        store.job_checkpoint(&key).expect("final checkpoint"),
        Some(ProviderCursor::new(1, "cp-2"))
    );
}

// ---------------------------------------------------------------------------
// Feature availability, backend swap, and revocation isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_provider_keeps_cached_reader_available() {
    let _guard = TEST_LOCK.lock().await;
    let harness = RecoveryHarness::imap_only();
    let account = gmail_account("disabled-gmail");

    // Seed the cache exactly as a previous build with the native provider
    // enabled would have left it, then publish a configuration naming it.
    {
        let store = harness.store();
        store.upsert_account(&account).expect("mirror account");
        let inbox = folder(&account.id, "disabled-inbox");
        store.upsert_folder(&inbox).expect("folder");
        let message = cached_message(&account.id, &inbox, "Cached while enabled");
        store.upsert_message(&message).expect("message");
        store
            .upsert_body(
                &message.id,
                &pige_core::store::Body {
                    plain_text: Some("still readable offline".to_string()),
                    ..pige_core::store::Body::default()
                },
            )
            .expect("body");
        let path = pige_core::config::document_path(&harness.dir.path().join("config"));
        std::fs::create_dir_all(path.parent().unwrap()).expect("config dir");
        pige_core::config::save(
            &path,
            &pige_core::config::ConfigDocument {
                accounts: vec![account.clone()],
                ..pige_core::config::ConfigDocument::default()
            },
        )
        .expect("write config");
    }

    harness.init();
    let snapshot = api::snapshot();
    let summary = snapshot
        .accounts
        .iter()
        .find(|summary| summary.id == account.id)
        .expect("the unavailable account is still listed");
    assert!(!summary.available);
    assert_eq!(summary.provider, "gmail");
    assert!(
        summary
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("not available")),
        "{:?}",
        summary.unavailable_reason
    );
    assert!(
        !snapshot
            .available_providers
            .iter()
            .any(|provider| provider == "gmail"),
        "the feature-disabled build must not advertise Gmail"
    );
    assert!(
        snapshot
            .conversations
            .iter()
            .any(|conversation| conversation.thread_id == ThreadId("cached-thread".to_string())),
        "cached mail must remain in the list"
    );

    // Opening the cached conversation must not need the missing provider.
    let opened = api::open_conversation(&ThreadId("cached-thread".to_string()))
        .await
        .expect("cached reader opens");
    let detail = opened.selected_conversation.expect("selected conversation");
    let body = detail.messages[0].body.clone().expect("body");
    match body {
        pige_core::api::MessageBody::Document(_) => {}
        other => panic!("expected the cached document, got {other:?}"),
    }
}

#[tokio::test]
async fn backend_swap_preserves_drafts_not_uid_mappings() {
    let _guard = TEST_LOCK.lock().await;
    let harness = RecoveryHarness::provider_neutral();
    harness.init();
    let account = harness.add_imap("Swap").await;
    let draft = save_draft(&harness.store(), outgoing(&account, "Draft survives")).expect("draft");

    let (message_id, folder_id) = {
        let store = harness.store();
        let inbox = folder(&account, "swap-inbox");
        store.upsert_folder(&inbox).expect("folder");
        let message = cached_message(&account, &inbox, "Old backend mail");
        store.upsert_message(&message).expect("message");
        store
            .set_remote_message(
                &account,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("old-remote-message"),
                &message.id,
            )
            .expect("remote message mapping");
        store
            .set_remote_collection(
                &account,
                IMAP_SMTP_PROVIDER,
                &RemoteCollectionId::new("old-remote-collection"),
                &inbox.id,
            )
            .expect("remote collection mapping");
        store
            .set_checkpoint(
                &account_job(&account),
                1,
                &ProviderCursor::new(1, "old-backend-cursor"),
            )
            .expect("checkpoint");
        (message.id.clone(), inbox.id.clone())
    };

    let mut target = api::account(&account).expect("account");
    target.backend = MailBackend::gmail("google-user-swap");
    target.contact_sources = Vec::new();
    let staged = harness.stage("swapped-secret");
    api::switch_backend(&account, authenticated(target, staged))
        .await
        .expect("switch backend");

    let switched = api::account(&account).expect("account survives");
    assert_eq!(switched.id, account, "the local account id is preserved");
    assert_eq!(switched.provider_id(), ProviderId::from("gmail"));
    assert_eq!(
        harness.stored_password(&account).as_deref(),
        Some("swapped-secret")
    );

    let store = harness.store();
    let stored_draft = outbox_entry(&store, &draft)
        .expect("read draft")
        .expect("draft survives the swap");
    assert_eq!(stored_draft.state, SubmissionState::Draft);
    assert_eq!(stored_draft.message.subject, "Draft survives");

    assert!(
        store
            .remote_message_for(
                &account,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("old-remote-message")
            )
            .expect("read remote map")
            .is_none(),
        "old-provider message/UID mappings must not survive"
    );
    assert!(
        store
            .remote_collection_for(
                &account,
                IMAP_SMTP_PROVIDER,
                &RemoteCollectionId::new("old-remote-collection")
            )
            .expect("read remote collection")
            .is_none(),
        "old-provider collection mappings must not survive"
    );
    assert!(
        store
            .message(&message_id)
            .expect("read old message")
            .is_none(),
        "old-provider cached mail must be rebuilt, not reinterpreted"
    );
    assert!(
        store.folders(&account).expect("folders").is_empty(),
        "old-provider folders must be cleared (was {folder_id:?})"
    );
    assert!(
        store
            .job_checkpoint(&account_job(&account))
            .expect("read checkpoint")
            .is_none(),
        "an old-provider cursor must never be reused"
    );
}

#[tokio::test]
async fn revoke_one_account_does_not_block_others() {
    let _guard = TEST_LOCK.lock().await;
    let harness = RecoveryHarness::with_hanging_revoke();
    harness.init();
    let removed = harness.add_imap("Removed").await;
    let kept = harness.add_imap("Kept").await;

    // Cache a readable message for the account that must keep working.
    {
        let store = harness.store();
        let inbox = folder(&kept, "kept-inbox");
        store.upsert_folder(&inbox).expect("folder");
        let message = cached_message(&kept, &inbox, "Kept cached mail");
        store.upsert_message(&message).expect("message");
        store
            .upsert_body(
                &message.id,
                &pige_core::store::Body {
                    plain_text: Some("kept body".to_string()),
                    ..pige_core::store::Body::default()
                },
            )
            .expect("body");
    }

    // The authentication service's revoke never completes. Local removal must
    // still return; it revokes in the background.
    let snapshot = tokio::time::timeout(Duration::from_secs(2), api::remove_account(&removed))
        .await
        .expect("removal must not wait for provider revocation")
        .expect("local removal succeeds");
    assert!(
        snapshot
            .accounts
            .iter()
            .all(|account| account.id != removed),
        "the revoked account is removed locally"
    );
    assert!(
        snapshot.accounts.iter().any(|account| account.id == kept),
        "the other account remains visible"
    );

    // Let the background revoke task start and block.
    tokio::task::yield_now().await;

    assert!(api::account(&removed).is_err());
    assert!(api::account(&kept).is_ok());
    let opened = api::open_conversation(&ThreadId("cached-thread".to_string()))
        .await
        .expect("the other account's cached reader still works");
    assert!(opened.selected_conversation.is_some());
}
