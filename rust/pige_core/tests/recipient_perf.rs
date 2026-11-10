//! Task H2: bounded recipient lookup and header-history indexing measurements.
//!
//! The deterministic tests pin the resource bounds that make compose and
//! background learning safe at scale:
//!
//! - the public R2 query reads only the bounded recipient projection and returns
//!   a capped page, even with 100,000 mixed saved/Other/learned addresses;
//! - history pages are generated lazily, so a sparse mailbox with more than a
//!   million UID positions never materializes a million fake messages;
//! - at most two background account jobs run at once and cancellation releases
//!   the slot immediately;
//! - a restart resumes from the durable header checkpoint and still reaches the
//!   oldest message.
//!
//! The `#[ignore]`d tests are measurements, not gates: run them explicitly with
//! `--ignored --nocapture`. The 50 ms p95 recipient-lookup target belongs to the
//! reference Android device, not to a desktop run.
//!
//! Every fixture address is `example.com`; no real mail or provider is touched.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pige_core::Body;
use pige_core::Store;
use pige_core::account::{Account, AccountId, MailBackend, Security, ServerConfig};
use pige_core::api::{self, Config, DB_FILE_NAME, RecipientQuery};
use pige_core::contacts::AddressKey;
use pige_core::contacts::normalize_address;
use pige_core::contacts::rank::query_tokens;
use pige_core::mail::{
    Address, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId, StoredMessage,
    ThreadId, Uid,
};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::providers::fake::FakeProviderFactory;
use pige_core::providers::{
    BodyContent, ErrorKind, HistoryCoverage, HistoryPage, HistoryRequest, MailboxService,
    MessageRecord, OperationContext, ProviderError, RemoteAttachmentId, RemoteMessageId,
};
use pige_core::store::history_job;
use pige_core::sync::history::{
    HISTORY_MAX_CONCURRENT_JOBS, HISTORY_PAGE_SIZE, background_budget, learn_background_slice,
    learn_slice, learning_progress,
};
use pige_core::sync::scheduler::AccountScheduler;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::Notify;

/// The deterministic and measurement tests share process-global state (the API
/// singleton and the allocation tracker). Serialize every test in this binary.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Optional allocation tracking
// ---------------------------------------------------------------------------

/// A system allocator that can sample live heap bytes. Tracking is off for the
/// timed lookup benchmark so atomics do not distort the measurement.
struct TrackingAllocator;

static TRACK_HEAP: AtomicBool = AtomicBool::new(false);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            if TRACK_HEAP.load(Ordering::Relaxed) {
                PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
            }
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn begin_peak_tracking() -> usize {
    let baseline = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(baseline, Ordering::Relaxed);
    TRACK_HEAP.store(true, Ordering::Relaxed);
    baseline
}

fn end_peak_tracking(baseline: usize) -> usize {
    TRACK_HEAP.store(false, Ordering::Relaxed);
    PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(baseline)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn account(id: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: format!("{id}@example.com"),
        display_name: id.to_string(),
        backend: MailBackend::ImapSmtp {
            username: format!("{id}@example.com"),
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

fn ctx(account: &AccountId) -> OperationContext {
    OperationContext::with_timeout(account.clone(), 1, Duration::from_secs(60))
}

fn key(email: &str) -> AddressKey {
    normalize_address(&Address::new(None, email)).expect("fixture address must normalize")
}

fn open_store(dir: &TempDir, account: &Account) -> (PathBuf, Store) {
    let db_path = dir.path().join("data").join("pige.db");
    let store = Store::open(&db_path).expect("open store");
    store.migrate().expect("migrate store");
    store.upsert_account(account).expect("upsert account");
    (db_path, store)
}

/// The public API is process-global; this harness installs it over a temp DB.
struct ApiHarness {
    _dir: TempDir,
    db_path: PathBuf,
    factory: Arc<FakeProviderFactory>,
}

impl ApiHarness {
    fn new(accounts: &[Account]) -> Self {
        Self::seeded(accounts, |_db_path, _store| {})
    }

    fn seeded(accounts: &[Account], seed: impl FnOnce(&Path, &Store)) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let db_path = dir.path().join("data").join(DB_FILE_NAME);
        {
            let store = Store::open(&db_path).unwrap();
            store.migrate().unwrap();
            for account in accounts {
                store.upsert_account(account).unwrap();
            }
            seed(&db_path, &store);
        }

        let factory = Arc::new(FakeProviderFactory::new());
        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            config_dir: dir.path().join("config"),
            credentials: Arc::new(FakeCredentials::default()),
            notifier: Arc::new(FakeNotifier::default()),
            opener: Arc::new(FakeOpener::default()),
            providers: factory.clone(),
            transport: Arc::new(FakeTransportFactory::default()),
            backups: Arc::new(FakeBackupStore::new()),
            auth: None,
        })
        .expect("init api");

        Self {
            _dir: dir,
            db_path,
            factory,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.db_path).unwrap()
    }

    fn suggest(
        &self,
        sending: &AccountId,
        text: &str,
        limit: u16,
    ) -> Vec<pige_core::api::RecipientSuggestion> {
        api::suggest_recipients(RecipientQuery {
            sending_account: sending.clone(),
            text: text.to_string(),
            limit,
        })
        .expect("suggestions")
    }

    fn database_bytes(&self) -> u64 {
        database_bytes(&self.db_path)
    }
}

fn database_bytes(db_path: &Path) -> u64 {
    // WAL mode keeps recent pages in `-wal`; count the sidecars so the number
    // reflects what the fixture actually occupies on disk.
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            std::fs::metadata(format!("{}{suffix}", db_path.display()))
                .ok()
                .map(|metadata| metadata.len())
        })
        .sum()
}

// ---------------------------------------------------------------------------
// 100,000 mixed-address fixture
// ---------------------------------------------------------------------------

const FIXTURE_ADDRESSES: usize = 100_000;

fn fixture_email(index: usize) -> String {
    format!("person{index:06}@example.com")
}

fn fixture_name(index: usize) -> &'static str {
    match index % 10 {
        0..=3 => "Saved Person",
        4..=5 => "Other Person",
        6..=7 => "Incoming Person",
        8 => "Outgoing Person",
        _ => "Selected Person",
    }
}

/// Seed exactly 100,000 unique addresses with mixed durable sources:
/// 40k saved contacts, 20k Other contacts, 20k incoming-only, 10k outgoing, and
/// 10k deliberately selected. Learned rows carry real evidence rows so the
/// production projection rebuild proves the full path, not a pre-filled table.
fn seed_hundred_thousand_mixed(db_path: &Path, account: &Account) {
    let conn = Connection::open(db_path).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    tx.execute(
        "INSERT INTO contact_sources (account_id, source_id, display_name, kind)
         VALUES (?1, 'fixture-saved', 'Saved Fixture', 'saved')",
        [&account.id.0],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO contact_sources (account_id, source_id, display_name, kind)
         VALUES (?1, 'fixture-other', 'Other Fixture', 'other')",
        [&account.id.0],
    )
    .unwrap();

    {
        let mut contact = tx
            .prepare(
                "INSERT INTO contact_emails
                     (account_id, source_id, remote_contact_id, address_key, email, display_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .unwrap();
        let mut learned = tx
            .prepare(
                "INSERT INTO contact_addresses
                     (account_id, address_key, email, display_name, trust, outgoing,
                      last_seen, evidence_count)
                 VALUES (?1, ?2, ?3, ?4, 'ordinary', ?5, ?6, 1)",
            )
            .unwrap();
        let mut evidence = tx
            .prepare(
                "INSERT INTO correspondence_evidence
                     (account_id, evidence_id, origin, fingerprint, observed_at, last_seen)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?4)",
            )
            .unwrap();
        let mut evidence_address = tx
            .prepare(
                "INSERT INTO evidence_addresses
                     (account_id, evidence_id, address_key, role, email, display_name,
                      outgoing, trust, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'ordinary', ?8)",
            )
            .unwrap();

        for index in 0..FIXTURE_ADDRESSES {
            let email = fixture_email(index);
            let name = format!("{} {index:06}", fixture_name(index));
            let last_seen = 1_000 + index as i64;

            match index % 10 {
                0..=3 => {
                    contact
                        .execute(rusqlite::params![
                            account.id.0,
                            "fixture-saved",
                            format!("saved-{index}"),
                            email,
                            email,
                            name,
                        ])
                        .unwrap();
                }
                4..=5 => {
                    contact
                        .execute(rusqlite::params![
                            account.id.0,
                            "fixture-other",
                            format!("other-{index}"),
                            email,
                            email,
                            name,
                        ])
                        .unwrap();
                }
                remainder => {
                    let (origin, role, outgoing) = match remainder {
                        6 | 7 => ("history", "from", 0),
                        8 => ("accepted_send", "to", 1),
                        _ => ("selection", "to", 0),
                    };
                    learned
                        .execute(rusqlite::params![
                            account.id.0,
                            email,
                            email,
                            name,
                            outgoing,
                            last_seen,
                        ])
                        .unwrap();
                    let evidence_id = format!("fixture-evidence-{index}");
                    evidence
                        .execute(rusqlite::params![
                            account.id.0,
                            evidence_id,
                            origin,
                            last_seen,
                        ])
                        .unwrap();
                    evidence_address
                        .execute(rusqlite::params![
                            account.id.0,
                            evidence_id,
                            email,
                            role,
                            email,
                            name,
                            outgoing,
                            last_seen,
                        ])
                        .unwrap();
                }
            }
        }
    }

    tx.commit().unwrap();
}

fn distinct_fixture_addresses(db_path: &Path, account: &AccountId) -> i64 {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT COUNT(DISTINCT address_key) FROM recipient_index WHERE account_id = ?1",
        [&account.0],
        |row| row.get(0),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Deterministic lookup bound
// ---------------------------------------------------------------------------

#[test]
fn lookup_uses_bounded_projection() {
    let _guard = serial();
    let account = account("perf-account");
    let harness = ApiHarness::new(std::slice::from_ref(&account));
    let store = harness.store();
    seed_hundred_thousand_mixed(&harness.db_path, &account);
    store.rebuild_recipient_index().unwrap();

    assert_eq!(
        distinct_fixture_addresses(&harness.db_path, &account.id),
        FIXTURE_ADDRESSES as i64,
        "the fixture must expose exactly 100,000 unique addresses"
    );

    // The public R2 query caps its page regardless of the caller's limit.
    let capped = harness.suggest(&account.id, "person", 10_000);
    assert_eq!(capped.len(), 20, "the public lookup caps the limit at 20");

    let ten = harness.suggest(&account.id, "p", 10);
    assert_eq!(
        ten.len(),
        10,
        "a short prefix still fills the requested page"
    );

    assert!(
        harness.suggest(&account.id, "person", 0).is_empty(),
        "a zero limit returns no rows"
    );

    // Every source kind is reachable through the same bounded projection.
    for (query, expected) in [
        ("Saved Person 000120", "person000120@example.com"),
        ("Other Person 040015", "person040015@example.com"),
        ("Incoming Person 060016", "person060016@example.com"),
        ("Outgoing Person 080018", "person080018@example.com"),
        ("Selected Person 090019", "person090019@example.com"),
    ] {
        let hits = harness.suggest(&account.id, query, 20);
        assert!(
            hits.iter().any(|hit| hit.address.email == expected),
            "query {query:?} must surface {expected}: {:?}",
            hits.iter()
                .map(|hit| hit.address.email.as_str())
                .collect::<Vec<_>>()
        );
    }

    // A one-character prefix matches every row. The indexed top-N must stop
    // early instead of scanning.
    let tokens = vec!["p".to_string()];
    let steps = store
        .recipient_search_vm_steps(&account.id, "p", &tokens, 20)
        .unwrap();
    assert!(
        steps < 5_000,
        "a broad prefix must be bounded work, consumed {steps} VM steps"
    );

    // A selective prefix must not be found by scanning the whole rank order
    // until a match appears. These are the representative compose probes:
    // one email local part, one exact address, and one name + numeric token.
    for query in [
        "person099999",
        "person099999@example.com",
        "Saved Person 000120",
    ] {
        let tokens = query_tokens(query);
        let steps = store
            .recipient_search_vm_steps(&account.id, query, &tokens, 20)
            .unwrap();
        assert!(
            steps < 20_000,
            "selective prefix {query:?} must use a bounded selective path, consumed {steps} VM steps"
        );
    }

    let plan = store
        .explain_recipient_search(&account.id, "p", &tokens, 20)
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("recipient_tokens") && plan.contains("SEARCH recipient_tokens"),
        "token lookup must use the projection search index: {plan}"
    );
    assert!(
        plan.contains("recipient_index_account_rank"),
        "ranking must be driven by the bounded rank index: {plan}"
    );
    assert!(
        !plan.contains("SCAN recipient_index"),
        "a short prefix must not scan the whole projection: {plan}"
    );
    assert!(
        !plan.contains("correspondence_evidence") && !plan.contains("evidence_addresses"),
        "a keystroke must never touch the evidence ledger: {plan}"
    );

    assert_eq!(
        harness.factory.recorder().open_attempts(),
        0,
        "recipient lookup must never open a provider"
    );
}

// ---------------------------------------------------------------------------
// Lazy sparse header fixture
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MailboxLog {
    requests: Vec<HistoryRequest>,
    body_calls: usize,
    attachment_calls: usize,
    generated: usize,
    max_page: usize,
    max_uid: u32,
}

/// A mailbox that generates pages on demand. `total` messages occupy `spacing`
/// UID positions each, so a small message count can model a mailbox with more
/// than a million sparse UID positions without allocating a million records.
struct LazySparseMailbox {
    total: usize,
    spacing: u32,
    log: Arc<Mutex<MailboxLog>>,
    gate: Option<PageGate>,
    max_batch: Option<usize>,
    publish_learning: Option<Arc<tokio::sync::Notify>>,
    calls: usize,
    failure: Option<(usize, ProviderError)>,
}

struct PageGate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl LazySparseMailbox {
    fn new(total: usize, spacing: u32) -> Self {
        Self {
            total,
            spacing,
            log: Arc::new(Mutex::new(MailboxLog::default())),
            gate: None,
            max_batch: None,
            publish_learning: None,
            calls: 0,
            failure: None,
        }
    }

    fn log(&self) -> Arc<Mutex<MailboxLog>> {
        self.log.clone()
    }

    fn gated(mut self, gate: PageGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Notified after the first page is durably applied, so a measurement can
    /// start while the crawl is genuinely in flight.
    fn publish_on_first_page(mut self, notify: Arc<tokio::sync::Notify>) -> Self {
        self.publish_learning = Some(notify);
        self
    }

    fn uid_at(&self, index: usize) -> u32 {
        let remaining = self.total.saturating_sub(1 + index) as u32;
        1 + remaining.saturating_mul(self.spacing)
    }

    fn recipient_at(index: usize) -> String {
        format!("user{index:05}@example.com")
    }
}

fn record_at(mailbox: &LazySparseMailbox, index: usize) -> MessageRecord {
    let uid = mailbox.uid_at(index);
    MessageRecord {
        remote_id: RemoteMessageId::new(format!("INBOX:1:{uid}")),
        remote_thread: None,
        header: MessageHeader {
            message_id: Some(format!("<history-{index}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: format!("History {index}"),
            from: vec![Address::new(None, "sender@example.com")],
            to: vec![Address::new(None, LazySparseMailbox::recipient_at(index))],
            cc: Vec::new(),
            reply_to: Vec::new(),
            bcc: Vec::new(),
            date: 1_700_000_000 + index as i64,
            size: 64,
        },
        flags: Flags::default(),
        collections: Vec::new(),
        snippet: String::new(),
        has_attachments: false,
    }
}

fn parse_cursor(cursor: &pige_core::providers::ProviderCursor) -> Option<usize> {
    cursor.payload.strip_prefix("e1:")?.parse().ok()
}

#[async_trait]
impl MailboxService for LazySparseMailbox {
    async fn collections(
        &mut self,
        _ctx: &OperationContext,
    ) -> Result<Vec<pige_core::providers::CollectionRecord>, ProviderError> {
        Ok(Vec::new())
    }

    async fn body(
        &mut self,
        _id: &RemoteMessageId,
        _ctx: &OperationContext,
    ) -> Result<BodyContent, ProviderError> {
        self.log.lock().unwrap().body_calls += 1;
        Err(ProviderError::unsupported("body"))
    }

    async fn attachment(
        &mut self,
        _message: &RemoteMessageId,
        _id: &RemoteAttachmentId,
        _ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError> {
        self.log.lock().unwrap().attachment_calls += 1;
        Err(ProviderError::unsupported("attachment"))
    }

    async fn history(
        &mut self,
        request: HistoryRequest,
        _ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        let call = self.calls;
        self.calls += 1;
        {
            let mut log = self.log.lock().unwrap();
            log.requests.push(request.clone());
        }

        if let Some(gate) = self.gate.take() {
            gate.entered.notify_one();
            gate.release.notified().await;
        }

        if let Some((failing_call, error)) = &self.failure
            && *failing_call == call
        {
            return Err(error.clone());
        }

        let start = request.cursor.as_ref().and_then(parse_cursor).unwrap_or(0);
        let requested = request.page_size.max(1) as usize;
        let batch = self
            .max_batch
            .map_or(requested, |limit| limit.min(requested));
        let end = (start + batch).min(self.total);
        let messages: Vec<MessageRecord> =
            (start..end).map(|index| record_at(self, index)).collect();

        {
            let mut log = self.log.lock().unwrap();
            log.generated += messages.len();
            log.max_page = log.max_page.max(messages.len());
            log.max_uid = log.max_uid.max(
                (start..end)
                    .map(|index| self.uid_at(index))
                    .max()
                    .unwrap_or(0),
            );
        }

        if let Some(notify) = self.publish_learning.take() {
            notify.notify_one();
        }

        Ok(HistoryPage {
            messages,
            checkpoint: pige_core::providers::ProviderCursor::new(1, format!("e1:{end}")),
            has_more: end < self.total,
            coverage: HistoryCoverage {
                scanned: end as u64,
                total_hint: Some(self.total as u64),
            },
        })
    }
}

async fn run_to_completion(
    store: &Store,
    mailbox: &mut LazySparseMailbox,
    ctx: &OperationContext,
) -> pige_core::sync::history::LearningProgress {
    for slice in 0..500 {
        let progress = learn_slice(store, mailbox, ctx, background_budget())
            .await
            .unwrap_or_else(|error| panic!("slice {slice} failed: {error:?}"));
        if progress.complete {
            return progress;
        }
    }
    panic!("history did not finish in 500 slices");
}

// ---------------------------------------------------------------------------
// Bounded history memory
// ---------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn history_memory_scales_with_page_not_mailbox() {
    let _guard = serial();
    let account = account("history-memory");
    let dir = TempDir::new().unwrap();
    let (_db_path, store) = open_store(&dir, &account);
    let context = ctx(&account.id);

    // 1,205 messages spread over 1,204,001 UID positions. The mailbox holds no
    // message vector: every page is generated lazily from its index range.
    let mut mailbox = LazySparseMailbox::new(1_205, 1_000);
    assert!(mailbox.uid_at(0) > 1_000_000);
    assert_eq!(mailbox.uid_at(1_204), 1);

    let baseline = begin_peak_tracking();
    let progress = run_to_completion(&store, &mut mailbox, &context).await;
    let peak_delta = end_peak_tracking(baseline);

    assert!(progress.complete);
    assert_eq!(progress.scanned, 1_205);
    assert_eq!(progress.total_hint, Some(1_205));

    let log = mailbox.log();
    let log = log.lock().unwrap();
    assert_eq!(
        log.max_page, HISTORY_PAGE_SIZE as usize,
        "one page never exceeds the configured page size"
    );
    assert_eq!(
        log.generated, 1_205,
        "every fixture page is eventually fetched"
    );
    assert!(
        log.max_uid > 1_000_000,
        "the fixture models sparse UID positions above one million"
    );
    assert_eq!(log.body_calls, 0, "learning must never fetch a body");
    assert_eq!(
        log.attachment_calls, 0,
        "learning must never fetch an attachment"
    );

    assert!(
        store.messages_for_account(&account.id).unwrap().is_empty(),
        "history headers must not enter the visible mailbox cache"
    );
    assert_eq!(
        store.learned_addresses(&account.id).unwrap().len(),
        1_206,
        "1,205 recipients plus the shared sender"
    );

    // A 1,205-message mailbox must not need the hundreds of MiB a million
    // materialized records would. The page-sized peak is orders of magnitude
    // smaller; the bound leaves room for SQLite's cache and test scaffolding.
    assert!(
        peak_delta < 64 * 1024 * 1024,
        "history learning allocated {peak_delta} bytes; it must scale with the \
         page, not the mailbox"
    );
    println!(
        "history memory: 1,205 lazy messages, max page {}, peak heap delta {} KiB",
        log.max_page,
        peak_delta / 1024
    );
}

// ---------------------------------------------------------------------------
// Background slot cancellation
// ---------------------------------------------------------------------------

async fn background(
    store: Arc<Store>,
    mut mailbox: LazySparseMailbox,
    context: OperationContext,
    scheduler: Arc<AccountScheduler>,
) -> Result<pige_core::sync::history::LearningProgress, ProviderError> {
    learn_background_slice(
        store.as_ref(),
        &mut mailbox,
        &context,
        background_budget(),
        &scheduler,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn cancellation_releases_background_slot() {
    let _guard = serial();
    assert_eq!(
        HISTORY_MAX_CONCURRENT_JOBS, 2,
        "the configured account-job bound is two"
    );

    let dir = TempDir::new().unwrap();
    let first = account("slot-one");
    let second = account("slot-two");
    let third = account("slot-three");
    let (_db_path, store) = open_store(&dir, &first);
    for account in [&second, &third] {
        store.upsert_account(account).unwrap();
    }
    let store = Arc::new(store);
    let scheduler = Arc::new(AccountScheduler::new());

    let entered_one = Arc::new(Notify::new());
    let release_one = Arc::new(Notify::new());
    let entered_two = Arc::new(Notify::new());
    let release_two = Arc::new(Notify::new());

    let mailbox_one = LazySparseMailbox::new(50, 1_000).gated(PageGate {
        entered: entered_one.clone(),
        release: release_one.clone(),
    });
    let mailbox_two = LazySparseMailbox::new(50, 1_000).gated(PageGate {
        entered: entered_two.clone(),
        release: release_two.clone(),
    });
    let mailbox_three = LazySparseMailbox::new(10, 1_000);
    let third_log = mailbox_three.log();

    let first_context = ctx(&first.id);
    let first_task = {
        let store = store.clone();
        let scheduler = scheduler.clone();
        tokio::spawn(background(
            store,
            mailbox_one,
            first_context.clone(),
            scheduler,
        ))
    };
    tokio::time::timeout(Duration::from_secs(5), entered_one.notified())
        .await
        .expect("the first background job must enter its page");

    let second_context = ctx(&second.id);
    let second_task = {
        let store = store.clone();
        let scheduler = scheduler.clone();
        tokio::spawn(background(store, mailbox_two, second_context, scheduler))
    };
    tokio::time::timeout(Duration::from_secs(5), entered_two.notified())
        .await
        .expect("the second background job must enter its page");

    // Both process-wide slots are held. A third account must not open provider
    // work at all; it reports durable progress and is retried later.
    let blocked = tokio::time::timeout(
        Duration::from_secs(5),
        background(
            store.clone(),
            mailbox_three,
            ctx(&third.id),
            scheduler.clone(),
        ),
    )
    .await
    .expect("a blocked job must return immediately")
    .unwrap();
    assert!(!blocked.complete);
    assert!(
        third_log.lock().unwrap().requests.is_empty(),
        "more than two concurrent account jobs must never issue provider work"
    );

    // Cancelling one in-flight job releases its slot without waiting for the
    // page's network future to finish.
    first_context.cancellation.cancel();
    let cancelled = tokio::time::timeout(Duration::from_secs(5), first_task)
        .await
        .expect("cancellation must wake the in-flight job")
        .unwrap()
        .expect_err("a cancelled slice reports Cancelled");
    assert_eq!(cancelled.kind, ErrorKind::Cancelled);

    // The released slot admits the third account immediately, even while the
    // second job still holds its own slot.
    let third_mailbox = LazySparseMailbox::new(10, 1_000);
    let third_log = third_mailbox.log();
    let third_progress = tokio::time::timeout(
        Duration::from_secs(5),
        background(
            store.clone(),
            third_mailbox,
            ctx(&third.id),
            scheduler.clone(),
        ),
    )
    .await
    .expect("the third job must acquire the released slot")
    .unwrap();
    assert!(third_progress.complete);
    assert_eq!(
        third_log.lock().unwrap().requests.len(),
        1,
        "the third account actually ran after the slot was released"
    );

    release_two.notify_one();
    let second_progress = tokio::time::timeout(Duration::from_secs(5), second_task)
        .await
        .expect("the second job must finish once released")
        .unwrap()
        .unwrap();
    assert!(second_progress.complete);

    // The cancelled account kept its durable, not-yet-started progress and did
    // not mark the crawl complete.
    let durable = learning_progress(&store, &first.id).unwrap();
    assert!(!durable.complete);
}

// ---------------------------------------------------------------------------
// Restart reaches the oldest header
// ---------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn index_restart_eventually_reaches_oldest_header() {
    let _guard = serial();
    let account = account("history-restart");
    let dir = TempDir::new().unwrap();
    let (db_path, store) = open_store(&dir, &account);
    let context = ctx(&account.id);

    let mut mailbox = LazySparseMailbox::new(1_205, 1_000);
    let first = learn_slice(
        &store,
        &mut mailbox,
        &context,
        pige_core::sync::provider::WorkBudget::with_timeout(100, 2, Duration::from_secs(30)),
    )
    .await
    .unwrap();
    assert!(!first.complete);
    assert_eq!(first.scanned, 200);
    let checkpoint = store
        .job_checkpoint(&history_job(&account.id))
        .unwrap()
        .expect("the first two pages must be durable");
    assert_eq!(checkpoint.payload, "e1:200");
    drop(mailbox);
    drop(store);

    // A fresh process-style reopen must resume from the durable cursor.
    let store = Store::open(&db_path).unwrap();
    store.migrate().unwrap();
    let mut mailbox = LazySparseMailbox::new(1_205, 1_000);
    let restarted_log = mailbox.log();
    let progress = run_to_completion(&store, &mut mailbox, &context).await;

    assert!(progress.complete);
    assert_eq!(progress.scanned, 1_205);
    assert_eq!(
        restarted_log.lock().unwrap().requests[0].cursor.as_ref(),
        Some(&checkpoint),
        "a restart resumes from the durable checkpoint, not from zero"
    );
    assert!(
        store
            .learned_address(&account.id, &key("user01204@example.com"))
            .unwrap()
            .is_some(),
        "the crawl must reach the oldest header after the restart"
    );
    assert_eq!(
        store.learned_addresses(&account.id).unwrap().len(),
        1_206,
        "every fixture page is eventually learned"
    );

    let log = restarted_log.lock().unwrap();
    assert!(log.generated > 0);
    assert_eq!(log.body_calls, 0);
    assert_eq!(log.attachment_calls, 0);
    assert!(log.max_uid > 1_000_000);
}

// ---------------------------------------------------------------------------
// Cached reader measurements
// ---------------------------------------------------------------------------

fn cached_inbox(store: &Store, account: &Account, count: usize) {
    store
        .upsert_folder(&Folder {
            id: FolderId("inbox".to_string()),
            account_id: account.id.clone(),
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
            unread: 0,
            total: count as u32,
            uidvalidity: Some(1),
            uidnext: Some(count as u32 + 1),
        })
        .unwrap();

    for index in 0..count {
        let id = MessageId(format!("cached-{index}"));
        store
            .upsert_message(&StoredMessage {
                id: id.clone(),
                account_id: account.id.clone(),
                folder_id: Some(FolderId("inbox".to_string())),
                uid: Some(Uid(index as u32 + 1)),
                uidvalidity: Some(1),
                header: MessageHeader {
                    message_id: Some(format!("<cached-{index}@example.com>")),
                    in_reply_to: None,
                    references: Vec::new(),
                    subject: format!("Cached conversation {index:04}"),
                    from: vec![Address::new(
                        Some(format!("Sender {index}")),
                        "sender@example.com",
                    )],
                    to: vec![Address::new(None, account.email.clone())],
                    cc: Vec::new(),
                    reply_to: Vec::new(),
                    bcc: Vec::new(),
                    date: 1_700_000_000 + index as i64,
                    size: 256,
                },
                flags: Flags::default(),
                thread_id: ThreadId(format!("thread-{index}")),
                snippet: String::new(),
                has_attachments: false,
            })
            .unwrap();
        store
            .upsert_body(
                &id,
                &Body {
                    plain_text: Some(format!("cached body {index}")),
                    html: None,
                    fetched_at: 1,
                },
            )
            .unwrap();
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
#[ignore = "performance measurement; run explicitly with --ignored --nocapture"]
async fn cold_cached_inbox_and_body_open() {
    let _guard = serial();
    let account = account("cached-inbox");
    let harness = ApiHarness::seeded(std::slice::from_ref(&account), |_db_path, store| {
        cached_inbox(store, &account, 1_000);
    });

    let cold_started = Instant::now();
    let cold = api::snapshot();
    let cold_elapsed = cold_started.elapsed();
    assert_eq!(cold.conversations.len(), 300, "the first page is capped");
    assert!(
        cold.conversations[0]
            .subject
            .contains("Cached conversation")
    );

    let mut warm = Vec::with_capacity(20);
    for _ in 0..20 {
        let started = Instant::now();
        let snapshot = api::snapshot();
        warm.push(started.elapsed());
        assert_eq!(snapshot.conversations.len(), 300);
    }
    report_latency("cold cached inbox (1,000 cached messages)", &mut warm, None);
    println!("cold cached inbox first snapshot: {cold_elapsed:?}");

    let thread = ThreadId("thread-0".to_string());
    for _ in 0..5 {
        let _ = api::open_conversation(&thread).await.unwrap();
    }
    let mut opens = Vec::with_capacity(20);
    for _ in 0..20 {
        let started = Instant::now();
        let opened = api::open_conversation(&thread).await.unwrap();
        opens.push(started.elapsed());
        assert!(opened.selected_conversation.is_some());
    }
    report_latency("cached body on open", &mut opens, None);
    assert_eq!(
        harness.factory.recorder().open_attempts(),
        0,
        "a cached body opens from SQLite without opening a provider"
    );
}

// ---------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort_unstable();
    let index = ((samples.len() - 1) * percent) / 100;
    samples[index]
}

fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmHWM:") {
            let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
            return Some(kib * 1024);
        }
    }
    None
}

fn current_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
            return Some(kib * 1024);
        }
    }
    None
}

fn fixture_queries() -> Vec<String> {
    let mut queries = Vec::new();
    // 100 email local-part prefixes spread across the whole fixture.
    for position in 0..100 {
        let index = (position * 997) % FIXTURE_ADDRESSES;
        queries.push(format!("person{index:06}"));
    }
    // 50 full-address prefixes.
    for position in 0..50 {
        let index = (position * 1_999) % FIXTURE_ADDRESSES;
        queries.push(fixture_email(index));
    }
    // 25 name prefixes per durable group.
    for position in 0..25 {
        let index = (position * 37) % 40_000;
        queries.push(format!("Saved Person {index:06}"));
    }
    for position in 0..25 {
        let index = 40_000 + (position * 37) % 20_000;
        queries.push(format!("Other Person {index:06}"));
    }
    for position in 0..25 {
        let index = 60_000 + (position * 37) % 40_000;
        queries.push(format!("Person {index:06}"));
    }
    // 25 broad domain prefixes.
    for _ in 0..25 {
        queries.push("example.com".to_string());
    }
    assert!(queries.len() >= 200, "at least 200 representative queries");
    queries
}

fn measure_lookup(harness: &ApiHarness, account: &AccountId, queries: &[String]) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(queries.len());
    for query in queries {
        let started = Instant::now();
        let hits = harness.suggest(account, query, 10);
        samples.push(started.elapsed());
        assert!(hits.len() <= 20, "the public query remains bounded");
    }
    samples
}

fn report_latency(label: &str, samples: &mut [Duration], target: Option<Duration>) {
    let count = samples.len();
    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    let p50 = percentile(samples, 50);
    let p95 = percentile(samples, 95);
    let mean: Duration = samples.iter().sum::<Duration>() / count as u32;
    let target_note = target.map_or_else(
        || "target: none".to_string(),
        |target| {
            if cfg!(target_os = "android") {
                format!(
                    "p95 target {target:?}: {}",
                    if p95 <= target { "PASS" } else { "MISS" }
                )
            } else {
                format!(
                    "target {target:?} is the Android reference device; this run is {} and is not the target",
                    std::env::consts::OS
                )
            }
        },
    );
    println!(
        "{label}: queries={count} min={min:?} p50={p50:?} mean={mean:?} p95={p95:?} max={max:?} ({target_note})"
    );
}

/// A background crawl on a second SQLite connection, used to measure compose
/// keystrokes while history indexing is actively writing.
struct BackgroundCrawl {
    started: Arc<Notify>,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BackgroundCrawl {
    fn start(db_path: &Path, account: &Account) -> Self {
        let started = Arc::new(Notify::new());
        let done = Arc::new(AtomicBool::new(false));
        let db_path = db_path.to_path_buf();
        let account = account.clone();
        let started_signal = started.clone();
        let done_signal = done.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("background crawl runtime");
            runtime.block_on(async move {
                let store = Store::open(&db_path).expect("background store");
                store.migrate().expect("background migrate");
                let context = ctx(&account.id);
                let mut mailbox =
                    LazySparseMailbox::new(12_050, 100).publish_on_first_page(started_signal);
                for _ in 0..500 {
                    let progress = learn_slice(&store, &mut mailbox, &context, background_budget())
                        .await
                        .expect("background crawl slice");
                    if progress.complete {
                        break;
                    }
                }
                done_signal.store(true, Ordering::Relaxed);
            });
        });
        Self {
            started,
            done,
            thread: Some(thread),
        }
    }

    async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(30), self.started.notified())
            .await
            .expect("the background crawl must publish its first page");
    }
}

impl Drop for BackgroundCrawl {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("background crawl joins");
        }
    }
}

#[test]
#[ignore = "performance measurement; run explicitly with --ignored --nocapture"]
fn recipient_lookup_100k() {
    let _guard = serial();
    let account = account("perf-100k");
    let harness = ApiHarness::new(std::slice::from_ref(&account));
    let store = harness.store();

    let seed_started = Instant::now();
    seed_hundred_thousand_mixed(&harness.db_path, &account);
    let seed_elapsed = seed_started.elapsed();
    let rebuild_started = Instant::now();
    store.rebuild_recipient_index().unwrap();
    let rebuild_elapsed = rebuild_started.elapsed();
    assert_eq!(
        distinct_fixture_addresses(&harness.db_path, &account.id),
        FIXTURE_ADDRESSES as i64
    );

    let queries = fixture_queries();
    // Warm the SQLite statement and page caches with a representative sample.
    for query in queries.iter().take(10) {
        for _ in 0..3 {
            let _ = harness.suggest(&account.id, query, 10);
        }
    }

    let rss_before = current_rss_bytes();
    let mut idle = measure_lookup(&harness, &account.id, &queries);
    report_latency(
        "recipient lookup 100k (idle)",
        &mut idle,
        Some(Duration::from_millis(50)),
    );

    // Compose keystrokes while a background crawl writes evidence pages on a
    // second connection. This is the contention case the review cares about.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("measurement runtime");
    let crawl = BackgroundCrawl::start(&harness.db_path, &account);
    runtime.block_on(crawl.wait_started());
    let mut during = measure_lookup(&harness, &account.id, &queries);
    let crawl_still_running = !crawl.done.load(Ordering::Relaxed);
    report_latency(
        "recipient lookup 100k (background crawl)",
        &mut during,
        Some(Duration::from_millis(50)),
    );
    drop(crawl);

    let rss_after = current_rss_bytes();
    let peak_rss = peak_rss_bytes();
    let addresses_per_second =
        FIXTURE_ADDRESSES as f64 / rebuild_elapsed.as_secs_f64().max(f64::EPSILON);
    println!(
        "data: {FIXTURE_ADDRESSES} unique mixed addresses; seed={seed_elapsed:?}; \
         projection rebuild={rebuild_elapsed:?} ({addresses_per_second:.0} addresses/s); \
         sqlite={} bytes; rss before={} KiB after={} KiB peak={} KiB; \
         background_crawl_active_during_queries={crawl_still_running}; \
         build={} {}, target={}/{}, debug_assertions={}",
        harness.database_bytes(),
        rss_before.map_or(0, |bytes| bytes / 1024),
        rss_after.map_or(0, |bytes| bytes / 1024),
        peak_rss.map_or(0, |bytes| bytes / 1024),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
        cfg!(debug_assertions),
    );
}

#[test]
#[ignore = "performance measurement; run explicitly with --ignored --nocapture"]
fn history_indexing_sparse() {
    let _guard = serial();
    let indexed_account = account("index-sparse");
    let dir = TempDir::new().unwrap();
    let (db_path, store) = open_store(&dir, &indexed_account);
    let context = ctx(&indexed_account.id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("measurement runtime");

    // 12,050 headers with 100 UID positions between each: the newest UID is
    // 1,204,901, yet only 12,050 records are ever materialized.
    let mut mailbox = LazySparseMailbox::new(12_050, 100);
    assert!(mailbox.uid_at(0) > 1_000_000);

    runtime.block_on(async {
        let warm = learn_slice(&store, &mut mailbox, &context, background_budget())
            .await
            .expect("warm-up slice");
        assert!(!warm.complete);

        // Model a process restart: reopen the database and resume from the
        // durable checkpoint, timing only the resumed work.
        drop(store);
        let store = Store::open(&db_path).unwrap();
        store.migrate().unwrap();
        let resumed_started = Instant::now();
        let progress = run_to_completion(&store, &mut mailbox, &context).await;
        let resumed_elapsed = resumed_started.elapsed();

        assert!(progress.complete);
        assert_eq!(progress.scanned, 12_050);
        let log = mailbox.log();
        let log = log.lock().unwrap();
        assert_eq!(log.generated, 12_050);
        assert_eq!(log.body_calls, 0);
        assert_eq!(log.attachment_calls, 0);
        assert!(log.max_uid > 1_000_000);
        assert!(log.max_page <= HISTORY_PAGE_SIZE as usize);
        let rate = (progress.scanned - warm.scanned) as f64 / resumed_elapsed.as_secs_f64();
        println!(
            "history indexing: 12,050 sparse headers (max UID > 1,000,000); \
             warm-up={} messages; resumed={:?} for {} messages ({rate:.0} messages/s); \
             sqlite={} bytes; peak rss={} KiB; build={} {}, target={}/{}, debug_assertions={}",
            warm.scanned,
            resumed_elapsed,
            progress.scanned - warm.scanned,
            database_bytes(&db_path),
            peak_rss_bytes().map_or(0, |bytes| bytes / 1024),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY,
            cfg!(debug_assertions),
        );
    });

    // Quota backoff: a 429 on the second page leaves the first committed page's
    // checkpoint intact and records a durable delay that suppresses further
    // requests. Measure the configured backoff rather than sleeping through it.
    let backoff_account = account("index-backoff");
    let backoff_dir = TempDir::new().unwrap();
    let (_backoff_path, backoff_store) = open_store(&backoff_dir, &backoff_account);
    let mut failing = LazySparseMailbox::new(250, 1_000);
    {
        // Make the second page return a rate-limit error.
        let mut log = failing.log.lock().unwrap();
        log.requests.clear();
    }
    failing = failing.fail_second_page(ProviderError::rate_limited(Duration::from_secs(2)));
    let error = runtime
        .block_on(learn_slice(
            &backoff_store,
            &mut failing,
            &ctx(&backoff_account.id),
            background_budget(),
        ))
        .expect_err("the second page is rate limited");
    assert_eq!(error.kind, ErrorKind::RateLimited);
    let state = backoff_store.history_state(&backoff_account.id).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(state.retry_after_ms > now);
    let checkpoint = backoff_store
        .job_checkpoint(&history_job(&backoff_account.id))
        .unwrap()
        .expect("the first page checkpoint survives");
    assert_eq!(checkpoint.payload, "e1:100");
    println!(
        "quota backoff: configured 2s, durable retry in {} ms, checkpoint={}",
        state.retry_after_ms - now,
        checkpoint.payload
    );
}

impl LazySparseMailbox {
    fn fail_second_page(mut self, error: ProviderError) -> Self {
        self.failure = Some((1, error));
        self
    }
}
