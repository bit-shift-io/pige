//! Task R3: restart-safe, header-only history learning.
//!
//! The crawl must reach the oldest accessible message, never load history into
//! the visible mail cache, never fetch bodies or attachments, resume from a
//! durable checkpoint after a restart, and survive an expired cursor, a
//! rate-limit delay, or an interactive operation arriving between pages. All
//! fixture addresses are `example.com`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pige_core::Store;
use pige_core::account::{Account, AccountId, Security, ServerConfig};
use pige_core::contacts::AddressKey;
use pige_core::contacts::normalize_address;
use pige_core::mail::{Address, Flags, MessageHeader};
use pige_core::providers::{
    BodyContent, ChangePage, ErrorKind, HistoryCoverage, HistoryPage, HistoryRequest,
    IMAP_SMTP_PROVIDER, JobKey, JobKind, MailChange, MailboxService, MessageRecord,
    OperationContext, ProviderCapabilities, ProviderCursor, ProviderError, ProviderFactory,
    ProviderSession, RemoteAttachmentId, RemoteMessageId,
};
use pige_core::store::HistoryProgress;
use pige_core::sync::history::{
    LearningProgress, background_budget, foreground_budget, learn_account_slice, learn_slice,
    learn_slice_paged, learning_progress,
};
use pige_core::sync::provider::WorkBudget;
use pige_core::sync::scheduler::AccountScheduler;
use tempfile::TempDir;
use tokio::sync::Notify;

/// Serializes the tests that deliberately hold the process-wide history slots.
static SLOT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn account() -> Account {
    Account {
        id: AccountId("account-r3".to_string()),
        label: "R3".to_string(),
        email: "r3@example.com".to_string(),
        display_name: "R3 Test".to_string(),
        backend: pige_core::account::MailBackend::ImapSmtp {
            username: "r3@example.com".to_string(),
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

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    store.upsert_account(&account()).unwrap();
    (dir, store)
}

fn ctx(account: &AccountId) -> OperationContext {
    OperationContext::with_timeout(account.clone(), 1, Duration::from_secs(60))
}

fn key(email: &str) -> AddressKey {
    normalize_address(&Address::new(None, email)).expect("fixture address must normalize")
}

fn evidence_count(store: &Store, account: &AccountId, email: &str) -> u32 {
    store
        .learned_address(account, &key(email))
        .unwrap()
        .map(|learned| learned.evidence_count)
        .unwrap_or(0)
}

fn header(index: usize, recipient: &str) -> MessageHeader {
    MessageHeader {
        message_id: Some(format!("<history-{index}@example.com>")),
        in_reply_to: None,
        references: Vec::new(),
        subject: format!("History {index}"),
        from: vec![Address::new(None, "sender@example.com")],
        to: vec![Address::new(None, recipient)],
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: 1_700_000_000 + index as i64,
        size: 64,
    }
}

fn record(index: usize, recipient: &str, uid: u32) -> MessageRecord {
    MessageRecord {
        remote_id: RemoteMessageId::new(format!("INBOX:1:{uid}")),
        remote_thread: None,
        header: header(index, recipient),
        flags: Flags::default(),
        collections: Vec::new(),
        snippet: String::new(),
        has_attachments: false,
    }
}

fn unique_messages(count: usize) -> Vec<MessageRecord> {
    (0..count)
        .map(|index| record(index, &format!("user{index}@example.com"), index as u32 + 1))
        .collect()
}

/// One message per index, all addressed to the same correspondent, so a
/// duplicated page would visibly inflate its evidence count.
fn messages_to(count: usize, recipient: &str) -> Vec<MessageRecord> {
    (0..count)
        .map(|index| record(index, recipient, index as u32 + 1))
        .collect()
}

// ---------------------------------------------------------------------------
// A scripted, cursor-driven MailboxService
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MailboxLog {
    requests: Vec<HistoryRequest>,
    body_calls: usize,
    attachment_calls: usize,
}

struct PageGate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

struct ScriptedMailbox {
    messages: Vec<MessageRecord>,
    log: Arc<Mutex<MailboxLog>>,
    /// Cursor that the adapter no longer understands.
    reject_cursor: Option<ProviderCursor>,
    /// Cursor payload prefix; moves to `e2` after a rejection, modelling an
    /// adapter rebaseline.
    epoch: String,
    /// Failures keyed by zero-based call index.
    failures: HashMap<usize, ProviderError>,
    calls: usize,
    /// A short page that still reports `has_more`.
    max_batch: Option<usize>,
    /// Pause inside the first history call, for scheduler preemption tests.
    gate: Option<PageGate>,
}

impl ScriptedMailbox {
    fn new(messages: Vec<MessageRecord>) -> Self {
        Self {
            messages,
            log: Arc::new(Mutex::new(MailboxLog::default())),
            reject_cursor: None,
            epoch: "e1".to_string(),
            failures: HashMap::new(),
            calls: 0,
            max_batch: None,
            gate: None,
        }
    }

    fn log(&self) -> Arc<Mutex<MailboxLog>> {
        self.log.clone()
    }

    fn reject(mut self, cursor: ProviderCursor) -> Self {
        self.reject_cursor = Some(cursor);
        self
    }

    fn fail_at(mut self, call: usize, error: ProviderError) -> Self {
        self.failures.insert(call, error);
        self
    }

    fn max_batch(mut self, limit: usize) -> Self {
        self.max_batch = Some(limit);
        self
    }

    fn gate(mut self, gate: PageGate) -> Self {
        self.gate = Some(gate);
        self
    }
}

fn parse_cursor(cursor: &ProviderCursor) -> Option<(String, usize)> {
    let (epoch, index) = cursor.payload.split_once(':')?;
    Some((epoch.to_string(), index.parse().ok()?))
}

#[async_trait]
impl MailboxService for ScriptedMailbox {
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
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        let call = self.calls;
        self.calls += 1;
        self.log.lock().unwrap().requests.push(request.clone());

        if let Some(gate) = self.gate.take() {
            gate.entered.notify_one();
            gate.release.notified().await;
        }

        if let Some(error) = self.failures.remove(&call) {
            ctx.ensure_active()?;
            return Err(error);
        }
        ctx.ensure_active()?;

        if let Some(reject) = &self.reject_cursor
            && request.cursor.as_ref() == Some(reject)
        {
            // A cursor pointing at the old epoch is dead; the adapter now
            // rebaselines from no cursor under a new epoch.
            self.epoch = "e2".to_string();
            return Err(ProviderError::cursor_invalid(
                "the history cursor was written by a different version",
            ));
        }

        let (epoch, start) = match &request.cursor {
            Some(cursor) => parse_cursor(cursor)
                .ok_or_else(|| ProviderError::cursor_invalid("unreadable history cursor"))?,
            None => (self.epoch.clone(), 0),
        };
        if epoch != self.epoch {
            return Err(ProviderError::cursor_invalid("stale history epoch"));
        }

        let requested = request.page_size.max(1) as usize;
        let batch = self
            .max_batch
            .map_or(requested, |limit| limit.min(requested));
        let end = (start + batch).min(self.messages.len());
        let messages = self.messages[start..end].to_vec();
        let has_more = end < self.messages.len();
        let checkpoint = ProviderCursor::new(1, format!("{epoch}:{end}"));
        Ok(HistoryPage {
            messages,
            checkpoint,
            has_more,
            coverage: HistoryCoverage {
                scanned: end as u64,
                total_hint: Some(self.messages.len() as u64),
            },
        })
    }
}

/// A provider factory that hands out one scripted mailbox per account, exactly
/// like a real `open` would.
struct ScriptedFactory {
    mailboxes: tokio::sync::Mutex<HashMap<AccountId, ScriptedMailbox>>,
}

impl ScriptedFactory {
    fn new(entries: Vec<(AccountId, ScriptedMailbox)>) -> Self {
        Self {
            mailboxes: tokio::sync::Mutex::new(entries.into_iter().collect()),
        }
    }
}

#[async_trait]
impl ProviderFactory for ScriptedFactory {
    async fn open(
        &self,
        account: &Account,
        _ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        let mailbox = self
            .mailboxes
            .lock()
            .await
            .remove(&account.id)
            .ok_or_else(|| ProviderError::not_found("no scripted mailbox for this account"))?;
        let mut session = ProviderSession::new(ProviderCapabilities {
            mailbox: true,
            ..ProviderCapabilities::default()
        });
        session.mailbox = Some(Box::new(mailbox));
        Ok(session)
    }
}

fn account_named(id: &str) -> Account {
    let mut account = account();
    account.id = AccountId(id.to_string());
    account.email = format!("{id}@example.com");
    account.label = id.to_string();
    account
}

async fn run_to_completion(
    store: &Store,
    mailbox: &mut ScriptedMailbox,
    ctx: &OperationContext,
) -> LearningProgress {
    for slice in 0..40 {
        let progress = learn_slice(store, mailbox, ctx, background_budget())
            .await
            .unwrap_or_else(|error| panic!("slice {slice} failed: {error:?}"));
        if progress.complete {
            return progress;
        }
        assert!(progress.paused, "an incomplete slice must report paused");
    }
    panic!("history did not finish in 40 slices");
}

/// Scripted adapter pages that model a mid-crawl UID epoch reset: an old-epoch
/// page, an empty non-complete rebaseline page, then the new-epoch page.
struct EpochMailbox {
    pages: Vec<HistoryPage>,
    calls: usize,
}

#[async_trait]
impl MailboxService for EpochMailbox {
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
        Err(ProviderError::unsupported("body"))
    }

    async fn attachment(
        &mut self,
        _message: &RemoteMessageId,
        _id: &RemoteAttachmentId,
        _ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::unsupported("attachment"))
    }

    async fn history(
        &mut self,
        _request: HistoryRequest,
        _ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        let page = self
            .pages
            .get(self.calls)
            .cloned()
            .ok_or_else(|| ProviderError::transient("pages exhausted"))?;
        self.calls += 1;
        Ok(page)
    }
}

fn history_page(
    messages: Vec<MessageRecord>,
    checkpoint: &str,
    has_more: bool,
    scanned: u64,
) -> HistoryPage {
    HistoryPage {
        messages,
        checkpoint: ProviderCursor::new(1, checkpoint),
        has_more,
        coverage: HistoryCoverage {
            scanned,
            total_hint: Some(3),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn learns_beyond_initial_300() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let mut mailbox = ScriptedMailbox::new(unique_messages(1_205));

    let progress = run_to_completion(&store, &mut mailbox, &context).await;

    assert!(progress.complete);
    assert_eq!(progress.scanned, 1_205);
    assert_eq!(progress.total_hint, Some(1_205));
    assert!(
        store
            .learned_address(&account.id, &key("user300@example.com"))
            .unwrap()
            .is_some(),
        "learning must continue past the first visible window"
    );
    assert!(
        store
            .learned_address(&account.id, &key("user1204@example.com"))
            .unwrap()
            .is_some(),
        "the oldest message must be learned"
    );
    assert_eq!(
        store.learned_addresses(&account.id).unwrap().len(),
        1_206,
        "1205 recipients plus the shared sender"
    );
}

#[tokio::test]
async fn resume_after_restart_without_duplicate_counts() {
    let (dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let messages = messages_to(1_205, "alice@example.com");
    let mut mailbox = ScriptedMailbox::new(messages.clone());
    let log = mailbox.log();
    let budget = WorkBudget::with_timeout(100, 2, Duration::from_secs(10));

    // Two-page slices, interrupted long before the end.
    for _ in 0..3 {
        let progress = learn_slice(&store, &mut mailbox, &context, budget)
            .await
            .unwrap();
        assert!(!progress.complete);
    }
    let before_restart = store
        .job_checkpoint(&pige_core::store::history_job(&account.id))
        .unwrap()
        .expect("a checkpoint must be durable");
    assert_eq!(before_restart.payload, "e1:600");
    drop(mailbox);
    drop(store);

    // A fresh process opens the same database and a fresh session.
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    let mut mailbox = ScriptedMailbox::new(messages);
    let restarted_log = mailbox.log();
    let progress = run_to_completion(&store, &mut mailbox, &context).await;

    assert!(progress.complete);
    assert_eq!(progress.scanned, 1_205);
    let first_after_restart = &restarted_log.lock().unwrap().requests[0];
    assert_eq!(
        first_after_restart.cursor.as_ref(),
        Some(&before_restart),
        "a restart must resume from the durable checkpoint, not from zero"
    );
    assert_eq!(
        evidence_count(&store, &account.id, "alice@example.com"),
        1_205,
        "replaying a resumed page must not multiply the count"
    );
    assert_eq!(
        evidence_count(&store, &account.id, "sender@example.com"),
        1_205
    );
    assert!(
        log.lock().unwrap().requests.len() >= 3,
        "the interrupted run must have fetched pages"
    );
}

#[tokio::test]
async fn sparse_imap_uids_reach_oldest_message() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    // Sparse UIDs (large gaps), newest first, with an ancient UID 1 at the end.
    let mut messages: Vec<MessageRecord> = (0..1_204)
        .map(|index| {
            let uid = 50_000 - index as u32 * 37;
            record(index, &format!("sparse{index}@example.com"), uid)
        })
        .collect();
    messages.push(record(1_204, "oldest@example.com", 1));
    let mut mailbox = ScriptedMailbox::new(messages).max_batch(37);

    let progress = run_to_completion(&store, &mut mailbox, &context).await;

    assert!(progress.complete);
    assert_eq!(progress.scanned, 1_205);
    assert!(
        store
            .learned_address(&account.id, &key("oldest@example.com"))
            .unwrap()
            .is_some(),
        "sparse UIDs must not stop the crawl before UID 1"
    );
    assert!(
        store
            .learned_address(&account.id, &key("sparse1203@example.com"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn expired_cursor_replays_safely() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let mut mailbox = ScriptedMailbox::new(unique_messages(250));
    let log = mailbox.log();

    let first = learn_slice(
        &store,
        &mut mailbox,
        &context,
        WorkBudget::with_timeout(100, 1, Duration::from_secs(10)),
    )
    .await
    .unwrap();
    assert!(!first.complete);
    let stale = store
        .job_checkpoint(&pige_core::store::history_job(&account.id))
        .unwrap()
        .expect("checkpoint after the first page");

    mailbox = mailbox.reject(stale.clone());
    let progress = run_to_completion(&store, &mut mailbox, &context).await;

    assert!(progress.complete);
    assert_eq!(progress.scanned, 250);
    let requests = log.lock().unwrap().requests.clone();
    assert_eq!(requests[1].cursor.as_ref(), Some(&stale));
    assert!(
        requests[2].cursor.is_none(),
        "an expired cursor must restart once from no cursor"
    );
    assert_eq!(
        requests[3]
            .cursor
            .as_ref()
            .map(|cursor| cursor.payload.clone()),
        Some("e2:100".to_string()),
        "the rebaselined cursor must continue the new epoch"
    );
    assert_eq!(
        store.learned_addresses(&account.id).unwrap().len(),
        251,
        "replaying the first page must not duplicate evidence"
    );
}

#[tokio::test]
async fn uid_epoch_change_replays_without_duplicate_evidence() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    // Old epoch: alice (A) and bob (B).
    let old_a = record(0, "alice@example.com", 10);
    let old_b = record(1, "bob@example.com", 9);
    // New epoch: the identical logical message A under a different native id
    // (UIDVALIDITY is part of the id) plus a genuinely new message for carol.
    let mut new_a = old_a.clone();
    new_a.remote_id = RemoteMessageId::new("INBOX:2:15");
    let new_c = record(2, "carol@example.com", 5);
    let mut mailbox = EpochMailbox {
        pages: vec![
            history_page(vec![old_a, old_b], "epoch1:2", true, 2),
            // Adapter detected the changed epoch: non-complete rebaseline.
            history_page(Vec::new(), "epoch2:0", true, 2),
            history_page(vec![new_a, new_c], "epoch2:2", false, 2),
        ],
        calls: 0,
    };

    let mut progress = LearningProgress::default();
    for _ in 0..10 {
        progress = learn_slice(&store, &mut mailbox, &context, background_budget())
            .await
            .expect("slice");
        if progress.complete {
            break;
        }
        assert!(progress.paused, "an incomplete slice must report paused");
    }

    assert!(
        progress.complete,
        "history must continue past the rebaseline page"
    );
    assert_eq!(
        evidence_count(&store, &account.id, "alice@example.com"),
        1,
        "the same logical message under a changed epoch must not double its evidence"
    );
    assert_eq!(evidence_count(&store, &account.id, "bob@example.com"), 1);
    assert_eq!(
        evidence_count(&store, &account.id, "carol@example.com"),
        1,
        "the new epoch's new message must be learned"
    );
    assert_eq!(
        store.learned_addresses(&account.id).unwrap().len(),
        4,
        "alice, bob, carol, and the shared sender"
    );
}

#[tokio::test]
async fn new_arrival_during_crawl_is_not_lost() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let mut mailbox = ScriptedMailbox::new(unique_messages(250));
    let budget = WorkBudget::with_timeout(100, 1, Duration::from_secs(10));

    let first = learn_slice(&store, &mut mailbox, &context, budget)
        .await
        .unwrap();
    assert!(!first.complete);

    // A live delta arrives while history is paused. It is learned through the
    // normal sync path, not through history.
    let sync_job = JobKey {
        account: account.id.clone(),
        kind: JobKind::MailSync,
        source: "account".to_string(),
    };
    let arrival = ChangePage {
        changes: vec![MailChange::UpsertMessage {
            message: record(9_999, "newarrival@example.com", 9_999),
            is_new_arrival: true,
        }],
        checkpoint: ProviderCursor::new(1, "sync-1"),
        has_more: false,
    };
    store
        .apply_change_page(
            &context,
            &sync_job,
            IMAP_SMTP_PROVIDER,
            None,
            &arrival,
            None,
        )
        .unwrap();

    let progress = run_to_completion(&store, &mut mailbox, &context).await;

    assert!(progress.complete);
    assert!(
        store
            .learned_address(&account.id, &key("newarrival@example.com"))
            .unwrap()
            .is_some(),
        "the live arrival must be learned and stay learned"
    );
    assert!(
        store
            .learned_address(&account.id, &key("user249@example.com"))
            .unwrap()
            .is_some(),
        "the rest of history still has to be reached"
    );
}

#[tokio::test]
async fn history_does_not_fill_visible_mail_cache() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let mut mailbox = ScriptedMailbox::new(unique_messages(1_205));

    let progress = run_to_completion(&store, &mut mailbox, &context).await;
    assert!(progress.complete);

    assert!(
        store.messages_for_account(&account.id).unwrap().is_empty(),
        "history headers must not enter the visible mailbox cache"
    );
    assert!(
        !store.learned_addresses(&account.id).unwrap().is_empty(),
        "history still trains the evidence ledger"
    );
}

#[tokio::test]
async fn body_and_attachment_calls_remain_zero() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let mut mailbox = ScriptedMailbox::new(unique_messages(1_205));
    let log = mailbox.log();

    let progress = run_to_completion(&store, &mut mailbox, &context).await;
    assert!(progress.complete);

    let log = log.lock().unwrap();
    assert_eq!(log.body_calls, 0, "learning must never fetch a body");
    assert_eq!(
        log.attachment_calls, 0,
        "learning must never fetch an attachment"
    );
}

#[tokio::test]
async fn transient_429_preserves_checkpoint_and_retry_after() {
    let (dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let mut mailbox = ScriptedMailbox::new(unique_messages(250))
        .fail_at(1, ProviderError::rate_limited(Duration::from_secs(5)));

    let error = learn_slice(
        &store,
        &mut mailbox,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(10)),
    )
    .await
    .expect_err("the 429 must be reported");
    assert_eq!(error.kind, ErrorKind::RateLimited);

    let checkpoint = store
        .job_checkpoint(&pige_core::store::history_job(&account.id))
        .unwrap()
        .expect("the first committed page keeps its checkpoint");
    assert_eq!(checkpoint.payload, "e1:100");

    let retry_at = store.history_state(&account.id).unwrap().retry_after_ms;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        retry_at > now + 4_000,
        "the rate-limit delay must be durable, got {retry_at} vs {now}"
    );
    drop(mailbox);
    drop(store);

    // Reopen as a restart would. The delay and checkpoint survive.
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    assert_eq!(
        store.history_state(&account.id).unwrap().retry_after_ms,
        retry_at
    );
    let mut mailbox = ScriptedMailbox::new(unique_messages(250));
    let log = mailbox.log();
    let paused = learn_slice(&store, &mut mailbox, &context, background_budget())
        .await
        .unwrap();
    assert!(paused.paused && !paused.complete);
    assert!(
        log.lock().unwrap().requests.is_empty(),
        "a pending retry must not issue a provider request"
    );
    assert_eq!(
        store
            .job_checkpoint(&pige_core::store::history_job(&account.id))
            .unwrap()
            .expect("checkpoint survives the restart")
            .payload,
        "e1:100"
    );

    // Once the delay elapses the same durable cursor resumes.
    store.clear_history_retry_after(&context).unwrap();
    let progress = run_to_completion(&store, &mut mailbox, &context).await;
    assert!(progress.complete);
    assert_eq!(
        log.lock().unwrap().requests[0].cursor.as_ref(),
        Some(&checkpoint)
    );
}

#[tokio::test]
async fn partial_page_does_not_mark_complete() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    // A short page (25 of a requested 100) that still reports more history,
    // as an adapter does at a mailbox boundary.
    let mut mailbox = ScriptedMailbox::new(unique_messages(150)).max_batch(25);

    let progress = learn_slice(
        &store,
        &mut mailbox,
        &context,
        WorkBudget::with_timeout(100, 1, Duration::from_secs(10)),
    )
    .await
    .unwrap();

    assert!(!progress.complete, "has_more must dominate page length");
    assert!(progress.paused);
    assert_eq!(progress.scanned, 25);
    let checkpoint = store
        .job_checkpoint(&pige_core::store::history_job(&account.id))
        .unwrap()
        .expect("the fetched page checkpoint is committed");
    assert_eq!(checkpoint.payload, "e1:25");
    assert!(
        !store.history_state(&account.id).unwrap().progress.complete,
        "durable progress must not claim completion"
    );

    // A failed next page must leave that checkpoint untouched.
    let mut failing = ScriptedMailbox::new(unique_messages(150))
        .fail_at(0, ProviderError::transient("connection dropped mid-page"));
    let error = learn_slice(&store, &mut failing, &context, background_budget())
        .await
        .expect_err("a failed page is reported");
    assert_eq!(error.kind, ErrorKind::Transient);
    assert_eq!(
        store
            .job_checkpoint(&pige_core::store::history_job(&account.id))
            .unwrap()
            .expect("checkpoint did not advance")
            .payload,
        "e1:25"
    );
}

#[tokio::test]
async fn learning_progress_reflects_active_and_paused() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let store = Arc::new(store);

    let initial = learning_progress(&store, &account.id).unwrap();
    assert!(!initial.complete);
    assert!(
        !initial.paused,
        "a not-yet-started crawl must not read as paused"
    );

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mailbox = ScriptedMailbox::new(unique_messages(1_205)).gate(PageGate {
        entered: entered.clone(),
        release: release.clone(),
    });

    let history = {
        let store = store.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let mut mailbox = mailbox;
            learn_slice(&store, &mut mailbox, &context, background_budget()).await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("history must enter its first page");

    let active = learning_progress(&store, &account.id).unwrap();
    assert!(!active.complete);
    assert!(
        !active.paused,
        "an in-flight slice must be distinguishable from a stopped one"
    );

    release.notify_one();
    let paused = tokio::time::timeout(Duration::from_secs(5), history)
        .await
        .expect("slice must finish its budget")
        .unwrap()
        .unwrap();
    assert!(!paused.complete);
    assert!(paused.paused);
    let durable = learning_progress(&store, &account.id).unwrap();
    assert!(durable.paused, "a budget exit must persist paused");
    assert_eq!(durable.scanned, 500);

    let mut remaining = ScriptedMailbox::new(unique_messages(1_205));
    let done = run_to_completion(&store, &mut remaining, &context).await;
    assert!(done.complete);
    let finished = learning_progress(&store, &account.id).unwrap();
    assert!(finished.complete);
    assert!(!finished.paused, "a complete crawl is not paused");
}

#[tokio::test]
async fn tail_progress_does_not_regress_completion_or_scanned() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    store
        .record_history_progress(
            &context,
            HistoryProgress {
                scanned: 250,
                total_hint: Some(250),
                complete: true,
                paused: false,
            },
        )
        .unwrap();
    // A late StalePage/budget tail write carries older, incomplete progress.
    store
        .record_history_progress(
            &context,
            HistoryProgress {
                scanned: 100,
                total_hint: None,
                complete: false,
                paused: true,
            },
        )
        .unwrap();

    let state = store.history_state(&account.id).unwrap();
    assert!(state.progress.complete, "completion must not be clobbered");
    assert!(!state.progress.paused);
    assert_eq!(state.progress.scanned, 250, "scanned must be monotonic");
    assert_eq!(state.progress.total_hint, Some(250));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_account_slice_is_bounded_and_resumable() {
    let _serial = SLOT_TEST_LOCK.lock().await;
    let (_dir, store) = open_store();
    let account = account_named("foreground");
    store.upsert_account(&account).unwrap();
    let context = ctx(&account.id);

    let mailbox = ScriptedMailbox::new(messages_to(2_605, "bulk@example.com"));
    let first_log = mailbox.log();
    let factory: Arc<dyn ProviderFactory> =
        Arc::new(ScriptedFactory::new(vec![(account.id.clone(), mailbox)]));
    let scheduler = AccountScheduler::new();

    let first = learn_account_slice(
        &store,
        &factory,
        &account,
        &context,
        foreground_budget(),
        &scheduler,
    )
    .await
    .unwrap();
    assert!(!first.complete);
    assert!(first.paused);
    assert_eq!(
        first.scanned, 2_500,
        "foreground stops after its bounded page count"
    );
    assert_eq!(
        first_log.lock().unwrap().requests.len(),
        25,
        "the foreground budget is at most 25 pages"
    );

    let mailbox = ScriptedMailbox::new(messages_to(2_605, "bulk@example.com"));
    let resumed_log = mailbox.log();
    let factory: Arc<dyn ProviderFactory> =
        Arc::new(ScriptedFactory::new(vec![(account.id.clone(), mailbox)]));
    let second = learn_account_slice(
        &store,
        &factory,
        &account,
        &context,
        foreground_budget(),
        &scheduler,
    )
    .await
    .unwrap();

    assert!(second.complete);
    assert_eq!(second.scanned, 2_605);
    assert_eq!(
        resumed_log.lock().unwrap().requests[0]
            .cursor
            .as_ref()
            .map(|cursor| cursor.payload.clone()),
        Some("e1:2500".to_string()),
        "the next foreground call must resume, not restart"
    );
    assert_eq!(
        evidence_count(&store, &account.id, "bulk@example.com"),
        2_605
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_slices_respect_two_account_cap() {
    let _serial = SLOT_TEST_LOCK.lock().await;
    let (_dir, store) = open_store();
    let store = Arc::new(store);
    let first = account_named("cap-one");
    let second = account_named("cap-two");
    let third = account_named("cap-three");
    for account in [&first, &second, &third] {
        store.upsert_account(account).unwrap();
    }

    let entered_one = Arc::new(Notify::new());
    let release_one = Arc::new(Notify::new());
    let entered_two = Arc::new(Notify::new());
    let release_two = Arc::new(Notify::new());
    let mailbox_one = ScriptedMailbox::new(unique_messages(150)).gate(PageGate {
        entered: entered_one.clone(),
        release: release_one.clone(),
    });
    let mailbox_two = ScriptedMailbox::new(unique_messages(150)).gate(PageGate {
        entered: entered_two.clone(),
        release: release_two.clone(),
    });
    let mailbox_three = ScriptedMailbox::new(unique_messages(150));
    let third_log = mailbox_three.log();

    let factory: Arc<dyn ProviderFactory> = Arc::new(ScriptedFactory::new(vec![
        (first.id.clone(), mailbox_one),
        (second.id.clone(), mailbox_two),
        (third.id.clone(), mailbox_three),
    ]));
    let scheduler = Arc::new(AccountScheduler::new());

    let run = |account: Account, context: OperationContext| {
        let store = store.clone();
        let factory = factory.clone();
        let scheduler = scheduler.clone();
        tokio::spawn(async move {
            learn_account_slice(
                &store,
                &factory,
                &account,
                &context,
                foreground_budget(),
                &scheduler,
            )
            .await
        })
    };

    let one = run(first.clone(), ctx(&first.id));
    let two = run(second.clone(), ctx(&second.id));
    tokio::time::timeout(Duration::from_secs(5), entered_one.notified())
        .await
        .expect("first account must hold a slot");
    tokio::time::timeout(Duration::from_secs(5), entered_two.notified())
        .await
        .expect("second account must hold a slot");

    // Both process-wide slots are held. A third account must not open provider
    // work; it reports durable progress and is retried later.
    let blocked = tokio::time::timeout(
        Duration::from_secs(5),
        learn_account_slice(
            &store,
            &factory,
            &third,
            &ctx(&third.id),
            foreground_budget(),
            &scheduler,
        ),
    )
    .await
    .expect("a blocked account must return immediately")
    .unwrap();
    assert!(!blocked.complete);
    assert!(
        third_log.lock().unwrap().requests.is_empty(),
        "the two-account cap must prevent a third provider request"
    );

    release_one.notify_one();
    release_two.notify_one();
    let one = tokio::time::timeout(Duration::from_secs(5), one)
        .await
        .expect("first slice must finish")
        .unwrap()
        .unwrap();
    let two = tokio::time::timeout(Duration::from_secs(5), two)
        .await
        .expect("second slice must finish")
        .unwrap()
        .unwrap();
    assert!(one.complete);
    assert!(two.complete);
}

#[tokio::test]
async fn high_priority_send_preempts_between_pages() {
    let (_dir, store) = open_store();
    let store = Arc::new(store);
    let account = account();
    let context = ctx(&account.id);
    let scheduler = Arc::new(AccountScheduler::new());

    let entered = Arc::new(Notify::new());
    let release_page = Arc::new(Notify::new());
    let mailbox = ScriptedMailbox::new(unique_messages(300)).gate(PageGate {
        entered: entered.clone(),
        release: release_page.clone(),
    });
    let log = mailbox.log();

    let history = {
        let store = store.clone();
        let scheduler = scheduler.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let mut mailbox = mailbox;
            learn_slice_paged(
                &store,
                &mut mailbox,
                &context,
                WorkBudget::with_timeout(100, 5, Duration::from_secs(10)),
                &scheduler,
            )
            .await
        })
    };

    // History holds the gate inside the first page fetch.
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("history must enter its first page");

    // An interactive send queues behind it. It must get the gate at the page
    // boundary instead of waiting for the whole history slice.
    let send_acquired = Arc::new(Notify::new());
    let release_send = Arc::new(Notify::new());
    let send = {
        let scheduler = scheduler.clone();
        let send_ctx = context.clone();
        let send_acquired = send_acquired.clone();
        let release_send = release_send.clone();
        tokio::spawn(async move {
            scheduler
                .run_interactive(&send_ctx, async move {
                    send_acquired.notify_one();
                    release_send.notified().await;
                })
                .await
        })
    };
    // Let the send actually queue on the account gate.
    tokio::task::yield_now().await;
    release_page.notify_one();

    tokio::time::timeout(Duration::from_secs(5), send_acquired.notified())
        .await
        .expect("the interactive send must acquire the gate at the page boundary");
    let progress = tokio::time::timeout(Duration::from_secs(5), history)
        .await
        .expect("history must return instead of waiting for the gate")
        .unwrap()
        .expect("history slice completes");
    assert!(progress.paused && !progress.complete);
    assert_eq!(progress.scanned, 100, "only the completed page is durable");
    assert_eq!(
        log.lock().unwrap().requests.len(),
        1,
        "history must not fetch the next page while a send owns the gate"
    );

    release_send.notify_one();
    send.await.unwrap().unwrap();

    // The next slice resumes from the same durable checkpoint.
    let mut mailbox = ScriptedMailbox::new(unique_messages(300));
    let resumed_log = mailbox.log();
    let progress = run_to_completion(&store, &mut mailbox, &context).await;
    assert!(progress.complete);
    assert_eq!(
        resumed_log.lock().unwrap().requests[0]
            .cursor
            .as_ref()
            .map(|cursor| cursor.payload.clone()),
        Some("e1:100".to_string())
    );
}
