//! Conversation threading: reference chains, subject/participant fallback, and
//! the store/sync/API integration that turns them into real conversations.

use std::collections::HashMap;
use std::sync::Arc;

use pige_core::{
    AccountId, Address, AuthKind, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId,
    Security, ServerConfig, Store, StoredMessage, ThreadId, Uid,
    api::{self, AccountInput, Config, MessageBody},
    mail::{ThreadAssigner, normalized_subject},
    platform::fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    sync::sync_account,
    transport::{FakeTransport, FolderState, RemoteFolder, RemoteMessage},
};
use tempfile::TempDir;

fn address(email: &str) -> Address {
    Address::new(None, email)
}

fn header(message_id: &str, references: &[&str], subject: &str) -> MessageHeader {
    MessageHeader {
        message_id: Some(message_id.to_string()),
        in_reply_to: references.last().map(|id| id.to_string()),
        references: references.iter().map(|id| id.to_string()).collect(),
        subject: subject.to_string(),
        from: vec![address("sender@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        date: 0,
        size: 0,
    }
}

#[test]
fn references_chain_joins_a_conversation() {
    let account = AccountId("account".to_string());
    let participants = vec![address("alice@example.com"), address("bob@example.com")];
    let mut assigner = ThreadAssigner::new();

    let first = assigner.assign(
        &account,
        &header("<a@example.com>", &[], "Project update"),
        1_000,
        &participants,
    );
    let second = assigner.assign(
        &account,
        &header(
            "<b@example.com>",
            &["<a@example.com>"],
            "Re: Project update",
        ),
        2_000,
        &participants,
    );
    let third = assigner.assign(
        &account,
        &header(
            "<c@example.com>",
            &["<a@example.com>", "<b@example.com>"],
            "Re: Re: Project update",
        ),
        3_000,
        &participants,
    );

    assert_eq!(first, second);
    assert_eq!(second, third);
}

#[test]
fn broken_references_fall_back_to_subject_and_participants() {
    let account = AccountId("account".to_string());
    let participants = vec![address("alice@example.com"), address("bob@example.com")];
    let mut assigner = ThreadAssigner::new();

    let first = assigner.assign(
        &account,
        &header("<a@example.com>", &[], "Project update"),
        1_000,
        &participants,
    );
    // The reference points at a message that is not in the index; threading
    // must fall back to the normalized subject plus an overlapping participant.
    let second = assigner.assign(
        &account,
        &header(
            "<b@example.com>",
            &["<never-seen@example.com>"],
            "Re: Project update",
        ),
        1_000 + 5 * 24 * 60 * 60,
        &participants,
    );

    assert_eq!(first, second);
}

#[test]
fn different_subjects_stay_separate() {
    let account = AccountId("account".to_string());
    let participants = vec![address("alice@example.com"), address("bob@example.com")];
    let mut assigner = ThreadAssigner::new();

    let first = assigner.assign(
        &account,
        &header("<a@example.com>", &[], "Project update"),
        1_000,
        &participants,
    );
    let second = assigner.assign(
        &account,
        &header("<b@example.com>", &[], "Quarterly report"),
        1_000,
        &participants,
    );

    assert_ne!(first, second);
    assert_ne!(
        normalized_subject("Project update"),
        normalized_subject("Quarterly report")
    );
}

#[test]
fn subject_fallback_requires_an_overlapping_participant_and_recent_date() {
    let account = AccountId("account".to_string());
    let mut assigner = ThreadAssigner::new();

    let first = assigner.assign(
        &account,
        &header("<a@example.com>", &[], "Project update"),
        1_000,
        &[address("alice@example.com")],
    );
    let other_people = assigner.assign(
        &account,
        &header("<b@example.com>", &[], "Re: Project update"),
        1_000,
        &[address("carol@example.com")],
    );
    let too_old = assigner.assign(
        &account,
        &header("<c@example.com>", &[], "Re: Project update"),
        1_000 + 31 * 24 * 60 * 60,
        &[address("alice@example.com")],
    );

    assert_ne!(first, other_people);
    assert_ne!(first, too_old);
}

#[test]
fn bridged_threads_merge_and_survivors_are_updated() {
    let account = AccountId("account".to_string());
    let mut assigner = ThreadAssigner::new();

    let alpha = assigner.assign(
        &account,
        &header("<alpha@example.com>", &[], "Alpha"),
        1_000,
        &[address("alice@example.com")],
    );
    let beta = assigner.assign(
        &account,
        &header("<beta@example.com>", &[], "Beta"),
        2_000,
        &[address("bob@example.com")],
    );
    assert_ne!(alpha, beta);

    let bridge = assigner.assign(
        &account,
        &header(
            "<bridge@example.com>",
            &["<alpha@example.com>", "<beta@example.com>"],
            "Bridge",
        ),
        3_000,
        &[address("carol@example.com")],
    );

    // Both referenced messages now resolve to the survivor, including new
    // replies that only reference the losing side.
    let reply_to_beta = assigner.assign(
        &account,
        &header("<reply@example.com>", &["<beta@example.com>"], "Re: Bridge"),
        4_000,
        &[address("carol@example.com")],
    );
    assert_eq!(bridge, alpha);
    assert_eq!(reply_to_beta, alpha);
    assert_eq!(assigner.take_merges(), vec![(beta, alpha)]);
}

#[test]
fn cross_account_messages_with_the_same_message_id_stay_separate() {
    let first_account = AccountId("first".to_string());
    let second_account = AccountId("second".to_string());
    let mut assigner = ThreadAssigner::new();
    let participants = vec![address("alice@example.com")];

    let first = assigner.assign(
        &first_account,
        &header("<shared@example.com>", &[], "Shared subject"),
        1_000,
        &participants,
    );
    let second = assigner.assign(
        &second_account,
        &header("<shared@example.com>", &[], "Shared subject"),
        1_000,
        &participants,
    );

    assert_ne!(first, second);
}

#[test]
fn rethread_legacy_thread_ids_groups_existing_mail_and_keeps_uuid_threads() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("account");
    store.upsert_account(&account).unwrap();
    let folder = Folder {
        id: FolderId("folder".to_string()),
        account_id: account.id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 3,
        uidvalidity: Some(1),
        uidnext: Some(4),
    };
    store.upsert_folder(&folder).unwrap();

    let temporary = |uid: u32| ThreadId(format!("{}:{}:1:{}", account.id.0, folder.id.0, uid));
    let message = |uid: u32,
                   message_id: &str,
                   references: Vec<String>,
                   subject: &str,
                   date: i64,
                   thread_id: ThreadId| StoredMessage {
        id: MessageId(format!("message-{uid}")),
        account_id: account.id.clone(),
        folder_id: folder.id.clone(),
        uid: Uid(uid),
        uidvalidity: 1,
        header: MessageHeader {
            message_id: Some(message_id.to_string()),
            in_reply_to: references.last().cloned(),
            references,
            subject: subject.to_string(),
            from: vec![address("alice@example.com")],
            to: vec![address("bob@example.com")],
            cc: Vec::new(),
            date,
            size: 64,
        },
        flags: Flags::default(),
        thread_id,
        snippet: String::new(),
        has_attachments: false,
    };

    store
        .upsert_message(&message(
            1,
            "<a@example.com>",
            Vec::new(),
            "Project",
            1_000,
            temporary(1),
        ))
        .unwrap();
    // The reply has an unrelated subject, so only its reference can group it.
    store
        .upsert_message(&message(
            2,
            "<b@example.com>",
            vec!["<a@example.com>".to_string()],
            "Completely different",
            2_000,
            temporary(2),
        ))
        .unwrap();
    store
        .upsert_message(&message(
            3,
            "<c@example.com>",
            Vec::new(),
            "Other",
            3_000,
            ThreadId("kept-uuid".to_string()),
        ))
        .unwrap();

    let changed = store.rethread_legacy_threads(&account.id).unwrap();
    assert_eq!(changed, 2);

    let messages = store.messages(&folder.id, 10, None).unwrap();
    let first = messages
        .iter()
        .find(|message| message.uid == Uid(1))
        .unwrap();
    let second = messages
        .iter()
        .find(|message| message.uid == Uid(2))
        .unwrap();
    let stable = messages
        .iter()
        .find(|message| message.uid == Uid(3))
        .unwrap();

    assert_eq!(first.thread_id, second.thread_id);
    assert_ne!(first.thread_id, temporary(1));
    assert_eq!(stable.thread_id, ThreadId("kept-uuid".to_string()));
    assert_eq!(store.conversations(&folder.id, 10, None).unwrap().len(), 2);
}

fn account(id: &str) -> pige_core::Account {
    pige_core::Account {
        id: AccountId(id.to_string()),
        label: "Personal".to_string(),
        email: "ada@example.com".to_string(),
        display_name: "Ada Lovelace".to_string(),
        username: "ada@example.com".to_string(),
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
        auth: AuthKind::Password,
        sort_order: 0,
    }
}

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store
}

fn remote_message(
    uid: u32,
    message_id: &str,
    references: &[&str],
    subject: &str,
    from: &str,
    date: i64,
) -> RemoteMessage {
    RemoteMessage {
        uid: Uid(uid),
        header: MessageHeader {
            message_id: Some(message_id.to_string()),
            in_reply_to: references.last().map(|id| id.to_string()),
            references: references.iter().map(|id| id.to_string()).collect(),
            subject: subject.to_string(),
            from: vec![address(from)],
            to: vec![address("me@example.com")],
            cc: Vec::new(),
            date,
            size: 100,
        },
        flags: Flags::default(),
        size: 100,
    }
}

#[tokio::test]
async fn sync_merges_bridged_threads_and_updates_every_message() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("account");
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![RemoteFolder {
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
        }],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![
                remote_message(
                    1,
                    "<alpha@example.com>",
                    &[],
                    "Alpha",
                    "alice@example.com",
                    1_000,
                ),
                remote_message(
                    2,
                    "<beta@example.com>",
                    &[],
                    "Beta",
                    "bob@example.com",
                    2_000,
                ),
            ],
        )]),
        states: HashMap::from([(
            "INBOX".to_string(),
            FolderState {
                uidvalidity: 1,
                uidnext: 3,
                exists: 2,
            },
        )]),
        ..FakeTransport::default()
    };

    sync_account(&store, &account, &mut fake, 10).await.unwrap();
    let folder = store.folders(&account.id).unwrap()[0].clone();
    let before = store.messages(&folder.id, 10, None).unwrap();
    assert_eq!(before.len(), 2);
    assert_ne!(before[0].thread_id, before[1].thread_id);
    let alpha_thread = before
        .iter()
        .find(|message| message.header.message_id.as_deref() == Some("<alpha@example.com>"))
        .unwrap()
        .thread_id
        .clone();
    let beta_thread = before
        .iter()
        .find(|message| message.header.message_id.as_deref() == Some("<beta@example.com>"))
        .unwrap()
        .thread_id
        .clone();

    // A later message references both existing conversations.
    fake.messages.get_mut("INBOX").unwrap().push(remote_message(
        3,
        "<bridge@example.com>",
        &["<alpha@example.com>", "<beta@example.com>"],
        "Bridge",
        "carol@example.com",
        3_000,
    ));
    fake.states.insert(
        "INBOX".to_string(),
        FolderState {
            uidvalidity: 1,
            uidnext: 4,
            exists: 3,
        },
    );

    let report = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(report.errors.is_empty());

    let messages = store.messages(&folder.id, 10, None).unwrap();
    assert_eq!(messages.len(), 3);
    let thread = messages[0].thread_id.clone();
    assert_eq!(thread, alpha_thread, "the first reference is the survivor");
    assert!(
        messages.iter().all(|message| message.thread_id == thread),
        "all bridged messages must share the survivor thread: {messages:?}"
    );
    assert_ne!(thread, beta_thread);

    let conversations = store.conversations(&folder.id, 10, None).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].thread_id, thread);
    assert_eq!(conversations[0].message_count, 3);
    assert_eq!(conversations[0].unread, 3);

    // The aggregate row survives the merge and counts all three messages.
    let conn = rusqlite::Connection::open(dir.path().join("pige.db")).unwrap();
    let (rows, count, unread): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), MAX(message_count), MAX(unread) FROM threads WHERE id = ?1",
            [&thread.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(count, 3);
    assert_eq!(unread, 3);

    // Flag changes refresh the conversation aggregates too.
    let mut flags = messages[0].flags;
    flags.seen = true;
    store.set_flags(&messages[0].id, &flags).unwrap();
    let unread: i64 = conn
        .query_row(
            "SELECT unread FROM threads WHERE id = ?1",
            [&thread.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unread, 2);
    drop(conn);

    let losing_rows: i64 = rusqlite::Connection::open(dir.path().join("pige.db"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE id != ?1",
            [&thread.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(losing_rows, 0);
}

#[tokio::test]
async fn sync_keeps_same_message_id_separate_across_accounts() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let first = account("first");
    let second = account("second");
    store.upsert_account(&first).unwrap();
    store.upsert_account(&second).unwrap();

    let mut first_transport = sync_transport("<shared@example.com>", 1_000);
    let mut second_transport = sync_transport("<shared@example.com>", 1_000);

    sync_account(&store, &first, &mut first_transport, 10)
        .await
        .unwrap();
    sync_account(&store, &second, &mut second_transport, 10)
        .await
        .unwrap();

    let first_folder = store.folders(&first.id).unwrap()[0].clone();
    let second_folder = store.folders(&second.id).unwrap()[0].clone();
    let first_message = store.messages(&first_folder.id, 10, None).unwrap()[0].clone();
    let second_message = store.messages(&second_folder.id, 10, None).unwrap()[0].clone();

    assert_ne!(first_message.thread_id, second_message.thread_id);
}

fn sync_transport(message_id: &str, date: i64) -> FakeTransport {
    FakeTransport {
        folders: vec![RemoteFolder {
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
        }],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![remote_message(
                1,
                message_id,
                &[],
                "Shared subject",
                "alice@example.com",
                date,
            )],
        )]),
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

#[tokio::test]
async fn open_conversation_returns_every_message_in_the_thread() {
    let dir = TempDir::new().unwrap();
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

    let added = api::add_account(AccountInput {
        label: "Personal".to_string(),
        email: "ada@example.com".to_string(),
        display_name: "Ada Lovelace".to_string(),
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
    })
    .await
    .unwrap();
    let account_id = added.accounts[0].id.clone();

    let store = Store::open(dir.path().join("data").join(api::DB_FILE_NAME)).unwrap();
    store.migrate().unwrap();
    let folder = pige_core::Folder {
        id: pige_core::FolderId("inbox".to_string()),
        account_id: account_id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 1,
        total: 2,
        uidvalidity: Some(1),
        uidnext: Some(3),
    };
    store.upsert_folder(&folder).unwrap();

    let thread = ThreadId("conversation".to_string());
    for (uid, date, subject) in [(1, 1_000, "Hello"), (2, 2_000, "Re: Hello")] {
        let mut message = StoredMessage {
            id: MessageId(format!("message-{uid}")),
            account_id: account_id.clone(),
            folder_id: folder.id.clone(),
            uid: Uid(uid),
            uidvalidity: 1,
            header: MessageHeader {
                message_id: Some(format!("<{uid}@example.com>")),
                in_reply_to: None,
                references: Vec::new(),
                subject: subject.to_string(),
                from: vec![address("sender@example.com")],
                to: vec![address("me@example.com")],
                cc: Vec::new(),
                date,
                size: 64,
            },
            flags: Flags::default(),
            thread_id: thread.clone(),
            snippet: format!("snippet {uid}"),
            has_attachments: false,
        };
        message.header.date = date;
        store.upsert_message(&message).unwrap();
    }

    let raw = b"From: sender@example.com\r\nTo: me@example.com\r\nSubject: Re: Hello\r\n\
Content-Type: text/html; charset=utf-8\r\n\r\n\
<html><body><p>Newest body</p></body></html>\r\n"
        .to_vec();
    factory.add_transport(
        account_id.clone(),
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
                    uidnext: 3,
                    exists: 2,
                },
            )]),
            bodies: HashMap::from([("INBOX".to_string(), HashMap::from([(Uid(2), raw)]))]),
            ..FakeTransport::default()
        },
    );

    let snapshot = api::open_conversation(&thread).await.unwrap();
    let detail = snapshot
        .selected_conversation
        .expect("selected conversation");
    assert_eq!(detail.messages.len(), 2);
    assert_eq!(detail.messages[0].date, 2_000);
    assert_eq!(detail.messages[1].date, 1_000);
    assert!(matches!(
        detail.messages[0].body,
        Some(MessageBody::Document(_))
    ));
    assert!(detail.messages[1].body.is_none());
}
