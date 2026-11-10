//! Task F4 storage tests: durable remote identity and multi-collection
//! membership, including the v4 → current migration and its rollback.

mod common;

use std::path::Path;
use std::time::Duration;

use common::legacy::LegacyV4;
use pige_core::providers::{
    ChangePage, CollectionRecord, GMAIL_PROVIDER, IMAP_SMTP_PROVIDER, JobKey, JobKind, MailChange,
    MessageRecord, OperationContext, ProviderCursor, RemoteCollectionId, RemoteMessageId,
};
use pige_core::store::{ImapCoords, clear_migration_fault, inject_migration_fault};
use pige_core::{
    Account, AccountId, Flags, FolderId, FolderKind, MailBackend, MessageHeader, MessageId,
    SCHEMA_VERSION, Store, StoreError, ThreadId, Uid,
};
use rusqlite::Connection;
use tempfile::TempDir;

fn gmail_account(id: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: format!("{id}@example.com"),
        display_name: id.to_string(),
        backend: MailBackend::gmail(id),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn imap_account(id: &str) -> Account {
    use pige_core::account::{Security, ServerConfig};
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: format!("{id}@example.com"),
        display_name: id.to_string(),
        backend: MailBackend::imap_smtp(
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

fn raw(path: &Path) -> Connection {
    Connection::open(path).unwrap()
}

fn header(rfc_id: Option<&str>, subject: &str) -> MessageHeader {
    MessageHeader {
        message_id: rfc_id.map(str::to_string),
        in_reply_to: None,
        references: Vec::new(),
        subject: subject.to_string(),
        from: vec![pige_core::Address::new(None, "sender@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: 1_700_000_000,
        size: 128,
    }
}

fn collection(remote: &str, name: &str, kind: FolderKind) -> CollectionRecord {
    CollectionRecord {
        remote_id: RemoteCollectionId::new(remote),
        name: name.to_string(),
        kind,
        delimiter: Some("/".to_string()),
        total: Some(1),
        unread: Some(0),
    }
}

fn context(account: &AccountId) -> OperationContext {
    OperationContext::with_timeout(account.clone(), 1, Duration::from_secs(30))
}

fn job(account: &AccountId) -> JobKey {
    JobKey {
        account: account.clone(),
        kind: JobKind::MailSync,
        source: "sync".to_string(),
    }
}

/// A v4 database's local ids survive, and every dependent — body, attachment,
/// membership, and FTS hit — still points at them.
#[test]
fn v4_ids_and_dependents_survive() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let fixture = LegacyV4::create(&path);
    fixture
        .account("acc-1", "ada@example.com", 0)
        .folder("inbox", "acc-1", "INBOX", "inbox", 7)
        .message(
            "msg-1",
            "acc-1",
            "inbox",
            1,
            7,
            Some("<dup@example.com>"),
            "First",
            "thread-1",
            1,
        )
        // Same RFC Message-ID as msg-1: must not collapse.
        .message(
            "msg-2",
            "acc-1",
            "inbox",
            2,
            7,
            Some("<dup@example.com>"),
            "Second",
            "thread-2",
            2,
        )
        // No RFC Message-ID at all.
        .message(
            "msg-3", "acc-1", "inbox", 3, 7, None, "Third", "thread-3", 3,
        )
        .body("msg-1", "unique body alpha")
        .body("msg-3", "unique body gamma")
        .attachment("att-1", "msg-2", "2", "report.pdf")
        .outbox("out-1", "acc-1", "draft")
        .outbox("out-2", "acc-1", "queued")
        .outbox("out-3", "acc-1", "sending")
        .outbox("out-4", "acc-1", "sent")
        .outbox("out-5", "acc-1", "failed")
        .fts("msg-1", "acc-1", "inbox", "First", "unique body alpha")
        .fts("msg-3", "acc-1", "inbox", "Third", "unique body gamma");
    let outbox_before = fixture.count("SELECT COUNT(*) FROM outbox");
    assert!(fixture.has_column("messages", "folder_id"));

    let store = fixture.into_store();
    store.migrate().expect("migrate v4 to current");

    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    // Every local id and its header survive verbatim.
    for (id, subject, thread) in [
        ("msg-1", "First", "thread-1"),
        ("msg-2", "Second", "thread-2"),
        ("msg-3", "Third", "thread-3"),
    ] {
        let stored = store
            .message(&MessageId(id.to_string()))
            .unwrap()
            .unwrap_or_else(|| panic!("{id} must survive"));
        assert_eq!(stored.header.subject, subject);
        assert_eq!(stored.thread_id, ThreadId(thread.to_string()));
        assert_eq!(
            store.message_memberships(&stored.id).unwrap(),
            vec![FolderId("inbox".to_string())]
        );
    }

    // Bodies and attachments keep their exact MessageId references + content.
    assert_eq!(
        store
            .body(&MessageId("msg-1".to_string()))
            .unwrap()
            .unwrap()
            .plain_text
            .as_deref(),
        Some("unique body alpha")
    );
    assert_eq!(
        store
            .attachments(&MessageId("msg-2".to_string()))
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .body(&MessageId("msg-2".to_string()))
            .unwrap()
            .is_none()
    );

    // FTS still finds the same content under the same ids.
    let hits = pige_core::search::search(&store, "alpha", &pige_core::SearchScope::AllAccounts, 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].thread_id, ThreadId("thread-1".to_string()));

    // Timestamps and outbox rows are untouched.
    let conn = raw(&path);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM outbox", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        outbox_before
    );
    let has_old_folder: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'folder_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_old_folder, 0, "messages must no longer carry folder_id");
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

/// The same remote id seen by two accounts stays two distinct local messages.
#[test]
fn same_remote_id_across_accounts_is_distinct() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let fixture = LegacyV4::create(&path);
    fixture
        .account("acc-1", "ada@example.com", 0)
        .account("acc-2", "grace@example.com", 1)
        .folder("inbox-1", "acc-1", "INBOX", "inbox", 1)
        .folder("inbox-2", "acc-2", "INBOX", "inbox", 1)
        .message(
            "a-1",
            "acc-1",
            "inbox-1",
            1,
            1,
            Some("<shared@example.com>"),
            "A",
            "ta",
            1,
        )
        .message(
            "b-1",
            "acc-2",
            "inbox-2",
            1,
            1,
            Some("<shared@example.com>"),
            "B",
            "tb",
            2,
        );

    let store = fixture.into_store();
    store.migrate().unwrap();

    let a = MessageId("a-1".to_string());
    let b = MessageId("b-1".to_string());
    let acc1 = AccountId("acc-1".to_string());
    let acc2 = AccountId("acc-2".to_string());

    // Both backfilled to the identical IMAP coordinate tuple...
    assert_eq!(
        store.remote_message(&acc1, &a).unwrap(),
        store.remote_message(&acc2, &b).unwrap()
    );
    // ...but each account resolves to its own local message.
    let remote = store.remote_message(&acc1, &a).unwrap();
    assert_eq!(
        store
            .remote_message_for(&acc1, IMAP_SMTP_PROVIDER, &remote)
            .unwrap(),
        Some(a)
    );
    assert_eq!(
        store
            .remote_message_for(&acc2, IMAP_SMTP_PROVIDER, &remote)
            .unwrap(),
        Some(b)
    );
}

/// One native message can belong to several labels, and removing a label never
/// touches the content or its body.
#[test]
fn two_labels_share_one_native_message() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let store = Store::open(&path).unwrap();
    store.migrate().unwrap();
    let account = gmail_account("acc");
    store.upsert_account(&account).unwrap();

    let label_a = collection("LabelA", "Label A", FolderKind::Other);
    let label_b = collection("LabelB", "Label B", FolderKind::Other);
    let record = MessageRecord {
        remote_id: RemoteMessageId::new("native-1"),
        remote_thread: None,
        header: header(Some("<n1@example.com>"), "Native"),
        flags: Flags::default(),
        collections: vec![label_a.remote_id.clone(), label_b.remote_id.clone()],
        snippet: "native snippet".to_string(),
        has_attachments: false,
    };
    store
        .apply_change_page(
            &context(&account.id),
            &job(&account.id),
            GMAIL_PROVIDER,
            None,
            &ChangePage {
                changes: vec![
                    MailChange::UpsertCollection(label_a.clone()),
                    MailChange::UpsertCollection(label_b.clone()),
                    MailChange::UpsertMessage {
                        message: record.clone(),
                        is_new_arrival: true,
                    },
                ],
                checkpoint: ProviderCursor::new(1, "page-1"),
                has_more: false,
            },
            None,
        )
        .unwrap();

    let id = store
        .remote_message_for(&account.id, GMAIL_PROVIDER, &record.remote_id)
        .unwrap()
        .expect("native message mapped");
    let folder_a = store
        .remote_collection_for(&account.id, GMAIL_PROVIDER, &label_a.remote_id)
        .unwrap()
        .unwrap();
    let folder_b = store
        .remote_collection_for(&account.id, GMAIL_PROVIDER, &label_b.remote_id)
        .unwrap()
        .unwrap();

    let memberships = store.message_memberships(&id).unwrap();
    assert_eq!(memberships.len(), 2);
    assert!(memberships.contains(&folder_a) && memberships.contains(&folder_b));
    // One message, counted once per folder, not duplicated.
    assert_eq!(store.messages(&folder_a, 10, None).unwrap().len(), 1);
    assert_eq!(store.messages(&folder_b, 10, None).unwrap().len(), 1);

    // Give it a body, then remove one label membership.
    store
        .upsert_body(
            &id,
            &pige_core::store::Body {
                plain_text: Some("native body".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    store
        .apply_change_page(
            &context(&account.id),
            &job(&account.id),
            GMAIL_PROVIDER,
            Some(&ProviderCursor::new(1, "page-1")),
            &ChangePage {
                changes: vec![MailChange::SetMembership {
                    message: record.remote_id.clone(),
                    collection: label_a.remote_id.clone(),
                    present: false,
                }],
                checkpoint: ProviderCursor::new(1, "page-2"),
                has_more: false,
            },
            None,
        )
        .unwrap();

    assert_eq!(
        store.message_memberships(&id).unwrap(),
        vec![folder_b.clone()],
        "label A must be gone"
    );
    assert!(
        store.body(&id).unwrap().is_some(),
        "removing a label must keep the body"
    );
    assert_eq!(
        store.messages(&folder_b, 10, None).unwrap().len(),
        1,
        "the other label keeps the message"
    );
    assert!(store.messages(&folder_a, 10, None).unwrap().is_empty());
}

/// Two independent messages may share an RFC Message-ID; that header is not
/// identity.
#[test]
fn same_rfc_id_is_not_identity() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let store = Store::open(&path).unwrap();
    store.migrate().unwrap();
    let account = gmail_account("acc");
    store.upsert_account(&account).unwrap();
    let folder = FolderId("folder".to_string());
    store
        .upsert_folder(&pige_core::Folder {
            id: folder.clone(),
            account_id: account.id.clone(),
            name: "All".to_string(),
            delimiter: None,
            kind: FolderKind::Other,
            unread: 0,
            total: 0,
            uidvalidity: None,
            uidnext: None,
        })
        .unwrap();

    for (id, subject) in [("m-1", "One"), ("m-2", "Two")] {
        let mut h = header(Some("<same@example.com>"), subject);
        h.date = i64::from(id.len() as u32);
        store
            .upsert_message(&pige_core::StoredMessage {
                id: MessageId(id.to_string()),
                account_id: account.id.clone(),
                folder_id: Some(folder.clone()),
                uid: None,
                uidvalidity: None,
                header: h,
                flags: Flags::default(),
                thread_id: ThreadId(format!("t-{id}")),
                snippet: subject.to_string(),
                has_attachments: false,
            })
            .unwrap();
    }

    // Both messages exist, in the same folder, with the same RFC Message-ID.
    assert_eq!(store.messages(&folder, 10, None).unwrap().len(), 2);
    for id in ["m-1", "m-2"] {
        let stored = store.message(&MessageId(id.to_string())).unwrap().unwrap();
        assert_eq!(
            stored.header.message_id.as_deref(),
            Some("<same@example.com>")
        );
    }
}

/// A failed migration leaves the v4 database exactly as it was.
#[test]
fn migration_rollback_reopens_v4() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let fixture = LegacyV4::create(&path);
    fixture
        .account("acc-1", "ada@example.com", 0)
        .folder("inbox", "acc-1", "INBOX", "inbox", 7)
        .message(
            "msg-1",
            "acc-1",
            "inbox",
            1,
            7,
            Some("<a@example.com>"),
            "One",
            "t1",
            1,
        )
        .body("msg-1", "rollback body")
        .attachment("att-1", "msg-1", "2", "file.bin");

    inject_migration_fault(1);
    let store = fixture.into_store();
    let result = store.migrate();
    clear_migration_fault();
    assert!(
        result.is_err(),
        "the injected fault must fail the migration"
    );

    // The v4 schema and its data are intact: same version, old columns, same
    // body and attachment, all through the raw v4 connection.
    assert_eq!(store.schema_version().unwrap(), 4);
    let conn = raw(&path);
    let still_folder_id: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'folder_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        still_folder_id, 1,
        "rollback must restore the old messages table"
    );
    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(messages, 1);
    let bodies: i64 = conn
        .query_row("SELECT COUNT(*) FROM bodies", [], |row| row.get(0))
        .unwrap();
    assert_eq!(bodies, 1);
    let attachments: i64 = conn
        .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
        .unwrap();
    assert_eq!(attachments, 1);
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );

    // Clearing the fault lets the same database migrate cleanly.
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
}

/// A database written by a newer binary is rejected, not modified.
#[test]
fn newer_schema_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let store = Store::open(&path).unwrap();
    store.migrate().unwrap();
    {
        let conn = raw(&path);
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            [(SCHEMA_VERSION + 1).to_string()],
        )
        .unwrap();
    }
    assert!(matches!(
        store.migrate(),
        Err(StoreError::SchemaTooNew { .. })
    ));
}

/// Removing one membership preserves the content and its body.
#[test]
fn membership_removal_keeps_body() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let store = Store::open(&path).unwrap();
    store.migrate().unwrap();
    let account = gmail_account("acc");
    store.upsert_account(&account).unwrap();
    let a = FolderId("a".to_string());
    let b = FolderId("b".to_string());
    for folder in [&a, &b] {
        store
            .upsert_folder(&pige_core::Folder {
                id: folder.clone(),
                account_id: account.id.clone(),
                name: folder.0.clone(),
                delimiter: None,
                kind: FolderKind::Other,
                unread: 0,
                total: 0,
                uidvalidity: None,
                uidnext: None,
            })
            .unwrap();
    }
    let id = MessageId("m".to_string());
    store
        .upsert_message(&pige_core::StoredMessage {
            id: id.clone(),
            account_id: account.id.clone(),
            folder_id: Some(a.clone()),
            uid: None,
            uidvalidity: None,
            header: header(None, "Both"),
            flags: Flags::default(),
            thread_id: ThreadId("t".to_string()),
            snippet: String::new(),
            has_attachments: false,
        })
        .unwrap();
    store.add_membership(&id, &b).unwrap();
    store
        .upsert_body(
            &id,
            &pige_core::store::Body {
                plain_text: Some("keep me".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    store.remove_membership(&id, &a).unwrap();

    assert_eq!(store.message_memberships(&id).unwrap(), vec![b]);
    assert!(store.message(&id).unwrap().is_some());
    assert_eq!(
        store.body(&id).unwrap().unwrap().plain_text.as_deref(),
        Some("keep me")
    );
}

/// A page's changes and its checkpoint commit together, or not at all.
#[test]
fn page_and_cursor_commit_atomically() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let store = Store::open(&path).unwrap();
    store.migrate().unwrap();
    let account = gmail_account("acc");
    store.upsert_account(&account).unwrap();
    let key = job(&account.id);

    // A valid first page establishes the collection mapping.
    store
        .apply_change_page(
            &context(&account.id),
            &key,
            GMAIL_PROVIDER,
            None,
            &ChangePage {
                changes: vec![MailChange::UpsertCollection(collection(
                    "LabelA",
                    "Label A",
                    FolderKind::Other,
                ))],
                checkpoint: ProviderCursor::new(1, "cp-1"),
                has_more: true,
            },
            None,
        )
        .unwrap();

    // A page whose second change references an unknown collection must roll the
    // first change back and must not advance the checkpoint. (An unknown message
    // id is deliberately a no-op, because history replays late ranges; see
    // `history_unknown_ids_are_idempotent` in the Google adapter tests.)
    let failed = store.apply_change_page(
        &context(&account.id),
        &key,
        GMAIL_PROVIDER,
        Some(&ProviderCursor::new(1, "cp-1")),
        &ChangePage {
            changes: vec![
                MailChange::UpsertMessage {
                    message: MessageRecord {
                        remote_id: RemoteMessageId::new("native-x"),
                        remote_thread: None,
                        header: header(Some("<x@example.com>"), "X"),
                        flags: Flags::default(),
                        collections: vec![RemoteCollectionId::new("LabelA")],
                        snippet: String::new(),
                        has_attachments: false,
                    },
                    is_new_arrival: false,
                },
                MailChange::SetMembership {
                    message: RemoteMessageId::new("native-x"),
                    collection: RemoteCollectionId::new("LabelMissing"),
                    present: true,
                },
            ],
            checkpoint: ProviderCursor::new(1, "cp-bad"),
            has_more: false,
        },
        None,
    );
    assert!(matches!(
        failed,
        Err(StoreError::UnknownRemoteCollection(_))
    ));
    assert!(
        store
            .remote_message_for(
                &account.id,
                GMAIL_PROVIDER,
                &RemoteMessageId::new("native-x")
            )
            .unwrap()
            .is_none(),
        "the rolled-back upsert must not persist"
    );
    // Checkpoint is still the first page's.
    assert_eq!(
        store.checkpoint(&key).unwrap().unwrap().cursor,
        ProviderCursor::new(1, "cp-1")
    );

    // The corrected page commits both the message and its checkpoint.
    store
        .apply_change_page(
            &context(&account.id),
            &key,
            GMAIL_PROVIDER,
            Some(&ProviderCursor::new(1, "cp-1")),
            &ChangePage {
                changes: vec![MailChange::UpsertMessage {
                    message: MessageRecord {
                        remote_id: RemoteMessageId::new("native-x"),
                        remote_thread: None,
                        header: header(Some("<x@example.com>"), "X"),
                        flags: Flags::default(),
                        collections: vec![RemoteCollectionId::new("LabelA")],
                        snippet: String::new(),
                        has_attachments: false,
                    },
                    is_new_arrival: false,
                }],
                checkpoint: ProviderCursor::new(1, "cp-2"),
                has_more: false,
            },
            None,
        )
        .unwrap();
    assert!(
        store
            .remote_message_for(
                &account.id,
                GMAIL_PROVIDER,
                &RemoteMessageId::new("native-x")
            )
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store.checkpoint(&key).unwrap().unwrap().cursor,
        ProviderCursor::new(1, "cp-2")
    );
}

/// A provider collection upsert must not clobber the IMAP-owned folder state:
/// an omitted counter leaves the rollup alone, and `UIDVALIDITY`/`UIDNEXT` are
/// never rewritten, so no spurious epoch reset happens on the sync path.
#[test]
fn collection_upsert_preserves_folder_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let store = Store::open(&path).unwrap();
    store.migrate().unwrap();
    let account = imap_account("acc");
    store.upsert_account(&account).unwrap();

    let folder_id = FolderId("inbox".to_string());
    store
        .upsert_folder(&pige_core::Folder {
            id: folder_id.clone(),
            account_id: account.id.clone(),
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
            unread: 5,
            total: 9,
            uidvalidity: Some(42),
            uidnext: Some(100),
        })
        .unwrap();

    let key = job(&account.id);
    let record = |remote: &str, unread: Option<u32>, total: Option<u32>| CollectionRecord {
        remote_id: RemoteCollectionId::new(remote),
        name: "INBOX".to_string(),
        kind: FolderKind::Inbox,
        delimiter: Some("/".to_string()),
        total,
        unread,
    };

    // Counters omitted: the existing rollups and sync state must survive.
    store
        .apply_change_page(
            &context(&account.id),
            &key,
            IMAP_SMTP_PROVIDER,
            None,
            &ChangePage {
                changes: vec![MailChange::UpsertCollection(record("INBOX", None, None))],
                checkpoint: ProviderCursor::new(1, "cp-1"),
                has_more: false,
            },
            None,
        )
        .unwrap();

    let folder = store
        .folders(&account.id)
        .unwrap()
        .into_iter()
        .find(|f| f.id == folder_id)
        .expect("folder survives a collection upsert");
    assert_eq!(folder.unread, 5, "omitted unread must preserve the rollup");
    assert_eq!(folder.total, 9, "omitted total must preserve the rollup");
    assert_eq!(
        folder.uidvalidity,
        Some(42),
        "UIDVALIDITY must not be reset"
    );
    assert_eq!(folder.uidnext, Some(100), "UIDNEXT must not be cleared");

    // Counters reported: they update, but the IMAP epoch is still untouched.
    store
        .apply_change_page(
            &context(&account.id),
            &key,
            IMAP_SMTP_PROVIDER,
            Some(&ProviderCursor::new(1, "cp-1")),
            &ChangePage {
                changes: vec![MailChange::UpsertCollection(record(
                    "INBOX",
                    Some(2),
                    Some(3),
                ))],
                checkpoint: ProviderCursor::new(1, "cp-2"),
                has_more: false,
            },
            None,
        )
        .unwrap();
    let folder = store
        .folders(&account.id)
        .unwrap()
        .into_iter()
        .find(|f| f.id == folder_id)
        .unwrap();
    assert_eq!(folder.unread, 2);
    assert_eq!(folder.total, 3);
    assert_eq!(
        folder.uidvalidity,
        Some(42),
        "UIDVALIDITY must not be reset"
    );
    assert_eq!(folder.uidnext, Some(100), "UIDNEXT must not be cleared");
}

/// An IMAP sync mapping for a Gmail-typed account is keyed by the transport
/// (`imap-smtp`), so it matches the sync engine's own write instead of leaving
/// two mapping rows for one message.
#[test]
fn imap_mapping_is_keyed_by_transport() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pige.db");
    let store = Store::open(&path).unwrap();
    store.migrate().unwrap();
    // A Gmail-typed account that is nevertheless still syncing over IMAP.
    let account = gmail_account("acc");
    store.upsert_account(&account).unwrap();

    let folder_id = FolderId("inbox".to_string());
    store
        .upsert_folder(&pige_core::Folder {
            id: folder_id.clone(),
            account_id: account.id.clone(),
            name: "INBOX".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Inbox,
            unread: 0,
            total: 0,
            uidvalidity: None,
            uidnext: None,
        })
        .unwrap();

    let id = MessageId("m-1".to_string());
    store
        .upsert_message(&pige_core::StoredMessage {
            id: id.clone(),
            account_id: account.id.clone(),
            folder_id: Some(folder_id.clone()),
            uid: Some(Uid(7)),
            uidvalidity: Some(1),
            header: header(Some("<m1@example.com>"), "One"),
            flags: Flags::default(),
            thread_id: ThreadId("t-1".to_string()),
            snippet: String::new(),
            has_attachments: false,
        })
        .unwrap();

    let coords = ImapCoords::new("INBOX", 1, Uid(7));
    let remote = coords.encode();
    assert_eq!(
        store
            .remote_message_for(&account.id, IMAP_SMTP_PROVIDER, &remote)
            .unwrap(),
        Some(id.clone()),
        "the mapping must live under the transport's backend key"
    );
    assert_eq!(
        store
            .remote_message_for(&account.id, GMAIL_PROVIDER, &remote)
            .unwrap(),
        None,
        "no mapping may be filed under the account's configured backend"
    );
    assert_eq!(store.imap_coords(&id).unwrap(), Some(coords));

    // The sync engine's own mapping write upserts onto the same row.
    store
        .set_remote_message(&account.id, IMAP_SMTP_PROVIDER, &remote, &id)
        .unwrap();
    let conn = raw(&path);
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM remote_messages WHERE account_id = ?1 AND message_id = ?2",
            rusqlite::params![account.id.0, id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "one message must keep exactly one mapping row");
}
