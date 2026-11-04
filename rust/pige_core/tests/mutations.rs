//! Mutation tests: read state, move, archive, delete, and Gmail label rules.
//!
//! A single global state backs the API, so these tests serialize on `TEST_LOCK`
//! and re-initialize per test, mirroring `tests/api.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use pige_core::{
    AccountId, Address, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId, Security,
    ServerConfig, Store, StoredMessage, ThreadId, Uid,
    api::{self, AccountInput, Config, DB_FILE_NAME},
    platform::fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    transport::{Capabilities, FakeTransport, FlagOp, FolderState, RemoteMessage},
};
use tempfile::TempDir;
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

struct Harness {
    dir: TempDir,
    factory: Arc<FakeTransportFactory>,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());
        let notifier = Arc::new(FakeNotifier::default());
        let opener = Arc::new(FakeOpener::default());
        let factory = Arc::new(FakeTransportFactory::default());

        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            credentials,
            notifier,
            opener,
            transport: factory.clone(),
        })
        .expect("init api");

        Self { dir, factory }
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

fn folder(id: &str, account: &AccountId, name: &str, kind: FolderKind) -> Folder {
    Folder {
        id: FolderId(id.to_string()),
        account_id: account.clone(),
        name: name.to_string(),
        delimiter: Some("/".to_string()),
        kind,
        unread: 0,
        total: 0,
        uidvalidity: Some(1),
        uidnext: Some(2),
    }
}

fn seed_message(store: &Store, folder: &Folder, uid: u32, thread: &str) -> StoredMessage {
    let message = StoredMessage {
        id: MessageId(format!("{}-message-{uid}", folder.id.0)),
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
        thread_id: ThreadId(thread.to_string()),
        snippet: String::new(),
        has_attachments: false,
    };
    store.upsert_message(&message).unwrap();
    message
}

/// A transport that records mutation calls and responds to `select` for `INBOX`.
fn mutation_transport(capabilities: Capabilities) -> FakeTransport {
    FakeTransport {
        capabilities,
        states: HashMap::from([(
            "INBOX".to_string(),
            FolderState {
                uidvalidity: 1,
                uidnext: 2,
                exists: 1,
            },
        )]),
        ..FakeTransport::default()
    }
}

/// Add one account and register a transport for it.
async fn account_and_transport(harness: &Harness, capabilities: Capabilities) -> AccountId {
    let snapshot = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account");
    let id = snapshot.accounts[0].id.clone();
    harness
        .factory
        .add_transport(id.clone(), mutation_transport(capabilities));
    id
}

#[tokio::test]
async fn generic_move_uses_move_extension_when_available() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let archive = folder("archive", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&archive).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::move_thread(&ThreadId("thread".into()), &archive.id)
        .await
        .expect("move");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert_eq!(
        transport.move_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Archive".to_string())]
    );
    assert!(transport.copy_calls.is_empty());
    assert!(transport.delete_calls.is_empty());
}

#[tokio::test]
async fn generic_move_falls_back_to_copy_and_delete() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: false,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let archive = folder("archive", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&archive).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::move_thread(&ThreadId("thread".into()), &archive.id)
        .await
        .expect("move");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert!(transport.move_calls.is_empty());
    assert_eq!(
        transport.copy_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Archive".to_string())]
    );
    assert_eq!(
        transport.delete_calls,
        vec![("INBOX".to_string(), vec![Uid(1)])]
    );
}

#[tokio::test]
async fn gmail_move_copies_to_label_then_removes_source_label() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: true,
            move_ext: false,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let work = folder("work", &account, "Work", FolderKind::Other);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&work).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::move_thread(&ThreadId("thread".into()), &work.id)
        .await
        .expect("move");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert!(transport.move_calls.is_empty());
    assert_eq!(
        transport.copy_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Work".to_string())]
    );
    assert_eq!(
        transport.delete_calls,
        vec![("INBOX".to_string(), vec![Uid(1)])]
    );
}

#[tokio::test]
async fn gmail_archive_removes_source_label_without_copy() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: true,
            move_ext: false,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::archive_thread(&ThreadId("thread".into()))
        .await
        .expect("archive");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert!(transport.copy_calls.is_empty());
    assert!(transport.move_calls.is_empty());
    assert_eq!(
        transport.delete_calls,
        vec![("INBOX".to_string(), vec![Uid(1)])]
    );
}

#[tokio::test]
async fn generic_archive_moves_to_the_archive_folder() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let archive = folder("archive", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&archive).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::archive_thread(&ThreadId("thread".into()))
        .await
        .expect("archive");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert_eq!(
        transport.move_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Archive".to_string())]
    );
}

#[tokio::test]
async fn gmail_trash_copies_to_trash_then_removes_source() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: true,
            move_ext: false,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let trash = folder("trash", &account, "Trash", FolderKind::Trash);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&trash).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::trash_thread(&ThreadId("thread".into()))
        .await
        .expect("trash");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert!(transport.move_calls.is_empty());
    assert_eq!(
        transport.copy_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Trash".to_string())]
    );
    assert_eq!(
        transport.delete_calls,
        vec![("INBOX".to_string(), vec![Uid(1)])]
    );
}

#[tokio::test]
async fn generic_trash_moves_to_the_trash_folder() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let trash = folder("trash", &account, "Trash", FolderKind::Trash);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&trash).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::trash_thread(&ThreadId("thread".into()))
        .await
        .expect("trash");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert_eq!(
        transport.move_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Trash".to_string())]
    );
}

#[tokio::test]
async fn failed_remote_move_rolls_back_local_state() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let archive = folder("archive", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&archive).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    // Make the remote move fail.
    {
        let mut transport = harness.factory.transport(&account).await.unwrap();
        transport.fail_mutation = true;
        harness.factory.add_transport(account.clone(), transport);
    }

    api::select_folder(&inbox.id).await.expect("select inbox");
    let error = api::move_thread(&ThreadId("thread".into()), &archive.id)
        .await
        .expect_err("remote move failed");

    assert!(error.contains("mutation failed"), "got {error}");

    // The optimistic removal must have been rolled back.
    let messages = harness
        .open_store()
        .messages(&inbox.id, 10, None)
        .expect("read inbox");
    assert_eq!(messages.len(), 1, "message must be restored after rollback");
}

#[tokio::test]
async fn mutations_are_refused_offline() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let archive = folder("archive", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&archive).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    harness.factory.fail_imap(account.clone());

    api::select_folder(&inbox.id).await.expect("select inbox");
    let error = api::move_thread(&ThreadId("thread".into()), &archive.id)
        .await
        .expect_err("offline mutation refused");

    assert!(
        error.contains(api::RECONNECT_ERROR),
        "expected a reconnect error, got {error}"
    );

    // Nothing changed locally.
    let messages = harness
        .open_store()
        .messages(&inbox.id, 10, None)
        .expect("read inbox");
    assert_eq!(messages.len(), 1);
}

#[tokio::test]
async fn no_trash_folder_requires_confirmation_for_permanent_delete() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");

    // No Trash folder exists, so delete is permanent and must be confirmed.
    let refused = api::delete_permanently(&ThreadId("thread".into()), false)
        .await
        .expect_err("confirmation required");
    assert!(refused.to_lowercase().contains("confirm"), "got {refused}");

    let deleted = api::delete_permanently(&ThreadId("thread".into()), true)
        .await
        .expect("confirmed permanent delete");
    assert!(deleted.selected_conversation.is_none());
    let messages = harness.open_store().messages(&inbox.id, 10, None).unwrap();
    assert!(messages.is_empty(), "message must be gone");
}

#[tokio::test]
async fn archive_is_hidden_without_gmail_or_an_archive_folder() {
    let capabilities = Capabilities {
        gmail: false,
        move_ext: false,
    };

    // No Gmail and no Archive folder: archive is unavailable.
    assert!(!api::archive_available(
        &capabilities,
        FolderKind::Inbox,
        false
    ));
    // A generic server with an Archive folder can archive.
    assert!(api::archive_available(
        &capabilities,
        FolderKind::Inbox,
        true
    ));
    // Gmail can archive without an Archive folder (it removes the label).
    let gmail = Capabilities {
        gmail: true,
        move_ext: false,
    };
    assert!(api::archive_available(&gmail, FolderKind::Inbox, false));
    // Never archive from Trash, Spam, or Archive (Gmail All Mail included).
    assert!(!api::archive_available(&gmail, FolderKind::Trash, false));
    assert!(!api::archive_available(&gmail, FolderKind::Junk, false));
    assert!(!api::archive_available(&gmail, FolderKind::Archive, true));
}

#[tokio::test]
async fn mutations_scope_to_the_current_folder_and_never_touch_sent() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let sent = folder("sent", &account, "Sent", FolderKind::Sent);
    let archive = folder("archive", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&sent).unwrap();
    store.upsert_folder(&archive).unwrap();
    // One conversation spanning Inbox and Sent; the Sent copy must be untouched.
    seed_message(&store, &inbox, 1, "thread");
    seed_message(&store, &sent, 2, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::move_thread(&ThreadId("thread".into()), &archive.id)
        .await
        .expect("move");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert_eq!(
        transport.move_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Archive".to_string())],
        "only the Inbox copy may be moved"
    );

    let store = harness.open_store();
    let inbox_messages = store.messages(&inbox.id, 10, None).unwrap();
    assert!(inbox_messages.is_empty(), "Inbox copy must be gone");
    let sent_messages = store.messages(&sent.id, 10, None).unwrap();
    assert_eq!(sent_messages.len(), 1, "Sent copy must remain untouched");
}

/// A scripted server message, used to reflect a STORE or a server-side move
/// back into the fake mailbox across a sync.
fn remote(uid: u32, subject: &str, seen: bool) -> RemoteMessage {
    RemoteMessage {
        uid: Uid(uid),
        header: MessageHeader {
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: subject.to_string(),
            from: vec![Address::new(None, "sender@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            date: i64::from(uid),
            size: 64,
        },
        flags: Flags {
            seen,
            ..Flags::default()
        },
        size: 64,
    }
}

#[tokio::test]
async fn reader_open_read_survives_a_sync() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    // The reader-open path marks read through the server-backed, current-folder
    // scoped API.
    api::set_read(&ThreadId("thread".into()), true)
        .await
        .expect("mark read");

    // The server accepted the STORE; reflect it in the scripted mailbox.
    let mut transport = harness.factory.transport(&account).await.unwrap();
    transport.messages = HashMap::from([("INBOX".to_string(), vec![remote(1, "Subject 1", true)])]);
    transport.states = HashMap::from([(
        "INBOX".to_string(),
        FolderState {
            uidvalidity: 1,
            uidnext: 2,
            exists: 1,
        },
    )]);
    harness.factory.add_transport(account.clone(), transport);

    api::sync_all().await.expect("sync");

    let seen = harness.open_store().messages(&inbox.id, 10, None).unwrap()[0]
        .flags
        .seen;
    assert!(seen, "read state must survive a sync");
}

#[tokio::test]
async fn move_into_an_other_folder_syncs_the_destination() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let mut work = folder("work", &account, "Work", FolderKind::Other);
    work.uidvalidity = None;
    work.uidnext = None;
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&work).unwrap();
    seed_message(&store, &inbox, 1, "thread");

    // The destination already holds the moved message, as it would after MOVE.
    let transport = FakeTransport {
        capabilities: Capabilities {
            gmail: false,
            move_ext: true,
        },
        messages: HashMap::from([("Work".to_string(), vec![remote(1, "moved mail", false)])]),
        states: HashMap::from([(
            "Work".to_string(),
            FolderState {
                uidvalidity: 1,
                uidnext: 2,
                exists: 1,
            },
        )]),
        ..FakeTransport::default()
    };
    harness.factory.add_transport(account.clone(), transport);

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::move_thread(&ThreadId("thread".into()), &work.id)
        .await
        .expect("move");

    let visible = harness.open_store().messages(&work.id, 10, None).unwrap();
    assert_eq!(
        visible.len(),
        1,
        "moved conversation must appear in the destination label"
    );
}

#[tokio::test]
async fn archive_from_all_mail_is_refused() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: true,
            move_ext: false,
        },
    )
    .await;
    let store = harness.open_store();
    let all = folder("all", &account, "[Gmail]/All Mail", FolderKind::Archive);
    store.upsert_folder(&all).unwrap();
    seed_message(&store, &all, 1, "thread");

    api::select_folder(&all.id).await.expect("select all mail");
    let error = api::archive_thread(&ThreadId("thread".into()))
        .await
        .expect_err("archive from All Mail must be refused");
    assert!(error.contains("archive"), "got {error}");
}

#[tokio::test]
async fn gmail_move_from_all_mail_copies_without_deleting() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: true,
            move_ext: false,
        },
    )
    .await;
    let store = harness.open_store();
    let all = folder("all", &account, "[Gmail]/All Mail", FolderKind::Archive);
    let work = folder("work", &account, "Work", FolderKind::Other);
    store.upsert_folder(&all).unwrap();
    store.upsert_folder(&work).unwrap();
    seed_message(&store, &all, 1, "thread");

    api::select_folder(&all.id).await.expect("select all mail");
    api::move_thread(&ThreadId("thread".into()), &work.id)
        .await
        .expect("move");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert_eq!(
        transport.copy_calls,
        vec![(
            "[Gmail]/All Mail".to_string(),
            vec![Uid(1)],
            "Work".to_string()
        )]
    );
    assert!(
        transport.delete_calls.is_empty(),
        "All Mail membership must not be deleted"
    );
}

#[tokio::test]
async fn set_read_updates_only_the_current_folder() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let account = account_and_transport(
        &harness,
        Capabilities {
            gmail: false,
            move_ext: true,
        },
    )
    .await;
    let store = harness.open_store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let sent = folder("sent", &account, "Sent", FolderKind::Sent);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&sent).unwrap();
    seed_message(&store, &inbox, 1, "thread");
    seed_message(&store, &sent, 2, "thread");

    api::select_folder(&inbox.id).await.expect("select inbox");
    api::set_read(&ThreadId("thread".into()), true)
        .await
        .expect("set read");

    let transport = harness.factory.transport(&account).await.unwrap();
    let seen = Flags {
        seen: true,
        ..Flags::default()
    };
    assert_eq!(
        transport.store_flags_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], FlagOp::Add(seen))]
    );

    let store = harness.open_store();
    let inbox_messages = store.messages(&inbox.id, 10, None).unwrap();
    assert!(inbox_messages[0].flags.seen, "Inbox copy must be read");
    let sent_messages = store.messages(&sent.id, 10, None).unwrap();
    assert!(!sent_messages[0].flags.seen, "Sent copy must stay unread");
}
