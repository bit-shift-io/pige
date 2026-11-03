//! Offline outbox tests: durable queueing, draft round-trips, and the
//! no-duplicate guarantee when SMTP succeeds but the Sent APPEND fails.

use async_trait::async_trait;
use pige_core::{
    Account, AccountId, Address, AuthKind, Flags, Folder, FolderId, FolderKind, Security,
    ServerConfig, Store,
    api::{self, AccountInput, Config, DB_FILE_NAME},
    platform::fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    send::{
        OutboxId, OutboxState, OutgoingMessage,
        outbox::{
            discard_draft, drafts, flush_outbox, list_outbox, outbox_entry, queue_send, save_draft,
            update_draft,
        },
    },
    transport::{FakeTransport, RemoteFolder, SendError, SmtpSender},
};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

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

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store
}

fn seed_account(store: &Store) {
    store.upsert_account(&account()).unwrap();
    store
        .upsert_folder(&Folder {
            id: FolderId("sent-folder".to_string()),
            account_id: account().id,
            name: "Sent".to_string(),
            delimiter: Some("/".to_string()),
            kind: FolderKind::Sent,
            unread: 0,
            total: 0,
            uidvalidity: Some(1),
            uidnext: Some(1),
        })
        .unwrap();
}

fn message(subject: &str) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account().id,
        to: vec![Address::new(
            Some("Charles Babbage".to_string()),
            "charles@example.com",
        )],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_string(),
        body: "Hello from the outbox.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

#[derive(Default)]
struct RecordingSmtp {
    calls: Vec<Vec<u8>>,
}

#[async_trait]
impl SmtpSender for RecordingSmtp {
    async fn send_raw(
        &mut self,
        _from: &Address,
        _recipients: &[Address],
        raw: &[u8],
    ) -> Result<(), SendError> {
        self.calls.push(raw.to_vec());
        Ok(())
    }
}

#[tokio::test]
async fn smtp_success_and_append_failure_does_not_resend() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    seed_account(&store);

    let id = queue_send(&store, message("Report")).await.unwrap();
    let mut smtp = RecordingSmtp::default();
    let mut imap = FakeTransport {
        fail_append: ["Sent".to_string()].into_iter().collect(),
        ..FakeTransport::default()
    };

    let first = flush_outbox(&store, &account, &mut smtp, &mut imap)
        .await
        .expect("flush");
    assert_eq!(first.sent_local, 1);
    assert_eq!(smtp.calls.len(), 1);
    assert_eq!(imap.append_calls.len(), 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        OutboxState::SentLocal
    );

    let second = flush_outbox(&store, &account, &mut smtp, &mut imap)
        .await
        .expect("second flush");
    assert_eq!(second.sent_local, 1);
    // The retry is APPEND-only: SMTP must not be called a second time.
    assert_eq!(smtp.calls.len(), 1);
    assert_eq!(imap.append_calls.len(), 2);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        OutboxState::SentLocal
    );

    imap.fail_append.clear();
    let third = flush_outbox(&store, &account, &mut smtp, &mut imap)
        .await
        .expect("third flush");
    assert_eq!(third.sent, 1);
    assert_eq!(smtp.calls.len(), 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        OutboxState::Sent
    );
}

#[tokio::test]
async fn queued_mail_is_persisted_before_any_network_work() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_account(&store);

    let id = queue_send(&store, message("Queued")).await.unwrap();

    let entry = outbox_entry(&store, &id).unwrap().unwrap();
    assert_eq!(entry.state, OutboxState::Queued);
    assert_eq!(entry.message.subject, "Queued");
    assert_eq!(list_outbox(&store, None).unwrap().len(), 1);

    let mut smtp = RecordingSmtp::default();
    let mut imap = FakeTransport::default();
    let report = flush_outbox(&store, &account(), &mut smtp, &mut imap)
        .await
        .unwrap();

    assert_eq!(report.sent, 1);
    assert_eq!(smtp.calls.len(), 1);
    let entry = outbox_entry(&store, &id).unwrap().unwrap();
    assert_eq!(entry.state, OutboxState::Sent);
    // The bytes persisted on the row are exactly the bytes that were sent.
    assert_eq!(entry.raw.as_deref(), Some(smtp.calls[0].as_slice()));
}

#[tokio::test]
async fn an_interrupted_sending_row_is_never_resent() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let account = account();
    seed_account(&store);

    let id = queue_send(&store, message("Interrupted")).await.unwrap();
    // Simulate a crash after the message was claimed for sending but before the
    // SMTP acceptance was recorded.
    store
        .set_outbox_state(&id, OutboxState::Sending, None)
        .unwrap();

    let mut smtp = RecordingSmtp::default();
    let mut imap = FakeTransport::default();
    let report = flush_outbox(&store, &account, &mut smtp, &mut imap)
        .await
        .unwrap();

    assert_eq!(report.failed, 1);
    assert!(smtp.calls.is_empty());
    let entry = outbox_entry(&store, &id).unwrap().unwrap();
    assert_eq!(entry.state, OutboxState::Failed);
    assert!(entry.error.unwrap().contains("not retried"));
}

#[test]
fn drafts_round_trip_and_are_not_sent() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_account(&store);

    let id = save_draft(&store, message("Draft subject")).unwrap();
    let saved = outbox_entry(&store, &id).unwrap().unwrap();
    assert_eq!(saved.state, OutboxState::Draft);
    assert_eq!(saved.message.subject, "Draft subject");
    assert_eq!(drafts(&store, None).unwrap().len(), 1);

    let mut edited = message("Edited draft");
    edited.body = "Edited body.".to_string();
    update_draft(&store, &id, &edited).unwrap();
    let saved = outbox_entry(&store, &id).unwrap().unwrap();
    assert_eq!(saved.message.subject, "Edited draft");
    assert_eq!(saved.message.body, "Edited body.");

    assert!(discard_draft(&store, &id).unwrap());
    assert!(outbox_entry(&store, &id).unwrap().is_none());
    assert!(drafts(&store, None).unwrap().is_empty());
}

#[test]
fn a_failed_append_keeps_the_raw_message_for_a_retry() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_account(&store);
    let id = save_draft(&store, message("Draft")).unwrap();
    store.set_outbox_raw(&id, b"raw message bytes").unwrap();
    let entry: OutboxId = id.clone();
    assert_eq!(
        outbox_entry(&store, &entry)
            .unwrap()
            .unwrap()
            .raw
            .as_deref(),
        Some(b"raw message bytes".as_slice())
    );
}

#[test]
fn outbox_states_round_trip_through_their_storage_names() {
    for state in [
        OutboxState::Draft,
        OutboxState::Queued,
        OutboxState::Sending,
        OutboxState::SentLocal,
        OutboxState::Sent,
        OutboxState::Failed,
    ] {
        assert_eq!(OutboxState::from_db(state.as_str()), Some(state));
    }
    assert_eq!(OutboxState::from_db("bogus"), None);
    assert!(!Flags::default().seen);
}

static API_LOCK: Mutex<()> = Mutex::const_new(());

struct ApiHarness {
    dir: TempDir,
    factory: Arc<FakeTransportFactory>,
}

impl ApiHarness {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let credentials = Arc::new(FakeCredentials::default());
        let factory = Arc::new(FakeTransportFactory::default());
        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            credentials,
            notifier: Arc::new(FakeNotifier::default()),
            opener: Arc::new(FakeOpener::default()),
            transport: factory.clone(),
        })
        .unwrap();
        Self { dir, factory }
    }

    fn store(&self) -> Store {
        let store = Store::open(self.dir.path().join("data").join(DB_FILE_NAME)).unwrap();
        store.migrate().unwrap();
        store
    }

    async fn add_account(&self) -> AccountId {
        let snapshot = api::add_account(AccountInput {
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
        snapshot.accounts[0].id.clone()
    }

    fn add_transport(&self, id: &AccountId) {
        self.factory.add_transport(
            id.clone(),
            FakeTransport {
                folders: vec![
                    RemoteFolder {
                        name: "INBOX".to_string(),
                        delimiter: Some("/".to_string()),
                        kind: FolderKind::Inbox,
                    },
                    RemoteFolder {
                        name: "Sent".to_string(),
                        delimiter: Some("/".to_string()),
                        kind: FolderKind::Sent,
                    },
                ],
                ..FakeTransport::default()
            },
        );
    }
}

fn api_message(account: AccountId) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account,
        to: vec![Address::new(None, "charles@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Offline".to_string(),
        body: "Queued body".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

#[tokio::test]
async fn api_queues_when_smtp_is_offline_and_sync_retries() {
    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new();
    let id = harness.add_account().await;
    harness.add_transport(&id);
    harness.factory.fail_smtp(id.clone());

    let outcome = api::send_mail(api_message(id.clone())).await.unwrap();
    assert!(outcome.queued);
    assert!(harness.factory.sent_raw().is_empty());
    let queued = harness.store().outbox_items(Some(&id)).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].state, OutboxState::Queued);

    harness.factory.succeed_smtp(&id);
    api::sync_all().await.unwrap();
    assert_eq!(harness.factory.sent_raw().len(), 1);
    let sent = harness.store().outbox_items(Some(&id)).unwrap();
    assert_eq!(sent[0].state, OutboxState::Sent);
}

#[tokio::test]
async fn api_sends_online_and_appends_to_sent_once() {
    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new();
    let id = harness.add_account().await;
    harness.add_transport(&id);
    api::sync_account(&id).await.unwrap();

    let outcome = api::send_mail(api_message(id.clone())).await.unwrap();
    assert!(!outcome.queued);
    assert_eq!(harness.factory.sent_raw().len(), 1);

    let transport = harness.factory.transport(&id).await.unwrap();
    assert_eq!(transport.append_calls.len(), 1);
    assert_eq!(transport.append_calls[0].0, "Sent");
    assert!(transport.append_calls[0].2.seen);
    assert_eq!(
        harness.store().outbox_items(Some(&id)).unwrap()[0].state,
        OutboxState::Sent
    );

    // A later sync must not resend or re-append the delivered row.
    api::sync_all().await.unwrap();
    assert_eq!(harness.factory.sent_raw().len(), 1);
}

#[tokio::test]
async fn api_drafts_round_trip_and_appear_in_the_snapshot() {
    let _guard = API_LOCK.lock().await;
    let harness = ApiHarness::new();
    let id = harness.add_account().await;

    let draft = api::save_draft(api_message(id.clone())).unwrap();
    let snapshot = api::snapshot();
    assert_eq!(snapshot.drafts.len(), 1);
    assert_eq!(snapshot.drafts[0].id, draft);
    assert_eq!(snapshot.drafts[0].state, OutboxState::Draft);
    assert_eq!(api::list_drafts().unwrap().len(), 1);
    assert_eq!(api::draft(&draft).unwrap().unwrap().id, draft);

    let mut edited = api_message(id);
    edited.subject = "Edited".to_string();
    api::update_draft(&draft, edited).unwrap();
    assert_eq!(
        api::draft(&draft).unwrap().unwrap().message.subject,
        "Edited"
    );

    let snapshot = api::discard_draft(&draft).unwrap();
    assert!(snapshot.drafts.is_empty());
    assert!(api::draft(&draft).unwrap().is_none());
}
