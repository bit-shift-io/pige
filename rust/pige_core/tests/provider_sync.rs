//! Task F7: transactional provider sync and serialized account scheduling.
//!
//! These tests drive the provider-neutral orchestration in
//! [`pige_core::sync::provider`] with deliberately controlled ordering: a
//! scripted `SyncService` can pause between a page's read and its commit so a
//! concurrent mutation or account removal can interleave, and the store's
//! generation + checkpoint compare-and-swap must discard the stale page.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pige_core::account::{Account, AccountId, Security, ServerConfig};
use pige_core::auth::CredentialSecretRepository;
use pige_core::mail::{Address, Flags, Folder, FolderKind, MessageHeader, StoredMessage};
use pige_core::platform::CredentialStore;
use pige_core::platform::fake::{FakeCredentials, FakeTransportFactory};
use pige_core::providers::imap::ImapProviderFactory;
use pige_core::providers::imap::sync::decode_sync_cursor;
use pige_core::providers::{
    ChangePage, GMAIL_PROVIDER, IMAP_SMTP_PROVIDER, MailChange, MessageRecord, OperationContext,
    ProviderCapabilities, ProviderCursor, ProviderError, ProviderFactory, ProviderId,
    ProviderSession, RemoteCollectionId, RemoteMessageId, SyncRequest, SyncService,
};
use pige_core::sync::SyncError;
use pige_core::sync::provider::{self, WorkBudget};
use pige_core::sync::scheduler::{AccountScheduler, SchedulerError};
use pige_core::transport::{FakeTransport, FolderState, RemoteFolder, RemoteMessage};
use pige_core::{FolderId, MessageId, Store, StoreError, ThreadId, Uid};
use tempfile::TempDir;
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn imap_account(id: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: format!("{id}@example.com"),
        display_name: id.to_string(),
        backend: pige_core::MailBackend::imap_smtp(
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

/// A Gmail-typed account that is nevertheless still served by the IMAP
/// transport (a migrated Gmail-over-IMAP setup). Used to prove page application
/// is keyed by the session's transport, not the configured backend.
fn gmail_account(id: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: format!("{id}@example.com"),
        display_name: id.to_string(),
        backend: pige_core::MailBackend::gmail(id),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn open_store(account: &Account) -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    store.upsert_account(account).unwrap();
    (dir, store)
}

fn ctx(account: &AccountId, generation: u64) -> OperationContext {
    OperationContext::with_timeout(account.clone(), generation, Duration::from_secs(5))
}

fn budget(max_pages: u32) -> WorkBudget {
    budget_with(max_pages, 50)
}

fn budget_with(max_pages: u32, page_size: u32) -> WorkBudget {
    WorkBudget {
        page_size,
        max_pages,
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
    }
}

fn header(subject: &str) -> MessageHeader {
    MessageHeader {
        message_id: Some(format!("<{subject}@example.com>")),
        in_reply_to: None,
        references: Vec::new(),
        subject: subject.to_string(),
        from: vec![Address::new(None, "sender@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: 1_700_000_000,
        size: 64,
    }
}

fn collection(
    remote: &str,
    name: &str,
    kind: FolderKind,
) -> pige_core::providers::CollectionRecord {
    pige_core::providers::CollectionRecord {
        remote_id: RemoteCollectionId::new(remote),
        name: name.to_string(),
        kind,
        delimiter: Some("/".to_string()),
        total: None,
        unread: None,
    }
}

fn message_change(
    remote: &str,
    collection_remote: &str,
    subject: &str,
    is_new_arrival: bool,
    flags: Flags,
) -> MailChange {
    MailChange::UpsertMessage {
        message: MessageRecord {
            remote_id: RemoteMessageId::new(remote),
            remote_thread: None,
            header: header(subject),
            flags,
            collections: vec![RemoteCollectionId::new(collection_remote)],
            snippet: String::new(),
            has_attachments: false,
        },
        is_new_arrival,
    }
}

// ---------------------------------------------------------------------------
// A scripted SyncService that can pause between read and commit
// ---------------------------------------------------------------------------

struct ScriptedSync {
    /// Cursor the request must carry to select this page (`None` matches a
    /// first/reset request).
    pages: Vec<(Option<ProviderCursor>, ChangePage)>,
    /// Return [`ErrorKind::CursorInvalid`] when the request carries this cursor.
    reject_cursor: Option<ProviderCursor>,
    requests: Arc<Mutex<Vec<SyncRequest>>>,
    entered: Option<Arc<Notify>>,
    release: Option<Arc<Notify>>,
}

impl ScriptedSync {
    fn pages(pages: Vec<(Option<ProviderCursor>, ChangePage)>) -> Self {
        Self {
            pages,
            reject_cursor: None,
            requests: Arc::new(Mutex::new(Vec::new())),
            entered: None,
            release: None,
        }
    }

    fn rejecting_cursor(mut self, cursor: ProviderCursor) -> Self {
        self.reject_cursor = Some(cursor);
        self
    }

    fn gate(mut self, entered: Arc<Notify>, release: Arc<Notify>) -> Self {
        self.entered = Some(entered);
        self.release = Some(release);
        self
    }

    fn requests(&self) -> Arc<Mutex<Vec<SyncRequest>>> {
        self.requests.clone()
    }
}

#[async_trait]
impl SyncService for ScriptedSync {
    async fn next_page(
        &mut self,
        request: SyncRequest,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        if let Some(entered) = &self.entered {
            entered.notify_one();
        }
        if let Some(release) = &self.release {
            release.notified().await;
        }
        ctx.ensure_active()?;
        if let Some(reject) = &self.reject_cursor
            && request.cursor.as_ref() == Some(reject)
        {
            return Err(ProviderError::cursor_invalid(
                "the stored cursor was written by a different provider version",
            ));
        }
        if let Some((_, page)) = self
            .pages
            .iter()
            .find(|(cursor, _)| cursor == &request.cursor)
        {
            return Ok(page.clone());
        }
        // Unscripted: echo the cursor back with no changes.
        let checkpoint = request
            .cursor
            .clone()
            .unwrap_or_else(|| ProviderCursor::new(1, "start"));
        Ok(ChangePage {
            changes: Vec::new(),
            checkpoint,
            has_more: false,
        })
    }
}

fn provider_session(sync: ScriptedSync, provider: &str) -> ProviderSession {
    let mut session = ProviderSession::new(ProviderCapabilities {
        sync: true,
        ..ProviderCapabilities::default()
    });
    session.provider = ProviderId::from(provider);
    session.sync = Some(Box::new(sync));
    session
}

// ---------------------------------------------------------------------------
// The seven required tests
// ---------------------------------------------------------------------------

/// A page whose transaction fails (here, a change referencing an unknown
/// collection) commits nothing and does not advance the checkpoint; replaying
/// from the unchanged checkpoint applies the corrected page exactly once.
#[tokio::test]
async fn crash_before_page_commit_replays_safely() {
    let account = imap_account("acc-crash");
    let (_dir, store) = open_store(&account);
    let key = provider::account_job(&account.id);

    let broken = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(collection("INBOX", "INBOX", FolderKind::Inbox)),
            message_change("imap:1", "missing", "lost", false, Flags::default()),
        ],
        checkpoint: ProviderCursor::new(1, "cp-1"),
        has_more: false,
    };
    let mut session = provider_session(
        ScriptedSync::pages(vec![(None, broken)]),
        IMAP_SMTP_PROVIDER,
    );
    let outcome =
        provider::sync_account(&store, &mut session, &ctx(&account.id, 1), budget(4)).await;
    assert!(
        matches!(
            outcome,
            Err(SyncError::Store(StoreError::UnknownRemoteCollection(_)))
        ),
        "a failed page must surface its store error, got {outcome:?}"
    );
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        None,
        "a rolled-back page must not advance the checkpoint"
    );
    assert!(
        store
            .remote_message_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("imap:1")
            )
            .unwrap()
            .is_none(),
        "no message may be cached from a rolled-back page"
    );
    assert!(
        store.folders(&account.id).unwrap().is_empty(),
        "the rolled-back collection upsert must not persist"
    );

    // The replay (a fresh session) starts from the unadvanced checkpoint and
    // applies the corrected page once.
    let corrected = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(collection("INBOX", "INBOX", FolderKind::Inbox)),
            message_change("imap:1", "INBOX", "arrived", false, Flags::default()),
        ],
        checkpoint: ProviderCursor::new(1, "cp-1"),
        has_more: false,
    };
    let mut session = provider_session(
        ScriptedSync::pages(vec![(None, corrected)]),
        IMAP_SMTP_PROVIDER,
    );
    let report = provider::sync_account(&store, &mut session, &ctx(&account.id, 1), budget(4))
        .await
        .expect("the replay applies cleanly");
    assert_eq!(report.errors.len(), 0);
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        Some(ProviderCursor::new(1, "cp-1"))
    );
    let folder = store
        .folders(&account.id)
        .unwrap()
        .into_iter()
        .find(|folder| folder.name == "INBOX")
        .expect("INBOX is cached");
    assert_eq!(
        store.messages(&folder.id, 100, None).unwrap().len(),
        1,
        "the replayed message must be cached exactly once"
    );
}

/// A stored cursor the adapter cannot interpret restarts a scoped
/// reconciliation (a fresh, cursorless bootstrap) instead of failing the sync.
#[tokio::test]
async fn invalid_cursor_starts_scoped_reconciliation() {
    let account = imap_account("acc-cursor");
    let (_dir, store) = open_store(&account);
    let key = provider::account_job(&account.id);
    let invalid = ProviderCursor::new(99, "not-this-version");
    store.set_checkpoint(&key, 1, &invalid).unwrap();

    let reconciliation = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(collection("INBOX", "INBOX", FolderKind::Inbox)),
            message_change("imap:5", "INBOX", "rescued", false, Flags::default()),
        ],
        checkpoint: ProviderCursor::new(1, "reconciled"),
        has_more: false,
    };
    let sync = ScriptedSync::pages(vec![(None, reconciliation)]).rejecting_cursor(invalid.clone());
    let requests = sync.requests();
    let mut session = provider_session(sync, IMAP_SMTP_PROVIDER);
    let report = provider::sync_account(&store, &mut session, &ctx(&account.id, 1), budget(4))
        .await
        .expect("the invalid cursor is recovered from");

    assert_eq!(report.errors.len(), 0);
    let requests = requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        2,
        "one rejected attempt then a scoped retry"
    );
    assert_eq!(
        requests[0].cursor,
        Some(invalid),
        "the stored cursor is tried first"
    );
    assert_eq!(
        requests[1].cursor, None,
        "reconciliation restarts with no cursor"
    );
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        Some(ProviderCursor::new(1, "reconciled"))
    );
    assert!(
        store
            .remote_message_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("imap:5")
            )
            .unwrap()
            .is_some()
    );
}

/// An initial bootstrap imports pre-existing mail without notifying; the
/// checkpoint it writes is the anchor from which a later genuine arrival is
/// replayed and notified.
#[tokio::test]
async fn bootstrap_anchor_replays_arrival() {
    let account = imap_account("acc-arrival");
    let (_dir, store) = open_store(&account);
    let key = provider::account_job(&account.id);

    let bootstrap = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(collection("INBOX", "INBOX", FolderKind::Inbox)),
            message_change("imap:1", "INBOX", "older", false, Flags::default()),
        ],
        checkpoint: ProviderCursor::new(1, "anchor"),
        has_more: false,
    };
    let mut session = provider_session(
        ScriptedSync::pages(vec![(None, bootstrap)]),
        IMAP_SMTP_PROVIDER,
    );
    let first = provider::sync_account(&store, &mut session, &ctx(&account.id, 1), budget(4))
        .await
        .unwrap();
    assert!(
        first.new_inbox.is_empty(),
        "an initial import must not notify the user"
    );
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        Some(ProviderCursor::new(1, "anchor"))
    );

    // A genuine arrival above the anchor notifies exactly once.
    let arrival = ChangePage {
        changes: vec![message_change(
            "imap:2",
            "INBOX",
            "fresh",
            true,
            Flags::default(),
        )],
        checkpoint: ProviderCursor::new(1, "cp-2"),
        has_more: false,
    };
    let mut session = provider_session(
        ScriptedSync::pages(vec![(Some(ProviderCursor::new(1, "anchor")), arrival)]),
        IMAP_SMTP_PROVIDER,
    );
    let second = provider::sync_account(&store, &mut session, &ctx(&account.id, 1), budget(4))
        .await
        .unwrap();
    assert_eq!(second.new_inbox.len(), 1, "the arrival is notified");
    assert_eq!(second.new_inbox[0].subject, "fresh");

    // Re-running past the advanced checkpoint never re-notifies the arrival.
    let mut session = provider_session(ScriptedSync::pages(vec![]), IMAP_SMTP_PROVIDER);
    let third = provider::sync_account(&store, &mut session, &ctx(&account.id, 1), budget(4))
        .await
        .unwrap();
    assert!(
        third.new_inbox.is_empty(),
        "an already-anchored arrival must not notify twice"
    );
}

/// A page read before a mutation but committed after it must be discarded by
/// the checkpoint compare-and-swap, so the mutation's flag state survives.
#[tokio::test]
async fn late_page_after_mutation_cannot_revert_flags() {
    let account = imap_account("acc-late");
    let (_dir, store) = open_store(&account);
    let key = provider::account_job(&account.id);

    store
        .apply_change_page(
            &ctx(&account.id, 1),
            &key,
            IMAP_SMTP_PROVIDER,
            None,
            &ChangePage {
                changes: vec![
                    MailChange::UpsertCollection(collection("INBOX", "INBOX", FolderKind::Inbox)),
                    message_change("imap:1", "INBOX", "hello", false, Flags::default()),
                ],
                checkpoint: ProviderCursor::new(1, "cp-1"),
                has_more: false,
            },
        )
        .unwrap();
    let id = store
        .remote_message_for(
            &account.id,
            IMAP_SMTP_PROVIDER,
            &RemoteMessageId::new("imap:1"),
        )
        .unwrap()
        .unwrap();

    // The late page was read at cp-1 and still carries the old, unread flag.
    let late = ChangePage {
        changes: vec![message_change(
            "imap:1",
            "INBOX",
            "hello",
            false,
            Flags::default(),
        )],
        checkpoint: ProviderCursor::new(1, "cp-2"),
        has_more: false,
    };
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sync = ScriptedSync::pages(vec![(Some(ProviderCursor::new(1, "cp-1")), late)])
        .gate(entered.clone(), release.clone());
    let mut session = provider_session(sync, IMAP_SMTP_PROVIDER);

    let sync_ctx = ctx(&account.id, 1);
    let sync_fut = provider::sync_account(&store, &mut session, &sync_ctx, budget(4));
    let store_handle = &store;
    let mutation = async {
        entered.notified().await;
        // A concurrent mutation marks the message read and advances the job
        // checkpoint, fencing the in-flight page out.
        let read = Flags {
            seen: true,
            ..Flags::default()
        };
        store_handle.set_flags(&id, &read).unwrap();
        store_handle
            .set_checkpoint(&key, 1, &ProviderCursor::new(1, "cp-mutation"))
            .unwrap();
        release.notify_one();
    };
    let (result, ()) = tokio::join!(sync_fut, mutation);
    result.expect("a stale page is discarded, not fatal");

    let stored = store.message(&id).unwrap().unwrap();
    assert!(
        stored.flags.seen,
        "the late page must not revert the mutation's read flag"
    );
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        Some(ProviderCursor::new(1, "cp-mutation")),
        "the stale page must not clobber the mutation's checkpoint"
    );
}

/// A bounded initial window is not a full snapshot: messages older than the
/// window that are already cached must never be swept by applying it.
#[tokio::test]
async fn partial_window_does_not_sweep_older_messages() {
    let account = imap_account("acc-window");
    let (_dir, store) = open_store(&account);

    // Seed 30 older cached messages the 10-message window will not include.
    let folder_id = FolderId::new();
    store
        .upsert_folder(&Folder {
            id: folder_id.clone(),
            account_id: account.id.clone(),
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
            unread: 0,
            total: 0,
            uidvalidity: Some(1),
            uidnext: Some(31),
        })
        .unwrap();
    for uid in 1..=30u32 {
        store
            .upsert_message(&StoredMessage {
                id: MessageId::new(),
                account_id: account.id.clone(),
                folder_id: Some(folder_id.clone()),
                uid: Some(Uid(uid)),
                uidvalidity: Some(1),
                header: header(&format!("old-{uid}")),
                flags: Flags::default(),
                thread_id: ThreadId::new(),
                snippet: String::new(),
                has_attachments: false,
            })
            .unwrap();
    }

    // The server holds 50 messages; the provider boots with a window of 10.
    let transports = Arc::new(FakeTransportFactory::default());
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![RemoteFolder {
                name: "INBOX".to_string(),
                delimiter: Some("/".to_string()),
                kind: FolderKind::Inbox,
            }],
            states: HashMap::from([(
                "INBOX".to_string(),
                FolderState {
                    uidvalidity: 1,
                    uidnext: 51,
                    exists: 50,
                },
            )]),
            messages: HashMap::from([(
                "INBOX".to_string(),
                (1..=50u32)
                    .map(|uid| RemoteMessage {
                        uid: Uid(uid),
                        header: header(&format!("new-{uid}")),
                        flags: Flags::default(),
                        size: 64,
                    })
                    .collect(),
            )]),
            ..FakeTransport::default()
        },
    );

    let credentials: Arc<dyn CredentialStore> = Arc::new(FakeCredentials::new());
    let factory = ImapProviderFactory::new(
        transports.clone(),
        Arc::new(CredentialSecretRepository::new(credentials)),
    );
    let mut session = factory
        .open(&account, &ctx(&account.id, 1))
        .await
        .expect("open IMAP session");

    let report = provider::sync_account(
        &store,
        &mut session,
        &ctx(&account.id, 1),
        budget_with(4, 10),
    )
    .await
    .expect("bounded bootstrap syncs");
    assert_eq!(report.errors.len(), 0);

    let cached = store.messages(&folder_id, 1000, None).unwrap();
    assert!(
        cached.len() >= 40,
        "older cached messages must survive a bounded window, got {}",
        cached.len()
    );

    let checkpoint = store
        .job_checkpoint(&provider::account_job(&account.id))
        .unwrap()
        .expect("checkpoint written");
    let decoded = decode_sync_cursor(&checkpoint).expect("IMAP cursor");
    assert!(
        decoded
            .mailboxes
            .iter()
            .any(|mailbox| mailbox.name == "INBOX" && !mailbox.snapshot_complete),
        "a bounded window must not claim a complete snapshot"
    );
}

/// The checkpoint compare-and-swap rejects a page based on anything but the
/// store's current cursor, and the account generation is checked in the same
/// transaction.
#[tokio::test]
async fn stale_checkpoint_cas_rejected() {
    let account = imap_account("acc-cas");
    let (_dir, store) = open_store(&account);
    let key = provider::account_job(&account.id);
    let cp1 = ProviderCursor::new(1, "cp-1");
    store.set_checkpoint(&key, 1, &cp1).unwrap();

    let page = |checkpoint: &str| ChangePage {
        changes: vec![MailChange::UpsertCollection(collection(
            "LabelCAS",
            "Label CAS",
            FolderKind::Other,
        ))],
        checkpoint: ProviderCursor::new(1, checkpoint),
        has_more: false,
    };
    let mapped = |store: &Store| {
        store
            .remote_collection_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteCollectionId::new("LabelCAS"),
            )
            .unwrap()
            .is_some()
    };

    // A wrong previous cursor is rejected and applies nothing.
    let stale = store.apply_change_page(
        &ctx(&account.id, 1),
        &key,
        IMAP_SMTP_PROVIDER,
        Some(&ProviderCursor::new(1, "cp-0")),
        &page("cp-2"),
    );
    assert!(matches!(stale, Err(StoreError::StalePage)));
    // A missing expectation against an existing checkpoint is stale too.
    let unanchored = store.apply_change_page(
        &ctx(&account.id, 1),
        &key,
        IMAP_SMTP_PROVIDER,
        None,
        &page("cp-2"),
    );
    assert!(matches!(unanchored, Err(StoreError::StalePage)));
    assert_eq!(store.job_checkpoint(&key).unwrap(), Some(cp1.clone()));
    assert!(!mapped(&store), "a rejected page must not persist changes");

    // The matching previous cursor advances the checkpoint atomically.
    store
        .apply_change_page(
            &ctx(&account.id, 1),
            &key,
            IMAP_SMTP_PROVIDER,
            Some(&cp1),
            &page("cp-2"),
        )
        .unwrap();
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        Some(ProviderCursor::new(1, "cp-2"))
    );
    assert!(mapped(&store));

    // A stale generation is rejected inside the same transaction, even with a
    // matching cursor.
    store.invalidate_account(&account.id).unwrap();
    let stale_generation = store.apply_change_page(
        &ctx(&account.id, 1),
        &key,
        IMAP_SMTP_PROVIDER,
        Some(&ProviderCursor::new(1, "cp-2")),
        &page("cp-3"),
    );
    assert!(matches!(
        stale_generation,
        Err(StoreError::StaleGeneration { .. })
    ));
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        Some(ProviderCursor::new(1, "cp-2"))
    );
}

/// An account invalidated (removed) while its refresh waits on the network must
/// have the resulting page discarded, never committed.
#[tokio::test]
async fn removed_account_page_is_discarded() {
    let account = imap_account("acc-removed");
    let (_dir, store) = open_store(&account);
    let key = provider::account_job(&account.id);

    let page = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(collection("INBOX", "INBOX", FolderKind::Inbox)),
            message_change("imap:1", "INBOX", "after removal", false, Flags::default()),
        ],
        checkpoint: ProviderCursor::new(1, "cp-1"),
        has_more: false,
    };
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sync = ScriptedSync::pages(vec![(None, page)]).gate(entered.clone(), release.clone());
    let mut session = provider_session(sync, IMAP_SMTP_PROVIDER);

    let sync_ctx = ctx(&account.id, 1);
    let sync_fut = provider::sync_account(&store, &mut session, &sync_ctx, budget(4));
    let store_handle = &store;
    let account_id = account.id.clone();
    let remove = async {
        entered.notified().await;
        // The account is invalidated while the refresh waits for its page.
        store_handle.invalidate_account(&account_id).unwrap();
        release.notify_one();
    };
    let (result, ()) = tokio::join!(sync_fut, remove);
    assert!(
        matches!(
            result,
            Err(SyncError::Store(StoreError::StaleGeneration { .. }))
        ),
        "a removed account's page must be discarded, got {result:?}"
    );
    assert_eq!(
        store.job_checkpoint(&key).unwrap(),
        None,
        "a removed account's checkpoint must not advance"
    );
    assert!(
        store
            .remote_message_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("imap:1")
            )
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Carry-forward: page application is keyed by the session's transport
// ---------------------------------------------------------------------------

/// A Gmail-typed account synced over the IMAP transport writes `imap-smtp`-keyed
/// mappings and collections, never rows under the configured backend.
#[tokio::test]
async fn page_is_keyed_by_the_session_transport() {
    let account = gmail_account("acc-transport");
    let (_dir, store) = open_store(&account);

    let page = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(collection("INBOX", "INBOX", FolderKind::Inbox)),
            message_change("imap:7", "INBOX", "over imap", false, Flags::default()),
        ],
        checkpoint: ProviderCursor::new(1, "cp-1"),
        has_more: false,
    };
    let mut session = provider_session(ScriptedSync::pages(vec![(None, page)]), IMAP_SMTP_PROVIDER);
    provider::sync_account(&store, &mut session, &ctx(&account.id, 1), budget(4))
        .await
        .unwrap();

    assert!(
        store
            .remote_message_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteMessageId::new("imap:7")
            )
            .unwrap()
            .is_some(),
        "the message mapping is keyed by the transport"
    );
    assert!(
        store
            .remote_message_for(&account.id, GMAIL_PROVIDER, &RemoteMessageId::new("imap:7"))
            .unwrap()
            .is_none(),
        "nothing is keyed by the configured backend"
    );
    assert!(
        store
            .remote_collection_for(
                &account.id,
                IMAP_SMTP_PROVIDER,
                &RemoteCollectionId::new("INBOX")
            )
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .remote_collection_for(
                &account.id,
                GMAIL_PROVIDER,
                &RemoteCollectionId::new("INBOX")
            )
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// A second background pass is skipped (never queued) while the account gate is
/// held, so background work cannot pile up behind itself.
#[tokio::test]
async fn background_skips_a_busy_account_gate() {
    let scheduler = AccountScheduler::new();
    let account = AccountId::new();
    let first_ctx = ctx(&account, 1);
    let second_ctx = ctx(&account, 1);

    let (first, second) = tokio::join!(
        scheduler.run_background(&first_ctx, async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            "held"
        }),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            scheduler.run_background(&second_ctx, async { "ran" }).await
        },
    );
    assert_eq!(first.unwrap(), Some("held"));
    assert_eq!(
        second.unwrap(),
        None,
        "a background pass must not wait for a busy gate"
    );
}

/// An interactive wait for the account gate is bounded by context
/// cancellation rather than blocking behind background work.
#[tokio::test]
async fn interactive_wait_is_bounded_by_cancellation() {
    let scheduler = Arc::new(AccountScheduler::new());
    let account = AccountId::new();
    let release = Arc::new(Notify::new());
    let acquired = Arc::new(Notify::new());

    let holder = {
        let scheduler = scheduler.clone();
        let holder_ctx = ctx(&account, 1);
        let release = release.clone();
        let acquired = acquired.clone();
        tokio::spawn(async move {
            scheduler
                .run_background(&holder_ctx, async move {
                    acquired.notify_one();
                    release.notified().await;
                })
                .await
        })
    };
    acquired.notified().await;

    let waiting = ctx(&account, 1);
    waiting.cancellation.cancel();
    let started = tokio::time::Instant::now();
    let result = scheduler.run_interactive(&waiting, async {}).await;
    assert_eq!(result, Err(SchedulerError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "an interactive wait must not block behind background work"
    );

    release.notify_one();
    holder.await.unwrap().unwrap();
}

/// A deadline also bounds an interactive wait for a busy account gate.
#[tokio::test]
async fn interactive_wait_is_bounded_by_deadline() {
    let scheduler = Arc::new(AccountScheduler::new());
    let account = AccountId::new();
    let release = Arc::new(Notify::new());
    let acquired = Arc::new(Notify::new());

    let holder = {
        let scheduler = scheduler.clone();
        let holder_ctx = ctx(&account, 1);
        let release = release.clone();
        let acquired = acquired.clone();
        tokio::spawn(async move {
            scheduler
                .run_background(&holder_ctx, async move {
                    acquired.notify_one();
                    release.notified().await;
                })
                .await
        })
    };
    acquired.notified().await;

    let waiting = OperationContext::with_timeout(account, 1, Duration::from_millis(20));
    let result = scheduler.run_interactive(&waiting, async {}).await;
    assert_eq!(result, Err(SchedulerError::Deadline));

    release.notify_one();
    holder.await.unwrap().unwrap();
}
