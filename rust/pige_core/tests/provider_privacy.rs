//! Task H3: privacy regressions across logs, configuration backups,
//! clear/restart behaviour, account removal, outbox artifacts, and denied
//! contact scopes.
//!
//! These tests use synthetic markers only. A marker proves that a value did not
//! cross a boundary; it does **not** claim that cached mail is encrypted at
//! rest or that a logical clear is a forensic erase.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use log::{LevelFilter, Log, Metadata, Record};
use pige_core::MailBackend;
use pige_core::account::{Account, AccountId, Security, ServerConfig};
use pige_core::api::{self, AccountInput, Config};
use pige_core::mail::{Address, Flags, MessageHeader};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::platform::{BackupStore, CredentialStore};
use pige_core::providers::fake::{
    FakeCall, FakeProviderFactory, FakeProviderSession, FakeServices,
};
use pige_core::providers::{
    AddressBookService, ContactPage, ContactSourceId, ErrorKind, HistoryCoverage, HistoryPage,
    IMAP_SMTP_PROVIDER, MessageRecord, OperationContext, PreparedSubmission, ProviderCursor,
    ProviderError, ProviderFactory, ProviderId, RemoteContactId, RemoteMessageId, SentCopyPolicy,
    SubmissionOutcome, SubmissionReceipt,
};
use pige_core::send::{OutgoingMessage, SubmissionState, outbox_entry, queue_submission};
use pige_core::store::{Store, history_job};
use pige_core::sync::provider::WorkBudget;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Every test in this binary serializes: the process-wide API state and the
/// process-global logger are singletons.
static TEST_LOCK: Mutex<()> = Mutex::const_new(());

const SECRET_SENTINEL: &str = "synthetic-session-token-DO-NOT-LOG-8f3a";
const PRIVATE_KEY_SENTINEL: &str = "-----BEGIN SYNTHETIC PRIVATE KEY-----";
const CONTACT_NAME_SENTINEL: &str = "Synthetic Contact Name MVQ";
const CONTACT_EMAIL_SENTINEL: &str = "synthetic-contact-mvq@example.com";
const CORRESPONDENT_SENTINEL: &str = "synthetic-correspondent-mvq@example.com";
const QUERY_SENTINEL: &str = "synthetic-query-mvq";

// ---------------------------------------------------------------------------
// Captured logs
// ---------------------------------------------------------------------------

static LOG_BUFFER: OnceLock<StdMutex<Vec<String>>> = OnceLock::new();

struct CaptureLogger;

impl Log for CaptureLogger {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        if let Some(buffer) = LOG_BUFFER.get()
            && let Ok(mut buffer) = buffer.lock()
        {
            buffer.push(format!(
                "{} {}: {}",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

fn install_capture_logger() -> &'static StdMutex<Vec<String>> {
    let buffer = LOG_BUFFER.get_or_init(|| {
        let _ = log::set_logger(&CAPTURE_LOGGER);
        log::set_max_level(LevelFilter::Trace);
        StdMutex::new(Vec::new())
    });
    buffer.lock().expect("log buffer").clear();
    buffer
}

fn captured_logs(buffer: &StdMutex<Vec<String>>) -> String {
    buffer.lock().expect("log buffer").join("\n")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn account() -> Account {
    Account {
        id: AccountId("account-privacy".to_string()),
        label: "Privacy".to_string(),
        email: "privacy@example.com".to_string(),
        display_name: "Privacy".to_string(),
        backend: MailBackend::ImapSmtp {
            username: "privacy@example.com".to_string(),
            imap: ServerConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                security: Security::Tls,
            },
            smtp: ServerConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                security: Security::Tls,
            },
            auth: pige_core::AuthKind::Password,
        },
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store.upsert_account(&account()).expect("seed account");
    store
}

fn ctx(account: &AccountId) -> OperationContext {
    OperationContext::with_timeout(account.clone(), 1, Duration::from_secs(5))
}

fn message(subject: &str) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account().id,
        to: vec![Address::new(None, "recipient@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_string(),
        body: "Synthetic privacy fixture body.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

fn history_page(recipient: &str, checkpoint: &str) -> HistoryPage {
    HistoryPage {
        messages: vec![MessageRecord {
            remote_id: RemoteMessageId::new("INBOX:1:1"),
            remote_thread: None,
            header: MessageHeader {
                message_id: Some("<privacy-1@example.com>".to_string()),
                in_reply_to: None,
                references: Vec::new(),
                subject: "Privacy fixture".to_string(),
                from: vec![Address::new(None, "sender@example.com")],
                to: vec![Address::new(None, recipient)],
                cc: Vec::new(),
                reply_to: Vec::new(),
                bcc: Vec::new(),
                date: 1_700_000_000,
                size: 64,
            },
            flags: Flags::default(),
            collections: Vec::new(),
            snippet: String::new(),
            has_attachments: false,
        }],
        checkpoint: ProviderCursor::new(1, checkpoint),
        has_more: false,
        coverage: HistoryCoverage::default(),
    }
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

fn assert_tree_lacks(root: &Path, needle: &str, context: &str) {
    for path in files_under(root) {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(needle),
            "{context}: {needle:?} leaked into {}",
            path.display()
        );
    }
}

fn count_for(db_path: &Path, table: &str, account: &AccountId) -> i64 {
    let conn = Connection::open(db_path).expect("open database for projection count");
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE account_id = ?1"),
        [&account.0],
        |row| row.get(0),
    )
    .unwrap_or_else(|error| panic!("count {table}: {error}"))
}

// ---------------------------------------------------------------------------
// Process-wide API harness
// ---------------------------------------------------------------------------

struct ApiHarness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
    backups: Arc<FakeBackupStore>,
    providers: Arc<FakeProviderFactory>,
    transport: Arc<FakeTransportFactory>,
}

impl ApiHarness {
    fn new(providers: FakeProviderFactory) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        let backups = Arc::new(FakeBackupStore::new());
        let providers = Arc::new(providers);
        let transport = Arc::new(FakeTransportFactory::default());
        let harness = Self {
            dir,
            credentials,
            backups,
            providers,
            transport,
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
            auth: None,
        }
    }

    fn init(&self) {
        api::reset_for_tests();
        api::init(self.config()).expect("init api");
    }

    /// Drop the process-wide API state and install it again over the same
    /// directories, as a fresh process would.
    fn restart(&self) {
        api::reset_for_tests();
        api::init(self.config()).expect("re-init api");
    }

    fn db_path(&self) -> PathBuf {
        self.dir
            .path()
            .join("data")
            .join(pige_core::api::DB_FILE_NAME)
    }

    fn open_store(&self) -> Store {
        let store = Store::open(self.db_path()).expect("open harness store");
        store.migrate().expect("migrate harness store");
        store
    }

    async fn add_account(&self) -> AccountId {
        api::add_account_and_id(AccountInput {
            label: "Privacy".to_string(),
            email: "privacy@example.com".to_string(),
            display_name: "Privacy".to_string(),
            username: "privacy@example.com".to_string(),
            password: SECRET_SENTINEL.to_string(),
            imap: ServerConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                security: Security::Tls,
            },
            smtp: ServerConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                security: Security::Tls,
            },
        })
        .await
        .expect("add account")
        .0
    }
}

impl Drop for ApiHarness {
    fn drop(&mut self) {
        api::reset_for_tests();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logs_and_debug_omit_secret_sentinels() {
    let _guard = TEST_LOCK.lock().await;
    let logs = install_capture_logger();
    log::info!("privacy log capture probe");

    let error = ProviderError::new(ErrorKind::Transient, "provider unavailable")
        .with_source_detail(format!("Authorization: Bearer {SECRET_SENTINEL}"));
    assert!(
        !format!("{error}").contains(SECRET_SENTINEL),
        "Display must exclude adapter source detail"
    );
    assert!(
        !format!("{error:?}").contains(SECRET_SENTINEL),
        "Debug must exclude adapter source detail"
    );
    assert!(
        format!("{error:?}").contains("<redacted>"),
        "Debug must mark the source detail as redacted"
    );

    let script = FakeProviderSession::new()
        .fail(FakeCall::History, error)
        .fail(
            FakeCall::Contacts,
            ProviderError::forbidden("contacts permission denied"),
        );
    let harness = ApiHarness::new(FakeProviderFactory::new().session(script));
    let id = harness.add_account().await;

    // Persist a synthetic contact name and a learned correspondent so an
    // accidental debug dump of contact or suggestion rows would be visible.
    {
        let store = harness.open_store();
        let source = ContactSourceId::new("privacy.source");
        store
            .upsert_contact_source(&id, &source, Some("Privacy book"))
            .expect("contact source");
        store
            .upsert_contact_email(
                &id,
                &source,
                &RemoteContactId::new("contact-privacy"),
                &Address::new(
                    Some(CONTACT_NAME_SENTINEL.to_string()),
                    CONTACT_EMAIL_SENTINEL,
                ),
            )
            .expect("contact email");
        store
            .apply_history_page(
                &ctx(&id),
                None,
                &history_page(CORRESPONDENT_SENTINEL, "cp-1"),
            )
            .expect("history page");
        api::set_contact_sync(&id, &source, true).expect("enable contact source");
    }

    // Lookup is local-only; its query text must never be logged.
    let suggestions = api::suggest_recipients(api::RecipientQuery {
        sending_account: id.clone(),
        text: QUERY_SENTINEL.to_string(),
        limit: 10,
    })
    .expect("suggest recipients");
    assert!(suggestions.is_empty(), "the query has no matching fixture");

    api::run_maintenance(Duration::from_secs(5))
        .await
        .expect("maintenance pass");

    let captured = captured_logs(logs);
    assert!(
        captured.contains("privacy log capture probe"),
        "the capture logger must record entries"
    );
    assert!(
        captured.contains("maintenance history"),
        "the failing provider path must actually be logged:\n{captured}"
    );
    for marker in [
        SECRET_SENTINEL,
        PRIVATE_KEY_SENTINEL,
        CONTACT_NAME_SENTINEL,
        CONTACT_EMAIL_SENTINEL,
        CORRESPONDENT_SENTINEL,
        QUERY_SENTINEL,
    ] {
        assert!(
            !captured.contains(marker),
            "logs leaked {marker:?}:\n{captured}"
        );
    }

    // No configuration file, backup, or SQLite file may contain the secret.
    assert_tree_lacks(
        harness.dir.path(),
        SECRET_SENTINEL,
        "logs/config/cache tree",
    );
    assert_tree_lacks(
        harness.dir.path(),
        PRIVATE_KEY_SENTINEL,
        "logs/config/cache tree",
    );
}

#[tokio::test]
async fn backup_has_no_contact_or_correspondent_data() {
    let _guard = TEST_LOCK.lock().await;
    let harness = ApiHarness::new(FakeProviderFactory::new());
    let id = harness.add_account().await;

    {
        let store = harness.open_store();
        let source = ContactSourceId::new("privacy.backup");
        store
            .upsert_contact_source(&id, &source, Some("Backup book"))
            .expect("contact source");
        store
            .upsert_contact_email(
                &id,
                &source,
                &RemoteContactId::new("contact-backup"),
                &Address::new(
                    Some(CONTACT_NAME_SENTINEL.to_string()),
                    CONTACT_EMAIL_SENTINEL,
                ),
            )
            .expect("contact email");
        store
            .apply_history_page(
                &ctx(&id),
                None,
                &history_page(CORRESPONDENT_SENTINEL, "backup-cp"),
            )
            .expect("history page");
    }

    // The fixtures really are persisted; absence from the backup is meaningful.
    assert_eq!(count_for(&harness.db_path(), "contact_emails", &id), 1);
    assert_eq!(
        count_for(&harness.db_path(), "correspondence_evidence", &id),
        1
    );

    let name = api::backup_config().await.expect("backup configuration");
    let bytes = harness.backups.read(&name).expect("read backup");
    let backup = String::from_utf8(bytes).expect("utf-8 backup");
    assert!(
        backup.contains("privacy@example.com"),
        "the account configuration must still be backed up"
    );
    for marker in [
        CONTACT_NAME_SENTINEL,
        CONTACT_EMAIL_SENTINEL,
        CORRESPONDENT_SENTINEL,
    ] {
        assert!(
            !backup.contains(marker),
            "a configuration backup leaked {marker:?}"
        );
    }

    assert_tree_lacks(
        &harness.dir.path().join("config"),
        CONTACT_EMAIL_SENTINEL,
        "configuration directory",
    );
    assert_tree_lacks(
        &harness.dir.path().join("config"),
        CORRESPONDENT_SENTINEL,
        "configuration directory",
    );
}

#[tokio::test]
async fn clear_then_process_restart_keeps_history_disabled() {
    let _guard = TEST_LOCK.lock().await;
    // The harness owns the recorder-backed factory, so a later request is
    // observable.
    let harness = ApiHarness::new(FakeProviderFactory::new());
    let id = harness.add_account().await;

    {
        let store = harness.open_store();
        store
            .apply_history_page(
                &ctx(&id),
                None,
                &history_page(CORRESPONDENT_SENTINEL, "clear-cp"),
            )
            .expect("history page");
        assert!(
            !store
                .learning_state(&id)
                .expect("learning state")
                .history_disabled,
            "history starts enabled"
        );
    }

    api::clear_learned_recipients(&id).expect("clear learned recipients");
    {
        let store = harness.open_store();
        let state = store.learning_state(&id).expect("learning state");
        assert!(state.history_disabled, "clear must disable history");
        assert!(
            state.cache_backfill_disabled,
            "clear must disable cache backfill"
        );
    }

    harness.restart();

    {
        let store = harness.open_store();
        let state = store
            .learning_state(&id)
            .expect("learning state after restart");
        assert!(
            state.history_disabled,
            "a restart must not silently re-enable history"
        );
        assert!(
            state.cache_backfill_disabled,
            "a restart must not silently re-enable cache backfill"
        );
    }
    let progress = api::learning_progress(&id).expect("learning progress after restart");
    assert!(
        progress.paused,
        "cleared/restarting history must report paused, not running"
    );

    // A foreground learning request must not open the provider or write a new
    // history cursor for a cleared account.
    let _ = api::learn_history_now(&id).await.expect("learning request");
    assert!(
        harness.providers.recorder().history_requests().is_empty(),
        "cleared history must not make a provider request after restart"
    );
}

#[tokio::test]
async fn remove_account_deletes_all_private_projection_rows() {
    let _guard = TEST_LOCK.lock().await;
    let harness = ApiHarness::new(FakeProviderFactory::new());
    let id = harness.add_account().await;

    {
        let store = harness.open_store();
        let source = ContactSourceId::new("privacy.remove");
        store
            .upsert_contact_source(&id, &source, Some(CONTACT_NAME_SENTINEL))
            .expect("contact source");
        store
            .upsert_contact_email(
                &id,
                &source,
                &RemoteContactId::new("contact-remove"),
                &Address::new(
                    Some(CONTACT_NAME_SENTINEL.to_string()),
                    CONTACT_EMAIL_SENTINEL,
                ),
            )
            .expect("contact email");
        store
            .set_contact_source_enabled(&id, &source, true)
            .expect("contact source state");
        store
            .apply_history_page(
                &ctx(&id),
                None,
                &history_page(CORRESPONDENT_SENTINEL, "remove-cp"),
            )
            .expect("history page");
        // Ensure the durable learning-control row exists even on a fresh
        // account; it is one of the private projections removal must cascade.
        store
            .pause_learning(&id, false)
            .expect("learning control row");
        store
            .record_selection(
                &ctx(&id),
                &Address::new(
                    Some(CONTACT_NAME_SENTINEL.to_string()),
                    CONTACT_EMAIL_SENTINEL,
                ),
            )
            .expect("recipient selection");
        store
            .set_checkpoint(
                &history_job(&id),
                1,
                &ProviderCursor::new(1, "remove-history"),
            )
            .expect("history checkpoint");
    }

    for table in [
        "contact_sources",
        "contact_emails",
        "correspondence_evidence",
        "evidence_addresses",
        "contact_addresses",
        "contact_learning_state",
        "recipient_index",
        "recipient_tokens",
        "contact_source_state",
    ] {
        assert!(
            count_for(&harness.db_path(), table, &id) > 0,
            "{table} must have fixture rows before removal"
        );
    }

    api::remove_account(&id).await.expect("remove account");

    for table in [
        "contact_sources",
        "contact_emails",
        "correspondence_evidence",
        "evidence_addresses",
        "contact_addresses",
        "contact_learning_state",
        "recipient_index",
        "recipient_tokens",
        "contact_source_state",
        "pending_auth",
        "switch_intents",
    ] {
        assert_eq!(
            count_for(&harness.db_path(), table, &id),
            0,
            "{table} rows must not survive account removal"
        );
    }

    let conn = Connection::open(harness.db_path()).expect("open database");
    let jobs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_checkpoints WHERE job_key LIKE ?1",
            [format!("%{}%", id.0)],
            |row| row.get(0),
        )
        .expect("count job checkpoints");
    assert_eq!(jobs, 0, "account job checkpoints must not survive removal");
    assert_tree_lacks(
        harness.dir.path().join("config").as_path(),
        CONTACT_EMAIL_SENTINEL,
        "configuration after removal",
    );
}

#[tokio::test]
async fn outbox_artifacts_have_no_session_or_private_key() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let store = open_store(&dir);

    // The platform credential store owns the session/private material. It must
    // never be copied into an outbox row or prepared artifact.
    let credentials = FakeCredentials::default();
    credentials
        .set(&account().id, SECRET_SENTINEL)
        .expect("store synthetic secret");
    assert_eq!(
        credentials.secret(&account().id).as_deref(),
        Some(SECRET_SENTINEL)
    );

    let outbox_id =
        queue_submission(&store, message("Outbox privacy"), Vec::new()).expect("queue submission");

    let prepared = PreparedSubmission {
        version: 1,
        backend: ProviderId::from(IMAP_SMTP_PROVIDER),
        raw: Some(
            b"From: privacy@example.com\r\nTo: recipient@example.com\r\n\r\nhello\r\n".to_vec(),
        ),
        reference: Some("prepared/privacy".to_string()),
    };
    let script = FakeProviderSession::new()
        .prepared(prepared)
        .sent_copy_policy(SentCopyPolicy::ServerManaged)
        .submission_outcome(SubmissionOutcome::Accepted(SubmissionReceipt {
            operation: pige_core::providers::OperationId::new("outbox-privacy"),
            remote_message: None,
            remote_thread: None,
        }));
    let factory = FakeProviderFactory::new()
        .services(FakeServices::submission_only())
        .session(script);
    let context = ctx(&account().id);
    let mut session = factory
        .open(&account(), &context)
        .await
        .expect("open submission provider");
    let service = session.submission_service().expect("submission capability");

    pige_core::send::flush_provider_outbox(&store, &account(), service.as_mut(), &context)
        .await
        .expect("flush provider outbox");

    let entry = outbox_entry(&store, &outbox_id)
        .expect("read outbox entry")
        .expect("outbox entry");
    assert!(
        matches!(
            entry.state,
            SubmissionState::Complete | SubmissionState::Accepted
        ),
        "the fixture must persist an accepted artifact, got {:?}",
        entry.state
    );
    assert!(
        entry.prepared.is_some(),
        "the accepted artifact must be durable for reconciliation"
    );
    let debug = format!("{entry:?}");
    assert!(!debug.contains(SECRET_SENTINEL));
    assert!(!debug.contains(PRIVATE_KEY_SENTINEL));

    // Scan the database file itself, not just the typed struct, so a stray
    // diagnostic column would fail this test.
    assert_tree_lacks(dir.path(), SECRET_SENTINEL, "outbox database");
    assert_tree_lacks(dir.path(), PRIVATE_KEY_SENTINEL, "outbox database");
}

// ---------------------------------------------------------------------------
// Denied contact scope
// ---------------------------------------------------------------------------

struct DeniedAddressBook {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl AddressBookService for DeniedAddressBook {
    async fn next_page(
        &mut self,
        _source: &ContactSourceId,
        _cursor: Option<ProviderCursor>,
        _ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::forbidden("contacts permission denied"))
    }
}

#[tokio::test]
async fn denied_scope_is_not_infinite_background_retry() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let store = open_store(&dir);
    let account = account();
    let source = ContactSourceId::new("privacy.denied");
    store
        .upsert_contact_source(&account.id, &source, Some("Denied book"))
        .expect("contact source");
    let context = ctx(&account.id);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut service = DeniedAddressBook {
        calls: calls.clone(),
    };

    let error = pige_core::contacts::sync::sync_slice(
        &store,
        &mut service,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .expect_err("a denied scope must be reported");
    assert_eq!(error.kind, ErrorKind::Forbidden);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let state = store
        .contact_source_state(&account.id, &source)
        .expect("contact source state");
    assert!(
        !state.enabled,
        "a permission denial must disable the source"
    );
    assert_eq!(
        store
            .job_checkpoint(&pige_core::store::contacts_job(&account.id, &source))
            .expect("checkpoint"),
        None,
        "a denied source must not keep a resumable job"
    );

    // A subsequent background slice must return immediately without another
    // provider call. This is the difference between a disabled source and an
    // infinite retry loop.
    let progress = pige_core::contacts::sync::sync_slice(
        &store,
        &mut service,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .expect("a disabled source is skipped");
    assert!(progress.paused);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a denied source must not be retried by the background scheduler"
    );
}
