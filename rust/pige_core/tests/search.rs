//! Full-text search over the local cache: matching, scopes, query escaping,
//! the v1→v2 migration, and body-cache indexing.

use std::path::Path;
use std::sync::Arc;

use pige_core::{
    Account, AccountId, Address, AuthKind, Body, Flags, Folder, FolderId, FolderKind,
    MessageHeader, MessageId, SCHEMA_VERSION, Security, ServerConfig, Store, StoredMessage,
    ThreadId, Uid,
    api::{self, Config, DB_FILE_NAME},
    platform::fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    search::{SearchScope, fts_query, search},
};
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn account(id: &str, email: &str, order: i64) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: email.to_string(),
        display_name: id.to_string(),
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
        auth: AuthKind::Password,
        sort_order: order,
        color: None,
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
        uidnext: Some(1),
    }
}

fn message(
    folder: &Folder,
    uid: u32,
    thread: &str,
    subject: &str,
    from_name: &str,
    from_email: &str,
    date: i64,
) -> StoredMessage {
    StoredMessage {
        id: MessageId(format!("{}-message-{uid}", folder.id.0)),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: Uid(uid),
        uidvalidity: 1,
        header: MessageHeader {
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: subject.to_string(),
            from: vec![Address::new(Some(from_name.to_string()), from_email)],
            to: vec![Address::new(None, "inbox@example.com")],
            cc: Vec::new(),
            date,
            size: 64,
        },
        flags: Flags::default(),
        thread_id: ThreadId(thread.to_string()),
        snippet: String::new(),
        has_attachments: false,
    }
}

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store
}

fn seed_message(store: &Store, message: &StoredMessage) {
    store.upsert_message(message).expect("upsert message");
}

#[test]
fn search_matches_subject_sender_and_body() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();

    let subject_message = message(
        &inbox,
        1,
        "thread-subject",
        "Analytical Engine notes",
        "Ada Lovelace",
        "ada@example.com",
        1_700_000_001,
    );
    let sender_message = message(
        &inbox,
        2,
        "thread-sender",
        "Meeting recap",
        "Grace Hopper",
        "grace@example.com",
        1_700_000_002,
    );
    let body_message = message(
        &inbox,
        3,
        "thread-body",
        "Weekend reading",
        "Someone Else",
        "someone@example.com",
        1_700_000_003,
    );
    for item in [&subject_message, &sender_message, &body_message] {
        seed_message(&store, item);
    }
    store
        .upsert_body(
            &body_message.id,
            &Body {
                plain_text: Some("The difference engine is fascinating.".to_string()),
                html: None,
                fetched_at: 1,
            },
        )
        .unwrap();

    let subject_hits = search(&store, "analytical", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(subject_hits.len(), 1);
    assert_eq!(subject_hits[0].thread_id, subject_message.thread_id);
    assert_eq!(subject_hits[0].sender, "Ada Lovelace");

    let sender_hits = search(&store, "grace", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(sender_hits.len(), 1);
    assert_eq!(sender_hits[0].thread_id, sender_message.thread_id);

    let body_hits = search(&store, "difference", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(body_hits.len(), 1);
    assert_eq!(body_hits[0].thread_id, body_message.thread_id);
}

#[test]
fn search_scope_limits_to_folder_or_account() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let personal = account("personal", "ada@example.com", 0);
    let work = account("work", "grace@example.com", 1);
    store.upsert_account(&personal).unwrap();
    store.upsert_account(&work).unwrap();

    let personal_inbox = folder("personal-inbox", &personal.id, "INBOX", FolderKind::Inbox);
    let personal_archive = folder(
        "personal-archive",
        &personal.id,
        "Archive",
        FolderKind::Archive,
    );
    let work_inbox = folder("work-inbox", &work.id, "INBOX", FolderKind::Inbox);
    for item in [&personal_inbox, &personal_archive, &work_inbox] {
        store.upsert_folder(item).unwrap();
    }

    seed_message(
        &store,
        &message(
            &personal_inbox,
            1,
            "p-inbox-thread",
            "Quarterly plan",
            "Ada",
            "ada@example.com",
            100,
        ),
    );
    seed_message(
        &store,
        &message(
            &personal_archive,
            1,
            "p-archive-thread",
            "Quarterly archive",
            "Ada",
            "ada@example.com",
            200,
        ),
    );
    seed_message(
        &store,
        &message(
            &work_inbox,
            1,
            "w-inbox-thread",
            "Quarterly work",
            "Grace",
            "grace@example.com",
            300,
        ),
    );

    let all = search(&store, "quarterly", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(all.len(), 3);

    let scoped_account = search(
        &store,
        "quarterly",
        &SearchScope::Account(personal.id.clone()),
        20,
    )
    .unwrap();
    assert_eq!(
        scoped_account
            .iter()
            .map(|hit| hit.thread_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["p-archive-thread", "p-inbox-thread"]
    );

    let scoped_folder = search(
        &store,
        "quarterly",
        &SearchScope::Folder(work_inbox.id.clone()),
        20,
    )
    .unwrap();
    assert_eq!(scoped_folder.len(), 1);
    assert_eq!(
        scoped_folder[0].thread_id,
        ThreadId("w-inbox-thread".into())
    );
}

#[test]
fn search_returns_one_newest_hit_per_thread_and_respects_limit() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();

    // Two matching messages in one thread: only the newest may be returned.
    seed_message(
        &store,
        &message(
            &inbox,
            1,
            "thread-a",
            "report alpha",
            "Ada",
            "ada@example.com",
            100,
        ),
    );
    seed_message(
        &store,
        &message(
            &inbox,
            2,
            "thread-a",
            "report beta",
            "Ada",
            "ada@example.com",
            200,
        ),
    );
    seed_message(
        &store,
        &message(
            &inbox,
            3,
            "thread-b",
            "report gamma",
            "Grace",
            "grace@example.com",
            300,
        ),
    );

    let hits = search(&store, "report", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].thread_id, ThreadId("thread-b".into()));
    assert_eq!(hits[1].thread_id, ThreadId("thread-a".into()));
    assert_eq!(hits[1].date, 200);

    let limited = search(&store, "report", &SearchScope::AllAccounts, 1).unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].thread_id, ThreadId("thread-b".into()));
}

#[test]
fn fts_query_escapes_operators() {
    assert_eq!(fts_query("foo\" OR *"), "\"foo\" \"OR\"");
    assert_eq!(fts_query("hello   world"), "\"hello\" \"world\"");
    assert_eq!(fts_query(""), "");
    assert_eq!(fts_query("  * \" ( )  "), "");
    assert_eq!(fts_query("col:value"), "\"colvalue\"");
}

#[test]
fn empty_query_returns_no_hits() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    seed_message(
        &store,
        &message(
            &inbox,
            1,
            "thread",
            "anything",
            "Ada",
            "ada@example.com",
            100,
        ),
    );

    assert!(
        search(&store, "", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
    assert!(
        search(&store, "   ", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
    assert!(
        search(&store, "* \" OR", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
    assert!(
        search(&store, "anything", &SearchScope::AllAccounts, 0)
            .unwrap()
            .is_empty()
    );
    // Punctuation-only terms must not turn into an FTS syntax error.
    assert!(
        search(&store, "-", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
    assert!(
        search(&store, "\"", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn migration_from_v1_creates_fts_and_backfills_existing_mail() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("pige.db");
    {
        let store = Store::open(&db).expect("open store");
        store.migrate().expect("migrate");
        let account = account("personal", "ada@example.com", 0);
        store.upsert_account(&account).unwrap();
        let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
        store.upsert_folder(&inbox).unwrap();
        let stored = message(
            &inbox,
            1,
            "thread",
            "Historical subject",
            "Ada Lovelace",
            "ada@example.com",
            100,
        );
        seed_message(&store, &stored);
        store
            .upsert_body(
                &stored.id,
                &Body {
                    plain_text: Some("A cached historical body".to_string()),
                    html: Some("<p>html only</p>".to_string()),
                    fetched_at: 1,
                },
            )
            .unwrap();
    }

    // Rewind the database to the v1 shape: no FTS table, version 1 recorded.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE message_fts;
             DROP TRIGGER IF EXISTS message_fts_message_delete;
             DROP TRIGGER IF EXISTS message_fts_thread_update;
             DROP TRIGGER IF EXISTS message_fts_body_delete;",
        )
        .unwrap();
        conn.execute(
            "UPDATE meta SET value = '1' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    }

    let store = Store::open(&db).expect("reopen store");
    store.migrate().expect("migrate v1 to v2");
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    let subject = search(&store, "historical", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(subject.len(), 1);
    assert_eq!(subject[0].thread_id, ThreadId("thread".into()));

    let body = search(&store, "cached", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(body.len(), 1);
}

#[test]
fn caching_a_body_indexes_its_text_and_never_raw_html() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    let stored = message(
        &inbox,
        1,
        "thread",
        "Plain subject",
        "Ada",
        "ada@example.com",
        100,
    );
    seed_message(&store, &stored);

    assert!(
        search(&store, "zebra", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );

    store
        .upsert_body(
            &stored.id,
            &Body {
                plain_text: Some("A zebra crossing".to_string()),
                html: Some("<p>A giraffe crossing</p>".to_string()),
                fetched_at: 1,
            },
        )
        .unwrap();
    assert_eq!(
        search(&store, "zebra", &SearchScope::AllAccounts, 20)
            .unwrap()
            .len(),
        1
    );
    assert!(
        search(&store, "giraffe", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );

    // Replacing the cached body replaces its index entry.
    store
        .upsert_body(
            &stored.id,
            &Body {
                plain_text: Some("A yak crossing".to_string()),
                html: None,
                fetched_at: 2,
            },
        )
        .unwrap();
    assert!(
        search(&store, "zebra", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        search(&store, "yak", &SearchScope::AllAccounts, 20)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn merging_threads_moves_search_hits_to_the_survivor() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();

    seed_message(
        &store,
        &message(
            &inbox,
            1,
            "thread-loser",
            "merged report one",
            "Ada",
            "ada@example.com",
            100,
        ),
    );
    seed_message(
        &store,
        &message(
            &inbox,
            2,
            "thread-winner",
            "merged report two",
            "Ada",
            "ada@example.com",
            200,
        ),
    );
    assert_eq!(
        search(&store, "merged", &SearchScope::AllAccounts, 20)
            .unwrap()
            .len(),
        2
    );

    store
        .merge_threads(
            &account.id,
            &ThreadId("thread-loser".into()),
            &ThreadId("thread-winner".into()),
        )
        .unwrap();

    let hits = search(&store, "merged", &SearchScope::AllAccounts, 20).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].thread_id, ThreadId("thread-winner".into()));
    assert_eq!(hits[0].date, 200);
}

#[test]
fn html_only_body_is_searchable_without_indexing_markup() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    let stored = message(
        &inbox,
        1,
        "thread",
        "HTML issue",
        "Ada",
        "ada@example.com",
        100,
    );
    seed_message(&store, &stored);

    store
        .upsert_body(
            &stored.id,
            &Body {
                plain_text: None,
                html: Some(
                    "<p>The <b>visible</b> needle</p><script>hiddenmarker</script>".to_string(),
                ),
                fetched_at: 1,
            },
        )
        .unwrap();

    assert_eq!(
        search(&store, "visible", &SearchScope::AllAccounts, 20)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        search(&store, "needle", &SearchScope::AllAccounts, 20)
            .unwrap()
            .len(),
        1
    );
    // Markup and stripped script content are not visible text.
    assert!(
        search(&store, "hiddenmarker", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
    assert!(
        search(&store, "b", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn deleting_a_message_removes_it_from_search() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    seed_message(
        &store,
        &message(
            &inbox,
            5,
            "thread",
            "Transient subject",
            "Ada",
            "ada@example.com",
            100,
        ),
    );
    assert_eq!(
        search(&store, "transient", &SearchScope::AllAccounts, 20)
            .unwrap()
            .len(),
        1
    );

    store.delete_messages_below_uid(&inbox.id, Uid(6)).unwrap();
    assert!(
        search(&store, "transient", &SearchScope::AllAccounts, 20)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn api_search_mail_returns_hits_and_updates_the_snapshot() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    api::reset_for_tests();
    api::init(Config {
        data_dir: data_dir.clone(),
        cache_dir: dir.path().join("cache"),
        config_dir: dir.path().join("config"),
        credentials: Arc::new(FakeCredentials::new()),
        notifier: Arc::new(FakeNotifier::new()),
        opener: Arc::new(FakeOpener::new()),
        transport: Arc::new(FakeTransportFactory::new()),
        backups: Arc::new(pige_core::platform::fake::FakeBackupStore::new()),
    })
    .expect("init api");

    let store = open_database(&data_dir);
    let account = account("personal", "ada@example.com", 0);
    store.upsert_account(&account).unwrap();
    let inbox = folder("inbox", &account.id, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    seed_message(
        &store,
        &message(
            &inbox,
            1,
            "thread",
            "Searchable from the API",
            "Ada",
            "ada@example.com",
            100,
        ),
    );

    let hits = api::search_mail("searchable", SearchScope::AllAccounts)
        .await
        .expect("api search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject, "Searchable from the API");

    let snapshot = api::snapshot();
    assert_eq!(snapshot.search_query, "searchable");
    assert_eq!(snapshot.search_scope, SearchScope::AllAccounts);
    assert_eq!(snapshot.search_results, hits);

    let empty = api::search_mail("", SearchScope::AllAccounts)
        .await
        .expect("empty api search");
    assert!(empty.is_empty());
    assert!(api::snapshot().search_results.is_empty());

    api::reset_for_tests();
}

fn open_database(data_dir: &Path) -> Store {
    let store = Store::open(data_dir.join(DB_FILE_NAME)).expect("open seeded store");
    store.migrate().expect("migrate seeded store");
    store
}
