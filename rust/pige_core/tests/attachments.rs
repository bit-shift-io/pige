//! Attachment extraction, filename hardening, cache containment, and the
//! platform-open path.

use std::collections::HashMap;
use std::sync::Arc;

use pige_core::api::{self, Config, DB_FILE_NAME};
use pige_core::mail::{
    AttachmentDisposition, AttachmentError, AttachmentPart, Folder, FolderId, FolderKind,
    MAX_ATTACHMENT_BYTES, MessageHeader, MessageId, StoredMessage, ThreadId, Uid, cache_attachment,
    check_attachment_size, extract_attachments, safe_attachment_filename,
};
use pige_core::platform::fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory};
use pige_core::store::Store;
use pige_core::transport::{FakeTransport, FolderState, RemoteFolder};
use pige_core::{Account, AccountId, AuthKind, Flags, Security, ServerConfig};
use tempfile::TempDir;
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

const WITH_ATTACHMENT: &[u8] = include_bytes!("fixtures/mime/with-attachment.eml");
const WITH_CID: &[u8] = include_bytes!("fixtures/mime/with-cid.eml");

#[test]
fn filename_cannot_escape_the_cache_directory() {
    assert_eq!(
        safe_attachment_filename("../../etc/passwd", "file"),
        "passwd"
    );
    assert_eq!(safe_attachment_filename("..\\..\\x.exe", "file"), "x.exe");
    assert_eq!(safe_attachment_filename("", "file"), "file");
    assert_eq!(safe_attachment_filename("..", "file"), "file");
    assert_eq!(safe_attachment_filename(".", "file"), "file");
    assert_eq!(
        safe_attachment_filename("bad\u{0}name\n.txt", "file"),
        "badname.txt"
    );
}

#[test]
fn attachment_metadata_is_extracted() {
    let parts = extract_attachments(WITH_ATTACHMENT);

    assert_eq!(parts.len(), 1, "only the PDF is a user attachment");
    let part = &parts[0];
    assert!(!part.part_id.is_empty());
    assert_eq!(part.filename, "report.pdf");
    assert_eq!(part.content_type, "application/pdf");
    assert_eq!(part.size, b"hello attachment\n".len() as u64);
    assert_eq!(part.content_id, None);
    assert_eq!(part.disposition, AttachmentDisposition::Attachment);
}

#[test]
fn cid_parts_are_not_listed_as_user_attachments() {
    assert!(
        extract_attachments(WITH_CID).is_empty(),
        "an inline image referenced by Content-ID belongs to the body"
    );
}

#[test]
fn oversized_attachment_is_rejected() {
    let error = check_attachment_size(MAX_ATTACHMENT_BYTES + 1).expect_err("must reject");
    assert!(matches!(
        error,
        AttachmentError::TooLarge {
            size,
            limit,
        } if size == MAX_ATTACHMENT_BYTES + 1 && limit == MAX_ATTACHMENT_BYTES
    ));
    check_attachment_size(MAX_ATTACHMENT_BYTES).expect("the cap itself is allowed");

    // The cache writer enforces the same cap before creating any file.
    let dir = TempDir::new().unwrap();
    let message = MessageId("message-1".to_string());
    let part = AttachmentPart {
        part_id: "2".to_string(),
        filename: "big.bin".to_string(),
        content_type: "application/octet-stream".to_string(),
        size: MAX_ATTACHMENT_BYTES + 1,
        content_id: None,
        disposition: AttachmentDisposition::Attachment,
    };
    let bytes = vec![0u8; (MAX_ATTACHMENT_BYTES + 1) as usize];
    let error = cache_attachment(dir.path(), &message, &part, &bytes).expect_err("must reject");
    assert!(matches!(error, AttachmentError::TooLarge { .. }));
    assert!(!dir.path().join("attachments").exists());
}

#[test]
fn cached_attachment_path_stays_inside_the_attachments_directory() {
    let dir = TempDir::new().unwrap();
    let message = MessageId("../../outside".to_string());
    let part = AttachmentPart {
        part_id: "../../2".to_string(),
        filename: "../../../../etc/passwd".to_string(),
        content_type: "text/plain".to_string(),
        size: 4,
        content_id: None,
        disposition: AttachmentDisposition::Attachment,
    };

    let path = cache_attachment(dir.path(), &message, &part, b"data").expect("cache attachment");

    let root = dir.path().join("attachments");
    assert!(path.starts_with(&root));
    assert!(!path.to_string_lossy().contains(".."));
    assert_eq!(path.file_name().unwrap(), "passwd");
    assert_eq!(path.parent().unwrap().file_name().unwrap(), "2");
    assert_eq!(
        path.parent()
            .unwrap()
            .parent()
            .unwrap()
            .file_name()
            .unwrap(),
        "outside"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"data");
}

struct Harness {
    dir: TempDir,
    opener: Arc<FakeOpener>,
    factory: Arc<FakeTransportFactory>,
    store: Store,
    account_id: AccountId,
    message: StoredMessage,
}

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
        sort_order: 0,
    }
}

fn folder(account_id: &AccountId) -> Folder {
    Folder {
        id: FolderId("folder-1".to_string()),
        account_id: account_id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 1,
        uidvalidity: Some(1),
        uidnext: Some(2),
    }
}

fn message(folder: &Folder, thread: &str) -> StoredMessage {
    StoredMessage {
        id: MessageId("message-1".to_string()),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: Uid(1),
        uidvalidity: 1,
        header: MessageHeader {
            message_id: Some("<report@example.com>".to_string()),
            in_reply_to: None,
            references: Vec::new(),
            subject: "Quarterly report".to_string(),
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            date: 1_700_000_000,
            size: WITH_ATTACHMENT.len() as u32,
        },
        flags: Flags::default(),
        thread_id: ThreadId(thread.to_string()),
        snippet: String::new(),
        // Header sync cannot see MIME attachments; the flag starts false.
        has_attachments: false,
    }
}

async fn harness(thread: &str) -> Harness {
    let dir = TempDir::new().unwrap();
    let opener = Arc::new(FakeOpener::default());
    let factory = Arc::new(FakeTransportFactory::default());

    api::reset_for_tests();
    api::init(Config {
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
        config_dir: dir.path().join("config"),
        credentials: Arc::new(FakeCredentials::new()),
        notifier: Arc::new(FakeNotifier::new()),
        opener: opener.clone(),
        transport: factory.clone(),
        backups: Arc::new(pige_core::platform::fake::FakeBackupStore::new()),
    })
    .expect("init api");

    let store = Store::open(dir.path().join("data").join(DB_FILE_NAME)).expect("open store");
    store.migrate().expect("migrate store");

    let account = account();
    store.upsert_account(&account).unwrap();
    let folder = folder(&account.id);
    store.upsert_folder(&folder).unwrap();
    let message = message(&folder, thread);
    store.upsert_message(&message).unwrap();

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
        bodies: HashMap::from([(
            "INBOX".to_string(),
            HashMap::from([(Uid(1), WITH_ATTACHMENT.to_vec())]),
        )]),
        ..FakeTransport::default()
    };
    factory.add_transport(account.id.clone(), transport);

    Harness {
        dir,
        opener,
        factory,
        store,
        account_id: account.id,
        message,
    }
}

#[tokio::test]
async fn open_conversation_exposes_attachment_metadata() {
    let _guard = TEST_LOCK.lock().await;
    let harness = harness("thread-attachments").await;

    let snapshot = api::open_conversation(&harness.message.thread_id)
        .await
        .expect("open conversation");
    let detail = snapshot
        .selected_conversation
        .expect("selected conversation");
    let latest = &detail.messages[0];

    assert_eq!(latest.attachments.len(), 1);
    assert_eq!(latest.attachments[0].filename, "report.pdf");
    assert!(latest.has_attachments);

    let stored = harness
        .store
        .attachments(&latest.id)
        .expect("read stored attachments");
    assert_eq!(stored, latest.attachments);

    // Storing metadata recomputes the flag that header sync wrote as false.
    let persisted = harness
        .store
        .message(&latest.id)
        .expect("read message")
        .expect("message row");
    assert!(persisted.has_attachments);
}

#[tokio::test]
async fn storing_attachment_metadata_updates_the_message_flag() {
    let _guard = TEST_LOCK.lock().await;
    let harness = harness("thread-flag").await;
    let id = &harness.message.id;

    let before = harness
        .store
        .message(id)
        .expect("read message")
        .expect("row");
    assert!(!before.has_attachments);

    let parts = extract_attachments(WITH_ATTACHMENT);
    harness
        .store
        .store_attachments(id, &parts)
        .expect("store attachments");
    let with_attachments = harness
        .store
        .message(id)
        .expect("read message")
        .expect("row");
    assert!(with_attachments.has_attachments);

    // Removing the last part clears the flag in the same transaction.
    harness
        .store
        .store_attachments(id, &[])
        .expect("clear attachments");
    let without_attachments = harness
        .store
        .message(id)
        .expect("read message")
        .expect("row");
    assert!(!without_attachments.has_attachments);
    assert!(
        harness
            .store
            .attachments(id)
            .expect("read attachments")
            .is_empty()
    );
}

#[tokio::test]
async fn open_attachment_uses_the_platform_opener_and_caches_the_file() {
    let _guard = TEST_LOCK.lock().await;
    let harness = harness("thread-open").await;
    let part = extract_attachments(WITH_ATTACHMENT).remove(0);

    let path = api::open_attachment(&harness.message.id, &part.part_id)
        .await
        .expect("open attachment");

    let root = harness.dir.path().join("cache").join("attachments");
    assert!(path.starts_with(&root), "cache file escaped {root:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"hello attachment\n");
    assert_eq!(harness.opener.opened_files(), vec![path.clone()]);

    let stored = harness
        .store
        .attachment(&harness.message.id, &part.part_id)
        .expect("read attachment")
        .expect("attachment metadata");
    assert_eq!(stored.cached_path.as_deref(), Some(path.as_path()));

    // A second open reuses the cached bytes and never touches the network.
    let again = api::open_attachment(&harness.message.id, &part.part_id)
        .await
        .expect("reopen attachment");
    assert_eq!(again, path);
    let transport = harness
        .factory
        .transport(&harness.account_id)
        .await
        .expect("shared transport");
    assert_eq!(transport.fetch_body_calls.len(), 1);
    assert_eq!(harness.opener.opened_files().len(), 2);
}
