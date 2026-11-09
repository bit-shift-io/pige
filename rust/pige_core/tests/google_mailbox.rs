//! Task G2: native Gmail mailbox, history and semantic mutations.
//!
//! Every test points the adapter at a local HTTP server. No test reaches
//! Google, and no Gmail wire type is visible outside the adapter: the tests use
//! the public `GmailMailbox`/`GmailSync`/`GmailMutations` services and the
//! provider-neutral value types.

#![cfg(feature = "provider-google")]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;

use pige_core::account::AccountId;
use pige_core::auth::TokenSource;
use pige_core::contacts::{LearningOrigin, ObservationTrust, observe_message};
use pige_core::mail::{Address, AttachmentDisposition};
use pige_core::providers::google::{
    GmailMailbox, GmailMutations, GmailSync, GoogleClient, GoogleEndpoints, cursor_stage,
    request_collection,
};
use pige_core::providers::{
    ChangePage, CollectionRecord, ErrorKind, GMAIL_PROVIDER, HistoryRequest, JobKey, JobKind,
    MailChange, MailboxService, MessageRecord, MutationAction, MutationOutcome, MutationRequest,
    MutationService, OperationContext, OperationId, ProviderCursor, ProviderError,
    RemoteAttachmentId, RemoteCollectionId, RemoteMessageId, SyncRequest, SyncService,
};
use pige_core::{Account, MailBackend, Store};

// ---------------------------------------------------------------------------
// Local Gmail mock server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RecordedRequest {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    body: String,
}

impl RecordedRequest {
    fn query_value(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct MockState {
    profile_history_id: String,
    labels: Vec<Value>,
    messages: HashMap<String, Value>,
    message_order: Vec<String>,
    label_messages: HashMap<String, Vec<String>>,
    history_batches: Vec<Vec<Value>>,
    history_404: bool,
    metadata_failures: HashMap<String, u16>,
    attachments: HashMap<String, Vec<u8>>,
    requests: Vec<RecordedRequest>,
}

impl MockState {
    fn new(history_id: &str) -> Self {
        Self {
            profile_history_id: history_id.to_string(),
            ..Self::default()
        }
    }

    fn with_label(mut self, id: &str, name: &str) -> Self {
        self.labels.push(json!({
            "id": id,
            "name": name,
            "type": if id.starts_with("Label_") { "user" } else { "system" },
            "messagesTotal": 1,
            "messagesUnread": 0,
        }));
        self
    }

    fn put_message(&mut self, message: Value) {
        let id = message["id"].as_str().expect("message id").to_string();
        if !self.message_order.contains(&id) {
            self.message_order.push(id.clone());
        }
        self.messages.insert(id, message);
    }

    fn list_label(&mut self, label: &str, ids: &[&str]) {
        self.label_messages.insert(
            label.to_string(),
            ids.iter().map(|id| (*id).to_string()).collect(),
        );
    }

    fn rename_label(&mut self, id: &str, name: &str) {
        for label in &mut self.labels {
            if label["id"] == id {
                label["name"] = json!(name);
            }
        }
    }
}

struct MockServer {
    url: Url,
    state: Arc<Mutex<MockState>>,
    _task: JoinHandle<()>,
}

impl MockServer {
    async fn start(state: MockState) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind Gmail mock");
        let address = listener.local_addr().expect("mock address");
        let state = Arc::new(Mutex::new(state));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    let (status, body) = {
                        let mut state = shared.lock().expect("mock state");
                        state.requests.push(request.clone());
                        respond(&mut state, &request)
                    };
                    let reason = match status {
                        200 => "OK",
                        204 => "No Content",
                        404 => "Not Found",
                        500 => "Internal Server Error",
                        _ => "Error",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            url: Url::parse(&format!("http://{address}")).expect("mock url"),
            state,
            _task: task,
        }
    }

    fn url(&self) -> Url {
        self.url.clone()
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().expect("mock state").requests.clone()
    }

    fn update(&self, change: impl FnOnce(&mut MockState)) {
        change(&mut self.state.lock().expect("mock state"));
    }

    fn client(&self) -> GoogleClient {
        GoogleClient::new(
            Arc::new(StaticTokens),
            GoogleEndpoints {
                gmail_base: self.url(),
                people_base: self.url(),
            },
        )
    }
}

/// A TCP peer that accepts a request and never sends a response. Used to prove
/// the client bounds network calls by the operation deadline and cancellation.
async fn start_stalled_server() -> (Url, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind stalled server");
    let address = listener.local_addr().expect("stalled address");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                // Hold the connection open without a response.
                tokio::time::sleep(Duration::from_secs(60)).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (
        Url::parse(&format!("http://{address}")).expect("stalled url"),
        task,
    )
}

struct StaticTokens;

#[async_trait]
impl TokenSource for StaticTokens {
    async fn access_token(&self, _ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        Ok(SecretString::from("test-access-token"))
    }
}

fn respond(state: &mut MockState, request: &RecordedRequest) -> (u16, String) {
    let segments: Vec<&str> = request
        .path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let Some(tail) = segments.get(4..) else {
        return (404, error_body(404, "not found"));
    };

    match (request.method.as_str(), tail) {
        ("GET", ["profile"]) => (
            200,
            json!({ "historyId": state.profile_history_id }).to_string(),
        ),
        ("GET", ["labels"]) => (200, json!({ "labels": state.labels }).to_string()),
        ("GET", ["messages"]) => {
            let ids = match request.query_value("labelIds") {
                Some(label) => state.label_messages.get(label).cloned().unwrap_or_default(),
                None => state.message_order.clone(),
            };
            let messages: Vec<Value> = ids
                .into_iter()
                .map(|id| json!({ "id": id, "threadId": format!("thread-{id}") }))
                .collect();
            (
                200,
                json!({
                    "messages": messages,
                    "resultSizeEstimate": messages.len(),
                })
                .to_string(),
            )
        }
        ("GET", ["history"]) => {
            if state.history_404 {
                return (404, error_body(404, "history expired"));
            }
            let index = request
                .query_value("pageToken")
                .and_then(|token| token.strip_prefix("history-"))
                .and_then(|index| index.parse::<usize>().ok())
                .unwrap_or(0);
            let batch = state
                .history_batches
                .get(index)
                .cloned()
                .unwrap_or_default();
            let next =
                (index + 1 < state.history_batches.len()).then(|| format!("history-{}", index + 1));
            (
                200,
                json!({
                    "history": batch,
                    "historyId": state.profile_history_id,
                    "nextPageToken": next,
                })
                .to_string(),
            )
        }
        ("GET", ["messages", _id, "attachments", attachment]) => {
            match state.attachments.get(*attachment) {
                Some(bytes) => (
                    200,
                    json!({
                        "data": b64(bytes),
                        "size": bytes.len(),
                    })
                    .to_string(),
                ),
                None => (404, error_body(404, "attachment not found")),
            }
        }
        ("GET", ["messages", id]) => {
            if let Some(status) = state.metadata_failures.get(*id) {
                return (*status, error_body(*status, "temporary failure"));
            }
            match state.messages.get(*id) {
                Some(message) => (200, message.to_string()),
                None => (404, error_body(404, "message not found")),
            }
        }
        ("POST", ["messages", id, "modify"]) => {
            if let Some(message) = state.messages.get_mut(*id)
                && let Ok(body) = serde_json::from_str::<Value>(&request.body)
            {
                let mut labels: HashSet<String> = message["labelIds"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                for added in body["addLabelIds"].as_array().into_iter().flatten() {
                    if let Some(label) = added.as_str() {
                        labels.insert(label.to_string());
                    }
                }
                for removed in body["removeLabelIds"].as_array().into_iter().flatten() {
                    if let Some(label) = removed.as_str() {
                        labels.remove(label);
                    }
                }
                let mut labels: Vec<String> = labels.into_iter().collect();
                labels.sort();
                message["labelIds"] = json!(labels);
            }
            (200, json!({ "id": id }).to_string())
        }
        ("POST", ["messages", id, "trash"]) => {
            (200, json!({ "id": id, "labelIds": ["TRASH"] }).to_string())
        }
        ("DELETE", ["messages", id]) => {
            state.messages.remove(*id);
            (204, String::new())
        }
        _ => (404, error_body(404, "unknown route")),
    }
}

fn error_body(status: u16, message: &str) -> String {
    json!({
        "error": {
            "code": status,
            "message": message,
            "errors": [{ "reason": "notFound" }],
        }
    })
    .to_string()
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < header_end + 4 + content_length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_start = header_end + 4;
    let body =
        String::from_utf8_lossy(&buffer[body_start..buffer.len().min(body_start + content_length)])
            .to_string();
    let request_line = headers.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let url = Url::parse(&format!("http://mock{target}")).ok()?;
    Some(RecordedRequest {
        method,
        path: url.path().to_string(),
        query: url
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect(),
        body,
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn context() -> OperationContext {
    OperationContext::with_timeout(
        AccountId("gmail-g2".to_string()),
        1,
        Duration::from_secs(20),
    )
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn metadata_message(id: &str, labels: &[&str], subject: &str) -> Value {
    json!({
        "id": id,
        "threadId": format!("thread-{id}"),
        "labelIds": labels,
        "snippet": format!("snippet {id}"),
        "internalDate": "1700000000123",
        "sizeEstimate": 4096,
        "payload": {
            "mimeType": "multipart/alternative",
            "headers": [
                { "name": "Subject", "value": subject },
                { "name": "From", "value": "Sender <sender@example.com>" },
                { "name": "Reply-To", "value": "Reply <reply@example.com>" },
                { "name": "To", "value": "Me <me@example.com>" },
                { "name": "Cc", "value": "Copy <copy@example.com>" },
                { "name": "Message-ID", "value": format!("<{id}@example.com>") },
                { "name": "In-Reply-To", "value": "<parent@example.com>" },
                { "name": "References", "value": "<root@example.com> <parent@example.com>" },
                { "name": "Date", "value": "Tue, 14 Nov 2023 22:13:20 +0000" },
            ],
            "parts": [],
        }
    })
}

fn body_message(id: &str, labels: &[&str]) -> Value {
    json!({
        "id": id,
        "threadId": "thread-body",
        "labelIds": labels,
        "snippet": "hello",
        "internalDate": "1700000000123",
        "sizeEstimate": 256,
        "payload": {
            "mimeType": "multipart/mixed",
            "headers": [
                { "name": "Subject", "value": "Hello" },
                { "name": "From", "value": "Sender <sender@example.com>" },
                { "name": "Reply-To", "value": "Reply <reply@example.com>" },
                { "name": "To", "value": "Me <me@example.com>" },
                { "name": "Cc", "value": "Copy <copy@example.com>" },
                { "name": "Message-ID", "value": "<m1@example.com>" },
                { "name": "Date", "value": "Tue, 14 Nov 2023 22:13:20 +0000" },
            ],
            "parts": [
                {
                    "partId": "0",
                    "mimeType": "text/plain",
                    "body": { "size": 5, "data": b64(b"hello") },
                },
                {
                    "partId": "1",
                    "mimeType": "text/html",
                    "body": { "size": 12, "data": b64(b"<p>hello</p>") },
                },
                {
                    "partId": "2",
                    "mimeType": "application/pdf",
                    "filename": "report.pdf",
                    "headers": [
                        { "name": "Content-Disposition", "value": "attachment; filename=\"report.pdf\"" },
                        { "name": "Content-ID", "value": "<cid-1>" },
                    ],
                    "body": { "attachmentId": "att-1", "size": 4, "data": b64(b"%PDF") },
                },
            ],
        }
    })
}

fn upserts(changes: &[MailChange]) -> Vec<&MessageRecord> {
    changes
        .iter()
        .filter_map(|change| match change {
            MailChange::UpsertMessage { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

fn new_arrival_ids(changes: &[MailChange]) -> Vec<&RemoteMessageId> {
    changes
        .iter()
        .filter_map(|change| match change {
            MailChange::UpsertMessage {
                message,
                is_new_arrival: true,
            } => Some(&message.remote_id),
            _ => None,
        })
        .collect()
}

fn removed_ids(changes: &[MailChange]) -> Vec<&RemoteMessageId> {
    changes
        .iter()
        .filter_map(|change| match change {
            MailChange::RemoveMessage(id) => Some(id),
            _ => None,
        })
        .collect()
}

async fn drive_sync(sync: &mut GmailSync) -> (Vec<MailChange>, ProviderCursor) {
    let mut cursor = None;
    let mut changes = Vec::new();
    for _ in 0..40 {
        let page = sync
            .next_page(
                SyncRequest {
                    cursor: cursor.clone(),
                    initial_limit: 300,
                },
                &context(),
            )
            .await
            .expect("sync page");
        changes.extend(page.changes);
        cursor = Some(page.checkpoint);
        if !page.has_more {
            return (changes, cursor.expect("final cursor"));
        }
    }
    panic!("sync did not reach a stable cursor");
}

fn find_message<'a>(changes: &'a [MailChange], id: &str) -> Option<&'a MessageRecord> {
    upserts(changes)
        .into_iter()
        .find(|message| message.remote_id.as_str() == id)
}

fn collection_name<'a>(changes: &'a [MailChange], id: &str) -> Option<&'a str> {
    changes.iter().find_map(|change| match change {
        MailChange::UpsertCollection(CollectionRecord {
            remote_id, name, ..
        }) if remote_id.as_str() == id => Some(name.as_str()),
        _ => None,
    })
}

fn last_modify_body(server: &MockServer) -> Value {
    let request = server
        .requests()
        .into_iter()
        .rfind(|request| request.path.ends_with("/modify"))
        .expect("modify request");
    serde_json::from_str(&request.body).expect("modify body")
}

fn label_ids(server: &MockServer, id: &str) -> Vec<String> {
    server
        .state
        .lock()
        .expect("mock state")
        .messages
        .get(id)
        .and_then(|message| message["labelIds"].as_array())
        .map(|labels| {
            labels
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn test_account(id: &str) -> Account {
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

fn sync_job(account: &AccountId) -> JobKey {
    JobKey {
        account: account.clone(),
        kind: JobKind::MailSync,
        source: "sync".to_string(),
    }
}

async fn apply_action(mutations: &mut GmailMutations, action: MutationAction) {
    let result = mutations
        .apply(
            MutationRequest {
                operation: OperationId::new("mutation"),
                targets: vec![RemoteMessageId::new("m1")],
                source: None,
                action,
            },
            &context(),
        )
        .await
        .expect("mutation");
    assert_eq!(result.items[0].outcome, MutationOutcome::Applied);
}

// ---------------------------------------------------------------------------
// Named G2 tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_labels_share_one_message_and_body() {
    let mut state = MockState::new("100")
        .with_label("INBOX", "Inbox")
        .with_label("SENT", "Sent");
    state.put_message(body_message("m1", &["INBOX", "SENT"]));
    state.list_label("INBOX", &["m1"]);
    state.list_label("SENT", &["m1"]);
    state
        .attachments
        .insert("att-1".to_string(), b"%PDF".to_vec());
    let server = MockServer::start(state).await;
    let client = server.client();

    let mut sync = GmailSync::new(client.clone(), "me");
    let (changes, cursor) = drive_sync(&mut sync).await;

    let shared: Vec<_> = upserts(&changes)
        .into_iter()
        .filter(|message| message.remote_id.as_str() == "m1")
        .collect();
    assert_eq!(
        shared.len(),
        1,
        "one Gmail message must normalize to one upsert, not one per label"
    );
    let labels: HashSet<&str> = shared[0]
        .collections
        .iter()
        .map(RemoteCollectionId::as_str)
        .collect();
    assert!(labels.contains("INBOX"));
    assert!(labels.contains("SENT"));
    assert_eq!(
        shared[0].remote_thread.as_ref().unwrap().as_str(),
        "thread-body"
    );
    assert_eq!(shared[0].header.subject, "Hello");
    assert_eq!(
        shared[0].header.from,
        vec![Address::new(
            Some("Sender".to_string()),
            "sender@example.com"
        )]
    );
    assert_eq!(shared[0].header.reply_to[0].email, "reply@example.com");
    assert_eq!(
        shared[0].header.message_id.as_deref(),
        Some("<m1@example.com>")
    );
    assert_eq!(shared[0].header.date, 1_700_000_000);
    assert_eq!(shared[0].header.size, 256);
    assert!(shared[0].flags.seen);
    assert!(!shared[0].flags.flagged);
    assert_eq!(cursor_stage(&cursor).unwrap(), "delta");

    let metadata_calls = server
        .requests()
        .into_iter()
        .filter(|request| {
            request.method == "GET" && request.path == "/gmail/v1/users/me/messages/m1"
        })
        .count();
    assert_eq!(
        metadata_calls, 1,
        "the second label must reuse the first metadata fetch"
    );

    let mut mailbox = GmailMailbox::new(client, "me");
    let first = mailbox
        .body(&RemoteMessageId::new("m1"), &context())
        .await
        .expect("body");
    let second = mailbox
        .body(&RemoteMessageId::new("m1"), &context())
        .await
        .expect("body again");
    assert_eq!(first.plain_text.as_deref(), Some("hello"));
    assert_eq!(first.html.as_deref(), Some("<p>hello</p>"));
    assert_eq!(first.attachments.len(), 1);
    assert_eq!(first.attachments[0].remote_id.as_str(), "att-1");
    assert_eq!(first.attachments[0].filename, "report.pdf");
    assert_eq!(
        first.attachments[0].disposition,
        AttachmentDisposition::Attachment
    );
    assert_eq!(first.attachments[0].content_id.as_deref(), Some("cid-1"));
    assert_eq!(first.plain_text, second.plain_text);
    assert_eq!(first.html, second.html);

    let bytes = mailbox
        .attachment(
            &RemoteMessageId::new("m1"),
            &RemoteAttachmentId::new("att-1"),
            &context(),
        )
        .await
        .expect("attachment");
    assert_eq!(bytes, b"%PDF");
}

#[tokio::test]
async fn bootstrap_replays_anchor_changes() {
    let mut state = MockState::new("100").with_label("INBOX", "Inbox");
    state.put_message(metadata_message("m1", &["INBOX"], "Recent"));
    state.put_message(metadata_message(
        "m2",
        &["INBOX"],
        "Arrived during bootstrap",
    ));
    state.list_label("INBOX", &["m1"]);
    state.history_batches = vec![vec![json!({
        "messagesAdded": [{ "message": { "id": "m2" } }],
    })]];
    let server = MockServer::start(state).await;
    let mut sync = GmailSync::new(server.client(), "me");

    let (changes, cursor) = drive_sync(&mut sync).await;

    let requests = server.requests();
    assert!(
        requests[0].path.ends_with("/profile"),
        "the history anchor is captured before listing messages: {requests:?}"
    );
    let history = requests
        .iter()
        .find(|request| request.path.ends_with("/history"))
        .expect("history catch-up request");
    assert_eq!(
        history.query_value("startHistoryId"),
        Some("100"),
        "catch-up must replay from the pre-bootstrap anchor"
    );

    let m2 = find_message(&changes, "m2").expect("m2 replayed from history");
    assert!(
        !new_arrival_ids(&changes).contains(&&m2.remote_id),
        "bootstrap catch-up is historical, never a new arrival"
    );
    assert!(find_message(&changes, "m1").is_some());
    assert_eq!(cursor_stage(&cursor).unwrap(), "delta");
}

#[tokio::test]
async fn history_404_reconciles_without_emptying_cache() {
    let mut state = MockState::new("100").with_label("INBOX", "Inbox");
    state.put_message(metadata_message("m1", &["INBOX"], "Before expiry"));
    state.list_label("INBOX", &["m1"]);
    let server = MockServer::start(state).await;
    let mut sync = GmailSync::new(server.client(), "me");
    let (_changes, cursor) = drive_sync(&mut sync).await;
    assert_eq!(cursor_stage(&cursor).unwrap(), "delta");

    server.update(|state| {
        state.history_404 = true;
        state.profile_history_id = "200".to_string();
        state.put_message(metadata_message("m2", &["INBOX"], "During repair"));
        state.list_label("INBOX", &["m1", "m2"]);
    });

    let recovery = sync
        .next_page(
            SyncRequest {
                cursor: Some(cursor),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("expired history is a recovery, not an error");
    assert!(recovery.changes.is_empty());
    assert_eq!(cursor_stage(&recovery.checkpoint).unwrap(), "reconcile");

    // The provider cannot know an old cached message was deleted just because
    // the repair window did not list it. No sweep removals may be emitted.
    let mut cursor = Some(recovery.checkpoint);
    let mut repaired = Vec::new();
    let mut final_cursor = None;
    for _ in 0..20 {
        let page = sync
            .next_page(
                SyncRequest {
                    cursor: cursor.clone(),
                    initial_limit: 300,
                },
                &context(),
            )
            .await
            .expect("reconcile page");
        repaired.extend(page.changes);
        final_cursor = Some(page.checkpoint.clone());
        if !page.has_more {
            break;
        }
        cursor = Some(page.checkpoint);
    }

    assert!(
        find_message(&repaired, "m2").is_some(),
        "newer mail is recovered"
    );
    assert!(
        removed_ids(&repaired).is_empty(),
        "a bounded repair must not empty the cache: {:?}",
        removed_ids(&repaired)
    );
    assert_eq!(
        cursor_stage(final_cursor.as_ref().unwrap()).unwrap(),
        "delta"
    );
}

#[tokio::test]
async fn history_pages_do_not_skip_added_then_deleted_message() {
    let server = MockServer::start(MockState::new("100").with_label("INBOX", "Inbox")).await;
    let mut sync = GmailSync::new(server.client(), "me");
    let (_changes, cursor) = drive_sync(&mut sync).await;

    server.update(|state| {
        state.put_message(metadata_message("flash", &["INBOX"], "Short lived"));
        state.history_batches = vec![
            vec![json!({ "messagesAdded": [{ "message": { "id": "flash" } }] })],
            vec![json!({ "messagesDeleted": [{ "message": { "id": "flash" } }] })],
        ];
    });

    let first = sync
        .next_page(
            SyncRequest {
                cursor: Some(cursor),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("first history page");
    assert_eq!(upserts(&first.changes)[0].remote_id.as_str(), "flash");
    assert_eq!(new_arrival_ids(&first.changes).len(), 1);
    assert!(first.has_more);

    let second = sync
        .next_page(
            SyncRequest {
                cursor: Some(first.checkpoint),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("second history page");
    assert_eq!(
        removed_ids(&second.changes),
        vec![&RemoteMessageId::new("flash")],
        "the deletion on a later page is still applied"
    );
    assert!(!second.has_more);
}

#[tokio::test]
async fn deleted_get_404_during_page_is_tombstone() {
    let server = MockServer::start(MockState::new("100").with_label("INBOX", "Inbox")).await;
    let mut sync = GmailSync::new(server.client(), "me");
    let (_changes, cursor) = drive_sync(&mut sync).await;

    server.update(|state| {
        state.metadata_failures.insert("gone".to_string(), 404);
        state.history_batches = vec![vec![json!({
            "messagesAdded": [{ "message": { "id": "gone" } }],
        })]];
    });

    let page = sync
        .next_page(
            SyncRequest {
                cursor: Some(cursor),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("404 for a listed message is a tombstone, not a page failure");
    assert_eq!(
        removed_ids(&page.changes),
        vec![&RemoteMessageId::new("gone")]
    );
    assert!(upserts(&page.changes).is_empty());
}

#[tokio::test]
async fn partial_metadata_batch_does_not_advance_cursor() {
    let mut state = MockState::new("100").with_label("INBOX", "Inbox");
    state.put_message(metadata_message("m1", &["INBOX"], "One"));
    state.put_message(metadata_message("m2", &["INBOX"], "Two"));
    state.list_label("INBOX", &["m1", "m2"]);
    state.metadata_failures.insert("m2".to_string(), 500);
    let server = MockServer::start(state).await;
    let mut sync = GmailSync::new(server.client(), "me");

    let error = sync
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect_err("a metadata failure must fail the page");
    assert_eq!(error.kind, ErrorKind::Transient);

    // No checkpoint was returned, so the caller retries the same first page.
    server.update(|state| {
        state.metadata_failures.clear();
    });
    let page = sync
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("retry succeeds");
    assert_eq!(upserts(&page.changes).len(), 2);
}

#[tokio::test]
async fn user_label_rename_preserves_collection_id() {
    let state = MockState::new("100")
        .with_label("INBOX", "Inbox")
        .with_label("Label_7", "Projects")
        .with_label("STARRED", "Starred")
        .with_label("IMPORTANT", "Important")
        .with_label("CATEGORY_PERSONAL", "Personal");
    let server = MockServer::start(state).await;
    let client = server.client();
    let mut mailbox = GmailMailbox::new(client.clone(), "me");

    let before = mailbox.collections(&context()).await.expect("collections");
    let projects = before
        .iter()
        .find(|collection| collection.remote_id.as_str() == "Label_7")
        .expect("user label");
    assert_eq!(projects.name, "Projects");
    assert!(
        before.iter().all(|collection| {
            !matches!(
                collection.remote_id.as_str(),
                "STARRED" | "IMPORTANT" | "CATEGORY_PERSONAL" | "UNREAD" | "ALL" | "CHATS"
            )
        }),
        "state labels are flags, not collections: {:?}",
        before
            .iter()
            .map(|collection| collection.remote_id.as_str())
            .collect::<Vec<_>>()
    );

    let mut sync = GmailSync::new(client, "me");
    let (synced, cursor) = drive_sync(&mut sync).await;
    assert_eq!(
        collection_name(&synced, "Label_7"),
        Some("Projects"),
        "sync reports the collection under its label id"
    );

    server.update(|state| state.rename_label("Label_7", "Projects 2027"));
    let after = mailbox.collections(&context()).await.expect("collections");
    let renamed = after
        .iter()
        .find(|collection| collection.remote_id.as_str() == "Label_7")
        .expect("same label id survives rename");
    assert_eq!(renamed.name, "Projects 2027");

    // A delta refresh reports the rename without inventing a new collection id
    // or removing the old membership.
    server.update(|state| state.history_batches = vec![Vec::new()]);
    let delta = sync
        .next_page(
            SyncRequest {
                cursor: Some(cursor),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("delta after rename");
    assert_eq!(
        collection_name(&delta.changes, "Label_7"),
        Some("Projects 2027")
    );
    assert!(!delta.changes.iter().any(
        |change| matches!(change, MailChange::RemoveCollection(id) if id.as_str() == "Label_7")
    ));
}

#[tokio::test]
async fn archive_removes_inbox_not_unrelated_labels() {
    let mut state = MockState::new("100").with_label("INBOX", "Inbox");
    state.put_message(metadata_message("m1", &["INBOX", "Label_2"], "Keep"));
    let server = MockServer::start(state).await;
    let mut mutations = GmailMutations::new(server.client(), "me");

    let result = mutations
        .apply(
            MutationRequest {
                operation: OperationId::new("archive-1"),
                targets: vec![RemoteMessageId::new("m1")],
                source: None,
                action: MutationAction::Archive,
            },
            &context(),
        )
        .await
        .expect("archive");
    assert_eq!(result.items[0].outcome, MutationOutcome::Applied);

    let modify = server
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/messages/m1/modify"))
        .expect("modify request");
    let body: Value = serde_json::from_str(&modify.body).expect("json body");
    assert_eq!(body["removeLabelIds"], json!(["INBOX"]));
    assert_eq!(body["addLabelIds"], json!([]));
    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.method == "DELETE"),
        "archive is never a permanent delete"
    );
}

#[tokio::test]
async fn trash_is_not_permanent_delete() {
    let mut state = MockState::new("100").with_label("INBOX", "Inbox");
    state.put_message(metadata_message("m1", &["INBOX"], "Trash me"));
    let server = MockServer::start(state).await;
    let mut mutations = GmailMutations::new(server.client(), "me");

    let result = mutations
        .apply(
            MutationRequest {
                operation: OperationId::new("trash-1"),
                targets: vec![RemoteMessageId::new("m1")],
                source: None,
                action: MutationAction::Trash,
            },
            &context(),
        )
        .await
        .expect("trash");
    assert_eq!(result.items[0].outcome, MutationOutcome::Applied);

    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.method == "POST" && request.path.ends_with("/messages/m1/trash")),
        "trash must use Gmail's recoverable trash endpoint"
    );
    assert!(
        !requests.iter().any(|request| request.method == "DELETE"),
        "trash is not a permanent delete"
    );
}

#[tokio::test]
async fn modify_scope_disables_permanent_delete() {
    let mut state = MockState::new("100").with_label("INBOX", "Inbox");
    state.put_message(metadata_message("m1", &["INBOX"], "Keep"));
    let server = MockServer::start(state).await;
    let mut mutations = GmailMutations::new(server.client(), "me");

    assert!(!mutations.capabilities().permanent_delete);
    let result = mutations
        .apply(
            MutationRequest {
                operation: OperationId::new("delete-1"),
                targets: vec![RemoteMessageId::new("m1")],
                source: None,
                action: MutationAction::DeletePermanently,
            },
            &context(),
        )
        .await
        .expect("rejection is a per-item result");
    match &result.items[0].outcome {
        MutationOutcome::Rejected(error) => assert_eq!(error.kind, ErrorKind::Unsupported),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.method == "DELETE"),
        "a nonfunctional destructive action must never reach the wire"
    );
}

#[tokio::test]
async fn all_history_not_limited_to_inbox() {
    let mut state = MockState::new("100")
        .with_label("INBOX", "Inbox")
        .with_label("SPAM", "Spam")
        .with_label("TRASH", "Trash");
    state.put_message(metadata_message("inbox-1", &["INBOX"], "Inbox mail"));
    state.put_message(metadata_message("spam-1", &["SPAM"], "Spam mail"));
    state.put_message(metadata_message("trash-1", &["TRASH"], "Trash mail"));
    let server = MockServer::start(state).await;
    let mut mailbox = GmailMailbox::new(server.client(), "me");

    let page = mailbox
        .history(
            HistoryRequest {
                cursor: None,
                page_size: 50,
            },
            &context(),
        )
        .await
        .expect("history page");
    let ids: HashSet<&str> = page
        .messages
        .iter()
        .map(|message| message.remote_id.as_str())
        .collect();
    assert!(ids.contains("inbox-1"));
    assert!(
        ids.contains("spam-1") && ids.contains("trash-1"),
        "accessible Spam/Trash are header observations, not hidden by an INBOX filter"
    );
    let spam = page
        .messages
        .iter()
        .find(|message| message.remote_id.as_str() == "spam-1")
        .expect("spam record");
    assert!(
        spam.collections
            .iter()
            .any(|collection| collection.as_str() == "SPAM")
    );
    // Spam/Trash membership is exposed so the learner can keep the observation
    // available but suppressed from ordinary ranking.
    let spam_observations = observe_message(
        &AccountId("gmail-g2".to_string()),
        spam,
        LearningOrigin::History,
    );
    assert!(!spam_observations.is_empty());
    assert!(
        spam_observations
            .iter()
            .all(|observation| observation.trust == ObservationTrust::SpamOrTrash)
    );
    assert!(page.coverage.scanned >= 3);
    assert_eq!(page.coverage.total_hint, Some(3));

    let requests = server.requests();
    let list = requests
        .iter()
        .find(|request| request.method == "GET" && request.path.ends_with("/messages"))
        .expect("history message listing");
    assert_eq!(list.query_value("labelIds"), None);
    assert_eq!(list.query_value("includeSpamTrash"), Some("true"));
    assert!(
        requests
            .iter()
            .filter(|request| request.path.contains("/messages/"))
            .all(|request| request.query_value("format") == Some("metadata")),
        "history learning is header-only"
    );
}

#[tokio::test]
async fn other_folder_on_demand_is_supported() {
    let mut state = MockState::new("100")
        .with_label("INBOX", "Inbox")
        .with_label("Label_42", "Receipts");
    state.put_message(metadata_message("m-user", &["Label_42"], "Receipt"));
    state.list_label("Label_42", &["m-user"]);
    let server = MockServer::start(state).await;
    let mut sync = GmailSync::new(server.client(), "me");

    let cursor =
        request_collection(None, &RemoteCollectionId::new("Label_42")).expect("request cursor");
    assert_eq!(cursor_stage(&cursor).unwrap(), "bootstrap");
    let page = sync
        .next_page(
            SyncRequest {
                cursor: Some(cursor),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("on-demand page");
    let message = find_message(&page.changes, "m-user").expect("user-label message");
    assert!(
        message
            .collections
            .iter()
            .any(|collection| collection.as_str() == "Label_42")
    );
    // Only the requested user label is listed during this on-demand bootstrap.
    let requests = server.requests();
    let label_lists: Vec<&str> = requests
        .iter()
        .filter(|request| request.path.ends_with("/messages"))
        .filter_map(|request| request.query_value("labelIds"))
        .collect();
    assert_eq!(label_lists, vec!["Label_42"]);
}

#[tokio::test]
async fn star_and_read_actions_match_their_labels() {
    let mut state = MockState::new("100").with_label("INBOX", "Inbox");
    state.put_message(metadata_message("m1", &["INBOX"], "Star me"));
    let server = MockServer::start(state).await;
    let mut mutations = GmailMutations::new(server.client(), "me");

    apply_action(&mut mutations, MutationAction::SetStar(true)).await;
    let body = last_modify_body(&server);
    assert_eq!(body["addLabelIds"], json!(["STARRED"]));
    assert_eq!(body["removeLabelIds"], json!([]));
    assert!(
        label_ids(&server, "m1")
            .iter()
            .any(|label| label == "STARRED")
    );

    apply_action(&mut mutations, MutationAction::SetStar(false)).await;
    let body = last_modify_body(&server);
    assert_eq!(body["addLabelIds"], json!([]));
    assert_eq!(body["removeLabelIds"], json!(["STARRED"]));
    assert!(
        !label_ids(&server, "m1")
            .iter()
            .any(|label| label == "STARRED")
    );

    apply_action(&mut mutations, MutationAction::SetRead(false)).await;
    let body = last_modify_body(&server);
    assert_eq!(body["addLabelIds"], json!(["UNREAD"]));
    assert_eq!(body["removeLabelIds"], json!([]));
    assert!(
        label_ids(&server, "m1")
            .iter()
            .any(|label| label == "UNREAD")
    );

    apply_action(&mut mutations, MutationAction::SetRead(true)).await;
    let body = last_modify_body(&server);
    assert_eq!(body["addLabelIds"], json!([]));
    assert_eq!(body["removeLabelIds"], json!(["UNREAD"]));
    assert!(
        !label_ids(&server, "m1")
            .iter()
            .any(|label| label == "UNREAD")
    );
}

#[tokio::test]
async fn history_unknown_ids_are_idempotent() {
    let mut state = MockState::new("100")
        .with_label("INBOX", "Inbox")
        .with_label("Label_9", "Projects");
    state.put_message(metadata_message("recent", &["INBOX"], "Recent"));
    state.list_label("INBOX", &["recent"]);
    // Exists in Gmail but is older than the bounded bootstrap window, so the
    // store has never seen it when its label change arrives.
    state.put_message(metadata_message("old", &["INBOX", "Label_9"], "Old"));
    let server = MockServer::start(state).await;
    let mut sync = GmailSync::new(server.client(), "me");
    let (bootstrap_changes, cursor) = drive_sync(&mut sync).await;

    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path().join("pige.db")).expect("store");
    store.migrate().expect("migrate");
    let account = test_account("gmail-g2");
    store.upsert_account(&account).expect("account");
    let key = sync_job(&account.id);
    store
        .apply_change_page(
            &context(),
            &key,
            GMAIL_PROVIDER,
            None,
            &ChangePage {
                changes: bootstrap_changes,
                checkpoint: cursor.clone(),
                has_more: false,
            },
        )
        .expect("bootstrap applies");
    assert!(
        store
            .remote_message_for(&account.id, GMAIL_PROVIDER, &RemoteMessageId::new("old"))
            .expect("lookup")
            .is_none(),
        "the out-of-window message is not cached"
    );

    // One history page: an add followed by a delete for an id the store never
    // cached, plus a label change for mail older than the bootstrap window.
    server.update(|state| {
        state.history_batches = vec![vec![
            json!({ "messagesAdded": [{ "message": { "id": "flash" } }] }),
            json!({ "messagesDeleted": [{ "message": { "id": "flash" } }] }),
            json!({
                "labelsAdded": [{
                    "message": { "id": "old" },
                    "labelIds": ["Label_9"],
                }],
            }),
        ]];
    });
    let page = sync
        .next_page(
            SyncRequest {
                cursor: Some(cursor.clone()),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("history page");
    assert!(
        removed_ids(&page.changes)
            .iter()
            .any(|id| id.as_str() == "flash"),
        "the unknown add+delete is still represented as a tombstone"
    );
    assert!(
        page.changes.iter().any(|change| matches!(
            change,
            MailChange::SetMembership { message, collection, present: true }
                if message.as_str() == "old" && collection.as_str() == "Label_9"
        )),
        "the out-of-window label change is still represented"
    );

    // Applying either for an unknown remote id is a no-op, not a Store error.
    store
        .apply_change_page(
            &context(),
            &key,
            GMAIL_PROVIDER,
            Some(&cursor),
            &ChangePage {
                changes: page.changes,
                checkpoint: page.checkpoint,
                has_more: page.has_more,
            },
        )
        .expect("unknown remote ids apply idempotently");
}

#[tokio::test]
async fn request_honors_deadline_and_cancellation() {
    let (url, task) = start_stalled_server().await;
    let client = GoogleClient::new(
        Arc::new(StaticTokens),
        GoogleEndpoints {
            gmail_base: url.clone(),
            people_base: url,
        },
    );
    let mut mailbox = GmailMailbox::new(client, "me");
    let deadline = OperationContext::with_timeout(
        AccountId("gmail-g2".to_string()),
        1,
        Duration::from_millis(200),
    );
    let started = std::time::Instant::now();
    let error = mailbox
        .collections(&deadline)
        .await
        .expect_err("a stalled connection must hit the deadline");
    assert_eq!(error.kind, ErrorKind::Transient);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the request outlived its deadline: {:?}",
        started.elapsed()
    );
    task.abort();

    let (url, task) = start_stalled_server().await;
    let client = GoogleClient::new(
        Arc::new(StaticTokens),
        GoogleEndpoints {
            gmail_base: url.clone(),
            people_base: url,
        },
    );
    let mut mailbox = GmailMailbox::new(client, "me");
    let ctx = context();
    let cancellation = ctx.cancellation.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancellation.cancel();
    });
    let error = mailbox
        .collections(&ctx)
        .await
        .expect_err("cancellation must abort the request");
    canceller.await.expect("canceller");
    assert_eq!(error.kind, ErrorKind::Cancelled);
    task.abort();
}

#[tokio::test]
async fn all_mail_on_demand_lists_account_wide() {
    let mut state = MockState::new("100")
        .with_label("INBOX", "Inbox")
        .with_label("SPAM", "Spam");
    state.put_message(metadata_message("inbox-1", &["INBOX"], "Inbox mail"));
    state.put_message(metadata_message("spam-1", &["SPAM"], "Spam mail"));
    let server = MockServer::start(state).await;
    let mut sync = GmailSync::new(server.client(), "me");

    let cursor = request_collection(None, &RemoteCollectionId::new("pige:all-mail"))
        .expect("all-mail cursor");
    let page = sync
        .next_page(
            SyncRequest {
                cursor: Some(cursor),
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("on-demand all mail");
    let listing = server
        .requests()
        .into_iter()
        .find(|request| request.method == "GET" && request.path.ends_with("/messages"))
        .expect("message listing");
    assert_eq!(
        listing.query_value("labelIds"),
        None,
        "the virtual collection must not be sent as a Gmail label id"
    );

    let inbox = find_message(&page.changes, "inbox-1").expect("inbox message");
    assert!(
        inbox
            .collections
            .iter()
            .any(|collection| collection.as_str() == "pige:all-mail")
    );
    let spam = find_message(&page.changes, "spam-1").expect("spam message");
    assert!(
        !spam
            .collections
            .iter()
            .any(|collection| collection.as_str() == "pige:all-mail"),
        "Spam is not part of the All Mail view"
    );
}
