//! Task G4: Gmail native submission and ambiguous-send recovery.
//!
//! Every test points the adapter at a local HTTP mock. No test reaches Google,
//! no SDK/wire type is visible outside `providers/google`, and the fixtures
//! prove the durable contract the outbox relies on: native send records the
//! provider message/thread, never appends a Sent copy, never treats a matching
//! subject as delivery proof, and never repeats a send after an ambiguous
//! result.

#![cfg(feature = "provider-google")]

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

use pige_core::Store;
use pige_core::account::{Account, AccountId, MailBackend};
use pige_core::auth::TokenSource;
use pige_core::contacts::{HeaderRole, normalize_address};
use pige_core::mail::{Address, AttachmentDisposition, parse_header_block};
use pige_core::providers::google::{GmailSubmission, GoogleClient, GoogleEndpoints};
use pige_core::providers::{
    ErrorKind, OperationContext, OperationId, ProviderError, ReconcileOutcome, SentCopyPolicy,
    SubmissionAttachment, SubmissionOutcome, SubmissionRequest, SubmissionService,
};
use pige_core::send::{
    SubmissionState, flush_provider_outbox, outbox_entry, queue_submission, raw_message_id,
};

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
    send_as: Vec<Value>,
    /// Messages that exist in Sent. Search sees exactly these.
    sent: Vec<Value>,
    /// When true, `messages.send` is accepted but the response is delayed past
    /// the caller's deadline.
    stall_send: bool,
    requested_send_status: Option<u16>,
    requests: Vec<RecordedRequest>,
}

impl MockState {
    fn new(sender: &str) -> Self {
        Self {
            send_as: vec![json!({
                "sendAsEmail": sender,
                "isPrimary": true,
                "verificationStatus": "accepted",
            })],
            ..Self::default()
        }
    }

    fn sent_message(id: &str, thread: &str, message_id: &str, subject: &str) -> Value {
        json!({
            "id": id,
            "threadId": thread,
            "labelIds": ["SENT"],
            "snippet": subject,
            "payload": {
                "headers": [
                    { "name": "Message-ID", "value": message_id },
                    { "name": "Subject", "value": subject },
                ],
            }
        })
    }

    fn sent_message_id(message: &Value) -> Option<&str> {
        message["payload"]["headers"]
            .as_array()?
            .iter()
            .find(|header| {
                header["name"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("message-id"))
            })
            .and_then(|header| header["value"].as_str())
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
                    let (status, body, stall) = {
                        let mut state = shared.lock().expect("mock state");
                        state.requests.push(request.clone());
                        respond(&mut state, &request)
                    };
                    if stall {
                        // The provider accepted the write but its response is
                        // delayed beyond the caller's operation deadline.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    let reason = match status {
                        200 => "OK",
                        204 => "No Content",
                        400 => "Bad Request",
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

    fn update(&self, change: impl FnOnce(&mut MockState)) {
        change(&mut self.state.lock().expect("mock state"));
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().expect("mock state").requests.clone()
    }

    fn request_count(&self, path_suffix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with(path_suffix))
            .count()
    }

    fn client(&self) -> GoogleClient {
        GoogleClient::new(
            Arc::new(StaticTokens),
            GoogleEndpoints {
                gmail_base: self.url.clone(),
                people_base: self.url.clone(),
            },
        )
    }
}

struct StaticTokens;

#[async_trait]
impl TokenSource for StaticTokens {
    async fn access_token(&self, _ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        Ok(SecretString::from("test-access-token"))
    }
}

fn respond(state: &mut MockState, request: &RecordedRequest) -> (u16, String, bool) {
    let segments: Vec<&str> = request
        .path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let Some(tail) = segments.get(4..) else {
        return (404, error_body(404, "not found"), false);
    };

    match (request.method.as_str(), tail) {
        ("GET", ["settings", "sendAs"]) => {
            (200, json!({ "sendAs": state.send_as }).to_string(), false)
        }
        ("POST", ["messages", "send"]) => {
            if let Some(status) = state.requested_send_status {
                return (status, error_body(status, "rejected send"), false);
            }
            let body: Value = serde_json::from_str(&request.body).unwrap_or_else(|_| json!({}));
            let raw = body["raw"]
                .as_str()
                .map(|encoded| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(encoded)
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let message_id =
                raw_message_id(&raw).unwrap_or_else(|| "<missing@example.com>".to_string());
            let subject = raw
                .split(|byte| *byte == b'\n')
                .find_map(|line| {
                    line.strip_prefix(b"Subject: ")
                        .map(|value| String::from_utf8_lossy(value).trim().to_string())
                })
                .unwrap_or_default();
            let id = format!("gmail-msg-{}", state.sent.len() + 1);
            let thread = "gmail-thread-1".to_string();
            state
                .sent
                .push(MockState::sent_message(&id, &thread, &message_id, &subject));
            let stall = state.stall_send;
            (
                200,
                json!({ "id": id, "threadId": thread, "labelIds": ["SENT"] }).to_string(),
                stall,
            )
        }
        ("GET", ["messages"]) => {
            let wanted = request
                .query_value("q")
                .and_then(|query| query.strip_prefix("rfc822msgid:"))
                .map(str::trim)
                .map(|value| value.trim_matches(|c| c == '<' || c == '>'));
            let matches: Vec<Value> = state
                .sent
                .iter()
                .filter(|message| {
                    wanted.is_some_and(|wanted| {
                        MockState::sent_message_id(message)
                            .map(|value| value.trim_matches(|c| c == '<' || c == '>') == wanted)
                            .unwrap_or(false)
                    })
                })
                .map(|message| json!({ "id": message["id"], "threadId": message["threadId"] }))
                .collect();
            (
                200,
                json!({
                    "messages": matches,
                    "resultSizeEstimate": matches.len(),
                })
                .to_string(),
                false,
            )
        }
        ("GET", ["messages", id]) => match state
            .sent
            .iter()
            .find(|message| message["id"].as_str() == Some(*id))
        {
            Some(message) => (200, message.to_string(), false),
            None => (404, error_body(404, "message not found"), false),
        },
        _ => (404, error_body(404, "unknown route"), false),
    }
}

fn error_body(status: u16, message: &str) -> String {
    json!({
        "error": {
            "code": status,
            "message": message,
            "errors": [{ "reason": "invalidArgument" }],
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

fn account() -> Account {
    Account {
        id: AccountId("gmail-g4".to_string()),
        label: "Gmail".to_string(),
        email: "ada@example.com".to_string(),
        display_name: "Ada Lovelace".to_string(),
        backend: MailBackend::gmail("google-user-1"),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn context(timeout: Duration) -> OperationContext {
    OperationContext::with_timeout(account().id, 1, timeout)
}

fn address(email: &str) -> Address {
    Address::new(None, email)
}

fn message() -> pige_core::send::OutgoingMessage {
    pige_core::send::OutgoingMessage {
        account_id: account().id,
        to: vec![Address::new(
            Some("Charles Babbage".to_string()),
            "charles@example.com",
        )],
        cc: vec![address("grace@example.com")],
        bcc: vec![address("hidden@example.com")],
        subject: "Native Gmail submission".to_string(),
        body: "Hello from the native Gmail adapter.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

fn request() -> SubmissionRequest {
    SubmissionRequest {
        operation: OperationId::new("operation-1"),
        message: message(),
        attachments: Vec::new(),
        prepared: None,
    }
}

fn submission(mock: &MockServer) -> GmailSubmission {
    GmailSubmission::new(mock.client(), account())
}

async fn prepared(service: &mut GmailSubmission, request: SubmissionRequest) -> SubmissionRequest {
    let prepared = service
        .prepare(&request, &context(Duration::from_secs(5)))
        .await
        .expect("prepare");
    SubmissionRequest {
        prepared: Some(prepared),
        ..request
    }
}

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("temp dir");
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store.upsert_account(&account()).expect("seed account");
    (dir, store)
}

fn evidence_count(store: &Store, email: &str) -> u32 {
    let key = normalize_address(&address(email)).expect("fixture address");
    store
        .learned_address(&account().id, &key)
        .expect("read learned address")
        .map(|learned| learned.evidence_count)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accepted_receipt_records_message_and_thread() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);
    assert_eq!(
        service.sent_copy_policy(),
        SentCopyPolicy::ServerManaged,
        "Gmail writes the Sent copy itself"
    );

    let request = prepared(&mut service, request()).await;
    let outcome = service
        .submit(&request, &context(Duration::from_secs(5)))
        .await
        .expect("submit");

    let SubmissionOutcome::Accepted(receipt) = outcome else {
        panic!("expected an accepted submission, got {outcome:?}");
    };
    assert_eq!(receipt.operation, request.operation);
    assert_eq!(
        receipt.remote_message.as_ref().map(|id| id.as_str()),
        Some("gmail-msg-1")
    );
    assert_eq!(
        receipt.remote_thread.as_ref().map(|id| id.as_str()),
        Some("gmail-thread-1")
    );
}

#[tokio::test]
async fn gmail_never_calls_smtp_or_append() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);
    let request = prepared(&mut service, request()).await;

    let outcome = service
        .submit(&request, &context(Duration::from_secs(5)))
        .await
        .expect("submit");
    let SubmissionOutcome::Accepted(receipt) = outcome else {
        panic!("expected acceptance");
    };

    let requests = mock.requests();
    assert!(
        requests
            .iter()
            .all(|request| request.path.starts_with("/gmail/v1/users/me/")),
        "every native send path must stay inside the Gmail API: {requests:?}"
    );
    assert!(
        !requests.iter().any(|request| {
            let path = request.path.to_ascii_lowercase();
            path.contains("append") || path.contains("smtp") || path.contains("imap")
        }),
        "native submission must never use SMTP or an IMAP append: {requests:?}"
    );
    assert_eq!(mock.request_count("/messages/send"), 1);

    let before = mock.requests().len();
    service
        .finalize_sent(&request, &receipt, &context(Duration::from_secs(5)))
        .await
        .expect("server-managed finalize");
    assert_eq!(
        mock.requests().len(),
        before,
        "finalize_sent must be a no-op for Gmail; never APPEND a second copy"
    );
}

#[tokio::test]
async fn timeout_after_send_is_uncertain() {
    let (_dir, store) = open_store();
    let id = queue_submission(&store, message(), Vec::new()).expect("queue submission");
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    mock.update(|state| state.stall_send = true);
    let mut service = submission(&mock);

    let report = flush_provider_outbox(
        &store,
        &account(),
        &mut service,
        &context(Duration::from_millis(800)),
    )
    .await
    .expect("flush");

    assert_eq!(report.uncertain, 1);
    let entry = outbox_entry(&store, &id)
        .expect("read outbox")
        .expect("row");
    assert_eq!(entry.state, SubmissionState::Uncertain);
    assert_eq!(
        mock.request_count("/messages/send"),
        1,
        "an ambiguous send must not be retried automatically"
    );
}

#[tokio::test]
async fn same_subject_is_not_reconciliation_proof() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);
    let request = prepared(&mut service, request()).await;
    let prepared = request.prepared.clone().expect("prepared");
    let message_id = raw_message_id(prepared.raw.as_deref().expect("raw")).expect("message id");

    // A different message that merely shares the subject and is already in Sent.
    mock.update(|state| {
        state.sent.push(MockState::sent_message(
            "sent-other",
            "thread-other",
            "<other@example.com>",
            "Native Gmail submission",
        ));
    });

    let outcome = service
        .reconcile(&request, &context(Duration::from_secs(5)))
        .await
        .expect("reconcile");
    assert_eq!(
        outcome,
        ReconcileOutcome::Unknown,
        "a matching subject is not delivery proof"
    );
    let search = mock
        .requests()
        .into_iter()
        .find(|request| request.method == "GET" && request.path.ends_with("/messages"))
        .expect("search request");
    let query = search.query_value("q").expect("search query");
    assert!(
        query.starts_with("rfc822msgid:"),
        "reconciliation must search by stable Message-ID, not subject: {query}"
    );
    assert!(!query.contains("subject:"));

    // Exact message identity plus the Sent label is strong evidence.
    mock.update(|state| {
        state.sent.push(MockState::sent_message(
            "sent-exact",
            "thread-exact",
            &message_id,
            "Native Gmail submission",
        ));
    });
    let outcome = service
        .reconcile(&request, &context(Duration::from_secs(5)))
        .await
        .expect("reconcile");
    let ReconcileOutcome::Accepted(receipt) = outcome else {
        panic!("exact Message-ID in Sent must reconcile as accepted, got {outcome:?}");
    };
    assert_eq!(
        receipt.remote_message.as_ref().map(|id| id.as_str()),
        Some("sent-exact")
    );
    assert_eq!(
        receipt.remote_thread.as_ref().map(|id| id.as_str()),
        Some("thread-exact")
    );
}

#[tokio::test]
async fn retry_after_draft_preparation_does_not_duplicate_send() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);

    let first = prepared(&mut service, request()).await;
    let first_prepared = first.prepared.clone().expect("prepared");
    assert_eq!(
        mock.request_count("/messages/send"),
        0,
        "preparation alone must not send"
    );

    // A retry reuses the durable prepared MIME/Message-ID instead of rendering a
    // second artifact.
    let second = prepared(&mut service, first.clone()).await;
    let second_prepared = second.prepared.clone().expect("prepared");
    assert_eq!(first_prepared.raw, second_prepared.raw);
    assert_eq!(first_prepared.reference, second_prepared.reference);
    assert_eq!(mock.request_count("/messages/send"), 0);

    let outcome = service
        .submit(&second, &context(Duration::from_secs(5)))
        .await
        .expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));
    assert_eq!(
        mock.request_count("/messages/send"),
        1,
        "the retry must send the prepared message exactly once"
    );
}

#[tokio::test]
async fn from_alias_is_validated() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);

    // The configured account address is a verified send-as alias.
    prepared(&mut service, request()).await;

    // A Gmail account that does not offer this address as a send-as alias must
    // fail before any message is submitted.
    mock.update(|state| {
        state.send_as = vec![json!({
            "sendAsEmail": "other@example.com",
            "isPrimary": true,
            "verificationStatus": "accepted",
        })];
    });
    let error = service
        .prepare(&request(), &context(Duration::from_secs(5)))
        .await
        .expect_err("an unknown From alias must be rejected");
    assert_eq!(error.kind, ErrorKind::Forbidden);

    // An unverified custom alias is not usable either.
    mock.update(|state| {
        state.send_as = vec![json!({
            "sendAsEmail": "ada@example.com",
            "isPrimary": false,
            "verificationStatus": "pending",
        })];
    });
    let error = service
        .prepare(&request(), &context(Duration::from_secs(5)))
        .await
        .expect_err("an unverified From alias must be rejected");
    assert_eq!(error.kind, ErrorKind::Forbidden);

    // Once verified it prepares normally.
    mock.update(|state| {
        state.send_as = vec![json!({
            "sendAsEmail": "ada@example.com",
            "isPrimary": false,
            "verificationStatus": "accepted",
        })];
    });
    prepared(&mut service, request()).await;
    assert_eq!(
        mock.request_count("/messages/send"),
        0,
        "alias validation must happen before any send"
    );
}

#[tokio::test]
async fn to_cc_bcc_recipient_sets_are_preserved() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);
    let mut request = request();
    request.message.to.push(address("second-to@example.com"));
    request.message.cc.push(address("second-cc@example.com"));
    request.message.bcc.push(address("second-bcc@example.com"));

    let request = prepared(&mut service, request).await;
    let prepared = request.prepared.clone().expect("prepared");
    let raw = prepared.raw.as_deref().expect("raw bytes");
    let header = parse_header_block(raw).expect("parse prepared headers");
    assert_eq!(header.to, request.message.to);
    assert_eq!(header.cc, request.message.cc);
    assert_eq!(
        header.bcc, request.message.bcc,
        "the Gmail API derives recipients from the RFC headers, including Bcc"
    );

    let text = String::from_utf8(raw.to_vec()).expect("utf-8 MIME");
    assert!(text.contains("Bcc:"));
    for recipient in request.message.recipients() {
        assert!(
            text.contains(&recipient.email),
            "prepared MIME must contain {}",
            recipient.email
        );
    }

    service
        .submit(&request, &context(Duration::from_secs(5)))
        .await
        .expect("submit");
    let send = mock
        .requests()
        .into_iter()
        .find(|request| request.method == "POST" && request.path.ends_with("/messages/send"))
        .expect("send request");
    let body: Value = serde_json::from_str(&send.body).expect("send body");
    let sent_raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body["raw"].as_str().expect("raw field"))
        .expect("base64url raw");
    assert_eq!(
        sent_raw, raw,
        "the API payload must carry the exact prepared bytes, Bcc included"
    );
}

#[tokio::test]
async fn reply_references_and_thread_hint_are_consistent() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);
    let mut request = request();
    request.message.in_reply_to = Some("<parent@example.com>".to_string());
    request.message.references = vec![
        "<root@example.com>".to_string(),
        "<parent@example.com>".to_string(),
    ];

    let request = prepared(&mut service, request).await;
    let raw = request
        .prepared
        .as_ref()
        .and_then(|prepared| prepared.raw.as_deref())
        .expect("raw bytes");
    let header = parse_header_block(raw).expect("parse prepared headers");
    assert_eq!(
        header.in_reply_to.as_deref(),
        Some("<parent@example.com>"),
        "reply threading requires In-Reply-To"
    );
    assert_eq!(
        header.references,
        vec![
            "<root@example.com>".to_string(),
            "<parent@example.com>".to_string()
        ],
        "reply threading requires a consistent References chain"
    );

    service
        .submit(&request, &context(Duration::from_secs(5)))
        .await
        .expect("submit");
    let send = mock
        .requests()
        .into_iter()
        .find(|request| request.method == "POST" && request.path.ends_with("/messages/send"))
        .expect("send request");
    let body: Value = serde_json::from_str(&send.body).expect("send body");
    assert!(
        body.get("threadId").is_none(),
        "no separate thread hint is invented; Gmail threads from the RFC reply headers"
    );
}

#[tokio::test]
async fn structured_parts_are_encoded_in_mime() {
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);
    let attachment_bytes = vec![0x00, 0x01, 0x7f, 0xff, 0x80, 0x42, 0x00, 0xc3];
    let mut request = request();
    request.attachments = vec![SubmissionAttachment {
        id: "part-1".to_string(),
        filename: "report.pdf".to_string(),
        content_type: "application/pdf".to_string(),
        bytes: attachment_bytes.clone(),
        content_id: Some("cid-1".to_string()),
        disposition: AttachmentDisposition::Inline,
    }];

    let request = prepared(&mut service, request).await;
    let raw = request
        .prepared
        .as_ref()
        .and_then(|prepared| prepared.raw.clone())
        .expect("raw bytes");
    let text = String::from_utf8(raw).expect("utf-8 MIME");

    assert!(text.contains("multipart/mixed"), "parts need a mix part");
    assert!(text.contains("Content-Type: application/pdf"));
    assert!(text.contains("filename=\"report.pdf\""));
    assert!(text.contains("Content-ID: <cid-1>"));
    let encoded = base64::engine::general_purpose::STANDARD.encode(&attachment_bytes);
    assert!(
        text.replace("\r\n", "").contains(&encoded),
        "the structured part payload must be encoded into MIME"
    );
}

#[tokio::test]
async fn successful_send_trains_once() {
    let (_dir, store) = open_store();
    let id = queue_submission(&store, message(), Vec::new()).expect("queue submission");
    let mock = MockServer::start(MockState::new("ada@example.com")).await;
    let mut service = submission(&mock);

    let first = flush_provider_outbox(
        &store,
        &account(),
        &mut service,
        &context(Duration::from_secs(5)),
    )
    .await
    .expect("first flush");
    assert_eq!(first.sent, 1);
    assert_eq!(
        outbox_entry(&store, &id)
            .expect("read outbox")
            .expect("row")
            .state,
        SubmissionState::Complete
    );
    for (recipient, role) in [
        ("charles@example.com", HeaderRole::To),
        ("grace@example.com", HeaderRole::Cc),
        ("hidden@example.com", HeaderRole::Bcc),
    ] {
        assert_eq!(
            evidence_count(&store, recipient),
            1,
            "accepted send must train {recipient} exactly once"
        );
        let key = normalize_address(&address(recipient)).expect("address key");
        assert_eq!(
            store.learned_roles(&account().id, &key).expect("roles"),
            vec![role]
        );
    }

    // A second flush sees a complete row and must not train again or resend.
    let second = flush_provider_outbox(
        &store,
        &account(),
        &mut service,
        &context(Duration::from_secs(5)),
    )
    .await
    .expect("second flush");
    assert_eq!(second.sent, 0);
    assert_eq!(mock.request_count("/messages/send"), 1);
    for recipient in [
        "charles@example.com",
        "grace@example.com",
        "hidden@example.com",
    ] {
        assert_eq!(
            evidence_count(&store, recipient),
            1,
            "a completed submission must not train {recipient} again"
        );
    }
}
