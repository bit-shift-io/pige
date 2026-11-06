//! Read-path sync engine tests against the scripted `FakeTransport`.

use std::collections::{HashMap, HashSet};

use pige_core::{
    Account, AccountId, Address, AuthKind, Flags, FolderKind, MessageHeader, Security,
    ServerConfig, Store, Uid,
    search::{SearchScope, search},
    sync::sync_account,
    transport::{FakeTransport, FolderState, RemoteFolder, RemoteMessage, UidRange},
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

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store
}

fn folder(name: &str, kind: FolderKind) -> RemoteFolder {
    RemoteFolder {
        name: name.to_string(),
        delimiter: Some("/".to_string()),
        kind,
    }
}

fn state(uidvalidity: u32, uidnext: u32) -> FolderState {
    FolderState {
        uidvalidity,
        uidnext,
        exists: uidnext.saturating_sub(1),
    }
}

fn msg(uid: u32, subject: &str, from: &str) -> RemoteMessage {
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
            size: 100,
        },
        flags: Flags::default(),
        size: 100,
    }
}

#[tokio::test]
async fn first_sync_inserts_newest_headers_and_reports_inbox_mail() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![
                msg(1, "one", "a@example.com"),
                msg(2, "two", "b@example.com"),
                msg(3, "three", "c@example.com"),
                msg(4, "four", "d@example.com"),
                msg(5, "five", "e@example.com"),
            ],
        )]),
        states: HashMap::from([("INBOX".to_string(), state(1, 6))]),
        ..FakeTransport::default()
    };

    let report = sync_account(&store, &account, &mut fake, 10).await.unwrap();

    assert!(report.errors.is_empty());
    assert_eq!(report.synced_folders, 1);

    let folders = store.folders(&account.id).unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].name, "INBOX");
    assert_eq!(folders[0].uidvalidity, Some(1));
    assert_eq!(folders[0].uidnext, Some(6));
    assert_eq!(folders[0].total, 5);

    let messages = store.messages(&folders[0].id, 10, None).unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0].uid, Uid(5)); // newest first

    assert_eq!(report.new_inbox.len(), 5);
    assert_eq!(report.new_inbox[0].subject, "five");
    assert_eq!(report.new_inbox[0].from, "e@example.com");
}

#[tokio::test]
async fn later_sync_fetches_only_uids_above_high_water() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![
                msg(1, "one", "a@example.com"),
                msg(2, "two", "b@example.com"),
                msg(3, "three", "c@example.com"),
            ],
        )]),
        states: HashMap::from([("INBOX".to_string(), state(1, 4))]),
        ..FakeTransport::default()
    };

    let first = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert_eq!(first.synced_folders, 1);
    assert_eq!(
        fake.fetch_headers_calls,
        vec![(
            "INBOX".to_string(),
            UidRange {
                from: Uid(1),
                to: Uid(3)
            }
        )]
    );

    // The server gains two messages.
    fake.messages.get_mut("INBOX").unwrap().extend([
        msg(4, "four", "d@example.com"),
        msg(5, "five", "e@example.com"),
    ]);
    fake.states.insert("INBOX".to_string(), state(1, 6));

    let second = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert_eq!(second.synced_folders, 1);
    assert_eq!(fake.fetch_headers_calls.len(), 2);
    assert_eq!(
        fake.fetch_headers_calls[1],
        (
            "INBOX".to_string(),
            UidRange {
                from: Uid(4),
                to: Uid(5)
            }
        )
    );

    let folders = store.folders(&account.id).unwrap();
    let messages = store.messages(&folders[0].id, 20, None).unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(second.new_inbox.len(), 2);
    assert_eq!(second.new_inbox[0].subject, "five");
}

#[tokio::test]
async fn uidvalidity_change_drops_stale_messages_and_refetches() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![
                msg(1, "stale-one", "a@example.com"),
                msg(2, "stale-two", "b@example.com"),
                msg(3, "stale-three", "c@example.com"),
            ],
        )]),
        states: HashMap::from([("INBOX".to_string(), state(1, 4))]),
        ..FakeTransport::default()
    };

    sync_account(&store, &account, &mut fake, 10).await.unwrap();

    // The server rotates UIDVALIDITY, invalidating everything cached.
    fake.messages.insert(
        "INBOX".to_string(),
        vec![
            msg(1, "fresh-one", "x@example.com"),
            msg(2, "fresh-two", "y@example.com"),
        ],
    );
    fake.states.insert("INBOX".to_string(), state(2, 3));

    let report = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(report.errors.is_empty());

    let folders = store.folders(&account.id).unwrap();
    let messages = store.messages(&folders[0].id, 20, None).unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|m| m.uidvalidity == 2));
    let subjects: Vec<&str> = messages.iter().map(|m| m.header.subject.as_str()).collect();
    assert!(subjects.contains(&"fresh-one"));
    assert!(subjects.contains(&"fresh-two"));

    assert_eq!(
        fake.fetch_headers_calls[1],
        (
            "INBOX".to_string(),
            UidRange {
                from: Uid(1),
                to: Uid(2)
            }
        )
    );
}

#[tokio::test]
async fn flag_changes_are_reconciled_without_refetching_headers() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![
                msg(1, "one", "a@example.com"),
                msg(2, "two", "b@example.com"),
            ],
        )]),
        states: HashMap::from([("INBOX".to_string(), state(1, 3))]),
        ..FakeTransport::default()
    };

    sync_account(&store, &account, &mut fake, 10).await.unwrap();

    // The server marks the first message as seen; no new messages arrive.
    fake.messages.get_mut("INBOX").unwrap()[0].flags.seen = true;
    fake.clear_calls();

    let report = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(report.errors.is_empty());

    let folders = store.folders(&account.id).unwrap();
    let messages = store.messages(&folders[0].id, 20, None).unwrap();
    let seen = messages.iter().find(|m| m.uid == Uid(1)).unwrap();
    assert!(seen.flags.seen);

    // Headers were not refetched for already-cached UIDs.
    assert!(fake.fetch_headers_calls.is_empty());
    // Flags were reconciled for the cached UIDs.
    assert_eq!(
        fake.fetch_flags_calls,
        vec![("INBOX".to_string(), vec![Uid(1), Uid(2)])]
    );
}

#[tokio::test]
async fn expunged_uids_are_removed_from_the_cache_and_search() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![
                msg(1, "keepme", "a@example.com"),
                msg(2, "expungeme", "b@example.com"),
            ],
        )]),
        states: HashMap::from([("INBOX".to_string(), state(1, 3))]),
        ..FakeTransport::default()
    };

    sync_account(&store, &account, &mut fake, 10).await.unwrap();
    let folder_id = store.folders(&account.id).unwrap()[0].id.clone();
    assert_eq!(store.messages(&folder_id, 10, None).unwrap().len(), 2);
    assert_eq!(
        search(&store, "expungeme", &SearchScope::AllAccounts, 10)
            .unwrap()
            .len(),
        1,
        "the message must be indexed before it is expunged"
    );

    // The server expunges UID 2; the next FETCH response no longer lists it.
    fake.messages
        .get_mut("INBOX")
        .unwrap()
        .retain(|message| message.uid != Uid(2));
    fake.states.insert("INBOX".to_string(), state(1, 3));

    let report = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(report.errors.is_empty());

    let remaining = store.messages(&folder_id, 10, None).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].uid, Uid(1));
    assert!(
        search(&store, "expungeme", &SearchScope::AllAccounts, 10)
            .unwrap()
            .is_empty(),
        "the expunged message's search hit must be gone too"
    );
}

#[tokio::test]
async fn newest_snippets_are_filled_without_fetching_full_bodies() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let messages: Vec<RemoteMessage> = (1..=100)
        .map(|uid| msg(uid, &format!("subject {uid}"), "a@example.com"))
        .collect();
    let texts: HashMap<Uid, String> = (1..=100)
        .map(|uid| (Uid(uid), format!("snippet {uid}")))
        .collect();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([("INBOX".to_string(), messages)]),
        states: HashMap::from([("INBOX".to_string(), state(1, 101))]),
        texts: HashMap::from([("INBOX".to_string(), texts)]),
        ..FakeTransport::default()
    };

    let report = sync_account(&store, &account, &mut fake, 200)
        .await
        .unwrap();
    assert!(report.errors.is_empty());

    let folders = store.folders(&account.id).unwrap();
    let stored = store.messages(&folders[0].id, 200, None).unwrap();
    assert_eq!(stored.len(), 100);
    for message in &stored {
        assert_eq!(message.snippet, format!("snippet {}", message.uid.0));
    }

    // Full bodies were never fetched.
    assert!(fake.fetch_body_calls.is_empty());

    // Bodies remain in the 'headers' state: only snippets were filled.
    let conn = rusqlite::Connection::open(dir.path().join("pige.db")).unwrap();
    let non_headers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE body_state != 'headers'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(non_headers, 0);
}

#[tokio::test]
async fn one_failing_folder_does_not_abort_the_account() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![
            folder("INBOX", FolderKind::Inbox),
            folder("Sent", FolderKind::Sent),
            folder("Trash", FolderKind::Trash),
        ],
        messages: HashMap::from([
            ("INBOX".to_string(), vec![msg(1, "one", "a@example.com")]),
            ("Sent".to_string(), vec![msg(1, "sent", "a@example.com")]),
            ("Trash".to_string(), vec![msg(1, "trash", "a@example.com")]),
        ]),
        states: HashMap::from([
            ("INBOX".to_string(), state(1, 2)),
            ("Sent".to_string(), state(1, 2)),
            ("Trash".to_string(), state(1, 2)),
        ]),
        fail_select: HashSet::from(["Sent".to_string()]),
        ..FakeTransport::default()
    };

    let report = sync_account(&store, &account, &mut fake, 10).await.unwrap();

    assert_eq!(report.synced_folders, 2); // INBOX + Trash; Sent failed
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("Sent"));
    assert_eq!(report.new_inbox.len(), 1);

    let folders = store.folders(&account.id).unwrap();
    let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
    let sent = folders.iter().find(|f| f.name == "Sent").unwrap();
    let trash = folders.iter().find(|f| f.name == "Trash").unwrap();
    assert_eq!(inbox.uidnext, Some(2));
    assert_eq!(trash.uidnext, Some(2));
    assert_eq!(sent.uidnext, None); // select failed, not synced
}

#[tokio::test]
async fn existing_folder_keeps_its_id_across_syncs() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([("INBOX".to_string(), vec![msg(1, "one", "a@example.com")])]),
        states: HashMap::from([("INBOX".to_string(), state(1, 2))]),
        ..FakeTransport::default()
    };

    let first = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(first.errors.is_empty());
    let id1 = store.folders(&account.id).unwrap()[0].id.clone();

    let second = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(second.errors.is_empty());

    let folders = store.folders(&account.id).unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].id, id1);
}

#[tokio::test]
async fn reset_sync_inserts_mail_but_emits_no_new_mail_summaries() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([(
            "INBOX".to_string(),
            vec![
                msg(1, "old-one", "a@example.com"),
                msg(2, "old-two", "b@example.com"),
            ],
        )]),
        states: HashMap::from([("INBOX".to_string(), state(1, 3))]),
        ..FakeTransport::default()
    };

    sync_account(&store, &account, &mut fake, 10).await.unwrap();

    // UIDVALIDITY rotates; the same UIDs reappear with new content.
    fake.messages.insert(
        "INBOX".to_string(),
        vec![
            msg(1, "new-one", "x@example.com"),
            msg(2, "new-two", "y@example.com"),
        ],
    );
    fake.states.insert("INBOX".to_string(), state(2, 3));

    let report = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(report.errors.is_empty());
    assert_eq!(report.synced_folders, 1);
    assert_eq!(report.new_inbox.len(), 0); // no re-notification after a reset

    let folders = store.folders(&account.id).unwrap();
    let messages = store.messages(&folders[0].id, 20, None).unwrap();
    assert_eq!(messages.len(), 2);
    let subjects: Vec<&str> = messages.iter().map(|m| m.header.subject.as_str()).collect();
    assert!(subjects.contains(&"new-one"));
    assert!(subjects.contains(&"new-two"));
}

#[tokio::test]
async fn retry_after_partial_failure_preserves_thread_id() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    store.upsert_account(&account).unwrap();

    let mut fake = FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        messages: HashMap::from([("INBOX".to_string(), vec![msg(1, "one", "a@example.com")])]),
        states: HashMap::from([("INBOX".to_string(), state(1, 2))]),
        fail_fetch_texts: HashSet::from(["INBOX".to_string()]),
        ..FakeTransport::default()
    };

    // First sync inserts the header, then fails fetching the snippet.
    let first = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert_eq!(first.synced_folders, 0);
    assert_eq!(first.errors.len(), 1);

    let folders = store.folders(&account.id).unwrap();
    let thread_before = store.messages(&folders[0].id, 10, None).unwrap()[0]
        .thread_id
        .clone();

    // Retry succeeds and must not rewrite the message's thread id.
    fake.fail_fetch_texts.clear();
    let second = sync_account(&store, &account, &mut fake, 10).await.unwrap();
    assert!(second.errors.is_empty());
    assert_eq!(second.synced_folders, 1);

    let messages = store.messages(&folders[0].id, 10, None).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].thread_id, thread_before);
}
