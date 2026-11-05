//! API surface tests: initialization, snapshots, account addition, sync
//! orchestration, unread rollups, and paging. A single global state backs the
//! API, so these tests serialize on `TEST_LOCK` and re-initialize per test.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use pige_core::{
    AccountId, Address, Body, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId,
    Security, ServerConfig, Store, StoredMessage, ThreadId, Uid,
    api::{self, AccountInput, Config, CoreEvent, DB_FILE_NAME, MessageBody, SyncStatus},
    html::{Block, Inline, MailDocument},
    platform::{
        BackupStore, CredentialStore,
        fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    },
    transport::{FakeTransport, FolderState, RemoteFolder, RemoteMessage, UidRange},
};
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::{Mutex, Semaphore};

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

struct Harness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
    notifier: Arc<FakeNotifier>,
    factory: Arc<FakeTransportFactory>,
    backups: Arc<pige_core::platform::fake::FakeBackupStore>,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        let notifier = Arc::new(FakeNotifier::default());
        let opener = Arc::new(FakeOpener::default());
        let factory = Arc::new(FakeTransportFactory::default());
        let backups = Arc::new(pige_core::platform::fake::FakeBackupStore::new());

        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            config_dir: dir.path().join("config"),
            credentials: credentials.clone(),
            notifier: notifier.clone(),
            opener,
            transport: factory.clone(),
            backups: backups.clone(),
        })
        .expect("init api");

        Self {
            dir,
            credentials,
            notifier,
            factory,
            backups,
        }
    }

    fn db_path(&self) -> PathBuf {
        self.dir.path().join("data").join(DB_FILE_NAME)
    }

    fn open_store(&self) -> Store {
        let store = Store::open(self.db_path()).expect("open seeded store");
        store.migrate().expect("migrate seeded store");
        store
    }
}

fn input(label: &str, email: &str) -> AccountInput {
    AccountInput {
        label: label.to_string(),
        email: email.to_string(),
        display_name: label.to_string(),
        username: email.to_string(),
        password: format!("secret-{label}"),
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
    }
}

fn folder(id: &str, account: &AccountId, name: &str, kind: FolderKind, unread: u32) -> Folder {
    Folder {
        id: FolderId(id.to_string()),
        account_id: account.clone(),
        name: name.to_string(),
        delimiter: Some("/".to_string()),
        kind,
        unread,
        total: unread,
        uidvalidity: Some(1),
        uidnext: Some(1),
    }
}

fn remote_message(uid: u32, subject: &str, from: &str) -> RemoteMessage {
    RemoteMessage {
        uid: Uid(uid),
        header: MessageHeader {
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: subject.to_string(),
            from: vec![Address::new(None, from)],
            to: Vec::new(),
            cc: Vec::new(),
            date: i64::from(uid),
            size: 64,
        },
        flags: Flags::default(),
        size: 64,
    }
}

fn inbox_transport(messages: Vec<RemoteMessage>) -> FakeTransport {
    let count = messages.len() as u32;
    let uidnext = messages.iter().map(|m| m.uid.0).max().unwrap_or(0) + 1;
    FakeTransport {
        folders: vec![RemoteFolder {
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
        }],
        messages: HashMap::from([("INBOX".to_string(), messages)]),
        states: HashMap::from([(
            "INBOX".to_string(),
            FolderState {
                uidvalidity: 1,
                uidnext,
                exists: count,
            },
        )]),
        ..FakeTransport::default()
    }
}

fn stored_message(folder: &Folder, uid: u32) -> StoredMessage {
    StoredMessage {
        id: MessageId(format!("message-{uid}")),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: Uid(uid),
        uidvalidity: 1,
        header: MessageHeader {
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: format!("Subject {uid}"),
            from: vec![Address::new(None, "sender@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            date: i64::from(uid),
            size: 64,
        },
        flags: Flags::default(),
        thread_id: ThreadId(format!("thread-{uid}")),
        snippet: format!("snippet {uid}"),
        has_attachments: false,
    }
}

/// Store one message with explicit display data. The local id is folder-scoped
/// so the same UID can exist in two accounts.
fn seed_conversation(
    store: &Store,
    folder: &Folder,
    uid: u32,
    date: i64,
    subject: &str,
    thread: &str,
) {
    let mut message = stored_message(folder, uid);
    message.id = MessageId(format!("{}-message-{uid}", folder.id.0));
    message.header.date = date;
    message.header.subject = subject.to_string();
    message.thread_id = ThreadId(thread.to_string());
    store.upsert_message(&message).unwrap();
}

#[tokio::test]
async fn add_account_appears_in_snapshot_with_no_folders() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let snapshot = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");

    assert_eq!(snapshot.accounts.len(), 1);
    assert_eq!(snapshot.accounts[0].label, "Personal");
    assert_eq!(snapshot.accounts[0].email, "ada@example.com");
    assert_eq!(snapshot.accounts[0].unread, 0);

    assert_eq!(snapshot.sections.len(), 1);
    assert_eq!(snapshot.sections[0].label, "Personal");
    assert_eq!(snapshot.sections[0].email, "ada@example.com");
    assert!(snapshot.sections[0].folders.is_empty());
    assert_eq!(snapshot.sections[0].unread, 0);

    assert_eq!(snapshot.unified_inbox_unread, 0);
    assert!(snapshot.unified_selected);
    assert_eq!(snapshot.selected_folder, None);
    assert!(snapshot.conversations.is_empty());
    assert_eq!(snapshot.status, SyncStatus::Idle);

    let id = snapshot.accounts[0].id.clone();
    assert_eq!(
        harness.credentials.get(&id).unwrap().as_deref(),
        Some("secret-Personal")
    );
    let persisted = harness.open_store().accounts().unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].email, "ada@example.com");
}

#[tokio::test]
async fn sync_all_updates_snapshot_and_emits_new_mail() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let id = added.accounts[0].id.clone();

    harness.factory.add_transport(
        id.clone(),
        inbox_transport(vec![
            remote_message(1, "one", "a@example.com"),
            remote_message(2, "two", "b@example.com"),
        ]),
    );

    let mut events = api::subscribe();
    let snapshot = api::sync_all().await.expect("sync all");

    assert_eq!(snapshot.status, SyncStatus::Idle);
    assert_eq!(snapshot.sections.len(), 1);
    assert_eq!(snapshot.sections[0].account_id, id);
    assert_eq!(snapshot.sections[0].folders.len(), 1);
    assert_eq!(snapshot.sections[0].folders[0].name, "INBOX");
    assert_eq!(snapshot.sections[0].folders[0].unread, 2);
    assert_eq!(snapshot.sections[0].unread, 2);
    assert_eq!(snapshot.unified_inbox_unread, 2);
    assert_eq!(snapshot.conversations.len(), 2);
    assert!(snapshot.unified_selected);
    assert_eq!(snapshot.selected_folder, None);
    assert_eq!(snapshot.conversations[0].subject, "two");

    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }

    let started = seen
        .iter()
        .position(|event| matches!(event, CoreEvent::SyncStarted { account } if account == &id))
        .expect("sync started event");
    let new_mail = seen
        .iter()
        .position(|event| matches!(event, CoreEvent::NewMail(mail) if mail.subject == "two"))
        .expect("new mail event");
    let finished = seen
        .iter()
        .position(
            |event| matches!(event, CoreEvent::SyncFinished { account, new_mail } if account == &id && *new_mail == 2),
        )
        .expect("sync finished event");

    assert!(started < new_mail);
    assert!(new_mail < finished);
    assert!(
        seen.iter()
            .any(|event| matches!(event, CoreEvent::SnapshotChanged))
    );
    assert_eq!(harness.notifier.notifications().len(), 2);
}

#[tokio::test]
async fn snapshot_keeps_unified_inbox_separate_from_account_sections() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let first = api::add_account(input("Work", "work@example.com"))
        .await
        .expect("first account");
    let first_id = first.accounts[0].id.clone();
    let second = api::add_account(input("Personal", "me@example.com"))
        .await
        .expect("second account");
    let second_id = second.accounts[1].id.clone();

    let store = harness.open_store();
    store
        .upsert_folder(&folder(
            "work-inbox",
            &first_id,
            "INBOX",
            FolderKind::Inbox,
            1,
        ))
        .unwrap();
    store
        .upsert_folder(&folder(
            "personal-inbox",
            &second_id,
            "INBOX",
            FolderKind::Inbox,
            2,
        ))
        .unwrap();

    let snapshot = api::snapshot();

    // The unified inbox is a top-level rollup, not an account section, and the
    // section list preserves the account navigation order (sort_order).
    assert_eq!(snapshot.unified_inbox_unread, 3);
    assert_eq!(
        snapshot
            .sections
            .iter()
            .map(|section| section.label.as_str())
            .collect::<Vec<_>>(),
        vec!["Work", "Personal"]
    );
    assert_eq!(
        snapshot
            .accounts
            .iter()
            .map(|account| account.label.as_str())
            .collect::<Vec<_>>(),
        vec!["Work", "Personal"]
    );
    assert!(
        snapshot
            .sections
            .iter()
            .all(|section| !section.folders.is_empty())
    );
}

#[tokio::test]
async fn unified_selection_merges_inbox_conversations_across_accounts() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let work = api::add_account(input("Work", "work@example.com"))
        .await
        .expect("work account");
    let work_id = work.accounts[0].id.clone();
    let personal = api::add_account(input("Personal", "me@example.com"))
        .await
        .expect("personal account");
    let personal_id = personal.accounts[1].id.clone();

    let store = harness.open_store();
    let work_inbox = folder("work-inbox", &work_id, "INBOX", FolderKind::Inbox, 0);
    let personal_inbox = folder(
        "personal-inbox",
        &personal_id,
        "INBOX",
        FolderKind::Inbox,
        0,
    );
    let work_sent = folder("work-sent", &work_id, "Sent", FolderKind::Sent, 0);
    for item in [&work_inbox, &personal_inbox, &work_sent] {
        store.upsert_folder(item).unwrap();
    }

    seed_conversation(&store, &work_inbox, 1, 100, "work 100", "thread-work-100");
    seed_conversation(&store, &work_inbox, 2, 400, "work 400", "thread-work-400");
    seed_conversation(&store, &work_inbox, 10, 900, "work tie", "thread-z");
    seed_conversation(
        &store,
        &personal_inbox,
        1,
        300,
        "personal 300",
        "thread-personal-300",
    );
    seed_conversation(
        &store,
        &personal_inbox,
        2,
        200,
        "personal 200",
        "thread-personal-200",
    );
    seed_conversation(&store, &personal_inbox, 10, 900, "personal tie", "thread-a");
    seed_conversation(&store, &work_sent, 3, 1_000, "sent ignored", "thread-sent");

    let snapshot = api::select_unified().await.expect("select unified");

    assert!(snapshot.unified_selected);
    assert_eq!(snapshot.selected_folder, None);
    assert_eq!(
        snapshot
            .conversations
            .iter()
            .map(|conversation| conversation.subject.as_str())
            .collect::<Vec<_>>(),
        vec![
            "personal tie",
            "work tie",
            "work 400",
            "personal 300",
            "personal 200",
            "work 100",
        ]
    );
    assert_eq!(
        snapshot
            .conversations
            .iter()
            .map(|conversation| conversation.account_id.clone())
            .collect::<Vec<_>>(),
        vec![
            personal_id.clone(),
            work_id.clone(),
            work_id.clone(),
            personal_id.clone(),
            personal_id.clone(),
            work_id.clone(),
        ]
    );
}

#[tokio::test]
async fn selecting_unified_and_a_folder_switch_the_active_view() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();
    let store = harness.open_store();
    let inbox = folder("inbox", &account_id, "INBOX", FolderKind::Inbox, 0);
    store.upsert_folder(&inbox).unwrap();
    seed_conversation(&store, &inbox, 1, 10, "hello", "thread-hello");

    let unified = api::select_unified().await.expect("select unified");
    assert!(unified.unified_selected);
    assert_eq!(unified.selected_folder, None);
    assert_eq!(unified.conversations.len(), 1);

    let selected = api::select_folder(&inbox.id).await.expect("select folder");
    assert!(!selected.unified_selected);
    assert_eq!(selected.selected_folder.as_ref(), Some(&inbox.id));

    let unified_again = api::select_unified().await.expect("select unified again");
    assert!(unified_again.unified_selected);
    assert_eq!(unified_again.selected_folder, None);
}

#[tokio::test]
async fn unified_selection_pages_the_merged_cached_list() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let first = api::add_account(input("Work", "work@example.com"))
        .await
        .expect("first account");
    let first_id = first.accounts[0].id.clone();
    let second = api::add_account(input("Personal", "me@example.com"))
        .await
        .expect("second account");
    let second_id = second.accounts[1].id.clone();

    let store = harness.open_store();
    let first_inbox = folder("a-inbox", &first_id, "INBOX", FolderKind::Inbox, 0);
    let second_inbox = folder("b-inbox", &second_id, "INBOX", FolderKind::Inbox, 0);
    store.upsert_folder(&first_inbox).unwrap();
    store.upsert_folder(&second_inbox).unwrap();

    for uid in 1..=160 {
        seed_conversation(
            &store,
            &first_inbox,
            uid,
            i64::from(uid),
            &format!("first {uid}"),
            &format!("first-thread-{uid}"),
        );
        seed_conversation(
            &store,
            &second_inbox,
            uid,
            i64::from(uid) + 160,
            &format!("second {uid}"),
            &format!("second-thread-{uid}"),
        );
    }

    let snapshot = api::select_unified().await.expect("select unified");
    assert_eq!(snapshot.conversations.len(), 300);
    assert!(snapshot.has_more);
    assert_eq!(snapshot.conversations.first().unwrap().date, 320);
    assert_eq!(snapshot.conversations.last().unwrap().date, 21);

    let snapshot = api::load_older_unified().await.expect("load older unified");
    assert_eq!(snapshot.conversations.len(), 320);
    assert!(!snapshot.has_more);
    assert_eq!(snapshot.conversations.last().unwrap().date, 1);
}

#[tokio::test]
async fn unread_rollups_cover_folder_account_and_unified_inbox() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let first = api::add_account(input("Work", "work@example.com"))
        .await
        .expect("first account");
    let first_id = first.accounts[0].id.clone();
    let second = api::add_account(input("Personal", "me@example.com"))
        .await
        .expect("second account");
    let second_id = second.accounts[1].id.clone();

    let store = harness.open_store();
    store
        .upsert_folder(&folder(
            "work-inbox",
            &first_id,
            "INBOX",
            FolderKind::Inbox,
            2,
        ))
        .unwrap();
    store
        .upsert_folder(&folder("work-sent", &first_id, "Sent", FolderKind::Sent, 1))
        .unwrap();
    store
        .upsert_folder(&folder(
            "personal-inbox",
            &second_id,
            "INBOX",
            FolderKind::Inbox,
            3,
        ))
        .unwrap();

    let snapshot = api::snapshot();
    assert_eq!(snapshot.sections.len(), 2);

    let work = &snapshot.sections[0];
    assert_eq!(work.unread, 3);
    assert_eq!(snapshot.accounts[0].unread, 3);
    let inbox = work
        .folders
        .iter()
        .find(|folder| folder.name == "INBOX")
        .expect("work inbox");
    assert_eq!(inbox.unread, 2);
    let sent = work
        .folders
        .iter()
        .find(|folder| folder.name == "Sent")
        .expect("work sent");
    assert_eq!(sent.unread, 1);

    let personal = &snapshot.sections[1];
    assert_eq!(personal.unread, 3);
    assert_eq!(snapshot.accounts[1].unread, 3);

    // Unified inbox counts only Inbox folders: 2 + 3, not Sent.
    assert_eq!(snapshot.unified_inbox_unread, 5);
}

#[tokio::test]
async fn sync_all_continues_when_one_account_fails() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let good = api::add_account(input("Good", "good@example.com"))
        .await
        .expect("good account");
    let good_id = good.accounts[0].id.clone();
    let bad = api::add_account(input("Bad", "bad@example.com"))
        .await
        .expect("bad account");
    let bad_id = bad.accounts[1].id.clone();

    harness.factory.add_transport(
        good_id.clone(),
        inbox_transport(vec![remote_message(1, "hello", "friend@example.com")]),
    );
    harness.factory.fail_imap(bad_id.clone());

    let mut events = api::subscribe();
    let snapshot = api::sync_all().await.expect("sync all keeps going");

    assert_eq!(snapshot.accounts.len(), 2);
    let good_section = snapshot
        .sections
        .iter()
        .find(|section| section.account_id == good_id)
        .expect("good section");
    let bad_section = snapshot
        .sections
        .iter()
        .find(|section| section.account_id == bad_id)
        .expect("bad section");
    assert_eq!(good_section.folders.len(), 1);
    assert!(bad_section.folders.is_empty());
    assert_eq!(snapshot.unified_inbox_unread, 1);
    assert_eq!(snapshot.status, SyncStatus::Idle);

    let mut saw_error = false;
    while let Ok(event) = events.try_recv() {
        if let CoreEvent::Error(message) = event {
            saw_error = true;
            assert!(message.contains("Bad") || message.contains(&bad_id.0));
        }
    }
    assert!(saw_error, "a failed account must emit an error event");
}

#[tokio::test]
async fn load_older_extends_the_current_list() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = Folder {
        id: FolderId("inbox".to_string()),
        account_id: account_id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 400,
        uidvalidity: Some(1),
        uidnext: Some(401),
    };
    store.upsert_folder(&inbox).unwrap();
    for uid in 1..=400 {
        store.upsert_message(&stored_message(&inbox, uid)).unwrap();
    }

    let snapshot = api::select_folder(&inbox.id).await.expect("select folder");
    assert_eq!(snapshot.selected_folder.as_ref(), Some(&inbox.id));
    assert_eq!(snapshot.conversations.len(), 300);
    assert!(snapshot.has_more);
    assert_eq!(snapshot.conversations.first().unwrap().date, 400);
    assert_eq!(snapshot.conversations.last().unwrap().date, 101);

    let snapshot = api::load_older(&inbox.id).await.expect("load older");
    assert_eq!(snapshot.conversations.len(), 400);
    assert!(!snapshot.has_more);
    assert_eq!(snapshot.conversations.last().unwrap().date, 1);
}

#[tokio::test]
async fn load_older_backfills_remote_mail_when_cache_is_exhausted() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    // 400 remote messages, but the initial sync window only caches the newest
    // 300. Older rows must come from the server, not a pre-seeded database.
    let messages = (1..=400)
        .map(|uid| remote_message(uid, &format!("Subject {uid}"), "sender@example.com"))
        .collect();
    harness
        .factory
        .add_transport(account_id.clone(), inbox_transport(messages));

    let snapshot = api::sync_all().await.expect("initial sync");
    assert_eq!(snapshot.conversations.len(), 300);
    // Unified paging is cached-only for now; the server hint lives on the
    // folder view below.
    assert!(!snapshot.has_more);
    let inbox = snapshot
        .sections
        .first()
        .and_then(|section| section.folders.first())
        .map(|folder| folder.id.clone())
        .expect("synced inbox");
    let snapshot = api::select_folder(&inbox).await.expect("select inbox");
    assert_eq!(snapshot.conversations.len(), 300);
    assert!(snapshot.has_more);

    let snapshot = api::load_older(&inbox).await.expect("backfill older");
    assert_eq!(snapshot.conversations.len(), 400);
    assert!(!snapshot.has_more);

    let transport = harness
        .factory
        .transport(&account_id)
        .await
        .expect("shared fake transport");
    assert!(
        transport.fetch_headers_calls.iter().any(|(name, range)| {
            name == "INBOX"
                && *range
                    == UidRange {
                        from: Uid(1),
                        to: Uid(100),
                    }
        }),
        "expected a backfill fetch for UIDs 1..=100, got {:?}",
        transport.fetch_headers_calls
    );
}

#[tokio::test]
async fn load_older_threads_reference_chain_before_subject_fallback() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = Folder {
        id: FolderId("inbox".to_string()),
        account_id: account_id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 3,
        uidvalidity: Some(1),
        uidnext: Some(4),
    };
    store.upsert_folder(&inbox).unwrap();

    // Only the newest message is cached; the older reference chain must come
    // from the backfill path.
    let mut newest = stored_message(&inbox, 3);
    newest.header.date = 3_000;
    newest.header.subject = "Newest".to_string();
    newest.thread_id = ThreadId("newest-thread".to_string());
    store.upsert_message(&newest).unwrap();

    let parent = remote_message(1, "Alpha", "alice@example.com");
    let mut child = remote_message(2, "Beta", "bob@example.com");
    child.header.references = vec!["<1@example.com>".to_string()];
    child.header.in_reply_to = Some("<1@example.com>".to_string());
    harness
        .factory
        .add_transport(account_id, inbox_transport(vec![parent, child]));

    let snapshot = api::select_folder(&inbox.id).await.expect("select inbox");
    assert_eq!(snapshot.conversations.len(), 1);
    let snapshot = api::load_older(&inbox.id).await.expect("backfill older");
    assert_eq!(snapshot.conversations.len(), 2);

    let messages = harness
        .open_store()
        .messages(&inbox.id, 10, None)
        .expect("read cached messages");
    let parent = messages
        .iter()
        .find(|message| message.uid == Uid(1))
        .expect("backfilled parent");
    let child = messages
        .iter()
        .find(|message| message.uid == Uid(2))
        .expect("backfilled child");

    // Alpha and Beta must join through References, not the subject fallback.
    assert_eq!(
        parent.thread_id, child.thread_id,
        "backfill must assign parents before their replies"
    );
}

#[tokio::test]
async fn load_older_handles_duplicate_timestamps() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = Folder {
        id: FolderId("inbox".to_string()),
        account_id,
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 350,
        uidvalidity: Some(1),
        uidnext: Some(351),
    };
    store.upsert_folder(&inbox).unwrap();
    for uid in 1..=350 {
        let mut message = stored_message(&inbox, uid);
        message.header.date = 1_700_000_000;
        store.upsert_message(&message).unwrap();
    }

    let snapshot = api::select_folder(&inbox.id).await.expect("select folder");
    assert_eq!(snapshot.conversations.len(), 300);
    assert!(snapshot.has_more);

    let snapshot = api::load_older(&inbox.id).await.expect("load older");
    assert_eq!(snapshot.conversations.len(), 350);
    assert!(!snapshot.has_more);
}

#[tokio::test]
async fn sync_account_syncs_only_the_requested_account() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();
    harness.factory.add_transport(
        account_id.clone(),
        inbox_transport(vec![remote_message(1, "hello", "friend@example.com")]),
    );

    let mut events = api::subscribe();
    let snapshot = api::sync_account(&account_id)
        .await
        .expect("sync one account");

    assert_eq!(snapshot.conversations.len(), 1);
    assert_eq!(snapshot.unified_inbox_unread, 1);
    let mut started = false;
    let mut finished = false;
    while let Ok(event) = events.try_recv() {
        match event {
            CoreEvent::SyncStarted { account } if account == account_id => started = true,
            CoreEvent::SyncFinished {
                account,
                new_mail: 1,
            } if account == account_id => finished = true,
            _ => {}
        }
    }
    assert!(started && finished);
}

#[tokio::test]
async fn sync_all_errors_when_every_account_fails() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Bad", "bad@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();
    harness.factory.fail_imap(account_id);

    let error = api::sync_all().await.expect_err("all accounts failed");
    assert!(error.contains("imap connection failed"));
    assert!(matches!(api::snapshot().status, SyncStatus::Error(_)));
}

#[tokio::test]
async fn add_account_credential_failure_leaves_no_partial_account() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    harness.credentials.fail_next_set();

    let error = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect_err("credential write failed");
    assert!(error.contains("store password"));
    assert!(api::snapshot().accounts.is_empty());
    assert!(harness.credentials.deleted().is_empty());
}

#[tokio::test]
async fn add_account_store_failure_rolls_back_credential() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let conn = Connection::open(harness.db_path()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_account_insert BEFORE INSERT ON accounts
         BEGIN SELECT RAISE(FAIL, 'forced insert failure'); END;",
    )
    .unwrap();

    let error = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect_err("account insert failed");
    // The document is saved first, then the SQLite mirror is rebuilt; a mirror
    // failure rolls the document and the credential back.
    assert!(error.contains("update account cache"), "{error}");
    assert!(api::snapshot().accounts.is_empty());
    assert_eq!(harness.credentials.deleted().len(), 1);
}

#[tokio::test]
async fn add_account_reports_credential_rollback_failure() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let conn = Connection::open(harness.db_path()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_account_insert BEFORE INSERT ON accounts
         BEGIN SELECT RAISE(FAIL, 'forced insert failure'); END;",
    )
    .unwrap();
    harness.credentials.fail_delete();

    let error = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect_err("account insert failed");
    assert!(error.contains("credential rollback failed"));
    assert_eq!(harness.credentials.deleted().len(), 1);
}

#[tokio::test]
async fn add_account_keeps_password_out_of_sqlite() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let snapshot = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let id = snapshot.accounts[0].id.clone();
    assert_eq!(
        harness.credentials.secret(&id).as_deref(),
        Some("secret-Personal")
    );

    let needle = b"secret-Personal";
    assert_no_secret_in_dir(&harness.dir.path().join("data"), needle);
}

#[tokio::test]
async fn overlapping_sync_calls_do_not_duplicate_new_mail() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();
    harness.factory.add_transport(
        account_id,
        inbox_transport(vec![
            remote_message(1, "one", "a@example.com"),
            remote_message(2, "two", "b@example.com"),
        ]),
    );

    let mut events = api::subscribe();
    let (first, second) = tokio::join!(api::sync_all(), api::sync_all());
    first.expect("first sync");
    second.expect("second sync");

    assert_eq!(api::snapshot().conversations.len(), 2);
    let mut new_mail = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, CoreEvent::NewMail(_)) {
            new_mail += 1;
        }
    }
    assert_eq!(new_mail, 2, "new mail must only be announced once");
    assert_eq!(harness.notifier.notifications().len(), 2);
}

#[tokio::test]
async fn snapshot_reads_are_consistent_during_writes() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let first = api::add_account(input("Work", "work@example.com"))
        .await
        .expect("first account");
    let first_id = first.accounts[0].id.clone();
    let second = api::add_account(input("Personal", "me@example.com"))
        .await
        .expect("second account");
    let second_id = second.accounts[1].id.clone();

    let store = harness.open_store();
    store
        .upsert_folder(&folder("a-inbox", &first_id, "INBOX", FolderKind::Inbox, 1))
        .unwrap();
    store
        .upsert_folder(&folder(
            "b-inbox",
            &second_id,
            "INBOX",
            FolderKind::Inbox,
            1,
        ))
        .unwrap();

    let db_path = harness.db_path();
    let writer = thread::spawn(move || {
        let conn = Connection::open(db_path).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        for step in 0..200 {
            let unread = if step % 2 == 0 { 2 } else { 1 };
            let tx = conn.unchecked_transaction().unwrap();
            tx.execute(
                "UPDATE folders SET unread = ?1 WHERE id = 'a-inbox'",
                [unread],
            )
            .unwrap();
            tx.execute(
                "UPDATE folders SET unread = ?1 WHERE id = 'b-inbox'",
                [unread],
            )
            .unwrap();
            tx.commit().unwrap();
        }
    });

    for _ in 0..1_000 {
        let snapshot = api::snapshot();
        assert!(
            snapshot.unified_inbox_unread == 2 || snapshot.unified_inbox_unread == 4,
            "snapshot mixed two committed folder states: {}",
            snapshot.unified_inbox_unread
        );
    }
    writer.join().unwrap();
}

#[tokio::test]
async fn open_conversation_fetches_and_caches_the_latest_body() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = folder("inbox", &account_id, "INBOX", FolderKind::Inbox, 0);
    store.upsert_folder(&inbox).unwrap();
    let mut message = stored_message(&inbox, 1);
    message.thread_id = ThreadId("thread-body".to_string());
    store.upsert_message(&message).unwrap();

    let raw = b"From: sender@example.com\r\nTo: me@example.com\r\nSubject: Hello\r\n\
Content-Type: text/html; charset=utf-8\r\n\r\n\
<html><body><p>Hello <b>world</b></p></body></html>\r\n"
        .to_vec();
    let transport = FakeTransport {
        folders: vec![RemoteFolder {
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
        }],
        states: HashMap::from([(
            "INBOX".to_string(),
            FolderState {
                uidvalidity: 1,
                uidnext: 2,
                exists: 1,
            },
        )]),
        bodies: HashMap::from([("INBOX".to_string(), HashMap::from([(Uid(1), raw)]))]),
        ..FakeTransport::default()
    };
    harness.factory.add_transport(account_id.clone(), transport);

    let snapshot = api::open_conversation(&ThreadId("thread-body".to_string()))
        .await
        .expect("open conversation");
    let detail = snapshot
        .selected_conversation
        .as_ref()
        .expect("selected conversation");
    assert_eq!(detail.messages.len(), 1);
    let body = detail.messages[0].body.as_ref().expect("body present");
    let MessageBody::Document(doc) = body else {
        panic!("expected a parsed document, got {body:?}");
    };
    assert!(!doc.is_empty());
    assert!(mail_doc_text(doc).contains("Hello"));

    // The body was fetched exactly once and then cached.
    let first = harness
        .factory
        .transport(&account_id)
        .await
        .expect("shared transport");
    assert_eq!(first.fetch_body_calls.len(), 1);

    // Reopening uses the cache and never hits the network again.
    let reopened = api::open_conversation(&ThreadId("thread-body".to_string()))
        .await
        .expect("reopen conversation");
    assert!(matches!(
        reopened.selected_conversation.unwrap().messages[0].body,
        Some(MessageBody::Document(_))
    ));
    let second = harness
        .factory
        .transport(&account_id)
        .await
        .expect("shared transport");
    assert_eq!(
        second.fetch_body_calls.len(),
        1,
        "second open must read the cached body"
    );
}

#[tokio::test]
async fn oversized_body_is_not_fetched_or_cached() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = folder("inbox", &account_id, "INBOX", FolderKind::Inbox, 0);
    store.upsert_folder(&inbox).unwrap();
    let mut message = stored_message(&inbox, 1);
    message.thread_id = ThreadId("thread-huge".to_string());
    message.header.size = 20 * 1024 * 1024; // 20 MiB, over the 10 MiB cap
    store.upsert_message(&message).unwrap();

    harness.factory.add_transport(
        account_id.clone(),
        inbox_transport(vec![remote_message(1, "huge", "sender@example.com")]),
    );

    let snapshot = api::open_conversation(&ThreadId("thread-huge".to_string()))
        .await
        .expect("open conversation");
    let detail = snapshot
        .selected_conversation
        .as_ref()
        .expect("selected conversation");
    assert_eq!(detail.messages[0].body, Some(MessageBody::TooLarge));

    // No fetch was attempted and no body was cached.
    let transport = harness
        .factory
        .transport(&account_id)
        .await
        .expect("shared transport");
    assert!(transport.fetch_body_calls.is_empty());
    assert!(
        harness
            .open_store()
            .body(&detail.messages[0].id)
            .unwrap()
            .is_none()
    );
}

/// Flatten a document to its visible text, for body assertions in API tests.
fn mail_doc_text(doc: &MailDocument) -> String {
    fn render_blocks(blocks: &[Block], out: &mut String) {
        for block in blocks {
            match block {
                Block::Paragraph(inlines)
                | Block::Heading {
                    content: inlines, ..
                } => {
                    render_inlines(inlines, out);
                }
                Block::List { items, .. } => {
                    for item in items {
                        render_blocks(item, out);
                    }
                }
                Block::Quote(inner) => render_blocks(inner, out),
                Block::Pre(text) => out.push_str(text),
                Block::Table(rows) => {
                    for row in rows {
                        for cell in row {
                            render_inlines(cell, out);
                        }
                    }
                }
                Block::Rule | Block::Image { .. } => {}
            }
        }
    }
    fn render_inlines(inlines: &[Inline], out: &mut String) {
        for inline in inlines {
            match inline {
                Inline::Text { text, .. } => out.push_str(text),
                Inline::Link { content, .. } => render_inlines(content, out),
                Inline::Image { .. } => {}
            }
        }
    }
    let mut out = String::new();
    render_blocks(&doc.blocks, &mut out);
    out
}

fn assert_send<T: Send>(_: T) {}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn api_sync_futures_are_send() {
    assert_send(api::sync_all());
    let id = AccountId("account".to_string());
    assert_send(api::sync_account(&id));
    let folder = FolderId("folder".to_string());
    assert_send(api::load_older(&folder));
    assert_send(api::load_older_unified());
    assert_send(api::select_folder(&folder));
    assert_send(api::select_unified());
}

#[test]
fn store_is_send_and_sync() {
    assert_send_sync::<Store>();
}

fn assert_no_secret_in_dir(dir: &std::path::Path, needle: &[u8]) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| {
        panic!("read data dir {}: {error}", dir.display());
    });
    for entry in entries {
        let entry = entry.expect("data dir entry");
        let path = entry.path();
        if path.is_dir() {
            assert_no_secret_in_dir(&path, needle);
        } else if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !contains_bytes(&bytes, needle),
                "password leaked into {}",
                path.display()
            );
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
async fn backfill_result_is_discarded_when_selection_changes() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = Folder {
        id: FolderId("inbox".to_string()),
        account_id: account_id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 400,
        uidvalidity: Some(1),
        uidnext: Some(401),
    };
    store.upsert_folder(&inbox).unwrap();
    for uid in 101..=400 {
        store.upsert_message(&stored_message(&inbox, uid)).unwrap();
    }

    let archive = Folder {
        id: FolderId("archive".to_string()),
        account_id: account_id.clone(),
        name: "Archive".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Archive,
        unread: 0,
        total: 350,
        uidvalidity: Some(1),
        uidnext: Some(351),
    };
    store.upsert_folder(&archive).unwrap();
    for uid in 1..=350 {
        let mut message = stored_message(&archive, uid);
        message.id = MessageId(format!("archive-message-{uid}"));
        message.thread_id = ThreadId(format!("archive-thread-{uid}"));
        store.upsert_message(&message).unwrap();
    }

    let started = Arc::new(Semaphore::new(0));
    let pause = Arc::new(Semaphore::new(0));
    let messages = (1..=400)
        .map(|uid| remote_message(uid, &format!("Subject {uid}"), "sender@example.com"))
        .collect();
    let mut transport = inbox_transport(messages);
    transport.fetch_headers_started = Some(started.clone());
    transport.fetch_headers_pause = Some(pause.clone());
    harness.factory.add_transport(account_id, transport);

    let inbox_snapshot = api::select_folder(&inbox.id).await.expect("select inbox");
    assert_eq!(inbox_snapshot.conversations.len(), 300);
    assert!(inbox_snapshot.has_more);

    let folder = inbox.id.clone();
    let loading = tokio::spawn(async move { api::load_older(&folder).await });

    // Wait until the backfill is inside the network fetch, then switch folders.
    let permit = started.acquire().await.unwrap();
    drop(permit);
    let archive_snapshot = api::select_folder(&archive.id)
        .await
        .expect("select archive");
    assert_eq!(archive_snapshot.selected_folder.as_ref(), Some(&archive.id));
    assert_eq!(archive_snapshot.conversations.len(), 300);
    assert!(archive_snapshot.has_more);

    // Let the original backfill finish; its paging state must not leak into
    // the newly selected folder.
    pause.add_permits(1);
    loading.await.unwrap().expect("backfill completes");

    let snapshot = api::snapshot();
    assert_eq!(snapshot.selected_folder.as_ref(), Some(&archive.id));
    assert_eq!(snapshot.conversations.len(), 300);
    assert!(snapshot.has_more);
}

#[tokio::test]
async fn sync_finish_clears_stale_server_more() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let messages: Vec<_> = (1..=500)
        .map(|uid| remote_message(uid, &format!("Subject {uid}"), "sender@example.com"))
        .collect();
    harness
        .factory
        .add_transport(account_id.clone(), inbox_transport(messages.clone()));

    let snapshot = api::sync_all().await.expect("initial sync");
    let inbox = snapshot
        .sections
        .first()
        .and_then(|section| section.folders.first())
        .map(|folder| folder.id.clone())
        .expect("synced inbox");
    let snapshot = api::select_folder(&inbox).await.expect("select inbox");
    assert_eq!(snapshot.conversations.len(), 300);
    let snapshot = api::load_older(&inbox).await.expect("backfill");
    assert!(snapshot.has_more, "the server still has older mail");

    // The next sync reports a smaller total and no new UIDs. The old backfill
    // probe must not keep claiming there is more mail.
    let mut second = inbox_transport(messages);
    second.states.get_mut("INBOX").expect("inbox state").exists = 400;
    harness.factory.add_transport(account_id, second);

    let snapshot = api::sync_all().await.expect("second sync");
    assert_eq!(snapshot.selected_folder.as_ref(), Some(&inbox));
    assert_eq!(snapshot.conversations.len(), 400);
    assert!(!snapshot.has_more, "finish_sync must clear server_more");
}

#[tokio::test]
async fn opening_an_other_folder_syncs_it_when_no_cache_exists() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let transport = FakeTransport {
        folders: vec![
            RemoteFolder {
                name: "INBOX".to_string(),
                delimiter: Some("/".to_string()),
                kind: FolderKind::Inbox,
            },
            RemoteFolder {
                name: "Work".to_string(),
                delimiter: Some("/".to_string()),
                kind: FolderKind::Other,
            },
        ],
        messages: HashMap::from([
            (
                "INBOX".to_string(),
                vec![remote_message(1, "Inbox mail", "a@example.com")],
            ),
            (
                "Work".to_string(),
                vec![remote_message(1, "Label mail", "b@example.com")],
            ),
        ]),
        states: HashMap::from([
            (
                "INBOX".to_string(),
                FolderState {
                    uidvalidity: 1,
                    uidnext: 2,
                    exists: 1,
                },
            ),
            (
                "Work".to_string(),
                FolderState {
                    uidvalidity: 1,
                    uidnext: 2,
                    exists: 1,
                },
            ),
        ]),
        ..FakeTransport::default()
    };
    harness.factory.add_transport(account_id, transport);

    api::sync_all().await.expect("initial sync");
    let work = api::snapshot()
        .sections
        .first()
        .and_then(|section| {
            section
                .folders
                .iter()
                .find(|folder| folder.kind == FolderKind::Other)
        })
        .map(|folder| folder.id.clone())
        .expect("the label folder must be listed");

    // The account sync skips Other folders, so nothing is cached before open.
    assert!(
        harness
            .open_store()
            .messages(&work, 10, None)
            .unwrap()
            .is_empty()
    );

    let snapshot = api::select_folder(&work).await.expect("open label");
    assert_eq!(snapshot.selected_folder.as_ref(), Some(&work));
    assert_eq!(snapshot.conversations.len(), 1);
    assert_eq!(snapshot.conversations[0].subject, "Label mail");
}

#[tokio::test]
async fn earlier_messages_carry_their_cached_bodies() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = folder("inbox", &account_id, "INBOX", FolderKind::Inbox, 0);
    store.upsert_folder(&inbox).unwrap();
    let mut older = stored_message(&inbox, 1);
    older.thread_id = ThreadId("thread-two".to_string());
    older.header.date = 1;
    store.upsert_message(&older).unwrap();
    let mut newer = stored_message(&inbox, 2);
    newer.thread_id = ThreadId("thread-two".to_string());
    newer.header.date = 2;
    store.upsert_message(&newer).unwrap();
    store
        .upsert_body(
            &older.id,
            &Body {
                plain_text: Some("the earlier body".to_string()),
                html: None,
                fetched_at: 1,
            },
        )
        .unwrap();
    store
        .upsert_body(
            &newer.id,
            &Body {
                plain_text: Some("the newest body".to_string()),
                html: None,
                fetched_at: 2,
            },
        )
        .unwrap();

    let snapshot = api::open_conversation(&ThreadId("thread-two".to_string()))
        .await
        .expect("open conversation");
    let detail = snapshot.selected_conversation.expect("detail");
    assert_eq!(detail.messages.len(), 2);

    // Newest first; both carry a body so "Earlier messages" is not names only.
    assert!(detail.messages[0].body.is_some());
    assert!(
        detail.messages[1].body.is_some(),
        "the earlier message must expose its cached body"
    );
}

// --- Portable configuration: the document is the source of truth. ---

fn seeded_account(id: &str, label: &str) -> pige_core::Account {
    pige_core::Account {
        id: AccountId(id.to_string()),
        label: label.to_string(),
        email: format!("{id}@example.com"),
        display_name: label.to_string(),
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
        sort_order: 0,
    }
}

fn test_config(dir: &TempDir) -> Config {
    Config {
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
        config_dir: dir.path().join("config"),
        credentials: Arc::new(FakeCredentials::new()),
        notifier: Arc::new(FakeNotifier::new()),
        opener: Arc::new(FakeOpener::new()),
        transport: Arc::new(FakeTransportFactory::new()),
        backups: Arc::new(pige_core::platform::fake::FakeBackupStore::new()),
    }
}

fn seed_sqlite_account(dir: &TempDir, id: &str, label: &str) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let store = Store::open(data_dir.join(DB_FILE_NAME)).unwrap();
    store.migrate().unwrap();
    store.upsert_account(&seeded_account(id, label)).unwrap();
}

#[tokio::test]
async fn first_run_migrates_sqlite_accounts_into_pige_json() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    seed_sqlite_account(&dir, "acct", "Personal");
    {
        let store = Store::open(dir.path().join("data").join(DB_FILE_NAME)).unwrap();
        store
            .set_meta("settings", r#"{"notifications":false}"#)
            .unwrap();
    }

    api::reset_for_tests();
    api::init(test_config(&dir)).unwrap();

    let path = pige_core::config::document_path(&dir.path().join("config"));
    let doc = pige_core::config::load(&path)
        .unwrap()
        .expect("document written on first run");
    assert_eq!(doc.accounts.len(), 1);
    assert_eq!(doc.accounts[0].label, "Personal");
    assert!(!doc.settings.notifications);
}

#[tokio::test]
async fn a_present_document_is_authoritative_over_sqlite() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    seed_sqlite_account(&dir, "sqlite", "From SQLite");
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let mut doc = pige_core::config::ConfigDocument::default();
    doc.accounts.push(seeded_account("document", "From Document"));
    pige_core::config::save(&pige_core::config::document_path(&config_dir), &doc).unwrap();

    api::reset_for_tests();
    api::init(test_config(&dir)).unwrap();
    let labels: Vec<String> = api::snapshot()
        .accounts
        .into_iter()
        .map(|account| account.label)
        .collect();
    assert_eq!(labels, vec!["From Document".to_string()]);
}

#[tokio::test]
async fn corrupt_document_is_quarantined_and_recovered() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    seed_sqlite_account(&dir, "recovered", "Recovered");
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let path = pige_core::config::document_path(&config_dir);
    std::fs::write(&path, b"{ not json").unwrap();

    api::reset_for_tests();
    api::init(test_config(&dir)).unwrap();
    assert!(std::path::PathBuf::from(format!("{}.corrupt", path.display())).exists());
    let labels: Vec<String> = api::snapshot()
        .accounts
        .into_iter()
        .map(|account| account.label)
        .collect();
    assert_eq!(labels, vec!["Recovered".to_string()]);
}

#[tokio::test]
async fn renaming_an_account_keeps_its_cached_mail() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let snapshot = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let account_id = snapshot.accounts[0].id.clone();

    let inbox = folder("inbox-1", &account_id, "INBOX", FolderKind::Inbox, 1);
    {
        let store = harness.open_store();
        store.upsert_folder(&inbox).unwrap();
        store.upsert_message(&stored_message(&inbox, 1)).unwrap();
    }

    api::rename_account(&account_id, "Renamed")
        .await
        .expect("rename");

    let store = harness.open_store();
    assert_eq!(store.messages(&inbox.id, 10, None).unwrap().len(), 1);
    let labels: Vec<String> = store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|account| account.label)
        .collect();
    assert_eq!(labels, vec!["Renamed".to_string()]);
}

#[tokio::test]
async fn a_failed_document_write_leaves_memory_unchanged() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    seed_sqlite_account(&dir, "acct", "Original");
    // `config` is a regular file, so the document can be neither read nor
    // written; the rename must fail without changing what the app shows.
    std::fs::write(dir.path().join("config"), b"not a directory").unwrap();

    api::reset_for_tests();
    api::init(test_config(&dir)).unwrap();
    let before: Vec<String> = api::snapshot()
        .accounts
        .into_iter()
        .map(|account| account.label)
        .collect();
    assert_eq!(before, vec!["Original".to_string()]);

    let result = api::rename_account(&AccountId("acct".to_string()), "Changed").await;
    assert!(result.is_err());

    let after: Vec<String> = api::snapshot()
        .accounts
        .into_iter()
        .map(|account| account.label)
        .collect();
    assert_eq!(after, vec!["Original".to_string()]);
}

#[tokio::test]
async fn settings_persist_in_the_document_not_the_meta_table() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    api::reset_for_tests();
    api::init(test_config(&dir)).unwrap();

    let mut settings = api::settings();
    settings.notifications = false;
    settings.sync_minutes = 30;
    api::save_settings(&settings).unwrap();

    let path = pige_core::config::document_path(&dir.path().join("config"));
    let doc = pige_core::config::load(&path).unwrap().unwrap();
    assert!(!doc.settings.notifications);
    assert_eq!(doc.settings.sync_minutes, 30);

    api::reset_for_tests();
    api::init(test_config(&dir)).unwrap();
    let loaded = api::settings();
    assert!(!loaded.notifications);
    assert_eq!(loaded.sync_minutes, 30);

    // The API clamps before writing, so an out-of-range value never reaches
    // the document.
    let mut huge = api::settings();
    huge.sync_minutes = 999;
    api::save_settings(&huge).unwrap();
    assert_eq!(api::settings().sync_minutes, 60);
}

#[tokio::test]
async fn backup_writes_a_dated_file_without_passwords() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    api::add_account(input("Personal", "ada@example.com"))
        .await
        .unwrap();

    let name = api::backup_config().await.expect("backup");
    assert!(name.starts_with("pige-backup-"), "{name}");
    assert!(name.ends_with(".json"), "{name}");

    let text = String::from_utf8(harness.backups.read(&name).unwrap()).unwrap();
    assert!(text.contains("ada@example.com"));
    assert!(
        !text.contains("secret-Personal"),
        "a backup must never contain a password"
    );
}

#[tokio::test]
async fn list_backups_is_newest_first() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    harness
        .backups
        .write("pige-backup-2026-10-20-090000.json", b"a")
        .unwrap();
    harness
        .backups
        .write("pige-backup-2026-10-21-140530.json", b"b")
        .unwrap();

    assert_eq!(
        api::list_backups().await.unwrap(),
        vec![
            "pige-backup-2026-10-21-140530.json".to_string(),
            "pige-backup-2026-10-20-090000.json".to_string(),
        ]
    );
}

#[tokio::test]
async fn backup_reports_when_downloads_is_unavailable() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    let mut config = test_config(&dir);
    config.backups =
        Arc::new(pige_core::platform::fake::FakeBackupStore::without_downloads());
    api::reset_for_tests();
    api::init(config).unwrap();

    assert!(api::backup_config().await.is_err());
}

fn backup_document(accounts: Vec<pige_core::Account>, notifications: bool) -> Vec<u8> {
    let doc = pige_core::config::ConfigDocument {
        accounts,
        settings: pige_core::settings::Settings {
            notifications,
            ..pige_core::settings::Settings::default()
        },
        ..pige_core::config::ConfigDocument::default()
    };
    serde_json::to_vec_pretty(&doc).unwrap()
}

#[tokio::test]
async fn restore_replaces_accounts_and_settings() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let added = api::add_account(input("Keep", "keep@example.com"))
        .await
        .unwrap();
    let keep = api::account(&added.accounts[0].id).unwrap();
    api::add_account(input("Drop", "drop@example.com"))
        .await
        .unwrap();

    let bytes = backup_document(vec![keep], false);
    harness
        .backups
        .write("pige-backup-2026-10-21-140530.json", &bytes)
        .unwrap();

    let outcome = api::restore_config("pige-backup-2026-10-21-140530.json".to_string())
        .await
        .expect("restore");
    assert!(!outcome.settings.notifications);

    let labels: Vec<String> = api::snapshot()
        .accounts
        .into_iter()
        .map(|account| account.label)
        .collect();
    assert_eq!(labels, vec!["Keep".to_string()]);
}

#[tokio::test]
async fn restore_keeps_surviving_cache_and_drops_the_rest() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let added = api::add_account(input("Keep", "keep@example.com"))
        .await
        .unwrap();
    let keep_id = added.accounts[0].id.clone();
    let keep = api::account(&keep_id).unwrap();
    let dropped = api::add_account(input("Drop", "drop@example.com"))
        .await
        .unwrap();
    let drop_id = dropped
        .accounts
        .iter()
        .find(|account| account.label == "Drop")
        .unwrap()
        .id
        .clone();

    let keep_folder = folder("inbox-keep", &keep_id, "INBOX", FolderKind::Inbox, 1);
    let drop_folder = folder("inbox-drop", &drop_id, "INBOX", FolderKind::Inbox, 1);
    {
        let store = harness.open_store();
        store.upsert_folder(&keep_folder).unwrap();
        store.upsert_message(&stored_message(&keep_folder, 1)).unwrap();
        store.upsert_folder(&drop_folder).unwrap();
        store
            .upsert_message(&stored_message(&drop_folder, 2))
            .unwrap();
    }

    let bytes = backup_document(vec![keep], true);
    harness
        .backups
        .write("pige-backup-2026-10-21-140530.json", &bytes)
        .unwrap();
    api::restore_config("pige-backup-2026-10-21-140530.json".to_string())
        .await
        .expect("restore");

    let store = harness.open_store();
    assert_eq!(store.messages(&keep_folder.id, 10, None).unwrap().len(), 1);
    assert!(
        store
            .messages(&drop_folder.id, 10, None)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn restore_deletes_credentials_of_dropped_accounts() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let kept = api::add_account(input("Keep", "keep@example.com"))
        .await
        .unwrap();
    let keep_id = kept.accounts[0].id.clone();
    let keep = api::account(&keep_id).unwrap();
    let dropped = api::add_account(input("Drop", "drop@example.com"))
        .await
        .unwrap();
    let drop_id = dropped
        .accounts
        .iter()
        .find(|account| account.label == "Drop")
        .unwrap()
        .id
        .clone();
    assert!(harness.credentials.get(&drop_id).unwrap().is_some());

    let bytes = backup_document(vec![keep], true);
    harness
        .backups
        .write("pige-backup-2026-10-21-140530.json", &bytes)
        .unwrap();
    api::restore_config("pige-backup-2026-10-21-140530.json".to_string())
        .await
        .expect("restore");

    assert_eq!(
        harness.credentials.get(&drop_id).unwrap(),
        None,
        "a dropped account's password must not linger"
    );
    assert!(harness.credentials.get(&keep_id).unwrap().is_some());
}

#[tokio::test]
async fn restore_writes_a_pre_restore_copy() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let added = api::add_account(input("Keep", "keep@example.com"))
        .await
        .unwrap();
    let keep = api::account(&added.accounts[0].id).unwrap();

    let bytes = backup_document(vec![keep], true);
    harness
        .backups
        .write("pige-backup-2026-10-21-140530.json", &bytes)
        .unwrap();
    api::restore_config("pige-backup-2026-10-21-140530.json".to_string())
        .await
        .expect("restore");

    let document = harness.dir.path().join("config").join("pige.json");
    let pre = std::path::PathBuf::from(format!("{}.pre-restore", document.display()));
    assert!(pre.exists(), "a pre-restore copy must be kept");
}

#[tokio::test]
async fn restore_rejects_a_bad_name_and_leaves_state() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    api::add_account(input("Personal", "ada@example.com"))
        .await
        .unwrap();

    assert!(
        api::restore_config("../../etc/passwd".to_string())
            .await
            .is_err()
    );
    assert_eq!(api::snapshot().accounts.len(), 1);
    let _ = &harness;
}

#[tokio::test]
async fn factory_reset_clears_accounts_settings_cache_and_credentials() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .unwrap();
    let id = added.accounts[0].id.clone();

    let inbox = folder("inbox-1", &id, "INBOX", FolderKind::Inbox, 1);
    {
        let store = harness.open_store();
        store.upsert_folder(&inbox).unwrap();
        store.upsert_message(&stored_message(&inbox, 1)).unwrap();
    }
    let mut settings = api::settings();
    settings.notifications = false;
    api::save_settings(&settings).unwrap();

    let outcome = api::factory_reset().await.expect("factory reset");
    assert!(outcome.snapshot.accounts.is_empty());
    assert_eq!(outcome.settings, pige_core::settings::Settings::default());
    assert_eq!(harness.credentials.get(&id).unwrap(), None);

    let store = harness.open_store();
    assert!(store.accounts().unwrap().is_empty());
    assert!(store.messages(&inbox.id, 10, None).unwrap().is_empty());
}

#[tokio::test]
async fn migrating_settings_clears_the_old_meta_row() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    seed_sqlite_account(&dir, "acct", "Personal");
    {
        let store = Store::open(dir.path().join("data").join(DB_FILE_NAME)).unwrap();
        store
            .set_meta("settings", r#"{"notifications":false}"#)
            .unwrap();
    }

    api::reset_for_tests();
    api::init(test_config(&dir)).unwrap();

    let store = Store::open(dir.path().join("data").join(DB_FILE_NAME)).unwrap();
    assert_eq!(store.get_meta("settings").unwrap(), None);
    assert!(!api::settings().notifications);
}
