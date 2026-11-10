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
use pige_core::mail::{
    Address, AttachmentDisposition, AttachmentPart, Flags, MessageHeader, MessageId, StoredMessage,
    ThreadId,
};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::platform::{BackupStore, CredentialStore};
use pige_core::providers::fake::{FakeCall, FakeProviderFactory, FakeProviderSession};
use pige_core::providers::{
    AddressBookService, ContactPage, ContactSourceId, ErrorKind, HistoryCoverage, HistoryPage,
    MessageRecord, OperationContext, PreparedSubmission, ProviderCursor, ProviderError,
    ReconcileOutcome, RemoteContactId, RemoteMessageId, SentCopyPolicy, SubmissionAttachment,
    SubmissionOutcome, SubmissionReceipt, SubmissionRequest, SubmissionService,
};
use pige_core::send::{
    OutboxEntry, OutgoingMessage, SubmissionState, outbox_entry, queue_submission,
};
use pige_core::store::{Store, history_job};

#[cfg(feature = "provider-proton")]
use pige_core::providers::proton::ProtonKeyStores;
use pige_core::sync::provider::WorkBudget;
#[cfg(feature = "provider-proton")]
use proton_core::crypto::keys::{AddressKeys, KeyStore as ProtonKeyStore, StoredKey};
use rusqlite::Connection;
#[cfg(feature = "provider-proton")]
use secrecy::SecretString;
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

fn message_for(account_id: &AccountId, subject: &str) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account_id.clone(),
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

/// Marker scan shared by the positive assertion and its negative control. The
/// control asserts that deliberately injected markers *are* found, so a helper
/// that silently stopped looking cannot make the clean run pass.
fn scan_text(label: &str, text: &str, markers: &[&str]) -> Vec<String> {
    let mut findings = Vec::new();
    for marker in markers {
        if text.contains(marker) {
            findings.push(format!("{label}: {marker}"));
        }
    }
    findings
}

fn scan_bytes(label: &str, bytes: &[u8], markers: &[&str]) -> Vec<String> {
    scan_text(label, &String::from_utf8_lossy(bytes), markers)
}

fn scan_tree(root: &Path, markers: &[&str]) -> Vec<String> {
    let mut findings = Vec::new();
    for path in files_under(root) {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        findings.extend(scan_bytes(&path.display().to_string(), &bytes, markers));
    }
    findings
}

fn submission_request_fragments(request: &SubmissionRequest) -> Vec<(String, String)> {
    let mut fragments = vec![
        ("request debug".to_string(), format!("{request:?}")),
        (
            "request message".to_string(),
            format!("{:?}", request.message),
        ),
        ("request operation".to_string(), request.operation.0.clone()),
    ];
    if let Some(prepared) = &request.prepared {
        fragments.push((
            "request prepared backend".to_string(),
            prepared.backend.0.clone(),
        ));
        fragments.push((
            "request prepared reference".to_string(),
            prepared.reference.clone().unwrap_or_default(),
        ));
        if let Some(raw) = &prepared.raw {
            fragments.push((
                "request prepared raw".to_string(),
                String::from_utf8_lossy(raw).into_owned(),
            ));
        }
    }
    for (index, attachment) in request.attachments.iter().enumerate() {
        fragments.push((
            format!("request attachment {index}"),
            format!("{attachment:?}"),
        ));
        fragments.push((
            format!("request attachment {index} bytes"),
            String::from_utf8_lossy(&attachment.bytes).into_owned(),
        ));
    }
    fragments
}

fn scan_submission_requests(requests: &[SubmissionRequest], markers: &[&str]) -> Vec<String> {
    let mut findings = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        for (label, text) in submission_request_fragments(request) {
            findings.extend(scan_text(
                &format!("request {index} {label}"),
                &text,
                markers,
            ));
        }
    }
    findings
}

fn scan_outbox_entry(entry: &OutboxEntry, markers: &[&str]) -> Vec<String> {
    let mut fragments = vec![
        ("outbox debug".to_string(), format!("{entry:?}")),
        ("outbox message".to_string(), format!("{:?}", entry.message)),
        ("outbox operation".to_string(), entry.operation_id.0.clone()),
        (
            "outbox backend".to_string(),
            entry
                .backend
                .as_ref()
                .map(|backend| backend.0.clone())
                .unwrap_or_default(),
        ),
        (
            "outbox error".to_string(),
            entry.error.clone().unwrap_or_default(),
        ),
    ];
    if let Some(raw) = &entry.raw {
        fragments.push((
            "outbox raw".to_string(),
            String::from_utf8_lossy(raw).into_owned(),
        ));
    }
    if let Some(prepared) = &entry.prepared {
        fragments.push((
            "outbox prepared backend".to_string(),
            prepared.backend.0.clone(),
        ));
        fragments.push((
            "outbox prepared reference".to_string(),
            prepared.reference.clone().unwrap_or_default(),
        ));
        if let Some(raw) = &prepared.raw {
            fragments.push((
                "outbox prepared raw".to_string(),
                String::from_utf8_lossy(raw).into_owned(),
            ));
        }
    }
    let mut findings = Vec::new();
    for (label, text) in fragments {
        findings.extend(scan_text(&label, &text, markers));
    }
    findings
}

fn scan_outbox_parts(parts: &[SubmissionAttachment], markers: &[&str]) -> Vec<String> {
    let mut findings = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        findings.extend(scan_text(
            &format!("outbox part {index}"),
            &format!("{part:?}"),
            markers,
        ));
        findings.extend(scan_bytes(
            &format!("outbox part {index} bytes"),
            &part.bytes,
            markers,
        ));
    }
    findings
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

fn count_query(db_path: &Path, sql: &str, account: &AccountId) -> i64 {
    let conn = Connection::open(db_path).expect("open database for projection count");
    conn.query_row(sql, [&account.0], |row| row.get(0))
        .unwrap_or_else(|error| panic!("count query {sql}: {error}"))
}

#[cfg(feature = "provider-proton")]
fn synthetic_private_key_store() -> ProtonKeyStore {
    ProtonKeyStore {
        user_keys: Vec::new(),
        addresses: vec![AddressKeys {
            address_id: "synthetic-address".to_string(),
            email: "privacy@example.com".to_string(),
            keys: vec![StoredKey {
                armored: PRIVATE_KEY_SENTINEL.to_string(),
                passphrase: SecretString::from(String::new()),
            }],
        }],
    }
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
        .with_source_detail(format!(
            "Authorization: Bearer {SECRET_SENTINEL}\n{PRIVATE_KEY_SENTINEL}"
        ));
    for marker in [SECRET_SENTINEL, PRIVATE_KEY_SENTINEL] {
        assert!(
            !format!("{error}").contains(marker),
            "Display must exclude adapter source detail ({marker})"
        );
        assert!(
            !format!("{error:?}").contains(marker),
            "Debug must exclude adapter source detail ({marker})"
        );
    }
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
        // Cache content and a queued submission are private projections too:
        // removal must cascade the message, its attachment, the outbox row, and
        // the structured parts instead of only the contact/history tables.
        let cached = StoredMessage {
            id: MessageId("message-remove".to_string()),
            account_id: id.clone(),
            folder_id: None,
            uid: None,
            uidvalidity: None,
            header: MessageHeader {
                message_id: Some("<remove@example.com>".to_string()),
                in_reply_to: None,
                references: Vec::new(),
                subject: "Private cached message".to_string(),
                from: vec![Address::new(None, "sender@example.com")],
                to: vec![Address::new(None, "privacy@example.com")],
                cc: Vec::new(),
                reply_to: Vec::new(),
                bcc: Vec::new(),
                date: 1_700_000_000,
                size: 32,
            },
            flags: Flags::default(),
            thread_id: ThreadId("thread-remove".to_string()),
            snippet: "private snippet".to_string(),
            has_attachments: true,
        };
        store.upsert_message(&cached).expect("cached message");
        store
            .store_attachments(
                &cached.id,
                &[AttachmentPart {
                    part_id: "part-1".to_string(),
                    filename: "private.txt".to_string(),
                    content_type: "text/plain".to_string(),
                    size: 8,
                    content_id: None,
                    disposition: AttachmentDisposition::Attachment,
                }],
            )
            .expect("cached attachment");
        queue_submission(
            &store,
            message_for(&id, "Outbox removal fixture"),
            vec![SubmissionAttachment {
                id: "part-1".to_string(),
                filename: "private.txt".to_string(),
                content_type: "text/plain".to_string(),
                bytes: b"private!".to_vec(),
                content_id: None,
                disposition: AttachmentDisposition::Attachment,
            }],
        )
        .expect("queued outbox fixture");
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
        "messages",
        "threads",
        "outbox",
    ] {
        assert!(
            count_for(&harness.db_path(), table, &id) > 0,
            "{table} must have fixture rows before removal"
        );
    }
    for (label, sql) in [
        (
            "attachments",
            "SELECT COUNT(*) FROM attachments a
             JOIN messages m ON m.id = a.message_id WHERE m.account_id = ?1",
        ),
        (
            "outbox_parts",
            "SELECT COUNT(*) FROM outbox_parts p
             JOIN outbox o ON o.id = p.outbox_id WHERE o.account_id = ?1",
        ),
    ] {
        assert!(
            count_query(&harness.db_path(), sql, &id) > 0,
            "{label} must have fixture rows before removal"
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
        "messages",
        "threads",
        "outbox",
    ] {
        assert_eq!(
            count_for(&harness.db_path(), table, &id),
            0,
            "{table} rows must not survive account removal"
        );
    }
    for (label, sql) in [
        (
            "attachments",
            "SELECT COUNT(*) FROM attachments a
             JOIN messages m ON m.id = a.message_id WHERE m.account_id = ?1",
        ),
        (
            "outbox_parts",
            "SELECT COUNT(*) FROM outbox_parts p
             JOIN outbox o ON o.id = p.outbox_id WHERE o.account_id = ?1",
        ),
    ] {
        assert_eq!(
            count_query(&harness.db_path(), sql, &id),
            0,
            "{label} rows must not survive account removal"
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

/// A recording submission adapter that resolves the same session secret and
/// (under the Proton feature) private-key store a real adapter would. `clean`
/// only reads them; `inject_leak` copies them into the prepared artifact, which
/// is the exact bug the privacy scan must catch.
struct RecordingSubmission {
    credentials: Arc<FakeCredentials>,
    key_material: Option<String>,
    prepared: PreparedSubmission,
    inject_leak: bool,
    requests: Vec<SubmissionRequest>,
    prepare_calls: usize,
    submit_calls: usize,
}

impl RecordingSubmission {
    fn new(
        credentials: Arc<FakeCredentials>,
        key_material: Option<String>,
        prepared: PreparedSubmission,
        inject_leak: bool,
    ) -> Self {
        Self {
            credentials,
            key_material,
            prepared,
            inject_leak,
            requests: Vec::new(),
            prepare_calls: 0,
            submit_calls: 0,
        }
    }
}

#[async_trait]
impl SubmissionService for RecordingSubmission {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        SentCopyPolicy::ServerManaged
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        ctx.ensure_active()?;
        // Prove the adapter really resolves the seeded secret and key material
        // instead of the assertion passing because nothing was read.
        let session = self
            .credentials
            .get(&request.message.account_id)
            .map_err(ProviderError::transient)?
            .ok_or_else(|| ProviderError::reauthenticate("synthetic session secret is missing"))?;
        assert_eq!(session, SECRET_SENTINEL);
        if let Some(key) = &self.key_material {
            assert_eq!(key, PRIVATE_KEY_SENTINEL);
        }

        self.requests.push(request.clone());
        self.prepare_calls += 1;
        let mut prepared = self.prepared.clone();
        if self.inject_leak {
            let key = self
                .key_material
                .clone()
                .unwrap_or_else(|| PRIVATE_KEY_SENTINEL.to_string());
            prepared.reference = Some(format!("{SECRET_SENTINEL} {key}"));
            prepared.raw = Some(
                format!(
                    "From: privacy@example.com\r\nTo: recipient@example.com\r\n\r\n\
                     {SECRET_SENTINEL}\n{key}\n"
                )
                .into_bytes(),
            );
        }
        Ok(prepared)
    }

    async fn submit(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        ctx.ensure_active()?;
        self.requests.push(request.clone());
        self.submit_calls += 1;
        Ok(SubmissionOutcome::Accepted(SubmissionReceipt {
            operation: request.operation.clone(),
            remote_message: None,
            remote_thread: None,
        }))
    }

    async fn reconcile(
        &mut self,
        _request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        Ok(ReconcileOutcome::NotAccepted)
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

fn outbox_privacy_fixture() -> (Store, TempDir, Account, Arc<FakeCredentials>, KeyFixture) {
    let dir = TempDir::new().expect("temp dir");
    let store = open_store(&dir);
    let account = account();
    let credentials = Arc::new(FakeCredentials::default());
    credentials
        .set_session(&account.id, SECRET_SENTINEL)
        .expect("seed synthetic session secret");
    assert_eq!(
        credentials.secret(&account.id).as_deref(),
        Some(SECRET_SENTINEL),
        "the session secret must be readable from the credential store"
    );
    let key_fixture = seed_private_key_fixture(&account.id);
    (store, dir, account, credentials, key_fixture)
}

fn outbox_prepared(account: &Account) -> PreparedSubmission {
    PreparedSubmission {
        version: 1,
        backend: account.provider_id(),
        raw: Some(
            b"From: privacy@example.com\r\nTo: recipient@example.com\r\n\r\nhello\r\n".to_vec(),
        ),
        reference: Some("prepared/privacy".to_string()),
    }
}

fn queue_privacy_fixture(store: &Store, account: &Account) -> pige_core::send::OutboxId {
    queue_submission(
        store,
        message_for(&account.id, "Outbox privacy"),
        vec![SubmissionAttachment {
            id: "part-1".to_string(),
            filename: "note.txt".to_string(),
            content_type: "text/plain".to_string(),
            bytes: b"benign synthetic attachment".to_vec(),
            content_id: None,
            disposition: AttachmentDisposition::Attachment,
        }],
    )
    .expect("queue submission")
}

#[cfg(feature = "provider-proton")]
struct KeyFixture {
    armored: String,
    _stores: ProtonKeyStores,
}

#[cfg(not(feature = "provider-proton"))]
struct KeyFixture;

#[cfg(feature = "provider-proton")]
fn seed_private_key_fixture(account: &AccountId) -> KeyFixture {
    let keys = synthetic_private_key_store();
    let armored = keys.addresses[0].keys[0].armored.clone();
    let stores = ProtonKeyStores::new();
    let unlocked = stores.get_or_create(account);
    unlocked.install_test_keys("synthetic-uid", keys);
    assert!(
        unlocked.is_unlocked(),
        "the synthetic private key store must be seeded"
    );
    KeyFixture {
        armored,
        _stores: stores,
    }
}

#[cfg(not(feature = "provider-proton"))]
fn seed_private_key_fixture(_account: &AccountId) -> KeyFixture {
    KeyFixture
}

#[cfg(feature = "provider-proton")]
fn key_material(fixture: &KeyFixture) -> Option<String> {
    Some(fixture.armored.clone())
}

#[cfg(not(feature = "provider-proton"))]
fn key_material(_fixture: &KeyFixture) -> Option<String> {
    None
}

fn scan_outbox_artifacts(
    requests: &[SubmissionRequest],
    entry: &OutboxEntry,
    parts: &[SubmissionAttachment],
    root: &Path,
    markers: &[&str],
) -> Vec<String> {
    let mut findings = scan_submission_requests(requests, markers);
    findings.extend(scan_outbox_entry(entry, markers));
    findings.extend(scan_outbox_parts(parts, markers));
    // Scan the database file itself, not just the typed struct, so a stray
    // diagnostic column would fail this test.
    findings.extend(scan_tree(root, markers));
    findings
}

#[tokio::test]
async fn outbox_artifacts_have_no_session_or_private_key() {
    let _guard = TEST_LOCK.lock().await;
    let (store, dir, account, credentials, key_fixture) = outbox_privacy_fixture();
    let outbox_id = queue_privacy_fixture(&store, &account);

    let mut service = RecordingSubmission::new(
        credentials,
        key_material(&key_fixture),
        outbox_prepared(&account),
        false,
    );
    let context = ctx(&account.id);
    pige_core::send::flush_provider_outbox(&store, &account, &mut service, &context)
        .await
        .expect("flush provider outbox");

    assert_eq!(
        service.prepare_calls, 1,
        "prepare must run for the queued row"
    );
    assert_eq!(service.submit_calls, 1, "submit must run once");
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
    let parts = store.outbox_parts(&outbox_id).expect("outbox parts");
    assert_eq!(parts.len(), 1, "the structured part must be durable");

    let markers = [SECRET_SENTINEL, PRIVATE_KEY_SENTINEL];
    let findings = scan_outbox_artifacts(&service.requests, &entry, &parts, dir.path(), &markers);
    assert!(
        findings.is_empty(),
        "outbox request, prepared artifact, row, parts, or database leaked synthetic material:\n{findings:#?}"
    );
}

/// Negative control: if an adapter copies the same seeded session/key material
/// into the prepared artifact, every scan must report it. Without this, a
/// broken or empty scanner could make the positive test above pass.
#[tokio::test]
async fn outbox_privacy_scan_detects_injected_session_or_private_key() {
    let _guard = TEST_LOCK.lock().await;
    let (store, dir, account, credentials, key_fixture) = outbox_privacy_fixture();
    let outbox_id = queue_privacy_fixture(&store, &account);

    let mut service = RecordingSubmission::new(
        credentials,
        key_material(&key_fixture),
        outbox_prepared(&account),
        true,
    );
    let context = ctx(&account.id);
    pige_core::send::flush_provider_outbox(&store, &account, &mut service, &context)
        .await
        .expect("flush leaky provider outbox");

    let entry = outbox_entry(&store, &outbox_id)
        .expect("read outbox entry")
        .expect("outbox entry");
    let parts = store.outbox_parts(&outbox_id).expect("outbox parts");
    let markers = [SECRET_SENTINEL, PRIVATE_KEY_SENTINEL];

    let request_findings = scan_submission_requests(&service.requests, &markers);
    let entry_findings = scan_outbox_entry(&entry, &markers);
    let database_findings = scan_tree(dir.path(), &markers);
    assert!(
        !request_findings.is_empty(),
        "the scan must detect a leak in the SubmissionRequest handed to the adapter"
    );
    assert!(
        !entry_findings.is_empty(),
        "the scan must detect a leak in the persisted outbox/prepared artifact"
    );
    assert!(
        !database_findings.is_empty(),
        "the scan must detect a leak in the database file"
    );

    let findings = scan_outbox_artifacts(&service.requests, &entry, &parts, dir.path(), &markers);
    for marker in markers {
        assert!(
            findings.iter().any(|finding| finding.contains(marker)),
            "the leak scan missed {marker:?}:\n{findings:#?}"
        );
    }
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
