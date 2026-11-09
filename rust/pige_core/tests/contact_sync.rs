//! Task R6: generic contact synchronization against `AddressBookService`.
//!
//! Contact pages are source-scoped and written atomically with their cursor.
//! A complete snapshot generation sweeps only contacts absent from that
//! generation; an interrupted or failed snapshot never does. Tombstones remove
//! one remote contact from its source and never touch mail-derived evidence or
//! another source. Every fixture address is `example.com`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pige_core::Store;
use pige_core::account::{Account, AccountId, Security, ServerConfig};
use pige_core::api::{self, Config, DB_FILE_NAME};
use pige_core::contacts::normalize_address;
use pige_core::contacts::sync::sync_slice;
use pige_core::mail::{Address, Flags, MessageHeader};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::providers::fake::{FakeProviderFactory, FakeProviderSession, FakeServices};
use pige_core::providers::{
    BodyContent, ChangePage, CollectionRecord, ContactCapabilities, ContactChange, ContactPage,
    ContactRecord, ContactSourceId, ErrorKind, HistoryCoverage, HistoryPage, HistoryRequest,
    MailboxService, MessageRecord, OperationContext, ProviderCapabilities, ProviderCursor,
    ProviderError, ProviderFactory, ProviderSession, RemoteAttachmentId, RemoteContactId,
    RemoteMessageId, SyncRequest, SyncService,
};
use pige_core::store::contacts_job;
use pige_core::sync::provider::WorkBudget;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// Serializes tests that install the process-wide API state.
static API_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

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
    account_named("account-r6", "r6@example.com")
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

fn source(id: &str) -> ContactSourceId {
    ContactSourceId::new(id)
}

fn contact_change(remote: &str, name: Option<&str>, emails: &[&str]) -> ContactChange {
    ContactChange::Upsert(ContactRecord {
        remote_id: RemoteContactId::new(remote),
        version: None,
        names: name.into_iter().map(str::to_string).collect(),
        emails: emails
            .iter()
            .map(|email| Address::new(name.map(str::to_string), *email))
            .collect(),
    })
}

fn page(changes: Vec<ContactChange>, checkpoint: &str, has_more: bool) -> ContactPage {
    ContactPage {
        changes,
        checkpoint: ProviderCursor::new(1, checkpoint),
        has_more,
    }
}

fn contact_emails(store: &Store, account: &AccountId, source: &ContactSourceId) -> Vec<String> {
    store
        .contact_emails(account)
        .unwrap()
        .into_iter()
        .filter(|contact| &contact.source == source)
        .map(|contact| contact.address.email)
        .collect()
}

fn has_contact(store: &Store, account: &AccountId, source: &ContactSourceId, email: &str) -> bool {
    contact_emails(store, account, source)
        .iter()
        .any(|cached| cached == email)
}

fn history_page(recipient: &str, checkpoint: &str) -> HistoryPage {
    let header = MessageHeader {
        message_id: Some("<contact-sync-history@example.com>".to_string()),
        in_reply_to: None,
        references: Vec::new(),
        subject: "History".to_string(),
        from: vec![Address::new(None, "sender@example.com")],
        to: vec![Address::new(None, recipient)],
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: 1_700_000_000,
        size: 64,
    };
    HistoryPage {
        messages: vec![MessageRecord {
            remote_id: RemoteMessageId::new("INBOX:1:1"),
            remote_thread: None,
            header,
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

fn learned(store: &Store, account: &AccountId, email: &str) -> bool {
    let key = normalize_address(&Address::new(None, email)).unwrap();
    store.learned_address(account, &key).unwrap().is_some()
}

#[derive(Default)]
struct ContactLog {
    requests: Vec<(ContactSourceId, Option<ProviderCursor>)>,
}

/// A cursor/script driven fake `AddressBookService`. Pages are consumed in call
/// order, exactly like an adapter resuming from a checkpoint the caller passes.
struct ScriptedContacts {
    pages: Vec<ContactPage>,
    /// Failures keyed by zero-based call index.
    failures: HashMap<usize, ProviderError>,
    /// Call index that waits until the operation is cancelled.
    cancel_at: Option<usize>,
    entered: Option<Arc<Notify>>,
    log: Arc<Mutex<ContactLog>>,
    calls: usize,
}

impl ScriptedContacts {
    fn new(pages: Vec<ContactPage>) -> Self {
        Self {
            pages,
            failures: HashMap::new(),
            cancel_at: None,
            entered: None,
            log: Arc::new(Mutex::new(ContactLog::default())),
            calls: 0,
        }
    }

    fn fail_at(mut self, call: usize, error: ProviderError) -> Self {
        self.failures.insert(call, error);
        self
    }

    fn log(&self) -> Arc<Mutex<ContactLog>> {
        self.log.clone()
    }
}

#[async_trait]
impl pige_core::providers::AddressBookService for ScriptedContacts {
    async fn next_page(
        &mut self,
        source: &ContactSourceId,
        cursor: Option<ProviderCursor>,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        let call = self.calls;
        self.calls += 1;
        self.log
            .lock()
            .unwrap()
            .requests
            .push((source.clone(), cursor));

        if self.cancel_at == Some(call) {
            if let Some(entered) = &self.entered {
                entered.notify_one();
            }
            tokio::select! {
                _ = ctx.cancellation.cancelled() => {
                    return Err(ProviderError::cancelled("worker stopped"));
                }
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    return Err(ProviderError::transient("cancel gate timed out"));
                }
            }
        }

        ctx.ensure_active()?;
        if let Some(error) = self.failures.remove(&call) {
            return Err(error);
        }
        self.pages
            .get(call)
            .cloned()
            .ok_or_else(|| ProviderError::transient("scripted contact pages exhausted"))
    }
}

// ---------------------------------------------------------------------------
// Sync tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn contact_full_sync_sweeps_only_after_last_page() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let source = source("saved");

    store
        .upsert_contact_source(&account.id, &source, Some("Address book"))
        .unwrap();
    store
        .upsert_contact_email(
            &account.id,
            &source,
            &RemoteContactId::new("stale"),
            &Address::new(None, "stale@example.com"),
        )
        .unwrap();
    store
        .upsert_contact_email(
            &account.id,
            &source,
            &RemoteContactId::new("keep"),
            &Address::new(None, "keep@example.com"),
        )
        .unwrap();

    let first = page(
        vec![contact_change(
            "keep",
            Some("Keep Me"),
            &["keep@example.com"],
        )],
        "p1",
        true,
    );
    let second = page(
        vec![contact_change("new", None, &["new@example.com"])],
        "p2",
        false,
    );

    // Page one commits; page two fails. Nothing may be swept yet.
    let mut failing = ScriptedContacts::new(vec![first.clone(), second.clone()])
        .fail_at(1, ProviderError::transient("page two dropped"));
    let error = sync_slice(
        &store,
        &mut failing,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .expect_err("the failed page must be reported");
    assert_eq!(error.kind, ErrorKind::Transient);
    assert!(
        has_contact(&store, &account.id, &source, "stale@example.com"),
        "a failed snapshot must not sweep the previous cache"
    );
    assert!(has_contact(
        &store,
        &account.id,
        &source,
        "keep@example.com"
    ));
    assert!(!has_contact(
        &store,
        &account.id,
        &source,
        "new@example.com"
    ));
    let checkpoint = store
        .job_checkpoint(&contacts_job(&account.id, &source))
        .unwrap()
        .expect("page one's checkpoint is durable");
    assert_eq!(checkpoint.payload, "p1");

    // A fresh session resumes page two; only now is the snapshot complete and
    // the absent previous contact swept.
    let mut resumed = ScriptedContacts::new(vec![second]);
    let progress = sync_slice(
        &store,
        &mut resumed,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(progress.complete);
    assert!(
        !has_contact(&store, &account.id, &source, "stale@example.com"),
        "the completed snapshot generation sweeps absent contacts"
    );
    assert!(has_contact(
        &store,
        &account.id,
        &source,
        "keep@example.com"
    ));
    assert!(has_contact(&store, &account.id, &source, "new@example.com"));
    let final_cursor = store
        .job_checkpoint(&contacts_job(&account.id, &source))
        .unwrap()
        .expect("the completed snapshot keeps its final cursor for diagnostics");
    assert_eq!(final_cursor.payload, "p2");
    let state = store.contact_source_state(&account.id, &source).unwrap();
    assert!(state.complete);
    assert!(!state.snapshot_active);

    // The next slice is a new generation and must not resume the completed
    // cursor; its first request is from no cursor.
    let mut next = ScriptedContacts::new(vec![page(Vec::new(), "p3", false)]);
    let next_log = next.log();
    let progress = sync_slice(
        &store,
        &mut next,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(progress.complete);
    assert_eq!(
        next_log.lock().unwrap().requests[0].1,
        None,
        "a completed snapshot must start a fresh generation, not resume its final cursor"
    );
}

#[tokio::test]
async fn contact_tombstone_leaves_history_evidence() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let source = source("saved");
    let shared = "shared@example.com";

    // The same address is both a synced contact and a learned correspondent.
    store
        .apply_history_page(&context, None, &history_page(shared, "h1"))
        .unwrap();
    assert!(learned(&store, &account.id, shared));

    store
        .upsert_contact_source(&account.id, &source, Some("Address book"))
        .unwrap();
    store
        .upsert_contact_email(
            &account.id,
            &source,
            &RemoteContactId::new("gone"),
            &Address::new(Some("Shared Person".to_string()), shared),
        )
        .unwrap();

    let mut service = ScriptedContacts::new(vec![page(
        vec![ContactChange::Delete(RemoteContactId::new("gone"))],
        "p1",
        false,
    )]);
    let progress = sync_slice(
        &store,
        &mut service,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(progress.complete);

    assert!(
        !has_contact(&store, &account.id, &source, shared),
        "the tombstone removes the contact source row"
    );
    assert!(
        learned(&store, &account.id, shared),
        "a contact tombstone must never delete learned mail evidence"
    );
    let key = normalize_address(&Address::new(None, shared)).unwrap();
    let candidates = store
        .recipient_candidates(&account.id, shared, &[shared.to_string()], 64)
        .unwrap();
    assert!(
        candidates.iter().any(|candidate| candidate.key == key),
        "the address stays suggestible from its independent learned evidence"
    );
}

#[tokio::test]
async fn contacts_forbidden_disables_only_that_source() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let denied = source("denied");
    let allowed = source("allowed");

    store
        .upsert_contact_source(&account.id, &denied, Some("Denied book"))
        .unwrap();
    store
        .upsert_contact_email(
            &account.id,
            &denied,
            &RemoteContactId::new("existing"),
            &Address::new(None, "existing@example.com"),
        )
        .unwrap();
    store
        .apply_history_page(&context, None, &history_page("history@example.com", "h1"))
        .unwrap();

    let mut forbidden =
        ScriptedContacts::new(Vec::new()).fail_at(0, ProviderError::forbidden("no contacts scope"));
    let error = sync_slice(
        &store,
        &mut forbidden,
        &denied,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .expect_err("forbidden contacts are reported");
    assert_eq!(error.kind, ErrorKind::Forbidden);

    let denied_state = store.contact_source_state(&account.id, &denied).unwrap();
    assert!(
        !denied_state.enabled,
        "a permission denial disables only that source"
    );
    assert!(
        has_contact(&store, &account.id, &denied, "existing@example.com"),
        "disabling a source keeps its already-cached local contacts"
    );
    assert_eq!(
        store
            .job_checkpoint(&contacts_job(&account.id, &denied))
            .unwrap(),
        None,
        "disabling cancels the source's resumable job"
    );

    let mut working = ScriptedContacts::new(vec![page(
        vec![contact_change("allowed-1", None, &["allowed@example.com"])],
        "a1",
        false,
    )]);
    let progress = sync_slice(
        &store,
        &mut working,
        &allowed,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(progress.complete);
    assert!(
        store
            .contact_source_state(&account.id, &allowed)
            .unwrap()
            .enabled
    );
    assert!(has_contact(
        &store,
        &account.id,
        &allowed,
        "allowed@example.com"
    ));
    assert!(
        learned(&store, &account.id, "history@example.com"),
        "a contacts permission failure never stops mail-derived suggestions"
    );
}

#[tokio::test]
async fn independent_contact_source_works_without_mailbox() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let source = source("saved");

    let fake = FakeProviderFactory::new()
        .services(FakeServices {
            address_book: true,
            ..FakeServices::NONE
        })
        .session(FakeProviderSession::new().contact_page(page(
            vec![contact_change(
                "independent",
                Some("Independent"),
                &["independent@example.com"],
            )],
            "c1",
            false,
        )));
    let factory: &dyn ProviderFactory = &fake;
    let mut session = factory.open(&account, &context).await.unwrap();
    assert!(
        session.mailbox.is_none() && session.sync.is_none(),
        "the fixture must have no mailbox or mail sync service"
    );
    let service = session.address_book_service().unwrap();

    let progress = sync_slice(
        &store,
        service.as_mut(),
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(progress.complete);
    assert!(has_contact(
        &store,
        &account.id,
        &source,
        "independent@example.com"
    ));
    assert!(
        store.messages_for_account(&account.id).unwrap().is_empty(),
        "contacts must not require or populate the visible mail cache"
    );
}

#[tokio::test]
async fn background_deadline_returns_with_checkpoint() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let source = source("saved");

    let first = page(
        vec![contact_change("one", None, &["one@example.com"])],
        "p1",
        true,
    );
    let second = page(
        vec![contact_change("two", None, &["two@example.com"])],
        "p2",
        false,
    );
    let mut service = ScriptedContacts::new(vec![first, second.clone()]);
    let progress = sync_slice(
        &store,
        &mut service,
        &source,
        &context,
        WorkBudget::with_timeout(100, 1, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(!progress.complete);
    assert!(progress.paused);
    assert_eq!(progress.scanned, 1);
    let checkpoint = store
        .job_checkpoint(&contacts_job(&account.id, &source))
        .unwrap()
        .expect("the completed page is durable");
    assert_eq!(checkpoint.payload, "p1");

    // A slice whose wall-clock deadline has already passed does no network work
    // and leaves the checkpoint exactly where it was.
    let mut idle = ScriptedContacts::new(vec![second.clone()]);
    let idle_log = idle.log();
    let paused = sync_slice(
        &store,
        &mut idle,
        &source,
        &context,
        WorkBudget::new(100, 5, tokio::time::Instant::now()),
    )
    .await
    .unwrap();
    assert!(!paused.complete);
    assert!(paused.paused);
    assert_eq!(paused.scanned, 1);
    assert!(
        idle_log.lock().unwrap().requests.is_empty(),
        "an expired deadline must not issue a provider request"
    );
    assert_eq!(
        store
            .job_checkpoint(&contacts_job(&account.id, &source))
            .unwrap()
            .expect("checkpoint survives a deadline exit")
            .payload,
        "p1"
    );

    // A later slice resumes from that checkpoint and completes the snapshot.
    let mut resumed = ScriptedContacts::new(vec![second]);
    let resumed_log = resumed.log();
    let done = sync_slice(
        &store,
        &mut resumed,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(done.complete);
    assert_eq!(
        resumed_log.lock().unwrap().requests[0].1.as_ref(),
        Some(&checkpoint),
        "the resumed slice must send the durable cursor"
    );
    assert!(has_contact(&store, &account.id, &source, "two@example.com"));
}

// ---------------------------------------------------------------------------
// Process-wide API tests: bounded maintenance and restarts
// ---------------------------------------------------------------------------

struct ApiHarness {
    dir: TempDir,
    providers: Arc<dyn ProviderFactory>,
}

impl ApiHarness {
    fn new(providers: Arc<dyn ProviderFactory>) -> Self {
        let harness = Self {
            dir: TempDir::new().unwrap(),
            providers,
        };
        harness.install();
        harness
    }

    fn install(&self) {
        api::reset_for_tests();
        let credentials = Arc::new(FakeCredentials::default());
        let transport: Arc<dyn pige_core::platform::TransportFactory> =
            Arc::new(FakeTransportFactory::default());
        api::init(Config {
            data_dir: self.dir.path().join("data"),
            cache_dir: self.dir.path().join("cache"),
            config_dir: self.dir.path().join("config"),
            credentials,
            notifier: Arc::new(FakeNotifier::default()),
            opener: Arc::new(FakeOpener::default()),
            providers: self.providers.clone(),
            transport,
            backups: Arc::new(FakeBackupStore::new()),
            auth: None,
        })
        .expect("init api");
    }

    fn restart(&self) {
        api::reset_for_tests();
        self.install();
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
            label: "Contacts".to_string(),
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

/// A provider factory that only has an address book. The first page commits,
/// the second waits for the maintenance cancellation token.
struct GatedFactory {
    /// Shared by every opened session, like one adapter's connection state.
    calls: Arc<Mutex<usize>>,
    entered: Arc<Notify>,
}

impl GatedFactory {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(0)),
            entered: Arc::new(Notify::new()),
        }
    }
}

struct GatedAddressBook {
    calls: Arc<Mutex<usize>>,
    entered: Arc<Notify>,
}

#[async_trait]
impl ProviderFactory for GatedFactory {
    async fn open(
        &self,
        _account: &Account,
        _ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        let capabilities = ProviderCapabilities {
            contacts: ContactCapabilities {
                read: true,
                incremental_sync: true,
                ..ContactCapabilities::default()
            },
            ..ProviderCapabilities::default()
        };
        let mut session = ProviderSession::new(capabilities);
        session.address_book = Some(Box::new(GatedAddressBook {
            calls: self.calls.clone(),
            entered: self.entered.clone(),
        }));
        Ok(session)
    }
}

#[async_trait]
impl pige_core::providers::AddressBookService for GatedAddressBook {
    async fn next_page(
        &mut self,
        _source: &ContactSourceId,
        _cursor: Option<ProviderCursor>,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            let call = *calls;
            *calls += 1;
            call
        };
        if call == 0 {
            return Ok(page(
                vec![contact_change("first", None, &["first@example.com"])],
                "c1",
                true,
            ));
        }
        self.entered.notify_one();
        tokio::select! {
            _ = ctx.cancellation.cancelled() => Err(ProviderError::cancelled("worker stopped")),
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                Err(ProviderError::transient("cancel gate timed out"))
            }
        }
    }
}

#[tokio::test]
async fn worker_cancel_stops_native_slice() {
    let _guard = API_LOCK.lock().await;
    let factory = Arc::new(GatedFactory::new());
    let entered = factory.entered.clone();
    let harness = ApiHarness::new(factory);
    let account = harness.add_account().await;
    let source = source("saved");
    api::set_contact_sync(&account, &source, true).expect("enable contacts");

    let run = tokio::spawn(async move { api::run_maintenance(Duration::from_secs(20)).await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("maintenance must reach the contact page");
    api::cancel_maintenance();
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("cancellation must stop the native slice")
        .expect("maintenance task must not panic");
    assert!(result.is_ok(), "a WorkManager stop is a normal slice exit");

    let store = harness.open_store();
    assert_eq!(
        store
            .job_checkpoint(&contacts_job(&account, &source))
            .unwrap()
            .expect("the completed first page keeps its checkpoint")
            .payload,
        "c1"
    );
    assert!(
        has_contact(&store, &account, &source, "first@example.com"),
        "the committed page survives cancellation"
    );
}

#[tokio::test]
async fn disabling_contact_sync_keeps_cached_contacts() {
    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new(Arc::new(FakeProviderFactory::new()));
    let account = harness.add_account().await;
    let source = source("saved");

    {
        let store = harness.open_store();
        store
            .upsert_contact_source(&account, &source, Some("Address book"))
            .unwrap();
        store
            .upsert_contact_email(
                &account,
                &source,
                &RemoteContactId::new("kept"),
                &Address::new(None, "kept@example.com"),
            )
            .unwrap();
        store
            .set_checkpoint(
                &contacts_job(&account, &source),
                1,
                &ProviderCursor::new(1, "in-flight"),
            )
            .unwrap();
    }

    api::set_contact_sync(&account, &source, true).expect("enable contacts");
    api::set_contact_sync(&account, &source, false).expect("disable contacts");

    let store = harness.open_store();
    assert!(
        has_contact(&store, &account, &source, "kept@example.com"),
        "disabling keeps existing local source data"
    );
    assert_eq!(
        store
            .job_checkpoint(&contacts_job(&account, &source))
            .unwrap(),
        None,
        "disabling cancels the resumable job"
    );
    assert!(
        !store
            .contact_source_state(&account, &source)
            .unwrap()
            .enabled
    );
    let hits = api::suggest_recipients(api::RecipientQuery {
        sending_account: account,
        text: "kept".to_string(),
        limit: 10,
    })
    .expect("suggest cached contact");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].address.email, "kept@example.com");
}

#[tokio::test]
async fn contacts_are_usable_offline_after_restart() {    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new(Arc::new(FakeProviderFactory::new()));
    let account = harness.add_account().await;
    let source = source("saved");

    // Sync contacts into the persistent cache, then simulate the process being
    // killed and started again with no provider available.
    {
        let store = harness.open_store();
        let fake = FakeProviderFactory::new()
            .services(FakeServices {
                address_book: true,
                ..FakeServices::NONE
            })
            .session(FakeProviderSession::new().contact_page(page(
                vec![contact_change(
                    "offline",
                    Some("Offline Contact"),
                    &["offline@example.com"],
                )],
                "c1",
                false,
            )));
        let actual = account_named(&account.0, "ada@example.com");
        let mut session = fake.open(&actual, &ctx(&account)).await.unwrap();
        let service = session.address_book_service().unwrap();
        sync_slice(
            &store,
            service.as_mut(),
            &source,
            &ctx(&account),
            WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
        )
        .await
        .unwrap();
    }

    harness.restart();

    let hits = api::suggest_recipients(api::RecipientQuery {
        sending_account: account.clone(),
        text: "offline".to_string(),
        limit: 10,
    })
    .expect("suggest offline contacts");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].address.email, "offline@example.com");
    assert!(
        hits[0]
            .sources
            .contains(&pige_core::contacts::SuggestionSource::SavedContact),
        "the restarted app must still know this address came from a saved address book"
    );
}

// ---------------------------------------------------------------------------
// Maintenance budget/cancellation regressions
// ---------------------------------------------------------------------------

/// A mail-sync service that blocks in the provider call until the operation is
/// cancelled or its own deadline expires. This is how the tests observe whether
/// the maintenance token/deadline actually reached the mail phase.
struct BlockingSync {
    entered: Arc<Notify>,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl SyncService for BlockingSync {
    async fn next_page(
        &mut self,
        _request: SyncRequest,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        self.entered.notify_one();
        tokio::select! {
            _ = ctx.cancellation.cancelled() => Err(ProviderError::cancelled("worker stopped")),
            _ = tokio::time::sleep_until(ctx.deadline) => {
                Err(ProviderError::transient("mail pass deadline"))
            }
        }
    }
}

struct BlockingMailFactory {
    entered: Arc<Notify>,
    calls: Arc<Mutex<usize>>,
}

impl BlockingMailFactory {
    fn new() -> Self {
        Self {
            entered: Arc::new(Notify::new()),
            calls: Arc::new(Mutex::new(0)),
        }
    }
}

#[async_trait]
impl ProviderFactory for BlockingMailFactory {
    async fn open(
        &self,
        _account: &Account,
        _ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        let capabilities = ProviderCapabilities {
            sync: true,
            ..ProviderCapabilities::default()
        };
        let mut session = ProviderSession::new(capabilities);
        session.sync = Some(Box::new(BlockingSync {
            entered: self.entered.clone(),
            calls: self.calls.clone(),
        }));
        Ok(session)
    }
}

#[tokio::test]
async fn maintenance_deadline_bounds_mail_phase() {
    let _guard = API_LOCK.lock().await;
    let factory = Arc::new(BlockingMailFactory::new());
    let entered = factory.entered.clone();
    let calls = factory.calls.clone();
    let harness = ApiHarness::new(factory);
    let _account = harness.add_account().await;

    let run = tokio::spawn(async move { api::run_maintenance(Duration::from_millis(120)).await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("the mail phase must actually start");
    let result = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("the pass deadline must bound the mail phase");
    assert!(
        result.is_ok(),
        "a deadline stop is a normal pass exit"
    );
    assert_eq!(*calls.lock().unwrap(), 1, "the mail call was reached once");
}

#[tokio::test]
async fn maintenance_cancel_reaches_mail_phase() {
    let _guard = API_LOCK.lock().await;
    let factory = Arc::new(BlockingMailFactory::new());
    let entered = factory.entered.clone();
    let calls = factory.calls.clone();
    let harness = ApiHarness::new(factory);
    let _account = harness.add_account().await;

    let run = tokio::spawn(async move { api::run_maintenance(Duration::from_secs(20)).await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("the mail phase must actually start");
    api::cancel_maintenance();
    let result = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("a WorkManager stop must reach the in-flight mail call");
    assert!(
        result.is_ok(),
        "a WorkManager stop is a normal pass exit"
    );
    assert_eq!(*calls.lock().unwrap(), 1, "the mail call was reached once");
}

#[tokio::test]
async fn maintenance_stop_before_token_install_is_honored() {
    let _guard = API_LOCK.lock().await;
    let factory = Arc::new(BlockingMailFactory::new());
    let calls = factory.calls.clone();
    let harness = ApiHarness::new(factory);
    let _account = harness.add_account().await;

    // WorkManager stops the worker before the native pass installs its token.
    api::cancel_maintenance();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        api::run_maintenance(Duration::from_secs(20)),
    )
    .await
    .expect("a pre-install stop must not wait for a full pass");
    assert!(
        result.is_ok(),
        "a pre-install stop is a normal pass exit"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "a stopped pass must not start the mail phase"
    );
}

/// Slow history: sleeps until the budget it was actually given, so a pass that
/// hands history the whole deadline starves contacts.
struct SlowHistory {
    entered: Arc<Notify>,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl MailboxService for SlowHistory {
    async fn collections(
        &mut self,
        _ctx: &OperationContext,
    ) -> Result<Vec<CollectionRecord>, ProviderError> {
        Err(ProviderError::unsupported("collections"))
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
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        self.entered.notify_one();
        tokio::time::sleep_until(ctx.deadline).await;
        Err(ProviderError::transient("slow history"))
    }
}

struct OnePageContacts {
    entered: Arc<Notify>,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl pige_core::providers::AddressBookService for OnePageContacts {
    async fn next_page(
        &mut self,
        _source: &ContactSourceId,
        _cursor: Option<ProviderCursor>,
        _ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        self.entered.notify_one();
        Ok(page(
            vec![contact_change(
                "share",
                None,
                &["share@example.com"],
            )],
            "c1",
            false,
        ))
    }
}

struct SlowHistoryContactsFactory {
    history_entered: Arc<Notify>,
    history_calls: Arc<Mutex<usize>>,
    contact_entered: Arc<Notify>,
    contact_calls: Arc<Mutex<usize>>,
}

impl SlowHistoryContactsFactory {
    fn new() -> Self {
        Self {
            history_entered: Arc::new(Notify::new()),
            history_calls: Arc::new(Mutex::new(0)),
            contact_entered: Arc::new(Notify::new()),
            contact_calls: Arc::new(Mutex::new(0)),
        }
    }
}

#[async_trait]
impl ProviderFactory for SlowHistoryContactsFactory {
    async fn open(
        &self,
        _account: &Account,
        _ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        let capabilities = ProviderCapabilities {
            mailbox: true,
            contacts: ContactCapabilities {
                read: true,
                incremental_sync: true,
                ..ContactCapabilities::default()
            },
            ..ProviderCapabilities::default()
        };
        let mut session = ProviderSession::new(capabilities);
        session.mailbox = Some(Box::new(SlowHistory {
            entered: self.history_entered.clone(),
            calls: self.history_calls.clone(),
        }));
        session.address_book = Some(Box::new(OnePageContacts {
            entered: self.contact_entered.clone(),
            calls: self.contact_calls.clone(),
        }));
        Ok(session)
    }
}

#[tokio::test]
async fn contacts_get_budget_even_when_history_is_slow() {
    let _guard = API_LOCK.lock().await;
    let factory = Arc::new(SlowHistoryContactsFactory::new());
    let history_entered = factory.history_entered.clone();
    let history_calls = factory.history_calls.clone();
    let contact_calls = factory.contact_calls.clone();
    let harness = ApiHarness::new(factory);
    let account = harness.add_account().await;
    let source = source("saved");
    api::set_contact_sync(&account, &source, true).expect("enable contacts");

    let run = tokio::spawn(async move { api::run_maintenance(Duration::from_millis(300)).await });
    tokio::time::timeout(Duration::from_secs(1), history_entered.notified())
        .await
        .expect("history must run before contacts");
    let result = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("slow history must not consume the contact share");
    assert!(
        result.is_ok(),
        "a budget exit is a normal pass exit"
    );
    assert!(*history_calls.lock().unwrap() > 0, "history ran");
    assert!(
        *contact_calls.lock().unwrap() > 0,
        "contacts must still receive a slice after slow history"
    );

    let store = harness.open_store();
    assert!(
        has_contact(&store, &account, &source, "share@example.com"),
        "the contact page committed despite the slow history phase"
    );
}

#[tokio::test]
async fn contact_refresh_preserves_completed_until_first_page() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let source = source("saved");

    store
        .upsert_contact_source(&account.id, &source, Some("Address book"))
        .unwrap();
    let mut initial = ScriptedContacts::new(vec![page(
        vec![contact_change("one", None, &["one@example.com"])],
        "c1",
        false,
    )]);
    sync_slice(
        &store,
        &mut initial,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(store.contact_source_state(&account.id, &source).unwrap().complete);

    // Starting the next generation must not flip a synced source to "paused"
    // before any page of the refresh has committed.
    let start = store.contact_sync_start(&context, &source).unwrap();
    assert!(start.complete, "the prior completed snapshot stays visible");
    let active = store.contact_source_state(&account.id, &source).unwrap();
    assert!(active.complete && active.snapshot_active);
    assert_eq!(start.generation, active.snapshot_generation);

    // A second start before the first page resumes the same generation instead
    // of allocating another one.
    let again = store.contact_sync_start(&context, &source).unwrap();
    assert_eq!(again.generation, start.generation);
    assert!(again.complete);

    // The first committed page is the point where progress becomes in-flight.
    let mut partial = ScriptedContacts::new(vec![page(
        vec![contact_change("two", None, &["two@example.com"])],
        "c2",
        true,
    )]);
    let progress = sync_slice(
        &store,
        &mut partial,
        &source,
        &context,
        WorkBudget::with_timeout(100, 1, Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(!progress.complete);
    assert!(!store.contact_source_state(&account.id, &source).unwrap().complete);
}
