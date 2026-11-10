//! Task P2: native Proton mailbox metadata, event sync, history and mutations.
//!
//! Every test points the adapter at a local synthetic Proton API. No test
//! reaches Proton, and no vendored wire type is visible here: the tests drive
//! the public `ProtonMailbox`/`ProtonSync`/`ProtonMutations` services and the
//! provider-neutral value types.
//!
//! The named cases are the P2 RED/GREEN contract: opaque string identity,
//! folder/label membership, event-anchor replay after bootstrap, `More`
//! pagination that only commits a complete event cursor, an event reset that
//! resnapshots without erasing the cache, a delete that lands after the snapshot
//! page (tombstone), live-arrival notification boundaries, an offset history
//! crawl that reaches the oldest page, churn-safe reconciliation, and
//! per-target mutation outcomes.

#![cfg(feature = "provider-proton")]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use pige_core::account::AccountId;
use pige_core::auth::TokenSource;
use pige_core::mail::FolderKind;
use pige_core::providers::proton::{
    ProtonClient, ProtonEndpoints, ProtonMailbox, ProtonMutations, ProtonSync, UnlockedKeyStore,
    cursor_stage,
};
use pige_core::providers::{
    ChangePage, ErrorKind, HistoryRequest, MailChange, MailboxService, MessageRecord,
    MutationAction, MutationOutcome, MutationRequest, MutationService, OperationContext,
    OperationId, ProviderCursor, ProviderError, RemoteCollectionId, RemoteMessageId, SyncRequest,
    SyncService,
};

// ---------------------------------------------------------------------------
// Synthetic Proton API
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

type PageHook = Box<dyn FnOnce(&mut MockState) + Send>;

#[derive(Default)]
struct MockState {
    access: String,
    labels: Vec<Value>,
    /// Newest first, as Proton lists them.
    messages: Vec<Value>,
    counts: HashMap<String, (i64, i64)>,
    latest_event: String,
    event_batches: HashMap<String, Value>,
    /// Message ids the mutation endpoints reject with a typed 422.
    reject_ids: HashSet<String>,
    /// Test hook run immediately before the given listing page is served, so a
    /// test can mutate the account between the anchor read and the page fetch.
    page_hook: Option<(usize, PageHook)>,
    /// Listing pages that answer with a transient 503.
    fail_pages: HashSet<usize>,
    /// Artificial latency on the decorative counts endpoint, so a test can let
    /// the operation deadline expire while counts are in flight.
    count_delay: Option<Duration>,
    /// Artificial latency on the message-listing endpoint, to model a network
    /// call that never returns.
    page_delay: Option<Duration>,
    requests: Vec<RecordedRequest>,
}

impl MockState {
    fn new(access: &str, event: &str) -> Self {
        Self {
            access: access.to_string(),
            latest_event: event.to_string(),
            ..Self::default()
        }
    }

    fn with_label(mut self, id: &str, name: &str, label_type: i64) -> Self {
        self.labels.push(json!({
            "ID": id,
            "Name": name,
            "Color": "#fff",
            "Type": label_type,
            "Path": name,
            "Order": self.labels.len(),
        }));
        self
    }

    fn put_message(&mut self, message: Value) {
        let id = message["ID"].as_str().expect("message id").to_string();
        self.messages.retain(|existing| existing["ID"] != id);
        self.messages.push(message);
    }

    fn message(&self, id: &str) -> Option<&Value> {
        self.messages.iter().find(|message| message["ID"] == id)
    }

    fn event(&mut self, from: &str, batch: Value) {
        self.event_batches.insert(from.to_string(), batch);
    }
}

struct ProtonMock {
    server: MockServer,
    state: Arc<Mutex<MockState>>,
}

impl ProtonMock {
    async fn start(state: MockState) -> Self {
        let server = MockServer::start().await;
        let shared = Arc::new(Mutex::new(state));
        let responder_state = shared.clone();
        Mock::given(any())
            .respond_with(move |request: &Request| {
                let mut state = responder_state.lock().expect("mock state");
                let recorded = RecordedRequest {
                    method: request.method.as_str().to_string(),
                    path: request.url.path().to_string(),
                    query: request
                        .url
                        .query_pairs()
                        .map(|(name, value)| (name.into_owned(), value.into_owned()))
                        .collect(),
                    body: String::from_utf8_lossy(&request.body).into_owned(),
                };
                state.requests.push(recorded);
                respond(&mut state, request)
            })
            .mount(&server)
            .await;
        Self {
            server,
            state: shared,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state")
    }

    fn update(&self, change: impl FnOnce(&mut MockState)) {
        change(&mut self.state.lock().expect("mock state"));
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().expect("mock state").requests.clone()
    }

    /// Run `hook` just before `page` of the account-wide listing is served.
    fn set_page_hook(&self, page: usize, hook: impl FnOnce(&mut MockState) + Send + 'static) {
        self.update(|state| state.page_hook = Some((page, Box::new(hook))));
    }

    /// Make one listing page fail transiently.
    fn fail_page(&self, page: usize) {
        self.update(|state| {
            state.fail_pages.insert(page);
        });
    }

    fn clear_failed_pages(&self) {
        self.update(|state| state.fail_pages.clear());
    }

    fn endpoints(&self) -> ProtonEndpoints {
        ProtonEndpoints::new_insecure_for_tests(self.server.uri()).expect("mock endpoint")
    }

    fn client(&self, tokens: Arc<dyn TokenSource>) -> ProtonClient {
        let keys = Arc::new(UnlockedKeyStore::new());
        keys.install_test_session("session-uid");
        ProtonClient::new(tokens, keys, self.endpoints())
    }
}

fn authorized(request: &Request, state: &MockState) -> bool {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {}", state.access))
}

fn unauthorized() -> ResponseTemplate {
    ResponseTemplate::new(401).set_body_json(json!({ "Code": 401, "Error": "expired" }))
}

fn respond(state: &mut MockState, request: &Request) -> ResponseTemplate {
    let path = request.url.path();
    if !matches!(path, "/auth/v4/refresh") && !authorized(request, state) {
        return unauthorized();
    }
    let query = |key: &str| {
        request
            .url
            .query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
    };

    if path == "/core/v4/labels" && request.method.as_str() == "GET" {
        let filter = query("Type").and_then(|value| value.parse::<i64>().ok());
        let labels: Vec<Value> = state
            .labels
            .iter()
            .filter(|label| filter.is_none_or(|kind| label["Type"] == kind))
            .cloned()
            .collect();
        return ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Labels": labels,
        }));
    }

    if path == "/mail/v4/messages/count" && request.method.as_str() == "GET" {
        let counts: Vec<Value> = state
            .counts
            .iter()
            .map(|(label, (total, unread))| {
                json!({ "LabelID": label, "Total": total, "Unread": unread })
            })
            .collect();
        let response = ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Counts": counts,
        }));
        return match state.count_delay {
            Some(delay) => response.set_delay(delay),
            None => response,
        };
    }

    if path == "/mail/v4/messages" && request.method.as_str() == "GET" {
        let label = query("LabelID");
        let page = query("Page")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let page_size = query("PageSize")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(150);
        if let Some((hook_page, hook)) = state.page_hook.take() {
            if hook_page == page {
                hook(state);
            } else {
                state.page_hook = Some((hook_page, hook));
            }
        }
        if state.fail_pages.contains(&page) {
            return ResponseTemplate::new(503)
                .set_body_json(json!({ "Code": 500, "Error": "unavailable" }));
        }
        let mut matching: Vec<Value> = state
            .messages
            .iter()
            .filter(|message| {
                label.as_ref().is_none_or(|label| {
                    message["LabelIDs"]
                        .as_array()
                        .is_some_and(|labels| labels.iter().any(|value| value == label))
                })
            })
            .cloned()
            .collect();
        // The adapter always asks for `Sort=Time&Desc=1`.
        matching.sort_by(|left, right| {
            right["Time"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&left["Time"].as_i64().unwrap_or(0))
        });
        let total = matching.len();
        let start = (page * page_size).min(total);
        let end = (start + page_size).min(total);
        let response = ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Total": total,
            "Messages": matching[start..end],
        }));
        return match state.page_delay {
            Some(delay) => response.set_delay(delay),
            None => response,
        };
    }

    if path == "/core/v4/events/latest" && request.method.as_str() == "GET" {
        return ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "EventID": state.latest_event,
        }));
    }

    if let Some(from) = path.strip_prefix("/core/v4/events/") {
        let batch = state.event_batches.get(from).cloned().unwrap_or_else(|| {
            json!({
                "EventID": from,
                "More": 0,
                "Refresh": 0,
                "Messages": [],
                "MessageCounts": [],
            })
        });
        let mut body = batch;
        body["Code"] = json!(1000);
        return ResponseTemplate::new(200).set_body_json(body);
    }

    if let Some(operation) = path.strip_prefix("/mail/v4/messages/") {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let ids: Vec<String> = body["IDs"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if ids.iter().any(|id| state.reject_ids.contains(id)) {
            return ResponseTemplate::new(422)
                .set_body_json(json!({ "Code": 2501, "Error": "rejected" }));
        }
        match operation {
            "read" | "unread" => {
                for id in &ids {
                    if let Some(message) = state
                        .messages
                        .iter_mut()
                        .find(|message| message["ID"] == *id)
                    {
                        message["Unread"] = json!(u8::from(operation == "unread"));
                    }
                }
            }
            "label" | "unlabel" => {
                let label = body["LabelID"].as_str().unwrap_or_default().to_string();
                for id in &ids {
                    if let Some(message) = state
                        .messages
                        .iter_mut()
                        .find(|message| message["ID"] == *id)
                        && let Some(labels) = message["LabelIDs"].as_array_mut()
                    {
                        if operation == "label" {
                            if !labels.iter().any(|value| value == &label) {
                                labels.push(json!(label));
                            }
                        } else {
                            labels.retain(|value| value != &label);
                        }
                    }
                }
            }
            "delete" => {
                state
                    .messages
                    .retain(|message| !ids.iter().any(|id| message["ID"] == *id));
            }
            _ => {
                return ResponseTemplate::new(404)
                    .set_body_json(json!({ "Code": 404, "Error": "unknown operation" }));
            }
        }
        return ResponseTemplate::new(200).set_body_json(json!({ "Code": 1000 }));
    }

    ResponseTemplate::new(404).set_body_json(json!({ "Code": 404, "Error": "not found" }))
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct StaticTokens;

#[async_trait]
impl TokenSource for StaticTokens {
    async fn access_token(&self, _ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        Ok(SecretString::from("access-1"))
    }
}

/// A token source that advances on each fetch, so the wrapper's 401 retry asks
/// for a fresh token instead of replaying the rejected one.
struct RotatingTokens {
    calls: Mutex<usize>,
}

#[async_trait]
impl TokenSource for RotatingTokens {
    async fn access_token(&self, _ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        Ok(SecretString::from(format!("access-{calls}")))
    }
}

fn context() -> OperationContext {
    OperationContext::with_timeout(AccountId::new(), 1, Duration::from_secs(30))
}

fn message(
    id: &str,
    conversation: &str,
    subject: &str,
    labels: &[&str],
    sender: &str,
    recipient: &str,
    time: i64,
) -> Value {
    json!({
        "ID": id,
        "ConversationID": conversation,
        "Subject": subject,
        "Unread": 1,
        "Sender": { "Name": "Sender", "Address": sender },
        "ToList": [{ "Name": "Recipient", "Address": recipient }],
        "CCList": [],
        "BCCList": [],
        "Time": time,
        "Size": 512,
        "AddressID": "addr-1",
        "LabelIDs": labels,
        "ExternalID": format!("<{id}@example.com>"),
        "NumAttachments": 0,
        "Flags": 0,
    })
}

fn default_state() -> MockState {
    MockState::new("access-1", "event-0")
        .with_label("0", "Inbox", 4)
        .with_label("7", "Sent", 4)
        .with_label("3", "Trash", 4)
        .with_label("4", "Spam", 4)
        .with_label("8", "Drafts", 4)
        .with_label("6", "Archive", 4)
}

fn upserts(pages: &[ChangePage]) -> Vec<(MessageRecord, bool)> {
    pages
        .iter()
        .flat_map(|page| page.changes.iter())
        .filter_map(|change| match change {
            MailChange::UpsertMessage {
                message,
                is_new_arrival,
            } => Some((message.clone(), *is_new_arrival)),
            _ => None,
        })
        .collect()
}

fn removed(pages: &[ChangePage]) -> Vec<RemoteMessageId> {
    pages
        .iter()
        .flat_map(|page| page.changes.iter())
        .filter_map(|change| match change {
            MailChange::RemoveMessage(id) => Some(id.clone()),
            _ => None,
        })
        .collect()
}

async fn drive_sync(
    sync: &mut ProtonSync,
    ctx: &OperationContext,
    initial_limit: u32,
    max_pages: usize,
) -> Vec<ChangePage> {
    drive_sync_from(sync, ctx, None, initial_limit, max_pages).await
}

async fn drive_sync_from(
    sync: &mut ProtonSync,
    ctx: &OperationContext,
    mut cursor: Option<ProviderCursor>,
    initial_limit: u32,
    max_pages: usize,
) -> Vec<ChangePage> {
    let mut pages = Vec::new();
    for _ in 0..max_pages {
        let page = sync
            .next_page(
                SyncRequest {
                    cursor: cursor.clone(),
                    initial_limit,
                },
                ctx,
            )
            .await
            .expect("sync page");
        cursor = Some(page.checkpoint.clone());
        let more = page.has_more;
        pages.push(page);
        if !more {
            break;
        }
    }
    pages
}

/// Drive pages from `cursor` until the provider returns an error, returning it.
/// Used to prove a stalled/failed page does not corrupt durable sweep state.
async fn first_page_error(
    sync: &mut ProtonSync,
    ctx: &OperationContext,
    mut cursor: Option<ProviderCursor>,
) -> ProviderError {
    loop {
        match sync
            .next_page(
                SyncRequest {
                    cursor: cursor.clone(),
                    initial_limit: 10,
                },
                ctx,
            )
            .await
        {
            Ok(page) => {
                assert!(page.has_more, "expected an error before the stream ended");
                cursor = Some(page.checkpoint);
            }
            Err(error) => return error,
        }
    }
}

// ---------------------------------------------------------------------------
// Required tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proton_ids_are_not_numeric_uids() {
    let mut state = default_state();
    state.counts.insert("0".to_string(), (1, 1));
    state.put_message(message(
        "opaque-message-id==",
        "conversation-77",
        "Opaque identity",
        &["0"],
        "sender@example.com",
        "alice@example.com",
        1_700_000_000,
    ));
    let mock = ProtonMock::start(state).await;
    let client = mock.client(Arc::new(StaticTokens));
    let ctx = context();

    let mut mailbox = ProtonMailbox::new(client.clone());
    let collections = mailbox.collections(&ctx).await.expect("collections");
    let inbox = collections
        .iter()
        .find(|collection| collection.remote_id.as_str() == "0")
        .expect("inbox collection");
    assert_eq!(inbox.kind, FolderKind::Inbox);
    assert_eq!(inbox.total, Some(1));
    assert_eq!(inbox.unread, Some(1));

    let mut sync = ProtonSync::new(client);
    let pages = drive_sync(&mut sync, &ctx, 10, 20).await;
    let records = upserts(&pages);
    let record = records
        .iter()
        .map(|(record, _)| record)
        .find(|record| record.remote_id.as_str() == "opaque-message-id==")
        .expect("opaque message");
    // Proton's account-wide message id is an opaque string, not a dense IMAP
    // UID and not the numeric system-folder id.
    assert!(
        record.remote_id.as_str().parse::<u64>().is_err(),
        "a Proton message id must never be decoded as a numeric UID"
    );
    assert_eq!(
        record.remote_thread.as_ref().map(|thread| thread.as_str()),
        Some("conversation-77")
    );
    assert_eq!(
        record.collections,
        vec![RemoteCollectionId::new("0")],
        "system folder ids stay opaque collection ids"
    );
    assert_eq!(
        record.header.message_id.as_deref(),
        Some("<opaque-message-id==@example.com>")
    );
}

#[tokio::test]
async fn folder_and_labels_share_message_content() {
    let mut state = default_state().with_label("Label_work", "Work", 1);
    state.put_message(message(
        "shared-message",
        "shared-conversation",
        "Shared content",
        &["0", "Label_work"],
        "sender@example.com",
        "alice@example.com",
        1_700_000_000,
    ));
    state.put_message(message(
        "plain-message",
        "plain-conversation",
        "Inbox only",
        &["0"],
        "sender@example.com",
        "bob@example.com",
        1_700_000_100,
    ));
    let mock = ProtonMock::start(state).await;
    let client = mock.client(Arc::new(StaticTokens));
    let ctx = context();

    let mut mailbox = ProtonMailbox::new(client.clone());
    let collections = mailbox.collections(&ctx).await.expect("collections");
    assert!(
        collections
            .iter()
            .any(|collection| collection.remote_id.as_str() == "Label_work")
    );

    let mut sync = ProtonSync::new(client);
    let pages = drive_sync(&mut sync, &ctx, 10, 20).await;
    let page_records = upserts(&pages);
    let shared: Vec<&MessageRecord> = page_records
        .iter()
        .map(|(record, _)| record)
        .filter(|record| record.remote_id.as_str() == "shared-message")
        .collect();
    assert!(
        shared.len() >= 2,
        "the Inbox listing and the label listing must both yield the message"
    );
    let first = shared[0];
    for record in &shared {
        assert_eq!(record.header, first.header, "content must be identical");
        assert_eq!(record.flags, first.flags, "flags must be identical");
        assert_eq!(
            record.collections,
            vec![
                RemoteCollectionId::new("0"),
                RemoteCollectionId::new("Label_work")
            ]
        );
    }
    // Removing one label is an update, never a server delete.
    mock.update(|state| {
        state.latest_event = "event-after-label".to_string();
        state.event(
            "event-0",
            json!({
                "EventID": "event-after-label-done",
                "More": 0,
                "Refresh": 0,
                "Messages": [{
                    "ID": "shared-message",
                    "Action": 3,
                    "Message": message(
                        "shared-message",
                        "shared-conversation",
                        "Shared content",
                        &["0"],
                        "sender@example.com",
                        "alice@example.com",
                        1_700_000_000,
                    ),
                }],
                "MessageCounts": [],
            }),
        );
    });
    let checkpoint = pages.last().unwrap().checkpoint.clone();
    let page = sync
        .next_page(
            SyncRequest {
                cursor: Some(checkpoint),
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("delta after label removal");
    assert!(
        removed(std::slice::from_ref(&page)).is_empty(),
        "removing a label must not tombstone the message"
    );
    let updated = page
        .changes
        .iter()
        .find_map(|change| match change {
            MailChange::UpsertMessage { message, .. }
                if message.remote_id.as_str() == "shared-message" =>
            {
                Some(message)
            }
            _ => None,
        })
        .expect("label removal normalized as a full record");
    assert_eq!(updated.collections, vec![RemoteCollectionId::new("0")]);
}

#[tokio::test]
async fn bootstrap_replays_event_anchor() {
    let mut state = default_state();
    state.put_message(message(
        "during-bootstrap",
        "conv-1",
        "Snapshot",
        &["0"],
        "sender@example.com",
        "alice@example.com",
        1_700_000_000,
    ));
    state.latest_event = "event-anchor".to_string();
    state.event(
        "event-anchor",
        json!({
            "EventID": "event-anchor-done",
            "More": 0,
            "Refresh": 0,
            "Messages": [{
                "ID": "arrived-during-bootstrap",
                "Action": 1,
                "Message": message(
                    "arrived-during-bootstrap",
                    "conv-2",
                    "During bootstrap",
                    &["0"],
                    "sender@example.com",
                    "bob@example.com",
                    1_700_000_100,
                ),
            }],
            "MessageCounts": [],
        }),
    );
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let pages = drive_sync(&mut sync, &ctx, 10, 20).await;
    let records = upserts(&pages);
    assert!(
        records
            .iter()
            .any(|(record, _)| record.remote_id.as_str() == "during-bootstrap")
    );
    let replay = records
        .iter()
        .find(|(record, _)| record.remote_id.as_str() == "arrived-during-bootstrap")
        .expect("the event anchor was replayed after the snapshot");
    assert!(
        !replay.1,
        "mail replayed during the initial bootstrap is not a live notification"
    );
    let requests = mock.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/core/v4/events/event-anchor"),
        "bootstrap must replay from the captured anchor, requests: {requests:?}"
    );
}

#[tokio::test]
async fn more_events_page_commits_only_complete_cursor() {
    // One empty collection so the first bootstrap page lands directly on the
    // event anchor, as a real empty mailbox does.
    let mut state = MockState::new("access-1", "event-0").with_label("0", "Inbox", 4);
    state.event(
        "event-0",
        json!({
            "EventID": "event-1",
            "More": 1,
            "Refresh": 0,
            "Messages": [{
                "ID": "first-live",
                "Action": 1,
                "Message": message(
                    "first-live",
                    "conv-1",
                    "First",
                    &["0"],
                    "sender@example.com",
                    "alice@example.com",
                    1_700_000_000,
                ),
            }],
            "MessageCounts": [],
        }),
    );
    state.event(
        "event-1",
        json!({
            "EventID": "event-2",
            "More": 0,
            "Refresh": 0,
            "Messages": [{
                "ID": "second-live",
                "Action": 1,
                "Message": message(
                    "second-live",
                    "conv-2",
                    "Second",
                    &["0"],
                    "sender@example.com",
                    "bob@example.com",
                    1_700_000_100,
                ),
            }],
            "MessageCounts": [],
        }),
    );
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    // The empty mailbox bootstraps immediately to the event anchor.
    let first = sync
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("bootstrap");
    let more = sync
        .next_page(
            SyncRequest {
                cursor: Some(first.checkpoint.clone()),
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("first event page");
    assert!(more.has_more, "More=1 must keep the sync page open");
    assert_eq!(
        cursor_stage(&more.checkpoint).expect("stage"),
        "events",
        "a partial event batch must not commit a completed event id"
    );
    assert!(
        upserts(std::slice::from_ref(&more))
            .iter()
            .any(|(record, _)| record.remote_id.as_str() == "first-live")
    );

    let done = sync
        .next_page(
            SyncRequest {
                cursor: Some(more.checkpoint.clone()),
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("final event page");
    assert!(!done.has_more);
    assert_eq!(cursor_stage(&done.checkpoint).expect("stage"), "complete");
}

#[tokio::test]
async fn event_reset_resnapshots_without_losing_cache() {
    let mut state = default_state();
    state.put_message(message(
        "cached-message",
        "conv-1",
        "Cached",
        &["0"],
        "sender@example.com",
        "alice@example.com",
        1_700_000_000,
    ));
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let pages = drive_sync(&mut sync, &ctx, 10, 20).await;
    let checkpoint = pages.last().unwrap().checkpoint.clone();
    assert_eq!(cursor_stage(&checkpoint).expect("stage"), "complete");

    // A server-side reset invalidates the delta cursor but must not delete
    // cached messages: the adapter re-enumerates the recent window instead.
    mock.update(|state| {
        state.latest_event = "event-after-reset".to_string();
        state.event(
            "event-0",
            json!({
                "EventID": "event-after-reset",
                "More": 0,
                "Refresh": 1,
                "Messages": [],
                "MessageCounts": [],
            }),
        );
    });
    let reset = sync
        .next_page(
            SyncRequest {
                cursor: Some(checkpoint),
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("reset page");
    assert!(
        removed(std::slice::from_ref(&reset)).is_empty(),
        "a reset must not tombstone the cache"
    );
    assert_eq!(
        cursor_stage(&reset.checkpoint).expect("stage"),
        "reconcile",
        "an event reset starts the reconciliation sweep"
    );
    assert!(reset.has_more);

    let resnapshot = sync
        .next_page(
            SyncRequest {
                cursor: Some(reset.checkpoint.clone()),
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("reconcile page");
    assert!(
        removed(std::slice::from_ref(&resnapshot)).is_empty(),
        "the reconciliation sweep never emits deletions"
    );
    assert!(
        upserts(std::slice::from_ref(&resnapshot))
            .iter()
            .any(|(record, new)| { record.remote_id.as_str() == "cached-message" && !*new }),
        "the reset must re-upsert the cached message without notifying"
    );
}

#[tokio::test]
async fn message_deleted_mid_fetch_is_tombstone() {
    let mut state = default_state();
    state.put_message(message(
        "deleted-after-snapshot",
        "conv-1",
        "Deleted mid-fetch",
        &["0"],
        "sender@example.com",
        "alice@example.com",
        1_700_000_000,
    ));
    state.event(
        "event-0",
        json!({
            "EventID": "event-1",
            "More": 0,
            "Refresh": 0,
            "Messages": [{
                "ID": "deleted-after-snapshot",
                "Action": 0,
            }],
            "MessageCounts": [],
        }),
    );
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let pages = drive_sync(&mut sync, &ctx, 10, 20).await;
    assert!(
        upserts(&pages)
            .iter()
            .any(|(record, _)| record.remote_id.as_str() == "deleted-after-snapshot"),
        "the snapshot page saw the message before the delete event"
    );
    assert_eq!(
        removed(&pages),
        vec![RemoteMessageId::new("deleted-after-snapshot")],
        "a message deleted after the listing was fetched must become a tombstone"
    );
}

#[tokio::test]
async fn live_added_message_is_not_historical_notification() {
    let mut state = default_state();
    state.put_message(message(
        "historical",
        "conv-0",
        "Historical",
        &["0"],
        "sender@example.com",
        "alice@example.com",
        1_700_000_000,
    ));
    state.event(
        "event-0",
        json!({
            "EventID": "event-1",
            "More": 0,
            "Refresh": 0,
            "Messages": [{
                "ID": "during-bootstrap",
                "Action": 1,
                "Message": message(
                    "during-bootstrap",
                    "conv-1",
                    "During bootstrap",
                    &["0"],
                    "sender@example.com",
                    "bob@example.com",
                    1_700_000_050,
                ),
            }],
            "MessageCounts": [],
        }),
    );
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let pages = drive_sync(&mut sync, &ctx, 10, 20).await;
    let records = upserts(&pages);
    let historical = records
        .iter()
        .find(|(record, _)| record.remote_id.as_str() == "historical")
        .expect("snapshot message");
    assert!(!historical.1, "snapshot records are not notifications");
    let catch_up = records
        .iter()
        .find(|(record, _)| record.remote_id.as_str() == "during-bootstrap")
        .expect("catch-up record");
    assert!(
        !catch_up.1,
        "mail replayed from the bootstrap anchor is historical"
    );

    // A genuine delta after the bootstrap is a live arrival.
    mock.update(|state| {
        state.event(
            "event-1",
            json!({
                "EventID": "event-2",
                "More": 0,
                "Refresh": 0,
                "Messages": [{
                    "ID": "genuinely-live",
                    "Action": 1,
                    "Message": message(
                        "genuinely-live",
                        "conv-2",
                        "Live",
                        &["0"],
                        "sender@example.com",
                        "carol@example.com",
                        1_700_000_100,
                    ),
                }],
                "MessageCounts": [],
            }),
        );
    });
    let checkpoint = pages.last().unwrap().checkpoint.clone();
    let delta = sync
        .next_page(
            SyncRequest {
                cursor: Some(checkpoint),
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("delta");
    let live = upserts(std::slice::from_ref(&delta));
    assert!(
        live.iter()
            .any(|(record, new)| record.remote_id.as_str() == "genuinely-live" && *new),
        "a live-added message must be a notification"
    );
}

#[tokio::test]
async fn history_reaches_oldest_page() {
    let mut state = default_state();
    for index in 0..5 {
        state.put_message(message(
            &format!("history-{index}"),
            &format!("conv-{index}"),
            &format!("History {index}"),
            &["0"],
            "sender@example.com",
            &format!("history{index}@example.com"),
            1_700_000_000 - index as i64,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut mailbox = ProtonMailbox::new(mock.client(Arc::new(StaticTokens)));

    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut has_more = true;
    let mut pages = 0;
    while has_more {
        pages += 1;
        assert!(pages < 10, "history must terminate");
        let page = mailbox
            .history(
                HistoryRequest {
                    cursor: cursor.clone(),
                    page_size: 2,
                },
                &ctx,
            )
            .await
            .expect("history page");
        for record in &page.messages {
            seen.insert(record.remote_id.as_str().to_string());
        }
        assert!(page.coverage.total_hint.unwrap_or(0) >= 5);
        has_more = page.has_more;
        cursor = Some(page.checkpoint);
    }
    assert_eq!(pages, 3, "five records at two per page");
    for index in 0..5 {
        assert!(
            seen.contains(&format!("history-{index}")),
            "history must reach the oldest page, missing history-{index}"
        );
    }
    // Metadata-only: no full message/body fetch was issued.
    let requests = mock.requests();
    let page_numbers: Vec<u32> = requests
        .iter()
        .filter(|request| request.path == "/mail/v4/messages")
        .filter_map(|request| {
            request
                .query_value("Page")
                .and_then(|value| value.parse::<u32>().ok())
        })
        .collect();
    assert_eq!(page_numbers, vec![0, 1, 2], "offset pages were crawled");
    assert!(
        requests
            .iter()
            .all(|request| request.path != "/mail/v4/messages/history-4"),
        "history must never fetch a message body, requests: {requests:?}"
    );
}

#[tokio::test]
async fn paging_churn_reconciles_without_skipping_addresses() {
    let mut state = default_state();
    for index in 0..7 {
        // Newest first when reversed below.
        state.put_message(message(
            &format!("churn-{index}"),
            &format!("conv-{index}"),
            &format!("Churn {index}"),
            &["0"],
            "sender@example.com",
            &format!("churn{index}@example.com"),
            1_700_000_000 - index as i64,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut mailbox = ProtonMailbox::new(mock.client(Arc::new(StaticTokens)));

    let mut seen: HashSet<String> = HashSet::new();
    let first = mailbox
        .history(
            HistoryRequest {
                cursor: None,
                page_size: 3,
            },
            &ctx,
        )
        .await
        .expect("first page");
    for record in &first.messages {
        seen.insert(record.remote_id.as_str().to_string());
    }

    // Delete an item from the already-consumed page and add a live message.
    // A naive offset advance would skip the record that shifted backwards into
    // the consumed page; the adapter must overlap/reconcile instead.
    mock.update(|state| {
        state.messages.retain(|message| message["ID"] != "churn-0");
        state.put_message(message(
            "live-churn",
            "conv-live",
            "Live churn",
            &["0"],
            "sender@example.com",
            "live@example.com",
            1_700_000_500,
        ));
        state.latest_event = "event-churn".to_string();
    });

    let mut cursor = Some(first.checkpoint);
    let mut has_more = true;
    let mut pages = 1;
    while has_more {
        pages += 1;
        assert!(pages < 20, "churn reconciliation must terminate");
        let page = mailbox
            .history(
                HistoryRequest {
                    cursor: cursor.clone(),
                    page_size: 3,
                },
                &ctx,
            )
            .await
            .expect("churn page");
        for record in &page.messages {
            seen.insert(record.remote_id.as_str().to_string());
        }
        has_more = page.has_more;
        cursor = Some(page.checkpoint);
    }

    for index in 1..7 {
        assert!(
            seen.contains(&format!("churn-{index}")),
            "offset churn skipped churn-{index}; seen {seen:?}"
        );
    }
    assert!(
        seen.contains("live-churn"),
        "the message added during the crawl must be reached"
    );
}

#[tokio::test]
async fn mutation_partial_results_preserve_successes() {
    let mut state = default_state();
    for id in ["good-1", "bad-1", "good-2"] {
        state.put_message(message(
            id,
            &format!("conv-{id}"),
            id,
            &["0"],
            "sender@example.com",
            "alice@example.com",
            1_700_000_000,
        ));
    }
    state.reject_ids.insert("bad-1".to_string());
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut mutations = ProtonMutations::new(mock.client(Arc::new(StaticTokens)));

    let result = mutations
        .apply(
            MutationRequest {
                operation: OperationId::new("operation-1"),
                targets: vec![
                    RemoteMessageId::new("good-1"),
                    RemoteMessageId::new("bad-1"),
                    RemoteMessageId::new("good-2"),
                ],
                source: None,
                action: MutationAction::SetRead(true),
            },
            &ctx,
        )
        .await
        .expect("per-target results");
    assert_eq!(result.items.len(), 3);
    assert_eq!(result.items[0].outcome, MutationOutcome::Applied);
    assert!(matches!(
        result.items[1].outcome,
        MutationOutcome::Rejected(ref error) if error.kind == ErrorKind::Conflict
    ));
    assert_eq!(result.items[2].outcome, MutationOutcome::Applied);

    let requests = mock.requests();
    let read_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.path == "/mail/v4/messages/read")
        .collect();
    assert_eq!(
        read_requests.len(),
        3,
        "each target gets its own outcome-preserving request"
    );
    assert!(
        read_requests
            .iter()
            .any(|request| request.body.contains("bad-1"))
    );
    let state = mock.state();
    assert_eq!(state.message("good-1").unwrap()["Unread"], json!(0));
    assert_eq!(state.message("good-2").unwrap()["Unread"], json!(0));
    assert_eq!(
        state.message("bad-1").unwrap()["Unread"],
        json!(1),
        "a rejected target must not be reported as applied"
    );
}

// ---------------------------------------------------------------------------
// Additional adapter contracts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expired_access_token_refreshes_through_token_source() {
    let mut state = default_state();
    state.access = "access-2".to_string();
    let mock = ProtonMock::start(state).await;
    let tokens = Arc::new(RotatingTokens {
        calls: Mutex::new(0),
    });
    let client = mock.client(tokens.clone());
    let ctx = context();
    let mut mailbox = ProtonMailbox::new(client);

    mailbox
        .collections(&ctx)
        .await
        .expect("the wrapper refreshes once after a 401");
    assert_eq!(
        *tokens.calls.lock().unwrap(),
        2,
        "one rejected token then one refreshed token"
    );
    let requests = mock.requests();
    let label_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.path == "/core/v4/labels")
        .collect();
    assert_eq!(
        label_requests.len(),
        4,
        "one rejected token plus the three retried label-type fetches"
    );
    assert!(label_requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn snapshot_paging_churn_does_not_skip_messages() {
    let mut state = default_state();
    for index in 0..5 {
        state.put_message(message(
            &format!("boot-{index}"),
            &format!("conv-{index}"),
            &format!("Boot {index}"),
            &["0"],
            "sender@example.com",
            &format!("boot{index}@example.com"),
            1_700_000_000 - index as i64,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let first = sync
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 2,
            },
            &ctx,
        )
        .await
        .expect("first snapshot page");
    assert!(first.has_more);

    // A deletion in the consumed page shifts the next record backwards.
    mock.update(|state| {
        state.messages.retain(|message| message["ID"] != "boot-0");
        state.latest_event = "event-boot-churn".to_string();
    });

    let mut seen: HashSet<String> = upserts(std::slice::from_ref(&first))
        .into_iter()
        .map(|(record, _)| record.remote_id.as_str().to_string())
        .collect();
    let mut cursor = Some(first.checkpoint);
    let mut pages = 1;
    while let Some(next_cursor) = cursor {
        pages += 1;
        assert!(pages < 20, "bootstrap paging must terminate");
        let page = sync
            .next_page(
                SyncRequest {
                    cursor: Some(next_cursor),
                    initial_limit: 2,
                },
                &ctx,
            )
            .await
            .expect("snapshot page");
        for (record, _) in upserts(std::slice::from_ref(&page)) {
            seen.insert(record.remote_id.as_str().to_string());
        }
        if page.has_more {
            cursor = Some(page.checkpoint);
        } else {
            cursor = None;
        }
    }
    for index in 0..5 {
        assert!(
            seen.contains(&format!("boot-{index}")),
            "snapshot churn skipped boot-{index}; seen {seen:?}"
        );
    }
}

#[tokio::test]
async fn snapshot_final_page_churn_does_not_skip_messages() {
    let mut state = default_state();
    for index in 0..4 {
        state.put_message(message(
            &format!("boot-{index}"),
            &format!("conv-{index}"),
            &format!("Boot {index}"),
            &["0"],
            "sender@example.com",
            &format!("boot{index}@example.com"),
            1_700_000_000 - index as i64,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let first = sync
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 2,
            },
            &ctx,
        )
        .await
        .expect("first snapshot page");
    let mut seen: HashSet<String> = upserts(std::slice::from_ref(&first))
        .into_iter()
        .map(|(record, _)| record.remote_id.as_str().to_string())
        .collect();

    // The deletion lands between the second call's anchor read and its final
    // page fetch. A naive post-page cursor advance would move boot-2 backwards
    // into the already-consumed page 0 and skip it forever.
    mock.set_page_hook(1, |state| {
        state.messages.retain(|message| message["ID"] != "boot-0");
        state.latest_event = "event-final-page-churn".to_string();
    });

    let mut cursor = Some(first.checkpoint);
    let mut pages = 1;
    while let Some(next_cursor) = cursor {
        pages += 1;
        assert!(pages < 20, "snapshot paging must terminate");
        let page = sync
            .next_page(
                SyncRequest {
                    cursor: Some(next_cursor),
                    initial_limit: 2,
                },
                &ctx,
            )
            .await
            .expect("snapshot page");
        for (record, _) in upserts(std::slice::from_ref(&page)) {
            seen.insert(record.remote_id.as_str().to_string());
        }
        cursor = page.has_more.then_some(page.checkpoint);
    }

    for index in 1..4 {
        assert!(
            seen.contains(&format!("boot-{index}")),
            "final-page churn skipped boot-{index}; seen {seen:?}"
        );
    }
}

#[tokio::test]
async fn completed_reconcile_retires_absent_messages_after_event_gap() {
    let mut state = default_state();
    for (id, time) in [("retire-me", 1_700_000_100), ("keep-me", 1_700_000_000)] {
        state.put_message(message(
            id,
            &format!("conv-{id}"),
            id,
            &["0"],
            "sender@example.com",
            "alice@example.com",
            time,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    // One per-account client, as the factory caches it, shared across passes.
    let client = mock.client(Arc::new(StaticTokens));
    let mut sync = ProtonSync::new(client.clone());

    let first = drive_sync(&mut sync, &ctx, 10, 40).await;
    let checkpoint = first.last().expect("pages").checkpoint.clone();
    assert_eq!(cursor_stage(&checkpoint).expect("stage"), "complete");

    // The deletion and event retention gap happen while no delta can be read:
    // the next event request forces a reset. The completed reconciliation must
    // retire the message the server no longer lists.
    mock.update(|state| {
        state
            .messages
            .retain(|message| message["ID"] != "retire-me");
        state.latest_event = "event-gap".to_string();
        state.event(
            "event-0",
            json!({
                "EventID": "event-gap",
                "More": 0,
                "Refresh": 1,
                "Messages": [],
                "MessageCounts": [],
            }),
        );
    });

    // The reconcile runs in a *later pass*: the sync service is rebuilt, but it
    // shares the account's client. The completed-sweep baseline must survive
    // that boundary or deletions are never retired after a retention gap.
    let mut later_pass = ProtonSync::new(client.clone());
    let after = drive_sync_from(&mut later_pass, &ctx, Some(checkpoint), 10, 40).await;
    assert_eq!(
        removed(&after),
        vec![RemoteMessageId::new("retire-me")],
        "a completed sweep must retire a record deleted during the retention gap"
    );
    assert!(
        upserts(&after)
            .iter()
            .any(|(record, new)| record.remote_id.as_str() == "keep-me" && !*new),
        "the surviving message is re-upserted historically"
    );
    assert!(
        !upserts(&after)
            .iter()
            .any(|(record, _)| record.remote_id.as_str() == "retire-me"),
        "an absent record must never be re-upserted as present"
    );
}

#[tokio::test]
async fn transient_page_error_does_not_disable_retirement() {
    // A reconcile can span several passes. A transient listing failure must not
    // discard the accumulated sweep baseline, or the retry would fail safe by
    // never retiring a message the server already deleted.
    let mut state = default_state();
    for (id, time) in [("retire-me", 1_700_000_100), ("keep-me", 1_700_000_000)] {
        state.put_message(message(
            id,
            &format!("conv-{id}"),
            id,
            &["0"],
            "sender@example.com",
            "alice@example.com",
            time,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let client = mock.client(Arc::new(StaticTokens));
    let ctx = context();
    let mut sync = ProtonSync::new(client.clone());
    let first = drive_sync(&mut sync, &ctx, 10, 40).await;
    let checkpoint = first.last().expect("pages").checkpoint.clone();
    assert_eq!(cursor_stage(&checkpoint).expect("stage"), "complete");

    // Delete a message and force a retention-gap reset.
    mock.update(|state| {
        state
            .messages
            .retain(|message| message["ID"] != "retire-me");
        state.latest_event = "event-gap".to_string();
        state.event(
            "event-0",
            json!({
                "EventID": "event-gap",
                "More": 0,
                "Refresh": 1,
                "Messages": [],
                "MessageCounts": [],
            }),
        );
    });

    // The first reconcile listing page fails transiently.
    mock.fail_page(0);
    let mut later_pass = ProtonSync::new(client.clone());
    let error = first_page_error(&mut later_pass, &ctx, Some(checkpoint.clone())).await;
    assert_eq!(error.kind, ErrorKind::Transient);

    // Retry from the same checkpoint: retirement must still happen.
    mock.update(|state| state.fail_pages.clear());
    let after = drive_sync_from(&mut later_pass, &ctx, Some(checkpoint), 10, 40).await;
    assert_eq!(
        removed(&after),
        vec![RemoteMessageId::new("retire-me")],
        "a transient page error must not disable retirement for the sweep"
    );
}

#[tokio::test]
async fn failed_history_page_does_not_advance_or_skip() {
    let mut state = default_state();
    for index in 0..5 {
        state.put_message(message(
            &format!("history-{index}"),
            &format!("conv-{index}"),
            &format!("History {index}"),
            &["0"],
            "sender@example.com",
            &format!("history{index}@example.com"),
            1_700_000_000 - index as i64,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut mailbox = ProtonMailbox::new(mock.client(Arc::new(StaticTokens)));

    let first = mailbox
        .history(
            HistoryRequest {
                cursor: None,
                page_size: 2,
            },
            &ctx,
        )
        .await
        .expect("first page");
    let mut seen: HashSet<String> = first
        .messages
        .iter()
        .map(|record| record.remote_id.as_str().to_string())
        .collect();
    let resume = first.checkpoint;

    mock.fail_page(1);
    let error = mailbox
        .history(
            HistoryRequest {
                cursor: Some(resume.clone()),
                page_size: 2,
            },
            &ctx,
        )
        .await
        .expect_err("a failed page must not commit a checkpoint");
    assert_eq!(error.kind, ErrorKind::Transient);
    mock.clear_failed_pages();

    let mut cursor = Some(resume);
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 10, "history must terminate after the retry");
        let page = mailbox
            .history(
                HistoryRequest {
                    cursor: cursor.clone(),
                    page_size: 2,
                },
                &ctx,
            )
            .await
            .expect("retried page");
        for record in &page.messages {
            seen.insert(record.remote_id.as_str().to_string());
        }
        if !page.has_more {
            break;
        }
        cursor = Some(page.checkpoint);
    }
    for index in 0..5 {
        assert!(
            seen.contains(&format!("history-{index}")),
            "a failed page skipped history-{index}; seen {seen:?}"
        );
    }
}

#[tokio::test]
async fn history_page_size_is_pinned_in_the_cursor() {
    let mut state = default_state();
    for index in 0..5 {
        state.put_message(message(
            &format!("pinned-{index}"),
            &format!("conv-{index}"),
            &format!("Pinned {index}"),
            &["0"],
            "sender@example.com",
            &format!("pinned{index}@example.com"),
            1_700_000_000 - index as i64,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut mailbox = ProtonMailbox::new(mock.client(Arc::new(StaticTokens)));

    let first = mailbox
        .history(
            HistoryRequest {
                cursor: None,
                page_size: 2,
            },
            &ctx,
        )
        .await
        .expect("first page");
    // A caller changing its budget mid-crawl must not shift offsets.
    let second = mailbox
        .history(
            HistoryRequest {
                cursor: Some(first.checkpoint),
                page_size: 5,
            },
            &ctx,
        )
        .await
        .expect("second page");
    assert_eq!(second.messages.len(), 2, "the pinned size still pages two");
    let requests = mock.requests();
    let pinned_requests: Vec<Option<&str>> = requests
        .iter()
        .filter(|request| request.path == "/mail/v4/messages")
        .map(|request| request.query_value("PageSize"))
        .collect();
    assert_eq!(
        pinned_requests,
        vec![Some("2"), Some("2")],
        "every page must use the pinned size"
    );
}

#[tokio::test]
async fn snapshot_page_size_is_pinned_in_the_cursor() {
    let mut state = default_state();
    for index in 0..4 {
        state.put_message(message(
            &format!("pinned-{index}"),
            &format!("conv-{index}"),
            &format!("Pinned {index}"),
            &["0"],
            "sender@example.com",
            &format!("pinned{index}@example.com"),
            1_700_000_000 - index as i64,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let first = sync
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 2,
            },
            &ctx,
        )
        .await
        .expect("first snapshot page");
    let second = sync
        .next_page(
            SyncRequest {
                cursor: Some(first.checkpoint),
                initial_limit: 5,
            },
            &ctx,
        )
        .await
        .expect("second snapshot page");
    assert_eq!(
        upserts(std::slice::from_ref(&second)).len(),
        2,
        "the pinned size still pages two"
    );
    let requests = mock.requests();
    let inbox_sizes: Vec<Option<&str>> = requests
        .iter()
        .filter(|request| {
            request.path == "/mail/v4/messages" && request.query_value("LabelID") == Some("0")
        })
        .map(|request| request.query_value("PageSize"))
        .collect();
    assert_eq!(
        inbox_sizes,
        vec![Some("2"), Some("2")],
        "a changed initial_limit must not shift snapshot offsets"
    );
}

#[tokio::test]
async fn label_deleted_ahead_of_cursor_does_not_skip_or_retire() {
    // System folders come first (0,7,8,3,4,6), then user labels sorted by id.
    // Each message belongs only to one user label, so a skipped listing is
    // directly observable as a missing upsert.
    let mut state = default_state()
        .with_label("Label_a", "Alpha", 1)
        .with_label("Label_b", "Beta", 1)
        .with_label("Label_c", "Gamma", 1);
    for (id, label, time) in [
        ("msg-a", "Label_a", 1_700_000_300),
        ("msg-b", "Label_b", 1_700_000_200),
        ("msg-c", "Label_c", 1_700_000_100),
    ] {
        state.put_message(message(
            id,
            &format!("conv-{id}"),
            id,
            &[label],
            "sender@example.com",
            "alice@example.com",
            time,
        ));
    }
    let mock = ProtonMock::start(state).await;
    let ctx = context();
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    // A completed sweep establishes the retirement baseline (msg-a/b/c known).
    let first = drive_sync(&mut sync, &ctx, 10, 40).await;
    let checkpoint = first.last().expect("pages").checkpoint.clone();
    assert_eq!(cursor_stage(&checkpoint).expect("stage"), "complete");
    let baseline = upserts(&first);
    for id in ["msg-a", "msg-b", "msg-c"] {
        assert!(
            baseline
                .iter()
                .any(|(record, _)| record.remote_id.as_str() == id),
            "the baseline sweep must know {id}"
        );
    }

    // Force a reconciliation so a fresh sweep compares against that baseline.
    mock.update(|state| {
        state.latest_event = "event-label-reset".to_string();
        state.event(
            "event-0",
            json!({
                "EventID": "event-label-reset",
                "More": 0,
                "Refresh": 1,
                "Messages": [],
                "MessageCounts": [],
            }),
        );
    });

    // Delete Label_a as soon as the sweep has enumerated its page. Label_b is
    // still ahead of the collection cursor; a positional cursor would shift
    // Label_b into the consumed slot and skip it.
    let mut cursor = Some(checkpoint);
    let mut pages = Vec::new();
    let mut deleted = false;
    for _ in 0..40 {
        let page = sync
            .next_page(
                SyncRequest {
                    cursor: cursor.clone(),
                    initial_limit: 10,
                },
                &ctx,
            )
            .await
            .expect("reconcile page");
        let saw_a = upserts(std::slice::from_ref(&page))
            .iter()
            .any(|(record, _)| record.remote_id.as_str() == "msg-a");
        let more = page.has_more;
        cursor = Some(page.checkpoint.clone());
        pages.push(page);
        if saw_a && !deleted {
            deleted = true;
            mock.update(|state| state.labels.retain(|label| label["ID"] != "Label_a"));
        }
        if !more {
            break;
        }
    }
    assert!(deleted, "the sweep must reach the label deleted mid-sweep");

    let swept = upserts(&pages);
    for id in ["msg-b", "msg-c"] {
        assert!(
            swept
                .iter()
                .any(|(record, _)| record.remote_id.as_str() == id),
            "deleting a label ahead of the cursor skipped {id}; swept {:?}",
            swept
                .iter()
                .map(|(record, _)| record.remote_id.as_str())
                .collect::<Vec<_>>()
        );
    }
    assert!(
        removed(&pages).is_empty(),
        "a scope change mid-sweep must not retire anything; removed {:?}",
        removed(&pages)
    );
}

#[tokio::test]
async fn collections_honors_deadline_after_decorative_counts() {
    let mock = ProtonMock::start(default_state()).await;
    mock.update(|state| state.count_delay = Some(Duration::from_millis(200)));
    let ctx = OperationContext::with_timeout(AccountId::new(), 1, Duration::from_millis(50));
    let mut mailbox = ProtonMailbox::new(mock.client(Arc::new(StaticTokens)));

    // Count failures are decorative, but a deadline that expired while the
    // counts request was in flight must not be swallowed: the call fails.
    let error = mailbox
        .collections(&ctx)
        .await
        .expect_err("an expired deadline must fail collections");
    assert_eq!(error.kind, ErrorKind::Transient);
}

#[tokio::test]
async fn sweep_checkpoint_does_not_grow_with_the_mailbox() {
    // The sweep accumulates every observed message id. Keeping that set in the
    // durable cursor makes each page re-encode the whole mailbox (O(n^2) work).
    // The cursor must stay bounded while the adapter retains the baseline
    // locally.
    let mut state = default_state();
    for index in 0..600 {
        state.put_message(message(
            &format!("m{index}"),
            "c1",
            "swept",
            &["0"],
            "sender@example.com",
            "alice@example.com",
            1_700_000_000 + i64::from(index),
        ));
    }
    let mock = ProtonMock::start(state).await;
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));

    let pages = drive_sync(&mut sync, &context(), 25, 40).await;
    assert_eq!(upserts(&pages).len(), 600, "every message is swept once");

    let largest = pages
        .iter()
        .map(|page| page.checkpoint.payload.len())
        .max()
        .expect("at least one page");
    assert!(
        largest < 2048,
        "the sweep cursor must not carry the whole baseline; largest was {largest} bytes"
    );
}

#[tokio::test]
async fn hung_listing_page_is_bounded_by_context_deadline() {
    // A listing request that never returns must not outlive the operation
    // context: a provider call cannot pin its account slot indefinitely.
    let mut state = default_state();
    state.put_message(message(
        "m1",
        "c1",
        "one",
        &["0"],
        "sender@example.com",
        "alice@example.com",
        1_700_000_000,
    ));
    state.page_delay = Some(Duration::from_secs(5));
    let mock = ProtonMock::start(state).await;
    let mut sync = ProtonSync::new(mock.client(Arc::new(StaticTokens)));
    let ctx = OperationContext::with_timeout(AccountId::new(), 1, Duration::from_millis(50));

    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        sync.next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 10,
            },
            &ctx,
        ),
    )
    .await;
    assert!(
        outcome.is_ok(),
        "a hung listing must be bounded by the context deadline"
    );
    let error = outcome
        .expect("checked above")
        .expect_err("an expired deadline is an error");
    assert_eq!(error.kind, ErrorKind::Transient);
}
