//! Task R4: clear, pause, rebuild, and lifecycle privacy.
//!
//! Clearing learned recipients is account-scoped and logical: it removes the
//! evidence ledger and the derived suggestion projection (including explicit
//! selections) but preserves the account's synced contacts. It also disables
//! automatic `ExistingCache`/`History` ingestion and fences any crawl already
//! in flight, while a new accepted send, compose selection, or adapter-confirmed
//! live arrival may still train. Rebuilding starts a new learning epoch and
//! re-enables history. Pausing stops learning without deleting suggestions.
//!
//! Every fixture address is `example.com`.

use std::sync::Arc;
use std::time::Duration;

use pige_core::account::{Account, AccountId, Security, ServerConfig};
use pige_core::api::{self, Config, DB_FILE_NAME};
use pige_core::contacts::{
    normalize_address, AddressKey, EvidenceId, HeaderRole, LearningOrigin, Observation,
    ObservationTrust,
};
use pige_core::mail::{Address, Flags, MessageHeader};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::platform::BackupStore;
use pige_core::providers::fake::FakeProviderFactory;
use pige_core::providers::{
    ChangePage, HistoryCoverage, HistoryPage, JobKey, JobKind, MailChange, MessageRecord,
    OperationContext, OperationId, ProviderCursor, ProviderFactory, RemoteMessageId,
};
use pige_core::store::{HistoryProgress, history_job};
use pige_core::sync::history::{background_budget, learn_account_slice};
use pige_core::sync::scheduler::AccountScheduler;
use pige_core::Store;
use pige_core::StoreError;
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Serializes the tests that install the process-wide API state.
static API_LOCK: Mutex<()> = Mutex::const_new(());

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn account_named(id: &str, email: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: email.to_string(),
        display_name: id.to_string(),
        backend: pige_core::account::MailBackend::ImapSmtp {
            username: email.to_string(),
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

fn account() -> Account {
    account_named("account-r4", "r4@example.com")
}

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    store.upsert_account(&account()).unwrap();
    (dir, store)
}

fn ctx(account: &AccountId) -> OperationContext {
    OperationContext::with_timeout(account.clone(), 1, Duration::from_secs(5))
}

fn address(email: &str) -> Address {
    Address::new(None, email)
}

fn key(email: &str) -> AddressKey {
    normalize_address(&address(email)).expect("fixture address must normalize")
}

fn header(index: usize, recipient: &str) -> MessageHeader {
    MessageHeader {
        message_id: Some(format!("<lifecycle-{index}@example.com>")),
        in_reply_to: None,
        references: Vec::new(),
        subject: format!("Lifecycle {index}"),
        from: vec![address("sender@example.com")],
        to: vec![address(recipient)],
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: 1_700_000_000 + index as i64,
        size: 64,
    }
}

fn record(index: usize, recipient: &str) -> MessageRecord {
    MessageRecord {
        remote_id: RemoteMessageId::new(format!("INBOX:1:{}", index + 1)),
        remote_thread: None,
        header: header(index, recipient),
        flags: Flags::default(),
        collections: Vec::new(),
        snippet: String::new(),
        has_attachments: false,
    }
}

fn page(index: usize, recipient: &str, checkpoint: &str) -> HistoryPage {
    HistoryPage {
        messages: vec![record(index, recipient)],
        checkpoint: ProviderCursor::new(1, checkpoint),
        has_more: false,
        coverage: HistoryCoverage::default(),
    }
}

fn observation(email: &str) -> Observation {
    Observation {
        key: key(email),
        address: address(email),
        role: HeaderRole::To,
        last_seen: 1_700_000_100,
        outgoing: true,
        trust: ObservationTrust::Deliberate,
    }
}

fn learned(store: &Store, account: &AccountId, email: &str) -> bool {
    store
        .learned_address(account, &key(email))
        .unwrap()
        .is_some()
}

fn contact_candidate(store: &Store, account: &AccountId, email: &str) -> bool {
    store
        .recipient_candidates(account, &email.to_lowercase(), &[email.to_lowercase()], 64)
        .unwrap()
        .iter()
        .any(|candidate| candidate.address.email == email)
}

// ---------------------------------------------------------------------------
// Store-level lifecycle tests
// ---------------------------------------------------------------------------

#[test]
fn clear_then_resync_does_not_restore_history() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let old = "history@example.com";

    store
        .apply_history_page(&context, None, &page(1, old, "cp-1"))
        .unwrap();
    assert!(learned(&store, &account.id, old));

    store.clear_learned_recipients(&account.id).unwrap();
    assert!(!learned(&store, &account.id, old));
    assert_eq!(
        store.history_state(&account.id).unwrap().progress.scanned,
        0,
        "clear resets the durable crawl progress"
    );

    // The cleared history crawl cannot be replayed, even from the first page.
    let replay = store.apply_history_page(&context, None, &page(1, old, "cp-2"));
    assert!(
        matches!(
            replay,
            Err(StoreError::LearningDisabled | StoreError::LearningPaused)
        ),
        "history ingestion must stay disabled after clear, got {replay:?}"
    );
    assert!(!learned(&store, &account.id, old));

    // An ordinary metadata-only resync is not an arrival and must not learn.
    let key = JobKey {
        account: account.id.clone(),
        kind: JobKind::MailSync,
        source: "account".to_string(),
    };
    let change = ChangePage {
        changes: vec![MailChange::UpsertMessage {
            message: record(2, old),
            is_new_arrival: false,
        }],
        checkpoint: ProviderCursor::new(1, "sync-1"),
        has_more: false,
    };
    store
        .apply_change_page(&context, &key, "imap-smtp", None, &change)
        .unwrap();
    assert!(!learned(&store, &account.id, old));
}

#[test]
fn clear_then_new_send_learns() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    store
        .apply_history_page(&context, None, &page(1, "history@example.com", "cp-1"))
        .unwrap();
    store.clear_learned_recipients(&account.id).unwrap();

    let sent = "new-recipient@example.com";
    store
        .record_observations(
            &context,
            &EvidenceId::for_operation(&OperationId::new("op-r4-1")),
            LearningOrigin::AcceptedSend,
            None,
            &[observation(sent)],
        )
        .unwrap();
    assert!(
        learned(&store, &account.id, sent),
        "a new accepted send still trains after clear"
    );
    assert_eq!(
        store
            .learned_address(&account.id, &key(sent))
            .unwrap()
            .unwrap()
            .trust,
        ObservationTrust::Deliberate
    );

    // An explicit compose selection is equally allowed.
    let selected = "selected@example.com";
    store
        .record_selection(&context, &address(selected))
        .unwrap();
    assert!(learned(&store, &account.id, selected));
}

#[test]
fn stale_crawl_after_clear_cannot_commit() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let stale_epoch = store.learning_epoch(&account.id).unwrap();

    store.clear_learned_recipients(&account.id).unwrap();
    let stale = "stale-crawl@example.com";
    let result =
        store.apply_history_page_at_epoch(&context, stale_epoch, None, &page(1, stale, "cp-stale"));
    assert!(
        matches!(result, Err(StoreError::StaleLearningEpoch { .. })),
        "a page produced before clear must be fenced by the learning epoch, got {result:?}"
    );
    assert!(!learned(&store, &account.id, stale));
    assert_eq!(
        store.job_checkpoint(&history_job(&account.id)).unwrap(),
        None,
        "the stale page must not advance a checkpoint"
    );
}

#[test]
fn stale_slice_cannot_complete_progress_after_rebuild() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let old_epoch = store.learning_epoch(&account.id).unwrap();
    let old = "stale-final@example.com";

    // The slice fetched and committed its final page under `old_epoch`.
    store
        .apply_history_page_at_epoch(&context, old_epoch, None, &page(1, old, "cp-final"))
        .unwrap();

    // The user rebuilds before that slice persists its progress/complete flag.
    store.rebuild_learned_recipients(&account.id).unwrap();
    assert!(
        store.learning_epoch(&account.id).unwrap() > old_epoch,
        "rebuild starts a new learning epoch"
    );

    let stale = store.record_history_progress_at_epoch(
        &context,
        old_epoch,
        HistoryProgress {
            scanned: 1,
            total_hint: Some(1),
            complete: true,
            paused: false,
        },
    );
    assert!(
        matches!(stale, Err(StoreError::StaleLearningEpoch { .. })),
        "a stale slice must not write progress under the new epoch, got {stale:?}"
    );
    let state = store.history_state(&account.id).unwrap();
    assert!(
        !state.progress.complete,
        "a stale slice must not complete the rebuilt epoch"
    );
    assert_eq!(state.progress.scanned, 0);

    // The rebuilt epoch still crawls and completes under its own epoch.
    let fresh = "fresh-final@example.com";
    store
        .apply_history_page(&context, None, &page(2, fresh, "cp-new"))
        .unwrap();
    let new_epoch = store.learning_epoch(&account.id).unwrap();
    store
        .record_history_progress_at_epoch(
            &context,
            new_epoch,
            HistoryProgress {
                scanned: 1,
                total_hint: Some(1),
                complete: true,
                paused: false,
            },
        )
        .unwrap();
    assert!(store.history_state(&account.id).unwrap().progress.complete);
    assert!(learned(&store, &account.id, fresh));
}

#[test]
fn pause_preserves_existing_suggestions() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let kept = "kept@example.com";
    let blocked = "blocked@example.com";

    store
        .apply_history_page(&context, None, &page(1, kept, "cp-1"))
        .unwrap();
    store.pause_learning(&account.id, true).unwrap();

    assert!(
        learned(&store, &account.id, kept),
        "pausing must not delete existing suggestions"
    );

    // Every ingestion path is stopped while paused, including deliberate ones.
    let result = store.apply_history_page(&context, None, &page(2, blocked, "cp-2"));
    assert!(
        matches!(result, Err(StoreError::LearningPaused)),
        "history must not ingest while paused, got {result:?}"
    );
    store
        .record_observations(
            &context,
            &EvidenceId::for_operation(&OperationId::new("op-r4-paused")),
            LearningOrigin::AcceptedSend,
            None,
            &[observation(blocked)],
        )
        .unwrap();
    assert!(!learned(&store, &account.id, blocked));

    store.pause_learning(&account.id, false).unwrap();
    store
        .record_observations(
            &context,
            &EvidenceId::for_operation(&OperationId::new("op-r4-resumed")),
            LearningOrigin::AcceptedSend,
            None,
            &[observation(blocked)],
        )
        .unwrap();
    assert!(learned(&store, &account.id, blocked));
}

#[test]
fn explicit_rebuild_relearns_history() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let old = "old@example.com";
    let fresh = "fresh@example.com";

    store
        .apply_history_page(&context, None, &page(1, old, "cp-1"))
        .unwrap();
    store.clear_learned_recipients(&account.id).unwrap();
    let epoch_after_clear = store.learning_epoch(&account.id).unwrap();

    store.rebuild_learned_recipients(&account.id).unwrap();
    assert!(
        store.learning_epoch(&account.id).unwrap() > epoch_after_clear,
        "rebuild starts a new learning epoch"
    );

    // History is enabled again and the checkpoint restarted from scratch.
    let replay = store.apply_history_page(&context, None, &page(2, fresh, "cp-rebuild"));
    assert!(replay.is_ok(), "history must be enabled after rebuild");
    assert!(learned(&store, &account.id, fresh));
    assert!(
        !learned(&store, &account.id, old),
        "rebuild deliberately discards the previous epoch's learned data"
    );
}

#[test]
fn synced_contact_survives_clear() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let contact = "synced-contact@example.com";
    let learned_address = "learned@example.com";

    let source = pige_core::providers::ContactSourceId::new("source-r4");
    store
        .upsert_contact_source(&account.id, &source, Some("Address book"))
        .unwrap();
    store
        .upsert_contact_email(
            &account.id,
            &source,
            &pige_core::providers::RemoteContactId::new("contact-1"),
            &Address::new(Some("Synced Contact".to_string()), contact),
        )
        .unwrap();
    store
        .apply_history_page(&context, None, &page(1, learned_address, "cp-1"))
        .unwrap();

    store.clear_learned_recipients(&account.id).unwrap();

    assert_eq!(
        store.contact_emails(&account.id).unwrap().len(),
        1,
        "synced contacts are not learned evidence and must survive"
    );
    assert!(!learned(&store, &account.id, learned_address));
    assert!(!learned(&store, &account.id, contact));
    assert!(
        contact_candidate(&store, &account.id, contact),
        "the saved contact must still be suggested after the projection is rebuilt"
    );
    assert!(!contact_candidate(&store, &account.id, learned_address));
}

#[test]
fn remove_one_account_preserves_other_evidence() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    let first = account_named("account-r4-first", "first@example.com");
    let second = account_named("account-r4-second", "second@example.com");
    store.upsert_account(&first).unwrap();
    store.upsert_account(&second).unwrap();

    store
        .apply_history_page(
            &ctx(&first.id),
            None,
            &page(1, "first-learned@example.com", "cp-1"),
        )
        .unwrap();
    store
        .apply_history_page(
            &ctx(&second.id),
            None,
            &page(2, "second-learned@example.com", "cp-2"),
        )
        .unwrap();

    store.sync_accounts(std::slice::from_ref(&second)).unwrap();

    assert!(!learned(&store, &first.id, "first-learned@example.com"));
    assert!(learned(&store, &second.id, "second-learned@example.com"));
}

#[test]
fn clear_one_account_preserves_other_learned_evidence() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    let first = account_named("account-r4-clear-first", "first-clear@example.com");
    let second = account_named("account-r4-clear-second", "second-clear@example.com");
    store.upsert_account(&first).unwrap();
    store.upsert_account(&second).unwrap();

    store
        .apply_history_page(
            &ctx(&first.id),
            None,
            &page(1, "first-only@example.com", "cp-first"),
        )
        .unwrap();
    store
        .apply_history_page(
            &ctx(&second.id),
            None,
            &page(2, "second-only@example.com", "cp-second"),
        )
        .unwrap();

    store.clear_learned_recipients(&first.id).unwrap();

    assert!(!learned(&store, &first.id, "first-only@example.com"));
    assert!(
        learned(&store, &second.id, "second-only@example.com"),
        "clearing one account must not touch another account's evidence"
    );
    assert!(
        contact_candidate(&store, &second.id, "second-only@example.com"),
        "the other account's learned projection must survive"
    );
    assert!(!contact_candidate(
        &store,
        &first.id,
        "first-only@example.com"
    ));
}

#[tokio::test]
async fn paused_or_cleared_account_skips_provider_open() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let fake = Arc::new(FakeProviderFactory::new());
    let factory: Arc<dyn ProviderFactory> = fake.clone();
    let scheduler = AccountScheduler::new();

    store.pause_learning(&account.id, true).unwrap();
    let progress = learn_account_slice(
        &store,
        &factory,
        &account,
        &context,
        background_budget(),
        &scheduler,
    )
    .await
    .unwrap();
    assert!(progress.paused);
    assert_eq!(
        fake.recorder().open_attempts(),
        0,
        "a paused account must not open a provider session"
    );
    assert!(fake.recorder().history_requests().is_empty());

    store.pause_learning(&account.id, false).unwrap();
    store.clear_learned_recipients(&account.id).unwrap();
    fake.recorder().clear();
    let progress = learn_account_slice(
        &store,
        &factory,
        &account,
        &context,
        background_budget(),
        &scheduler,
    )
    .await
    .unwrap();
    assert!(progress.paused);
    assert_eq!(
        fake.recorder().open_attempts(),
        0,
        "a cleared account must not open a provider session until rebuild"
    );
    assert!(fake.recorder().history_requests().is_empty());
}

// ---------------------------------------------------------------------------
// Process-wide API lifecycle tests
// ---------------------------------------------------------------------------

struct ApiHarness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
    backups: Arc<FakeBackupStore>,
}

impl ApiHarness {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let credentials = Arc::new(FakeCredentials::default());
        let transport: Arc<dyn pige_core::platform::TransportFactory> =
            Arc::new(FakeTransportFactory::default());
        let backups = Arc::new(FakeBackupStore::new());
        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            config_dir: dir.path().join("config"),
            credentials: credentials.clone(),
            notifier: Arc::new(FakeNotifier::default()),
            opener: Arc::new(FakeOpener::default()),
            providers: Arc::new(pige_core::providers::fake::FakeProviderFactory::new()),
            transport,
            backups: backups.clone(),
            auth: None,
        })
        .expect("init api");
        Self {
            dir,
            credentials,
            backups,
        }
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.dir.path().join("data").join(DB_FILE_NAME)
    }

    fn open_store(&self) -> Store {
        let store = Store::open(self.db_path()).unwrap();
        store.migrate().unwrap();
        store
    }

    async fn add_account(&self) -> AccountId {
        let input = api::AccountInput {
            label: "Lifecycle".to_string(),
            email: "ada@example.com".to_string(),
            display_name: "Ada".to_string(),
            username: "ada@example.com".to_string(),
            password: "secret".to_string(),
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
        };
        api::add_account_and_id(input).await.expect("add account").0
    }
}

impl Drop for ApiHarness {
    fn drop(&mut self) {
        api::reset_for_tests();
    }
}

#[tokio::test]
async fn factory_reset_deletes_contacts_and_jobs() {
    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new();
    let id = harness.add_account().await;

    {
        let store = harness.open_store();
        let source = pige_core::providers::ContactSourceId::new("source-reset");
        store
            .upsert_contact_source(&id, &source, Some("Address book"))
            .unwrap();
        store
            .upsert_contact_email(
                &id,
                &source,
                &pige_core::providers::RemoteContactId::new("contact-reset"),
                &Address::new(None, "contact@example.com"),
            )
            .unwrap();
        store
            .apply_history_page(&ctx(&id), None, &page(1, "learned@example.com", "cp-1"))
            .unwrap();
        store
            .set_checkpoint(
                &history_job(&id),
                1,
                &ProviderCursor::new(1, "checkpoint-reset"),
            )
            .unwrap();
    }

    api::factory_reset().await.expect("factory reset");

    let conn = rusqlite::Connection::open(harness.db_path()).unwrap();
    for (table, query) in [
        ("contact_sources", "SELECT COUNT(*) FROM contact_sources"),
        ("contact_emails", "SELECT COUNT(*) FROM contact_emails"),
        (
            "correspondence_evidence",
            "SELECT COUNT(*) FROM correspondence_evidence",
        ),
        (
            "evidence_addresses",
            "SELECT COUNT(*) FROM evidence_addresses",
        ),
        (
            "contact_addresses",
            "SELECT COUNT(*) FROM contact_addresses",
        ),
        (
            "contact_learning_state",
            "SELECT COUNT(*) FROM contact_learning_state",
        ),
        ("recipient_index", "SELECT COUNT(*) FROM recipient_index"),
        ("recipient_tokens", "SELECT COUNT(*) FROM recipient_tokens"),
        ("job_checkpoints", "SELECT COUNT(*) FROM job_checkpoints"),
    ] {
        let count: i64 = conn.query_row(query, [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "{table} must be empty after a factory reset");
    }
}

#[tokio::test]
async fn config_backup_excludes_contacts_and_learned_addresses() {
    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new();
    let id = harness.add_account().await;

    {
        let store = harness.open_store();
        let source = pige_core::providers::ContactSourceId::new("source-backup");
        store
            .upsert_contact_source(&id, &source, Some("Address book"))
            .unwrap();
        store
            .upsert_contact_email(
                &id,
                &source,
                &pige_core::providers::RemoteContactId::new("contact-backup"),
                &Address::new(None, "backup-contact@example.com"),
            )
            .unwrap();
        store
            .apply_history_page(
                &ctx(&id),
                None,
                &page(1, "backup-learned@example.com", "cp-1"),
            )
            .unwrap();
    }

    let name = api::backup_config().await.expect("backup config");
    let bytes = harness.backups.read(&name).expect("read backup");
    let text = String::from_utf8(bytes).expect("utf-8 backup");

    assert!(
        text.contains("ada@example.com"),
        "the account configuration is still backed up: {text}"
    );
    assert!(
        !text.contains("backup-contact@example.com"),
        "synced contact addresses must not enter a configuration backup: {text}"
    );
    assert!(
        !text.contains("backup-learned@example.com"),
        "learned addresses must not enter a configuration backup: {text}"
    );
}

#[tokio::test]
async fn rebuild_keeps_a_blocked_sender_out_of_suggestions() {
    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new();
    let id = harness.add_account().await;
    let blocked = "blocked@example.com";
    let allowed = "allowed@example.com";

    {
        let store = harness.open_store();
        let observations = vec![observation(blocked), observation(allowed)];
        store
            .record_observations(
                &ctx(&id),
                &EvidenceId::for_operation(&OperationId::new("op-blocked")),
                LearningOrigin::AcceptedSend,
                None,
                &observations,
            )
            .unwrap();
    }
    api::block_sender(blocked).unwrap();

    // Clear and rebuild must not make a user-blocked address suggestible.
    api::clear_learned_recipients(&id).expect("clear learned recipients");
    api::rebuild_learned_recipients(&id).expect("rebuild learned recipients");
    {
        let store = harness.open_store();
        store
            .record_observations(
                &ctx(&id),
                &EvidenceId::for_operation(&OperationId::new("op-blocked-relearn")),
                LearningOrigin::AcceptedSend,
                None,
                &[observation(blocked), observation(allowed)],
            )
            .unwrap();
    }

    let query = |text: &str| api::RecipientQuery {
        sending_account: id.clone(),
        text: text.to_string(),
        limit: 10,
    };
    let blocked_hits = api::suggest_recipients(query("blocked")).expect("suggest blocked");
    assert!(
        blocked_hits.is_empty(),
        "a user-blocked sender must never be suggested after a rebuild"
    );
    let allowed_hits = api::suggest_recipients(query("allowed")).expect("suggest allowed");
    assert_eq!(allowed_hits.len(), 1);
    assert_eq!(allowed_hits[0].address.email, allowed);

    let _ = &harness.credentials;
}
