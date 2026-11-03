//! API surface tests: initialization, snapshots, account addition, sync
//! orchestration, unread rollups, and paging. A single global state backs the
//! API, so these tests serialize on `TEST_LOCK` and re-initialize per test.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use pige_core::{
    AccountId, Address, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId, Security,
    ServerConfig, Store, StoredMessage, ThreadId, Uid,
    api::{self, AccountInput, Config, CoreEvent, DB_FILE_NAME, SyncStatus},
    platform::{
        CredentialStore,
        fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    },
    transport::{FakeTransport, FolderState, RemoteFolder, RemoteMessage},
};
use tempfile::TempDir;
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

struct Harness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
    notifier: Arc<FakeNotifier>,
    factory: Arc<FakeTransportFactory>,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        let notifier = Arc::new(FakeNotifier::default());
        let opener = Arc::new(FakeOpener::default());
        let factory = Arc::new(FakeTransportFactory::default());

        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            credentials: credentials.clone(),
            notifier: notifier.clone(),
            opener,
            transport: factory.clone(),
        })
        .expect("init api");

        Self {
            dir,
            credentials,
            notifier,
            factory,
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
    assert!(snapshot.selected_folder.is_some());
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
async fn snapshot_orders_unified_inbox_before_account_sections() {
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
