//! Task G3: People saved contacts and Other contacts.
//!
//! Every test drives the adapter against a local HTTP mock. No test reaches
//! Google, and no People wire type is visible outside `providers/google`: the
//! tests use `GoogleAddressBook` and the provider-neutral contact contracts.
//!
//! The two sources are independent: saved contacts use
//! `people.connections.list`, Other contacts use `otherContacts.list`, and the
//! two scopes never stand in for one another. Contact pages feed the generic
//! R6 snapshot path, which owns the sweep after the final page.

#![cfg(feature = "provider-google")]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
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
use pige_core::contacts::rank::{
    OTHER_CONTACT_KIND, SAVED_CONTACT_KIND, query_tokens, rank_candidates,
};
use pige_core::contacts::sync::sync_slice;
use pige_core::contacts::{
    EvidenceId, HeaderRole, LearningOrigin, Observation, ObservationTrust, SuggestionSource,
    normalize_address,
};
use pige_core::mail::Address;
use pige_core::providers::google::{
    GOOGLE_OTHER_CONTACTS_SOURCE, GOOGLE_SAVED_CONTACTS_SOURCE, GmailMailbox, GoogleAddressBook,
    GoogleClient, GoogleEndpoints,
};
use pige_core::providers::{
    AddressBookService, ContactSourceId, ErrorKind, MailboxService, OperationContext,
    ProviderError, RemoteContactId,
};
use pige_core::sync::provider::WorkBudget;

// ---------------------------------------------------------------------------
// Local People + Gmail mock
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RecordedRequest {
    method: String,
    path: String,
    query: Vec<(String, String)>,
}

impl RecordedRequest {
    fn query_value(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn has_query(&self, key: &str) -> bool {
        self.query.iter().any(|(name, _)| name == key)
    }
}

#[derive(Default)]
struct MockState {
    saved_pages: Vec<Value>,
    other_pages: Vec<Value>,
    saved_delta: Option<(u16, Value)>,
    other_delta: Option<(u16, Value)>,
    saved_error: Option<(u16, String)>,
    other_error: Option<(u16, String)>,
    requests: Vec<RecordedRequest>,
}

impl MockState {
    fn with_saved_page(mut self, body: Value) -> Self {
        self.saved_pages.push(body);
        self
    }

    fn with_other_page(mut self, body: Value) -> Self {
        self.other_pages.push(body);
        self
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
            .expect("bind People mock");
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
                        403 => "Forbidden",
                        404 => "Not Found",
                        410 => "Gone",
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

    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["gmail", "v1", "users", "me", "labels"]) => {
            (200, json!({ "labels": [] }).to_string())
        }
        ("GET", ["gmail", "v1", "users", "me", "profile"]) => {
            (200, json!({ "historyId": "gmail-history-1" }).to_string())
        }
        ("GET", ["v1", "people", "me", "connections"]) => {
            if let Some((status, body)) = &state.saved_error {
                return (*status, body.clone());
            }
            if request.has_query("syncToken") {
                return match &state.saved_delta {
                    Some((status, body)) => (*status, body.to_string()),
                    None => (200, json!({ "connections": [] }).to_string()),
                };
            }
            let index = page_index(request, "saved-");
            match state.saved_pages.get(index) {
                Some(body) => (200, body.to_string()),
                None => (200, json!({ "connections": [] }).to_string()),
            }
        }
        ("GET", ["v1", "otherContacts"]) => {
            if let Some((status, body)) = &state.other_error {
                return (*status, body.clone());
            }
            if request.has_query("syncToken") {
                return match &state.other_delta {
                    Some((status, body)) => (*status, body.to_string()),
                    None => (200, json!({ "otherContacts": [] }).to_string()),
                };
            }
            let index = page_index(request, "other-");
            match state.other_pages.get(index) {
                Some(body) => (200, body.to_string()),
                None => (200, json!({ "otherContacts": [] }).to_string()),
            }
        }
        _ => (404, error_body(404, "notFound", "unknown route")),
    }
}

fn page_index(request: &RecordedRequest, prefix: &str) -> usize {
    request
        .query_value("pageToken")
        .and_then(|token| token.strip_prefix(prefix))
        .and_then(|index| index.parse::<usize>().ok())
        .unwrap_or(0)
}

fn error_body(status: u16, reason: &str, message: &str) -> String {
    json!({
        "error": {
            "code": status,
            "message": message,
            "status": if status == 403 { "PERMISSION_DENIED" } else { "FAILED_PRECONDITION" },
            "errors": [{ "reason": reason }],
        }
    })
    .to_string()
}

fn expired_sync_token() -> String {
    error_body(410, "EXPIRED_SYNC_TOKEN", "sync token expired")
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
        id: AccountId("google-people-g3".to_string()),
        label: "Google".to_string(),
        email: "g3@example.com".to_string(),
        display_name: "Google G3".to_string(),
        backend: MailBackend::Gmail {
            user_id: "google-user-1".to_string(),
        },
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    store.upsert_account(&account()).unwrap();
    (dir, store)
}

fn context() -> OperationContext {
    OperationContext::with_timeout(
        AccountId("google-people-g3".to_string()),
        1,
        Duration::from_secs(10),
    )
}

fn saved_source() -> ContactSourceId {
    ContactSourceId::from(GOOGLE_SAVED_CONTACTS_SOURCE)
}

fn other_source() -> ContactSourceId {
    ContactSourceId::from(GOOGLE_OTHER_CONTACTS_SOURCE)
}

fn person(resource: &str, name: &str, email: &str) -> Value {
    json!({
        "resourceName": resource,
        "etag": format!("etag-{resource}"),
        "names": [{ "displayName": name }],
        "emailAddresses": [{ "value": email }],
        "metadata": { "deleted": false },
    })
}

fn deleted_person(resource: &str) -> Value {
    json!({
        "resourceName": resource,
        "metadata": { "deleted": true },
    })
}

fn saved_page(contacts: Vec<Value>, next_page: Option<&str>, sync: Option<&str>) -> Value {
    let mut body = json!({ "connections": contacts });
    if let Some(token) = next_page {
        body["nextPageToken"] = json!(token);
    }
    if let Some(token) = sync {
        body["nextSyncToken"] = json!(token);
    }
    body
}

fn other_page(contacts: Vec<Value>, next_page: Option<&str>, sync: Option<&str>) -> Value {
    let mut body = json!({ "otherContacts": contacts });
    if let Some(token) = next_page {
        body["nextPageToken"] = json!(token);
    }
    if let Some(token) = sync {
        body["nextSyncToken"] = json!(token);
    }
    body
}

fn budget(pages: u32) -> WorkBudget {
    WorkBudget::with_timeout(100, pages, Duration::from_secs(10))
}

fn cached_emails(store: &Store, source: &ContactSourceId) -> Vec<String> {
    store
        .contact_emails(&AccountId("google-people-g3".to_string()))
        .unwrap()
        .into_iter()
        .filter(|contact| &contact.source == source)
        .map(|contact| contact.address.email)
        .collect()
}

fn has_cached(store: &Store, source: &ContactSourceId, email: &str) -> bool {
    cached_emails(store, source)
        .iter()
        .any(|cached| cached == email)
}

fn observe_learned(store: &Store, email: &str) {
    let address = Address::new(Some("Learned".to_string()), email);
    let key = normalize_address(&address).unwrap();
    let account_id = AccountId("google-people-g3".to_string());
    store
        .record_observations(
            &context(),
            &EvidenceId::from("message:g3-learned".to_string()),
            LearningOrigin::History,
            None,
            &[Observation {
                key: key.clone(),
                address,
                role: HeaderRole::To,
                last_seen: 1_700_000_000,
                outgoing: false,
                trust: ObservationTrust::Ordinary,
            }],
        )
        .unwrap();
    assert!(store.learned_address(&account_id, &key).unwrap().is_some());
}

fn learned(store: &Store, email: &str) -> bool {
    let address = Address::new(None, email);
    let key = normalize_address(&address).unwrap();
    store
        .learned_address(&AccountId("google-people-g3".to_string()), &key)
        .unwrap()
        .is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn saved_and_other_contacts_keep_distinct_sources() {
    let (_dir, store) = open_store();
    let server = MockServer::start(
        MockState::default()
            .with_saved_page(saved_page(
                vec![person(
                    "people/saved-1",
                    "Saved Person",
                    "saved@example.com",
                )],
                None,
                Some("saved-sync-1"),
            ))
            .with_other_page(other_page(
                vec![person(
                    "otherContacts/other-1",
                    "Other Person",
                    "other@example.com",
                )],
                None,
                Some("other-sync-1"),
            )),
    )
    .await;
    let saved = saved_source();
    let other = other_source();
    store
        .upsert_contact_source_with_kind(&account().id, &saved, None, SAVED_CONTACT_KIND)
        .unwrap();
    store
        .upsert_contact_source_with_kind(&account().id, &other, None, OTHER_CONTACT_KIND)
        .unwrap();

    let client = server.client();
    let mut saved_book = GoogleAddressBook::new(client.clone(), "me");
    let mut other_book = GoogleAddressBook::new(client, "me");
    let context = context();

    sync_slice(&store, &mut saved_book, &saved, &context, budget(5))
        .await
        .unwrap();
    sync_slice(&store, &mut other_book, &other, &context, budget(5))
        .await
        .unwrap();

    assert_eq!(cached_emails(&store, &saved), vec!["saved@example.com"]);
    assert_eq!(cached_emails(&store, &other), vec!["other@example.com"]);
    assert_eq!(
        store
            .contact_source_kind(&account().id, &saved)
            .unwrap()
            .as_deref(),
        Some(SAVED_CONTACT_KIND)
    );
    assert_eq!(
        store
            .contact_source_kind(&account().id, &other)
            .unwrap()
            .as_deref(),
        Some(OTHER_CONTACT_KIND)
    );

    let paths: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert!(paths.iter().any(|path| path == "/v1/people/me/connections"));
    assert!(paths.iter().any(|path| path == "/v1/otherContacts"));
}

#[tokio::test]
async fn saved_person_and_other_person_share_suggestion_not_remote_id() {
    let (_dir, store) = open_store();
    let id = account().id;
    let saved = saved_source();
    let other = other_source();
    store
        .upsert_contact_source_with_kind(&id, &saved, None, SAVED_CONTACT_KIND)
        .unwrap();
    store
        .upsert_contact_source_with_kind(&id, &other, None, OTHER_CONTACT_KIND)
        .unwrap();
    let address = Address::new(Some("Shared Person".to_string()), "share@example.com");
    store
        .upsert_contact_email(
            &id,
            &saved,
            &RemoteContactId::new("people/shared"),
            &address,
        )
        .unwrap();
    store
        .upsert_contact_email(
            &id,
            &other,
            &RemoteContactId::new("otherContacts/shared"),
            &address,
        )
        .unwrap();

    let folded = "share";
    let candidates = store
        .recipient_candidates(&id, folded, &query_tokens(folded), 64)
        .unwrap();
    let suggestions = rank_candidates(
        candidates,
        &id,
        folded,
        &HashSet::new(),
        &HashSet::new(),
        10,
    );

    assert_eq!(
        suggestions.len(),
        1,
        "identity is the address, not a remote id"
    );
    assert_eq!(suggestions[0].address.email, "share@example.com");
    assert_eq!(
        suggestions[0].sources,
        vec![
            SuggestionSource::SavedContact,
            SuggestionSource::OtherContact
        ]
    );
    assert!(!suggestions[0].address.email.contains("people/"));
}

#[tokio::test]
async fn deleted_person_removes_contact_source_only() {
    let (_dir, store) = open_store();
    let id = account().id;
    let saved = saved_source();
    let other = other_source();
    store
        .upsert_contact_source_with_kind(&id, &saved, None, SAVED_CONTACT_KIND)
        .unwrap();
    store
        .upsert_contact_source_with_kind(&id, &other, None, OTHER_CONTACT_KIND)
        .unwrap();
    let address = Address::new(Some("Gone Person".to_string()), "gone@example.com");
    store
        .upsert_contact_email(&id, &saved, &RemoteContactId::new("people/gone"), &address)
        .unwrap();
    store
        .upsert_contact_email(
            &id,
            &other,
            &RemoteContactId::new("otherContacts/gone"),
            &address,
        )
        .unwrap();
    observe_learned(&store, "gone@example.com");

    let server = MockServer::start(MockState::default().with_saved_page(saved_page(
        vec![deleted_person("people/gone")],
        None,
        Some("saved-sync-2"),
    )))
    .await;
    let mut book = GoogleAddressBook::new(server.client(), "me");

    sync_slice(&store, &mut book, &saved, &context(), budget(5))
        .await
        .unwrap();

    assert!(!has_cached(&store, &saved, "gone@example.com"));
    assert!(
        has_cached(&store, &other, "gone@example.com"),
        "a saved-contact tombstone must not remove the same address from Other contacts"
    );
    assert!(
        learned(&store, "gone@example.com"),
        "mail-derived evidence is independent of a contact source"
    );
}

#[tokio::test]
async fn expired_sync_token_triggers_full_sweep() {
    let (_dir, _store) = open_store();
    let server = MockServer::start({
        let mut state = MockState::default().with_saved_page(saved_page(
            vec![person("people/one", "One", "one@example.com")],
            None,
            Some("saved-token-1"),
        ));
        state.saved_delta = Some((
            410,
            serde_json::from_str::<Value>(&expired_sync_token()).unwrap(),
        ));
        state
    })
    .await;
    let mut book = GoogleAddressBook::new(server.client(), "me");

    let first = book
        .next_page(&saved_source(), None, &context())
        .await
        .unwrap();
    assert!(!first.has_more);
    let first_generation: Value = serde_json::from_str(&first.checkpoint.payload).unwrap();
    assert_eq!(first_generation["sweep"], 1);

    // Resuming the completed cursor requests an incremental delta. The mock
    // reports People's EXPIRED_SYNC_TOKEN; the adapter must fall back to a full
    // snapshot rather than surfacing the cursor as permanently broken.
    let second = book
        .next_page(&saved_source(), Some(first.checkpoint), &context())
        .await
        .unwrap();
    assert!(!second.has_more);
    assert_eq!(second.changes.len(), 1);
    let second_generation: Value = serde_json::from_str(&second.checkpoint.payload).unwrap();
    assert_eq!(second_generation["sweep"], 2);

    let requests = server.requests();
    let people: Vec<&RecordedRequest> = requests
        .iter()
        .filter(|request| request.path == "/v1/people/me/connections")
        .collect();
    assert_eq!(people.len(), 3, "full, expired delta, then full sweep");
    assert!(!people[0].has_query("syncToken"));
    assert_eq!(people[1].query_value("syncToken"), Some("saved-token-1"));
    assert!(
        !people[2].has_query("syncToken"),
        "an expired token is retried as a full sync without syncToken"
    );
    assert_eq!(people[2].query_value("requestSyncToken"), Some("true"));
    assert!(
        !people.iter().any(|request| request.path.contains("gmail")),
        "contacts never use a Gmail history cursor"
    );
}

#[tokio::test]
async fn missing_permission_keeps_mail_and_local_history() {
    let (_dir, store) = open_store();
    let id = account().id;
    let saved = saved_source();
    store
        .upsert_contact_source_with_kind(&id, &saved, None, SAVED_CONTACT_KIND)
        .unwrap();
    store
        .upsert_contact_email(
            &id,
            &saved,
            &RemoteContactId::new("people/cached"),
            &Address::new(None, "cached@example.com"),
        )
        .unwrap();
    observe_learned(&store, "history@example.com");

    let server = MockServer::start(MockState {
        saved_error: Some((
            403,
            error_body(403, "PERMISSION_DENIED", "no contacts scope"),
        )),
        ..MockState::default()
    })
    .await;
    let client = server.client();

    // Mail is a separate capability on the same authenticated client.
    let mut mailbox = GmailMailbox::new(client.clone(), "me");
    assert!(mailbox.collections(&context()).await.is_ok());

    let mut book = GoogleAddressBook::new(client, "me");
    let error = sync_slice(&store, &mut book, &saved, &context(), budget(5))
        .await
        .expect_err("a denied contacts scope must be reported");
    assert_eq!(error.kind, ErrorKind::Forbidden);
    assert!(
        !store.contact_source_state(&id, &saved).unwrap().enabled,
        "only the affected source is disabled"
    );
    assert!(has_cached(&store, &saved, "cached@example.com"));
    assert!(learned(&store, "history@example.com"));
}

#[tokio::test]
async fn partial_snapshot_does_not_delete_cached_contacts() {
    let (_dir, store) = open_store();
    let id = account().id;
    let saved = saved_source();
    store
        .upsert_contact_source_with_kind(&id, &saved, None, SAVED_CONTACT_KIND)
        .unwrap();
    store
        .upsert_contact_email(
            &id,
            &saved,
            &RemoteContactId::new("people/stale"),
            &Address::new(None, "stale@example.com"),
        )
        .unwrap();

    let server = MockServer::start(MockState::default().with_saved_page(saved_page(
        vec![person("people/new", "New", "new@example.com")],
        Some("saved-1"),
        None,
    )))
    .await;
    let mut book = GoogleAddressBook::new(server.client(), "me");

    let progress = sync_slice(&store, &mut book, &saved, &context(), budget(1))
        .await
        .unwrap();
    assert!(
        !progress.complete,
        "the slice stopped before the final page"
    );
    assert!(has_cached(&store, &saved, "new@example.com"));
    assert!(
        has_cached(&store, &saved, "stale@example.com"),
        "a partial snapshot must not sweep cached contacts"
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.query_value("pageToken").is_none()),
        "the first page is requested without a page token"
    );
}

#[tokio::test]
async fn all_pages_keep_identical_read_parameters() {
    let (_dir, _store) = open_store();
    let server = MockServer::start(
        MockState::default()
            .with_saved_page(saved_page(
                vec![person("people/one", "One", "one@example.com")],
                Some("saved-1"),
                None,
            ))
            .with_saved_page(saved_page(
                vec![person("people/two", "Two", "two@example.com")],
                None,
                Some("saved-sync"),
            )),
    )
    .await;
    let mut book = GoogleAddressBook::new(server.client(), "me");
    let context = context();

    let first = book
        .next_page(&saved_source(), None, &context)
        .await
        .unwrap();
    assert!(first.has_more);
    let second = book
        .next_page(&saved_source(), Some(first.checkpoint), &context)
        .await
        .unwrap();
    assert!(!second.has_more);

    let people: Vec<RecordedRequest> = server
        .requests()
        .into_iter()
        .filter(|request| request.path == "/v1/people/me/connections")
        .collect();
    assert_eq!(people.len(), 2);
    for request in &people {
        assert_eq!(
            request.query_value("personFields"),
            Some("names,emailAddresses,metadata")
        );
        assert_eq!(
            request.query_value("sources"),
            Some("READ_SOURCE_TYPE_CONTACT")
        );
        assert_eq!(request.query_value("pageSize"), Some("100"));
        assert_eq!(request.query_value("requestSyncToken"), Some("true"));
        assert!(!request.has_query("syncToken"));
        assert!(!request.has_query("organizations"));
    }
    assert!(people[0].query_value("pageToken").is_none());
    assert_eq!(people[1].query_value("pageToken"), Some("saved-1"));
}

#[tokio::test]
async fn other_contacts_scope_is_not_assumed_from_contacts_scope() {
    let (_dir, store) = open_store();
    let id = account().id;
    let saved = saved_source();
    let other = other_source();
    store
        .upsert_contact_source_with_kind(&id, &saved, None, SAVED_CONTACT_KIND)
        .unwrap();
    store
        .upsert_contact_source_with_kind(&id, &other, None, OTHER_CONTACT_KIND)
        .unwrap();

    let server = MockServer::start(MockState {
        saved_pages: vec![saved_page(
            vec![person("people/one", "One", "one@example.com")],
            None,
            Some("saved-sync"),
        )],
        other_error: Some((403, error_body(403, "PERMISSION_DENIED", "no other scope"))),
        ..MockState::default()
    })
    .await;
    let client = server.client();
    let mut saved_book = GoogleAddressBook::new(client.clone(), "me");
    let mut other_book = GoogleAddressBook::new(client, "me");
    let context = context();

    sync_slice(&store, &mut saved_book, &saved, &context, budget(5))
        .await
        .unwrap();
    let error = sync_slice(&store, &mut other_book, &other, &context, budget(5))
        .await
        .expect_err("the saved-contacts scope does not authorize Other contacts");
    assert_eq!(error.kind, ErrorKind::Forbidden);

    assert!(store.contact_source_state(&id, &saved).unwrap().enabled);
    assert!(!store.contact_source_state(&id, &other).unwrap().enabled);
    assert!(has_cached(&store, &saved, "one@example.com"));

    let other_request = server
        .requests()
        .into_iter()
        .find(|request| request.path == "/v1/otherContacts")
        .expect("Other contacts use their own endpoint");
    assert_eq!(
        other_request.query_value("readMask"),
        Some("names,emailAddresses,metadata")
    );
    assert!(!other_request.has_query("personFields"));
    assert!(
        !other_request
            .query_value("readMask")
            .unwrap_or_default()
            .contains("organizations")
    );
}
