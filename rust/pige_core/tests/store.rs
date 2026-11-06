//! Store-level tests: schema creation, account/folder persistence, message
//! caching, flags, pagination, and UIDVALIDITY resets.

use pige_core::{
    Account, AccountId, Address, AuthKind, Flags, Folder, FolderId, FolderKind, MessageHeader,
    MessageId, SCHEMA_VERSION, Security, ServerConfig, Store, StoredMessage, ThreadId, Uid,
};
use tempfile::TempDir;

fn account() -> Account {
    Account {
        id: AccountId("account-1".to_string()),
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
        sort_order: 3,
        color: None,
    }
}

fn folder(account_id: &AccountId) -> Folder {
    Folder {
        id: FolderId("folder-1".to_string()),
        account_id: account_id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 2,
        total: 10,
        uidvalidity: Some(7),
        uidnext: Some(42),
    }
}

fn message(folder: &Folder, uid: u32, uidvalidity: u32, date: i64) -> StoredMessage {
    StoredMessage {
        id: MessageId(format!("message-{uid}")),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: Uid(uid),
        uidvalidity,
        header: MessageHeader {
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: vec!["<root@example.com>".to_string()],
            subject: "Grüße".to_string(),
            from: vec![Address {
                name: Some("Ada Lovelace".to_string()),
                email: "ada@example.com".to_string(),
            }],
            to: vec![Address {
                name: None,
                email: "charles@example.com".to_string(),
            }],
            cc: Vec::new(),
            date,
            size: 512,
        },
        flags: Flags::default(),
        thread_id: ThreadId(format!("thread-{uid}")),
        snippet: "hello there".to_string(),
        has_attachments: false,
    }
}

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store
}

#[test]
fn prefetch_candidates_are_inbox_newest_uncached_unread_first() {
    use pige_core::store::Body;
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    let inbox = folder(&account.id);
    store.upsert_account(&account).unwrap();
    store.upsert_folder(&inbox).unwrap();
    let mut sent = inbox.clone();
    sent.id = FolderId("sent".into());
    sent.name = "Sent".into();
    sent.kind = FolderKind::Sent;
    store.upsert_folder(&sent).unwrap();
    for uid in 1..=9 {
        let mut m = message(if uid == 9 { &sent } else { &inbox }, uid, 7, uid as i64);
        m.header.size = match uid {
            3 => 10 * 1024 * 1024,
            4 => 10 * 1024 * 1024 + 1,
            _ => 100,
        };
        m.flags.seen = uid != 1;
        if uid == 7 {
            m.thread_id = ThreadId("thread-8".into());
        }
        store.upsert_message(&m).unwrap();
    }
    for uid in [5, 8] {
        store
            .upsert_body(
                &MessageId(format!("message-{uid}")),
                &Body {
                    plain_text: Some("cached".into()),
                    ..Body::default()
                },
            )
            .unwrap();
    }
    store
        .set_body_state(&MessageId("message-6".into()), "none")
        .unwrap();
    let candidates = store.prefetch_candidates(10 * 1024 * 1024, 50).unwrap();
    assert_eq!(
        candidates.iter().map(|c| c.uid.0).collect::<Vec<_>>(),
        vec![1, 3, 2]
    );
    assert_eq!(
        store
            .prefetch_candidates(10 * 1024 * 1024, 2)
            .unwrap()
            .iter()
            .map(|c| c.uid.0)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
}

#[test]
fn account_and_folder_round_trip() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let account = account();
    store.upsert_account(&account).unwrap();
    assert_eq!(store.accounts().unwrap(), vec![account.clone()]);

    let folder = folder(&account.id);
    store.upsert_folder(&folder).unwrap();
    assert_eq!(store.folders(&account.id).unwrap(), vec![folder]);
}

#[test]
fn schema_version_is_recorded() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn uidvalidity_change_drops_stale_messages() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let account = account();
    let folder = folder(&account.id);
    store.upsert_account(&account).unwrap();
    store.upsert_folder(&folder).unwrap();
    store
        .upsert_message(&message(&folder, 5, 1, 1_700_000_000))
        .unwrap();
    assert_eq!(store.messages(&folder.id, 10, None).unwrap().len(), 1);

    store
        .reset_folder(&account.id, "INBOX", 2)
        .expect("reset folder");

    assert!(store.messages(&folder.id, 10, None).unwrap().is_empty());
    let reset = &store.folders(&account.id).unwrap()[0];
    assert_eq!(reset.uidvalidity, Some(2));
    assert_eq!(reset.uidnext, None);
    assert_eq!(reset.unread, 0);
    assert_eq!(reset.total, 0);

    // Messages from the new UIDVALIDITY epoch are accepted again.
    store
        .upsert_message(&message(&folder, 5, 2, 1_700_000_100))
        .unwrap();
    assert_eq!(store.messages(&folder.id, 10, None).unwrap().len(), 1);
}

#[test]
fn messages_round_trip_and_flags_update() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let account = account();
    let folder = folder(&account.id);
    store.upsert_account(&account).unwrap();
    store.upsert_folder(&folder).unwrap();

    let stored = message(&folder, 9, 1, 1_700_000_000);
    store.upsert_message(&stored).unwrap();
    assert_eq!(store.message(&stored.id).unwrap(), Some(stored.clone()));

    let flags = Flags {
        seen: true,
        answered: true,
        flagged: true,
        deleted: false,
        draft: false,
    };
    store.set_flags(&stored.id, &flags).unwrap();
    assert_eq!(store.message(&stored.id).unwrap().unwrap().flags, flags);
}

#[test]
fn messages_page_by_date_and_delete_below_uid() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let account = account();
    let folder = folder(&account.id);
    store.upsert_account(&account).unwrap();
    store.upsert_folder(&folder).unwrap();

    for uid in 1..=5 {
        store
            .upsert_message(&message(&folder, uid, 1, 1_700_000_000 + i64::from(uid)))
            .unwrap();
    }

    let page = store.messages(&folder.id, 2, None).unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].uid, Uid(5));
    assert_eq!(page[1].uid, Uid(4));

    let older = store.messages(&folder.id, 10, Some(1_700_000_003)).unwrap();
    assert_eq!(older.len(), 2);
    assert_eq!(older[0].uid, Uid(2));

    let removed = store.delete_messages_below_uid(&folder.id, Uid(3)).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(store.messages(&folder.id, 10, None).unwrap().len(), 3);
}

#[test]
fn sync_accounts_keeps_surviving_cache_and_drops_removed_accounts() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let mut first = account();
    let first_folder = folder(&first.id);
    store.upsert_account(&first).unwrap();
    store.upsert_folder(&first_folder).unwrap();
    store
        .upsert_message(&message(&first_folder, 1, 1, 1_700_000_000))
        .unwrap();

    let mut second = account();
    second.id = AccountId("account-2".to_string());
    second.label = "Work".to_string();
    let mut second_folder = folder(&second.id);
    second_folder.id = FolderId("folder-2".to_string());
    store.upsert_account(&second).unwrap();
    store.upsert_folder(&second_folder).unwrap();
    let mut second_message = message(&second_folder, 1, 1, 1_700_000_100);
    second_message.id = MessageId("message-second-1".to_string());
    store.upsert_message(&second_message).unwrap();

    // Rename the first account in place and add a third; drop the second.
    first.label = "Personal Renamed".to_string();
    let mut third = account();
    third.id = AccountId("account-3".to_string());
    store.sync_accounts(&[first.clone(), third]).unwrap();

    // The renamed account keeps its cached mail (no blind delete-all).
    assert_eq!(store.messages(&first_folder.id, 10, None).unwrap().len(), 1);
    // The dropped account's cache cascaded away.
    assert!(
        store
            .messages(&second_folder.id, 10, None)
            .unwrap()
            .is_empty()
    );
    let labels: Vec<String> = store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|account| account.label)
        .collect();
    assert!(labels.contains(&"Personal Renamed".to_string()));
    assert!(!labels.contains(&"Work".to_string()));
    assert_eq!(labels.len(), 2);
}

#[test]
fn conversations_aggregate_messages_by_thread() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let account = account();
    let folder = folder(&account.id);
    store.upsert_account(&account).unwrap();
    store.upsert_folder(&folder).unwrap();

    let mut first = message(&folder, 1, 1, 1_700_000_000);
    first.thread_id = ThreadId("thread-a".to_string());
    first.snippet = "older message".to_string();
    let mut second = message(&folder, 2, 1, 1_700_000_100);
    second.thread_id = ThreadId("thread-a".to_string());
    second.snippet = "newest message".to_string();
    second.has_attachments = true;
    second.flags.seen = true;

    store.upsert_message(&first).unwrap();
    store.upsert_message(&second).unwrap();

    let conversations = store.conversations(&folder.id, 10, None).unwrap();
    assert_eq!(conversations.len(), 1);
    let conversation = &conversations[0];
    assert_eq!(conversation.thread_id, ThreadId("thread-a".to_string()));
    assert_eq!(conversation.message_count, 2);
    assert_eq!(conversation.unread, 1);
    assert_eq!(conversation.snippet, "newest message");
    assert_eq!(conversation.date, 1_700_000_100);
    assert!(conversation.has_attachments);
}

#[test]
fn set_snippet_updates_the_conversation_preview() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    let inbox = folder(&account.id);
    store.upsert_account(&account).unwrap();
    store.upsert_folder(&inbox).unwrap();
    store
        .upsert_message(&message(&inbox, 1, 7, 1_700_000_000))
        .unwrap();

    store
        .set_snippet(&MessageId("message-1".into()), "a decoded preview")
        .unwrap();

    let conversations = store.conversations(&inbox.id, 10, None).unwrap();
    assert_eq!(conversations[0].snippet, "a decoded preview");
}

#[test]
fn migration_repairs_stale_previews_from_cached_bodies() {
    use pige_core::store::Body;

    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    let inbox = folder(&account.id);
    store.upsert_account(&account).unwrap();
    store.upsert_folder(&inbox).unwrap();

    // A message whose preview was cached as raw base64, with a decoded body
    // already on disk: the body wins.
    let mut decoded = message(&inbox, 1, 7, 1_700_000_000);
    decoded.snippet = "SGVsbG8gd29ybGQ=".to_string();
    store.upsert_message(&decoded).unwrap();
    store
        .upsert_body(
            &decoded.id,
            &Body {
                plain_text: Some("Hello world from a cached body".to_string()),
                ..Body::default()
            },
        )
        .unwrap();

    // A message with only an encoded preview and no body: the stale preview is
    // cleared so gibberish is never shown.
    let mut stale = message(&inbox, 2, 7, 1_700_000_100);
    stale.snippet = "aGVsbG8gd29ybGQ=".to_string();
    store.upsert_message(&stale).unwrap();

    // Rewind the recorded version and replay v4.
    {
        let conn = rusqlite::Connection::open(dir.path().join("pige.db")).unwrap();
        conn.execute(
            "UPDATE meta SET value = '3' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    }
    store.migrate().unwrap();

    assert_eq!(
        store.message(&decoded.id).unwrap().unwrap().snippet,
        "Hello world from a cached body"
    );
    assert_eq!(store.message(&stale.id).unwrap().unwrap().snippet, "");
}
