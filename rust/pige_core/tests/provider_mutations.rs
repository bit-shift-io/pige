//! Integration tests for the provider-routed API: mailbox reads, mutations, and
//! the scheduler gate that serializes them. A scripted [`FakeProviderFactory`]
//! stands in for a real provider so every remote outcome (applied, rejected,
//! uncertain, unreachable) is deterministic.
//!
//! A single global state backs the API, so these serialize on `TEST_LOCK` and
//! re-initialize per test, mirroring the other API suites.

use std::sync::Arc;

use pige_core::{
    Account, AccountId, Address, Flags, Folder, FolderId, FolderKind, MailBackend, MessageHeader,
    MessageId, Security, ServerConfig, Store, StoredMessage, ThreadId, Uid,
    api::{self, AccountInput, Config, DB_FILE_NAME},
    mail::AttachmentDisposition,
    platform::{
        TransportFactory,
        fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    },
    providers::{
        AttachmentDescriptor, BodyContent, ChangePage, CollectionRecord, MailChange, MessageRecord,
        MutationCapabilities, MutationItemResult, MutationOutcome, MutationResult,
        ProviderCapabilities, ProviderCursor, ProviderError, RemoteAttachmentId,
        RemoteCollectionId, RemoteMessageId, SignatureStatus,
        fake::{FakeProviderFactory, FakeProviderSession, FakeRecorder},
    },
    store::ImapCoords,
};
use tempfile::TempDir;
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

struct Harness {
    dir: TempDir,
    providers: Arc<FakeProviderFactory>,
}

impl Harness {
    fn new(factory: FakeProviderFactory) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let providers = Arc::new(factory);
        let credentials = Arc::new(FakeCredentials::default());
        let transport: Arc<dyn TransportFactory> = Arc::new(FakeTransportFactory::new());

        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            config_dir: dir.path().join("config"),
            providers: providers.clone(),
            credentials,
            notifier: Arc::new(FakeNotifier::default()),
            opener: Arc::new(FakeOpener::default()),
            transport,
            backups: Arc::new(pige_core::platform::fake::FakeBackupStore::new()),
        })
        .expect("init api");

        Self { dir, providers }
    }

    fn recorder(&self) -> &FakeRecorder {
        self.providers.recorder()
    }

    fn store(&self) -> Store {
        let store =
            Store::open(self.dir.path().join("data").join(DB_FILE_NAME)).expect("open store");
        store.migrate().expect("migrate store");
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

async fn add_account() -> AccountId {
    api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("add account")
        .accounts[0]
        .id
        .clone()
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

fn header(uid: u32) -> MessageHeader {
    MessageHeader {
        message_id: Some(format!("<{uid}@example.com>")),
        in_reply_to: None,
        references: Vec::new(),
        subject: format!("Subject {uid}"),
        from: vec![Address::new(None, "sender@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: i64::from(uid),
        size: 64,
    }
}

/// Seed one message in `folder`. Because it carries both a folder and IMAP
/// coordinates, `upsert_message` also records its `(mailbox, UIDVALIDITY, UID)`
/// remote identity, which is exactly what the API names a mutation or body read
/// with.
fn seed_message(store: &Store, folder: &Folder, uid: u32, thread: &str) -> MessageId {
    let message = StoredMessage {
        id: MessageId(format!("{}-message-{uid}", folder.id.0)),
        account_id: folder.account_id.clone(),
        folder_id: Some(folder.id.clone()),
        uid: Some(Uid(uid)),
        uidvalidity: Some(1),
        header: header(uid),
        flags: Flags::default(),
        thread_id: ThreadId(thread.to_string()),
        snippet: String::new(),
        has_attachments: false,
    };
    store.upsert_message(&message).unwrap();
    message.id
}

/// The remote identity the API derives for `(INBOX, 1, uid)`.
fn remote(uid: u32) -> RemoteMessageId {
    ImapCoords::new("INBOX", 1, Uid(uid)).encode()
}

fn collection(name: &str, kind: FolderKind) -> CollectionRecord {
    CollectionRecord {
        remote_id: RemoteCollectionId::from(name),
        name: name.to_string(),
        kind,
        delimiter: Some("/".to_string()),
        total: Some(1),
        unread: None,
    }
}

fn record(remote_id: RemoteMessageId, collections: Vec<RemoteCollectionId>) -> MessageRecord {
    MessageRecord {
        remote_id,
        remote_thread: None,
        header: header(1),
        flags: Flags::default(),
        collections,
        snippet: String::new(),
        has_attachments: false,
    }
}

/// Two accounts with different backends both open a provider session; the API
/// never branches on the account's configured backend to reach the network.
#[tokio::test]
async fn mixed_accounts_open_without_protocol_branch() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new(FakeProviderFactory::new());

    let imap = add_account().await;
    // A native Gmail account, present only in the account mirror, as an
    // upgraded configuration would leave it.
    let gmail = Account {
        id: AccountId("gmail-native".to_string()),
        label: "Gmail".to_string(),
        email: "gmail@example.com".to_string(),
        display_name: "Gmail".to_string(),
        backend: MailBackend::gmail("user-1"),
        contact_sources: Vec::new(),
        sort_order: 1,
        color: None,
    };
    harness.store().upsert_account(&gmail).unwrap();

    let snapshot = api::sync_all().await.expect("sync both accounts");
    assert_eq!(
        harness.recorder().opens(),
        2,
        "both accounts open a provider session, regardless of backend"
    );
    for id in [&imap, &gmail.id] {
        let section = snapshot
            .sections
            .iter()
            .find(|section| &section.account_id == id)
            .unwrap_or_else(|| panic!("section for {id}"));
        assert!(
            section.capabilities.mailbox,
            "provider-neutral capabilities are recorded for {id}"
        );
    }
}

/// A message that belongs to two collections is one distinct message, counted
/// once in each collection and once account-wide.
#[tokio::test]
async fn label_memberships_count_distinct_messages() {
    let _guard = TEST_LOCK.lock().await;
    let inbox = collection("INBOX", FolderKind::Inbox);
    let work = collection("Work", FolderKind::Other);
    let page = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(inbox.clone()),
            MailChange::UpsertCollection(work.clone()),
            MailChange::UpsertMessage {
                message: record(
                    remote(1),
                    vec![inbox.remote_id.clone(), work.remote_id.clone()],
                ),
                is_new_arrival: true,
            },
        ],
        checkpoint: ProviderCursor::new(1, "page-1"),
        has_more: false,
    };
    let factory = FakeProviderFactory::new().session(FakeProviderSession::new().change_page(page));
    let harness = Harness::new(factory);
    let account = add_account().await;

    api::sync_all().await.expect("sync");

    let store = harness.store();
    let folders = store.folders(&account).unwrap();
    let inbox_id = folders
        .iter()
        .find(|f| f.name == "INBOX")
        .unwrap()
        .id
        .clone();
    let work_id = folders
        .iter()
        .find(|f| f.name == "Work")
        .unwrap()
        .id
        .clone();

    assert_eq!(
        store.messages(&inbox_id, 10, None).unwrap().len(),
        1,
        "the label's inbox membership counts one message"
    );
    assert_eq!(
        store.messages(&work_id, 10, None).unwrap().len(),
        1,
        "the second label counts the same one message, not a duplicate"
    );
    assert_eq!(
        store.messages_for_account(&account).unwrap().len(),
        1,
        "the account-wide view counts the message once"
    );
    assert_eq!(
        store.conversations(&work_id, 10, None).unwrap().len(),
        1,
        "the label's conversation list is distinct"
    );
}

/// A body fetch records the provider's signature trust state on the cached
/// message.
#[tokio::test]
async fn body_fetch_preserves_signature_status() {
    let _guard = TEST_LOCK.lock().await;
    let body = BodyContent {
        plain_text: Some("signed body".to_string()),
        html: None,
        raw: None,
        attachments: Vec::new(),
        signature: SignatureStatus::Verified,
    };
    let factory = FakeProviderFactory::new().session(FakeProviderSession::new().body(body));
    let harness = Harness::new(factory);
    let account = add_account().await;

    let store = harness.store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    let message = seed_message(&store, &inbox, 1, "thread");
    api::select_folder(&inbox.id).await.expect("select inbox");

    let snapshot = api::open_conversation(&ThreadId("thread".into()))
        .await
        .expect("open conversation");
    assert!(snapshot.selected_conversation.is_some());

    assert_eq!(
        harness.store().signature_status(&message).unwrap(),
        SignatureStatus::Verified,
        "the provider's signature state is persisted with the body"
    );
}

/// A provider that does not support permanent delete is refused before any
/// mutation is attempted.
#[tokio::test]
async fn unsupported_delete_is_not_attempted() {
    let _guard = TEST_LOCK.lock().await;
    let capabilities = ProviderCapabilities {
        mailbox: true,
        sync: true,
        mutations: MutationCapabilities::default(),
        ..ProviderCapabilities::default()
    };
    let factory = FakeProviderFactory::new().capabilities(capabilities);
    let harness = Harness::new(factory);
    let account = add_account().await;

    let store = harness.store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    seed_message(&store, &inbox, 1, "thread");
    api::select_folder(&inbox.id).await.expect("select inbox");

    let error = api::delete_permanently(&ThreadId("thread".into()), true)
        .await
        .expect_err("unsupported delete must fail");
    assert!(
        error.contains("permanently deleting") || error.contains("cannot"),
        "got {error}"
    );
    assert!(
        harness.recorder().mutation_requests().is_empty(),
        "the capability gate refuses before the mutation is attempted"
    );
}

/// A partial mutation honors each item: a confirmed item keeps its local effect,
/// while a rejected or uncertain item is rolled back and never retried.
#[tokio::test]
async fn partial_mutation_reconciles_only_uncertain_items() {
    let _guard = TEST_LOCK.lock().await;
    let result = MutationResult {
        items: vec![
            MutationItemResult {
                message: remote(1),
                outcome: MutationOutcome::Applied,
            },
            MutationItemResult {
                message: remote(2),
                outcome: MutationOutcome::Rejected(ProviderError::forbidden("no")),
            },
            MutationItemResult {
                message: remote(3),
                outcome: MutationOutcome::Unknown,
            },
        ],
    };
    let factory =
        FakeProviderFactory::new().session(FakeProviderSession::new().mutation_result(result));
    let harness = Harness::new(factory);
    let account = add_account().await;

    let store = harness.store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let dest = folder("dest", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&dest).unwrap();
    for uid in 1..=3 {
        seed_message(&store, &inbox, uid, "thread");
    }
    api::select_folder(&inbox.id).await.expect("select inbox");

    api::move_thread(&ThreadId("thread".into()), &dest.id)
        .await
        .expect_err("a partial mutation reports failure");

    assert_eq!(
        harness.recorder().mutation_requests().len(),
        1,
        "an uncertain or rejected item is never blindly retried"
    );

    let remaining = harness.store().messages(&inbox.id, 10, None).unwrap();
    let uids: Vec<Option<Uid>> = remaining.iter().map(|message| message.uid).collect();
    assert!(
        !uids.contains(&Some(Uid(1))),
        "the applied item keeps its local effect"
    );
    assert!(
        uids.contains(&Some(Uid(2))),
        "the rejected item is rolled back"
    );
    assert!(
        uids.contains(&Some(Uid(3))),
        "the uncertain item is reconciled locally, not left removed"
    );
}

/// An attachment fetch names the part by its persisted provider remote
/// descriptor, not by a local MIME part id.
#[tokio::test]
async fn attachment_fetch_uses_remote_descriptor() {
    let _guard = TEST_LOCK.lock().await;
    let descriptor = AttachmentDescriptor {
        remote_id: RemoteAttachmentId::from("remote-7"),
        part_id: "2".to_string(),
        filename: "report.txt".to_string(),
        content_type: "text/plain".to_string(),
        size: 5,
        content_id: None,
        disposition: AttachmentDisposition::Attachment,
    };
    let body = BodyContent {
        plain_text: Some("body".to_string()),
        html: None,
        raw: None,
        attachments: vec![descriptor],
        signature: SignatureStatus::NotApplicable,
    };
    let factory = FakeProviderFactory::new().session(
        FakeProviderSession::new()
            .body(body)
            .attachment_bytes(b"hello".to_vec()),
    );
    let harness = Harness::new(factory);
    let account = add_account().await;

    let store = harness.store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    store.upsert_folder(&inbox).unwrap();
    let message = seed_message(&store, &inbox, 1, "thread");
    api::select_folder(&inbox.id).await.expect("select inbox");

    let path = api::open_attachment(&message, "2")
        .await
        .expect("open attachment");
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");

    let calls = harness.recorder().attachment_calls();
    assert_eq!(calls.len(), 1, "one attachment fetch");
    assert_eq!(calls[0].0, remote(1));
    assert_eq!(
        calls[0].1,
        RemoteAttachmentId::from("remote-7"),
        "the provider's remote descriptor names the fetch"
    );
}

/// An unreachable account keeps the user-facing reconnect error and never
/// touches the network mutation surface.
#[tokio::test]
async fn offline_move_retains_reconnect_error() {
    let _guard = TEST_LOCK.lock().await;
    let factory =
        FakeProviderFactory::new().fail_open(ProviderError::transient("the server is down"));
    let harness = Harness::new(factory);
    let account = add_account().await;

    let store = harness.store();
    let inbox = folder("inbox", &account, "INBOX", FolderKind::Inbox);
    let dest = folder("dest", &account, "Archive", FolderKind::Archive);
    store.upsert_folder(&inbox).unwrap();
    store.upsert_folder(&dest).unwrap();
    seed_message(&store, &inbox, 1, "thread");
    api::select_folder(&inbox.id).await.expect("select inbox");

    let error = api::move_thread(&ThreadId("thread".into()), &dest.id)
        .await
        .expect_err("an unreachable account refuses the move");
    assert!(error.contains(api::RECONNECT_ERROR), "got {error}");
    assert!(
        harness.recorder().mutation_requests().is_empty(),
        "no mutation is attempted without a session"
    );
}
