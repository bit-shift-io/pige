//! Task P4: recipient policy, encrypted Proton submission and reconciliation.
//!
//! Every case runs against a local synthetic Proton API with runtime-generated
//! OpenPGP keys. No fixture contains real mail, keys, addresses or tokens; all
//! addresses are `example.com`. The named cases are the P4 RED/GREEN contract:
//! internal/external encryption policy, fail-closed key/contact lookup, explicit
//! ordinary-external classification, Bcc privacy, sender-alias validation,
//! encrypted attachments, interrupted preparation, uncertain sends, draft-vs-
//! delivery identity, ServerManaged Sent copies, and once-only acceptance
//! learning.

#![cfg(feature = "provider-proton")]
// Synthetic fixtures populate a large mock state incrementally; the builder
// shape is clearer than a six-field struct literal in every test.
#![allow(clippy::field_reassign_with_default)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use pige_core::Store;
use pige_core::account::{Account, AccountId, MailBackend};
use pige_core::auth::TokenSource;
use pige_core::mail::{Address, AttachmentDisposition};
use pige_core::providers::proton::{
    DeliverySecurity, ProtonClient, ProtonEndpoints, ProtonMailbox, ProtonRecipientPolicy,
    ProtonSubmission, UnlockedKeyStore, proton_core, proton_crypto,
};
use pige_core::providers::{
    ErrorKind, MailboxService, OperationContext, ProviderError, RemoteAttachmentId,
    RemoteMessageId, SentCopyPolicy, SubmissionAttachment, SubmissionOutcome, SubmissionReceipt,
    SubmissionRequest, SubmissionService,
};
use pige_core::send::{
    OutgoingMessage, SubmissionObserver, SubmissionState, flush_provider_outbox,
    flush_provider_outbox_with_observer, queue_submission,
};
use proton_core::crypto::keys::{AddressKeys, KeyStore, StoredKey};
use proton_crypto::crypto::{
    DataEncoding, Encryptor, EncryptorSync, KeyGenerator, KeyGeneratorAlgorithm, KeyGeneratorSync,
    PGPMessage, PGPProviderSync, Signer, SignerSync,
};
use tempfile::TempDir;

const ACCESS: &str = "synthetic-access";
const SENDER: &str = "me@example.com";

// ---------------------------------------------------------------------------
// Synthetic Proton API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RecordedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Default)]
struct MockState {
    contacts: Vec<Value>,
    /// API `/core/v4/keys/all` responses keyed by recipient email.
    recipient_keys: HashMap<String, Value>,
    drafts: HashMap<String, Value>,
    sent: HashMap<String, Value>,
    /// Encrypted data packets keyed by attachment id.
    attachments: HashMap<String, Vec<u8>>,
    fail_contacts: Option<u16>,
    fail_keys: Option<u16>,
    fail_send: Option<u16>,
    next_id: usize,
    draft_create_count: usize,
    send_count: usize,
    requests: Vec<RecordedRequest>,
    send_paths: Vec<String>,
}

impl MockState {
    fn send_packages(&self) -> Vec<Value> {
        self.requests
            .iter()
            .filter(|request| request.method == "POST")
            .filter(|request| {
                request.path.starts_with("/mail/v4/messages/")
                    && !request.path.ends_with("/receipt")
            })
            .filter_map(|request| request.json().get("Packages").cloned())
            .filter_map(|packages| packages.as_array().cloned())
            .flatten()
            .collect()
    }

    fn draft_bodies(&self) -> Vec<Value> {
        self.requests
            .iter()
            .filter(|request| request.method == "POST" && request.path == "/mail/v4/messages")
            .map(RecordedRequest::json)
            .collect()
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
        let responder = shared.clone();
        Mock::given(any())
            .respond_with(move |request: &Request| {
                let mut state = responder.lock().expect("mock state");
                respond(&mut state, request)
            })
            .mount(&server)
            .await;
        Self {
            server,
            state: shared,
        }
    }

    fn snapshot<T>(&self, read: impl FnOnce(&MockState) -> T) -> T {
        read(&self.state.lock().expect("mock state"))
    }

    fn client(&self, keys: KeyStore) -> ProtonClient {
        let store = Arc::new(UnlockedKeyStore::new());
        store.install_test_keys("synthetic-uid", keys);
        ProtonClient::new(
            Arc::new(StaticTokens),
            store,
            ProtonEndpoints::new_insecure_for_tests(self.server.uri()).expect("mock endpoint"),
        )
    }
}

struct StaticTokens;

#[async_trait]
impl TokenSource for StaticTokens {
    async fn access_token(&self, _ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        Ok(SecretString::from(ACCESS.to_string()))
    }
}

fn respond(state: &mut MockState, request: &Request) -> ResponseTemplate {
    state.requests.push(RecordedRequest {
        method: request.method.as_str().to_string(),
        path: request.url.path().to_string(),
        body: request.body.clone(),
    });
    let authorized = request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {ACCESS}"));
    if !authorized {
        return ResponseTemplate::new(401).set_body_json(json!({"Code": 401}));
    }
    let path = request.url.path();
    let method = request.method.as_str();
    let query = |name: &str| {
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };

    if path == "/contacts/v4/contacts" {
        if let Some(status) = state.fail_contacts {
            return ResponseTemplate::new(status).set_body_json(json!({"Code": status}));
        }
        let page = query("Page")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let page_size = query("PageSize")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100)
            .max(1);
        let start = page.saturating_mul(page_size);
        let contacts: Vec<Value> = state
            .contacts
            .iter()
            .skip(start)
            .take(page_size)
            .cloned()
            .collect();
        return ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Total": state.contacts.len(),
            "Contacts": contacts,
        }));
    }

    if path == "/core/v4/keys/all" {
        if let Some(status) = state.fail_keys {
            return ResponseTemplate::new(status).set_body_json(json!({"Code": status}));
        }
        let email = query("Email").unwrap_or_default();
        return ResponseTemplate::new(200).set_body_json(
            state
                .recipient_keys
                .get(&email)
                .cloned()
                .unwrap_or_else(|| external_keys("", &[])),
        );
    }

    if path == "/mail/v4/messages" && method == "GET" {
        let label = query("LabelID").unwrap_or_default();
        let source = if label == "7" {
            &state.sent
        } else {
            &state.drafts
        };
        let messages: Vec<Value> = source
            .values()
            .map(|message| {
                json!({
                    "ID": message["ID"],
                    "Subject": message["Subject"],
                    "ExternalID": message["ExternalID"],
                    "LabelIDs": message["LabelIDs"],
                    "Flags": message["Flags"],
                })
            })
            .collect();
        return ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Total": messages.len(),
            "Messages": messages,
        }));
    }

    if path == "/mail/v4/messages" && method == "POST" {
        state.draft_create_count += 1;
        let body: Value =
            serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({"Message": {}}));
        let mut message = body.get("Message").cloned().unwrap_or_else(|| json!({}));
        state.next_id += 1;
        let id = format!("draft-{}", state.next_id);
        message["ID"] = json!(id);
        message["LabelIDs"] = json!(["8"]);
        message["Flags"] = json!(0);
        if !message.get("ExternalID").is_some_and(Value::is_string) {
            message["ExternalID"] = json!(format!("<{id}@synthetic.example>"));
        }
        state.drafts.insert(id.clone(), message.clone());
        let mut response = message;
        response["ID"] = json!(id);
        return ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Message": response,
        }));
    }

    if let Some(id) = path.strip_prefix("/mail/v4/messages/") {
        if method == "POST" {
            state.send_count += 1;
            state.send_paths.push(id.to_string());
            if let Some(status) = state.fail_send {
                return ResponseTemplate::new(status).set_body_json(json!({"Code": status}));
            }
            if let Some(mut message) = state.drafts.remove(id) {
                message["LabelIDs"] = json!(["7"]);
                let flags = message["Flags"].as_u64().unwrap_or(0) | (1 << 1);
                message["Flags"] = json!(flags);
                state.sent.insert(id.to_string(), message);
            }
            return ResponseTemplate::new(200).set_body_json(json!({"Code": 1000}));
        }
        if method == "GET" {
            let message = state.drafts.get(id).or_else(|| state.sent.get(id)).cloned();
            return match message {
                Some(message) => ResponseTemplate::new(200).set_body_json(json!({
                    "Code": 1000,
                    "Message": message,
                })),
                None => ResponseTemplate::new(404).set_body_json(json!({"Code": 2501})),
            };
        }
    }

    if path == "/mail/v4/attachments" && method == "POST" {
        let filename = multipart_text(&request.body, "Filename").unwrap_or_default();
        let message_id = multipart_text(&request.body, "MessageID").unwrap_or_default();
        let content_id = multipart_text(&request.body, "ContentID").unwrap_or_default();
        let mime = multipart_text(&request.body, "MIMEType")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let key_packets = multipart_bytes(&request.body, "KeyPackets").unwrap_or_default();
        let data_packet = multipart_bytes(&request.body, "DataPacket").unwrap_or_default();
        state.next_id += 1;
        let id = format!("att-{}", state.next_id);
        state.attachments.insert(id.clone(), data_packet.clone());
        let metadata = json!({
            "ID": id,
            "Name": filename,
            "Size": data_packet.len(),
            "KeyPackets": STANDARD.encode(&key_packets),
            "MIMEType": mime,
            "ContentID": if content_id.is_empty() { Value::Null } else { json!(content_id) },
        });
        for message in state.drafts.values_mut().chain(state.sent.values_mut()) {
            if message["ID"].as_str() == Some(message_id.as_str()) {
                message["Attachments"]
                    .as_array_mut()
                    .map(|attachments| attachments.push(metadata.clone()))
                    .unwrap_or_else(|| message["Attachments"] = json!([metadata.clone()]));
                message["NumAttachments"] = json!(1);
            }
        }
        return ResponseTemplate::new(200).set_body_json(json!({
            "Code": 1000,
            "Attachment": { "ID": id },
        }));
    }

    if let Some(id) = path.strip_prefix("/mail/v4/attachments/")
        && method == "GET"
    {
        return match state.attachments.get(id) {
            Some(bytes) => ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(bytes.clone()),
            None => ResponseTemplate::new(404).set_body_json(json!({"Code": 2501})),
        };
    }

    ResponseTemplate::new(404).set_body_json(json!({"Code": 2501}))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn multipart_bytes(body: &[u8], name: &str) -> Option<Vec<u8>> {
    let needle = format!("name=\"{name}\"");
    let position = find_bytes(body, needle.as_bytes())?;
    let after = &body[position + needle.len()..];
    let header_end = find_bytes(after, b"\r\n\r\n")? + 4;
    let data = &after[header_end..];
    let end = find_bytes(data, b"\r\n--")?;
    Some(data[..end].to_vec())
}

fn multipart_text(body: &[u8], name: &str) -> Option<String> {
    multipart_bytes(body, name).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// Synthetic keys and recipients
// ---------------------------------------------------------------------------

struct GeneratedKey {
    stored: StoredKey,
    public: String,
}

fn provider() -> impl PGPProviderSync {
    proton_crypto::new_pgp_provider()
}

fn generate_key(email: &str) -> GeneratedKey {
    let provider = provider();
    let passphrase = "synthetic-passphrase";
    let private = provider
        .new_key_generator()
        .with_user_id("Synthetic", email)
        .with_algorithm(KeyGeneratorAlgorithm::ECC)
        .generate()
        .expect("generate key");
    let armored = String::from_utf8(
        provider
            .private_key_export(&private, passphrase, DataEncoding::Armor)
            .expect("export private")
            .as_ref()
            .to_vec(),
    )
    .expect("private armor");
    let public = String::from_utf8(
        provider
            .public_key_export(
                &provider
                    .private_key_to_public_key(&private)
                    .expect("derive public"),
                DataEncoding::Armor,
            )
            .expect("export public")
            .as_ref()
            .to_vec(),
    )
    .expect("public armor");
    GeneratedKey {
        stored: StoredKey {
            armored,
            passphrase: SecretString::from(passphrase.to_string()),
        },
        public,
    }
}

fn address_keys(address_id: &str, email: &str, keys: &[&GeneratedKey]) -> AddressKeys {
    AddressKeys {
        address_id: address_id.to_string(),
        email: email.to_string(),
        keys: keys.iter().map(|key| key.stored.clone()).collect(),
    }
}

fn key_store(addresses: Vec<AddressKeys>) -> KeyStore {
    KeyStore {
        user_keys: Vec::new(),
        addresses,
    }
}

fn encrypt_to(recipient_public: &str, text: &str) -> String {
    let provider = provider();
    let public = provider
        .public_key_import(recipient_public.as_bytes(), DataEncoding::Armor)
        .expect("import recipient key");
    let message = provider
        .new_encryptor()
        .with_encryption_key(&public)
        .with_utf8()
        .encrypt(text.as_bytes())
        .expect("encrypt card");
    String::from_utf8(message.armor().expect("armor card")).expect("armor utf8")
}

fn external_keys(email: &str, keys: &[(&str, u32)]) -> Value {
    let keys: Vec<Value> = keys
        .iter()
        .map(|(public, flags)| json!({"PublicKey": public, "Flags": flags, "Primary": 1}))
        .collect();
    json!({
        "Code": 1000,
        "RecipientType": 2,
        "Address": {"Email": email, "Keys": []},
        "Unverified": {"Email": email, "Keys": keys},
    })
}

fn internal_keys(email: &str, keys: &[(&str, u32)]) -> Value {
    let keys: Vec<Value> = keys
        .iter()
        .map(|(public, flags)| json!({"PublicKey": public, "Flags": flags, "Primary": 1}))
        .collect();
    json!({
        "Code": 1000,
        "RecipientType": 1,
        "Address": {"Email": email, "Keys": keys},
    })
}

fn contact(id: &str, email: &str, cards: Vec<Value>) -> Value {
    json!({
        "ID": id,
        "Name": "Synthetic Contact",
        "UID": format!("uid-{id}"),
        "ContactEmails": [{
            "ID": format!("email-{id}"),
            "Email": email,
            "Name": "Synthetic Contact",
            "ContactID": id,
        }],
        "Cards": cards,
    })
}

fn clear_card(vcard: &str) -> Value {
    json!({"Type": "CLEAR", "Data": vcard, "Signature": ""})
}

fn encrypted_card(vcard: &str, me: &GeneratedKey) -> Value {
    json!({
        "Type": "ENCRYPTED",
        "Data": encrypt_to(&me.public, vcard),
        "Signature": "",
    })
}

fn sign_card(data: &str, signer: &GeneratedKey) -> String {
    let provider = provider();
    let private = provider
        .private_key_import(
            signer.stored.armored.as_bytes(),
            signer.stored.passphrase.expose_secret().as_bytes(),
            DataEncoding::Armor,
        )
        .expect("import signing key");
    let signature = provider
        .new_signer()
        .with_signing_key(&private)
        .sign_detached(data.as_bytes(), DataEncoding::Armor)
        .expect("sign card");
    String::from_utf8(signature).expect("armored signature")
}

fn pinned_key_policy_card(key: &str, signer: Option<&GeneratedKey>) -> Value {
    let vcard = format!(
        "BEGIN:VCARD\nVERSION:4.0\nEMAIL:verify@example.com\nX-PM-ENCRYPT:true\nX-PM-VERIFY:true\nKEY;ENCODING=b:base64,{}\nEND:VCARD",
        STANDARD.encode(key.as_bytes())
    );
    let signature = signer
        .map(|signer| sign_card(&vcard, signer))
        .unwrap_or_default();
    json!({"Type": "CLEAR", "Data": vcard, "Signature": signature})
}

// ---------------------------------------------------------------------------
// Domain helpers
// ---------------------------------------------------------------------------

fn account() -> Account {
    Account {
        id: AccountId("proton-p4".to_string()),
        label: "Proton".to_string(),
        email: SENDER.to_string(),
        display_name: "Me".to_string(),
        backend: MailBackend::proton(None),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn context() -> OperationContext {
    OperationContext::with_timeout(account().id, 1, Duration::from_secs(30))
}

fn message(to: Vec<&str>, cc: Vec<&str>, bcc: Vec<&str>) -> OutgoingMessage {
    ics_message(
        to,
        cc,
        bcc,
        "Hello from P4",
        "Body of the synthetic message.",
    )
}

fn ics_message(
    to: Vec<&str>,
    cc: Vec<&str>,
    bcc: Vec<&str>,
    subject: &str,
    body: &str,
) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account().id,
        to: to
            .into_iter()
            .map(|email| Address::new(None, email))
            .collect(),
        cc: cc
            .into_iter()
            .map(|email| Address::new(None, email))
            .collect(),
        bcc: bcc
            .into_iter()
            .map(|email| Address::new(None, email))
            .collect(),
        subject: subject.to_string(),
        body: body.to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

fn request(message: OutgoingMessage) -> SubmissionRequest {
    SubmissionRequest {
        operation: pige_core::providers::OperationId::new("operation-p4"),
        message,
        attachments: Vec::new(),
        prepared: None,
    }
}

fn part(id: &str, bytes: &[u8]) -> SubmissionAttachment {
    SubmissionAttachment {
        id: id.to_string(),
        filename: format!("{id}.txt"),
        content_type: "text/plain".to_string(),
        bytes: bytes.to_vec(),
        content_id: None,
        disposition: AttachmentDisposition::Attachment,
    }
}

fn package_of_type(packages: &[Value], package_type: i64) -> &Value {
    packages
        .iter()
        .find(|package| package["Type"].as_i64() == Some(package_type))
        .unwrap_or_else(|| panic!("no package type {package_type}: {packages:?}"))
}

fn decrypt_package_body(body: &Value, entry: &Value, address: &AddressKeys) -> Vec<u8> {
    let key_packet = entry["BodyKeyPacket"].as_str().expect("body key packet");
    let data = STANDARD
        .decode(body["Body"].as_str().expect("body packet"))
        .expect("body base64");
    proton_core::crypto::decrypt_attachment(&provider(), address, key_packet, &data)
        .expect("decrypt package body")
}

fn open_store() -> (TempDir, Store, Account) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    let account = account();
    store.upsert_account(&account).unwrap();
    (dir, store, account)
}

fn stable_external_id(operation: &pige_core::providers::OperationId) -> String {
    format!("<pige-{}@pige.invalid>", operation.as_str())
}

// ---------------------------------------------------------------------------
// Named P4 tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn internal_recipients_are_encrypted() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let account = account();
    let mut submission = ProtonSubmission::new(mock.client(keys), account.clone());
    let mut req = request(message(vec!["bob@proton.example"], vec![], vec![]));

    let prepared = submission.prepare(&req, &context()).await.expect("prepare");
    req.prepared = Some(prepared);
    let outcome = submission.submit(&req, &context()).await.expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));

    let packages = mock.snapshot(MockState::send_packages);
    let internal = package_of_type(&packages, 1);
    assert!(
        packages
            .iter()
            .all(|package| package["Type"].as_i64() != Some(4))
    );
    let entry = &internal["Addresses"]["bob@proton.example"];
    assert!(entry.is_object());
    let plain = decrypt_package_body(
        internal,
        entry,
        &address_keys("recipient", "bob@proton.example", &[&bob]),
    );
    assert_eq!(
        String::from_utf8(plain).unwrap(),
        "Body of the synthetic message."
    );
}

#[tokio::test]
async fn external_required_pgp_is_honored() {
    let me = generate_key(SENDER);
    let external = generate_key("pgp@example.com");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    state.contacts = vec![contact(
        "contact-pgp",
        "pgp@example.com",
        vec![clear_card(
            "BEGIN:VCARD\nVERSION:4.0\nEMAIL:pgp@example.com\nX-PM-ENCRYPT:true\nEND:VCARD",
        )],
    )];
    // The published external key is not Proton-verified, but the contact's
    // mandatory encryption policy is satisfied by the usable OpenPGP key.
    state.recipient_keys.insert(
        "pgp@example.com".to_string(),
        external_keys("pgp@example.com", &[(&external.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let mut req = request(message(vec!["pgp@example.com"], vec![], vec![]));

    let prepared = submission.prepare(&req, &context()).await.expect("prepare");
    req.prepared = Some(prepared);
    let outcome = submission.submit(&req, &context()).await.expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));

    let packages = mock.snapshot(MockState::send_packages);
    let pgp = package_of_type(&packages, 8);
    assert!(
        packages
            .iter()
            .all(|package| package["Type"].as_i64() != Some(4))
    );
    let entry = &pgp["Addresses"]["pgp@example.com"];
    let plain = decrypt_package_body(
        pgp,
        entry,
        &address_keys("external", "pgp@example.com", &[&external]),
    );
    assert_eq!(
        String::from_utf8(plain).unwrap(),
        "Body of the synthetic message."
    );
}

#[tokio::test]
async fn unavailable_required_key_blocks_all_submission() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    state.contacts = vec![contact(
        "contact-required",
        "required@example.com",
        vec![clear_card(
            "BEGIN:VCARD\nVERSION:4.0\nEMAIL:required@example.com\nX-PM-ENCRYPT:true\nEND:VCARD",
        )],
    )];
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    // The required external recipient has no usable key at all.
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let req = request(message(
        vec!["bob@proton.example"],
        vec!["required@example.com"],
        vec![],
    ));

    let error = submission
        .prepare(&req, &context())
        .await
        .expect_err("mandatory key policy must block the whole operation");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    let lower = error.safe_message.to_ascii_lowercase();
    assert!(
        lower.contains("encrypt") || lower.contains("policy") || lower.contains("key"),
        "the safe error should name the policy: {error:?}"
    );
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
    assert_eq!(mock.snapshot(|state| state.send_count), 0);
}

#[tokio::test]
async fn encrypted_contact_policy_cannot_be_ignored() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    state.contacts = vec![contact(
        "contact-encrypted",
        "secret-policy@example.com",
        vec![encrypted_card(
            "BEGIN:VCARD\nVERSION:4.0\nEMAIL:secret-policy@example.com\nX-PM-ENCRYPT:true\nEND:VCARD",
            &me,
        )],
    )];
    // Keyless external. If the encrypted card were ignored this would be an
    // ordinary external send; the mandatory policy must block it instead.
    state.recipient_keys.insert(
        "secret-policy@example.com".to_string(),
        external_keys("secret-policy@example.com", &[]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let req = request(message(vec!["secret-policy@example.com"], vec![], vec![]));

    let error = submission
        .prepare(&req, &context())
        .await
        .expect_err("encrypted contact policy must be honored");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
}

#[tokio::test]
async fn lookup_failure_never_means_external_cleartext() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    // Successful key lookup is not enough: the contact policy lookup failed,
    // so ordinary external classification is not permitted.
    state.fail_contacts = Some(503);
    state.recipient_keys.insert(
        "external@example.com".to_string(),
        external_keys("external@example.com", &[]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let req = request(message(vec!["external@example.com"], vec![], vec![]));

    let error = submission
        .prepare(&req, &context())
        .await
        .expect_err("a failed policy lookup must block submission");
    assert!(matches!(
        error.kind,
        ErrorKind::Transient | ErrorKind::Offline | ErrorKind::RateLimited
    ));
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
    assert_eq!(mock.snapshot(|state| state.send_count), 0);
    assert!(
        mock.snapshot(MockState::send_packages).is_empty(),
        "no package may ever be built after a failed policy lookup"
    );
}

#[tokio::test]
async fn ordinary_external_is_explicitly_classified() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    state.recipient_keys.insert(
        "ordinary@example.com".to_string(),
        external_keys("ordinary@example.com", &[]),
    );
    let mock = ProtonMock::start(state).await;
    let client = mock.client(keys.clone());
    let mut policy = ProtonRecipientPolicy::new(client);
    let deliveries = policy
        .resolve(
            &message(vec!["ordinary@example.com"], vec![], vec![]),
            &context(),
        )
        .await
        .expect("policy");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].security, DeliverySecurity::ExternalOrdinary);

    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let mut req = request(message(vec!["ordinary@example.com"], vec![], vec![]));
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));
    let outcome = submission.submit(&req, &context()).await.expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));
    let packages = mock.snapshot(MockState::send_packages);
    let ordinary = package_of_type(&packages, 4);
    assert!(ordinary["BodyKey"]["Key"].as_str().is_some());
}

#[tokio::test]
async fn mixed_to_cc_bcc_packages_do_not_leak_bcc() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let carol = generate_key("carol@example.com");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    state.recipient_keys.insert(
        "carol@example.com".to_string(),
        external_keys("carol@example.com", &[(&carol.public, 3)]),
    );
    state.recipient_keys.insert(
        "dave@example.com".to_string(),
        external_keys("dave@example.com", &[]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let mut req = request(message(
        vec!["bob@proton.example"],
        vec!["carol@example.com"],
        vec!["dave@example.com"],
    ));
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));
    let outcome = submission.submit(&req, &context()).await.expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));

    let drafts = mock.snapshot(MockState::draft_bodies);
    let draft = &drafts[0]["Message"];
    assert_eq!(draft["ToList"][0]["Address"], json!("bob@proton.example"));
    assert_eq!(draft["CCList"][0]["Address"], json!("carol@example.com"));
    assert_eq!(draft["BCCList"][0]["Address"], json!("dave@example.com"));
    assert!(
        !draft["ToList"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["Address"] == json!("dave@example.com"))
    );
    assert!(
        !draft["CCList"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["Address"] == json!("dave@example.com"))
    );

    let packages = mock.snapshot(MockState::send_packages);
    let internal = package_of_type(&packages, 1);
    assert!(internal["Addresses"].get("carol@example.com").is_none());
    assert!(internal["Addresses"].get("dave@example.com").is_none());
    let pgp = package_of_type(&packages, 8);
    assert!(pgp["Addresses"].get("bob@proton.example").is_none());
    assert!(pgp["Addresses"].get("dave@example.com").is_none());
    let ordinary = package_of_type(&packages, 4);
    assert_eq!(
        ordinary["Addresses"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["dave@example.com"]
    );
}

#[tokio::test]
async fn wrong_sender_alias_is_rejected() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys(
            "bob@proton.example",
            &[(&generate_key("bob@proton.example").public, 3)],
        ),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account())
        .with_sender_alias("not-mine@example.com");
    let req = request(message(vec!["bob@proton.example"], vec![], vec![]));

    let error = submission
        .prepare(&req, &context())
        .await
        .expect_err("an unknown sender alias must be rejected");
    assert_eq!(error.kind, ErrorKind::Forbidden);
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
}

#[tokio::test]
async fn attachment_parts_are_encrypted_and_recoverable() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);

    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let client = mock.client(keys.clone());
    let mut submission = ProtonSubmission::new(client.clone(), account());
    let payload = b"encrypted attachment payload";
    let mut req = request(message(vec!["bob@proton.example"], vec![], vec![]));
    req.attachments = vec![part("part-1", payload)];
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));
    let outcome = submission.submit(&req, &context()).await.expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));

    let packages = mock.snapshot(MockState::send_packages);
    let internal = package_of_type(&packages, 1);
    let entry = &internal["Addresses"]["bob@proton.example"];
    let attachment_keys = entry["AttachmentKeyPackets"]
        .as_object()
        .expect("recipient-wrapped attachment key");
    assert_eq!(attachment_keys.len(), 1);
    assert!(
        internal.get("AttachmentKeys").is_none(),
        "internal packages must never carry cleartext attachment keys"
    );

    let (attachment_id, stored, message_id) = mock.snapshot(|state| {
        let (message_id, message) = state.sent.iter().next().expect("sent draft message");
        let attachment = message["Attachments"][0].clone();
        let id = attachment["ID"].as_str().unwrap().to_string();
        (
            id.clone(),
            state.attachments.get(&id).cloned().unwrap_or_default(),
            message_id.clone(),
        )
    });
    assert_ne!(
        stored, payload,
        "the uploaded data packet must be encrypted"
    );

    let mut mailbox = ProtonMailbox::new(client);
    let recovered = mailbox
        .attachment(
            &RemoteMessageId::new(message_id),
            &RemoteAttachmentId::new(attachment_id),
            &context(),
        )
        .await
        .expect("recover attachment");
    assert_eq!(recovered, payload);
}

#[tokio::test]
async fn draft_created_before_timeout_is_reconciled() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let (dir, store, account) = open_store();
    let id = queue_submission(
        &store,
        message(vec!["bob@proton.example"], vec![], vec![]),
        Vec::new(),
    )
    .expect("queue");
    let entry = store.outbox_entry(&id).unwrap().unwrap();
    let operation = entry.operation_id.clone();
    assert!(
        store
            .claim_outbox_for_prepare(&id, &operation, &account.provider_id())
            .unwrap()
    );

    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys(
            "bob@proton.example",
            &[(&generate_key("bob@proton.example").public, 3)],
        ),
    );
    state.drafts.insert(
        "draft-existing".to_string(),
        json!({
            "ID": "draft-existing",
            "Subject": "Hello from P4",
            "ToList": [{"Name": "bob@proton.example", "Address": "bob@proton.example"}],
            "CCList": [],
            "BCCList": [],
            "Sender": {"Address": SENDER, "Name": ""},
            "Body": "interrupted",
            "MIMEType": "text/plain",
            "LabelIDs": ["8"],
            "Flags": 0,
            "ExternalID": stable_external_id(&operation),
        }),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account.clone());
    let report = flush_provider_outbox(&store, &account, &mut submission, &context())
        .await
        .expect("flush");

    assert_eq!(report.sent, 1);
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
    assert_eq!(mock.snapshot(|state| state.send_count), 1);
    assert_eq!(
        mock.snapshot(|state| state.send_paths.clone()),
        vec!["draft-existing".to_string()]
    );
    assert_eq!(
        store.outbox_entry(&id).unwrap().unwrap().state,
        SubmissionState::Complete
    );
    drop(dir);
}

#[tokio::test]
async fn send_timeout_is_uncertain_not_retry() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let (dir, store, account) = open_store();
    let id = queue_submission(
        &store,
        message(vec!["bob@proton.example"], vec![], vec![]),
        Vec::new(),
    )
    .expect("queue");

    let mut state = MockState::default();
    state.fail_send = Some(503);
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys(
            "bob@proton.example",
            &[(&generate_key("bob@proton.example").public, 3)],
        ),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account.clone());
    let report = flush_provider_outbox(&store, &account, &mut submission, &context())
        .await
        .expect("flush");
    assert_eq!(report.uncertain, 1);
    assert_eq!(mock.snapshot(|state| state.send_count), 1);
    assert_eq!(
        store.outbox_entry(&id).unwrap().unwrap().state,
        SubmissionState::Uncertain
    );

    // A second flush must not resend an uncertain row.
    let report = flush_provider_outbox(&store, &account, &mut submission, &context())
        .await
        .expect("second flush");
    assert_eq!(report.uncertain, 1);
    assert_eq!(mock.snapshot(|state| state.send_count), 1);
    drop(dir);
}

#[tokio::test]
async fn draft_reference_is_not_delivery_receipt() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let mut req = request(message(vec!["bob@proton.example"], vec![], vec![]));
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));

    let reconciled = submission
        .reconcile(&req, &context())
        .await
        .expect("reconcile");
    assert_eq!(
        reconciled,
        pige_core::providers::ReconcileOutcome::NotAccepted,
        "a draft that was never sent is not a delivery receipt"
    );
    assert_eq!(mock.snapshot(|state| state.send_count), 0);
}

#[tokio::test]
async fn sent_message_reconciles_as_accepted() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let mut req = request(message(vec!["bob@proton.example"], vec![], vec![]));
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));
    let receipt = match submission.submit(&req, &context()).await.expect("submit") {
        SubmissionOutcome::Accepted(receipt) => receipt,
        other => panic!("unexpected outcome: {other:?}"),
    };

    let reconciled = submission
        .reconcile(&req, &context())
        .await
        .expect("reconcile");
    match reconciled {
        pige_core::providers::ReconcileOutcome::Accepted(found) => {
            assert_eq!(found.operation, receipt.operation);
            assert_eq!(found.remote_message, receipt.remote_message);
        }
        other => panic!("a Sent message must reconcile as accepted: {other:?}"),
    }
}

#[tokio::test]
async fn sent_is_not_appended() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    assert_eq!(submission.sent_copy_policy(), SentCopyPolicy::ServerManaged);
    let mut req = request(message(vec!["bob@proton.example"], vec![], vec![]));
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));
    let receipt = match submission.submit(&req, &context()).await.expect("submit") {
        SubmissionOutcome::Accepted(receipt) => receipt,
        other => panic!("unexpected outcome: {other:?}"),
    };
    let before = mock.snapshot(|state| state.requests.len());
    submission
        .finalize_sent(&req, &receipt, &context())
        .await
        .expect("finalize");
    let after = mock.snapshot(|state| state.requests.len());
    assert_eq!(before, after, "the ServerManaged Sent copy never APPENDs");
    assert_eq!(mock.snapshot(|state| state.send_count), 1);
}

#[tokio::test]
async fn accepted_receipt_trains_only_once() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let (dir, store, account) = open_store();
    let id = queue_submission(
        &store,
        message(vec!["bob@proton.example"], vec![], vec![]),
        Vec::new(),
    )
    .expect("queue");

    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys(
            "bob@proton.example",
            &[(&generate_key("bob@proton.example").public, 3)],
        ),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account.clone());
    let mut observer = CountingObserver::default();
    flush_provider_outbox_with_observer(
        &store,
        &account,
        &mut submission,
        &context(),
        &mut observer,
    )
    .await
    .expect("first flush");
    assert_eq!(observer.accepted, 1);
    assert_eq!(
        store.outbox_entry(&id).unwrap().unwrap().state,
        SubmissionState::Complete
    );

    flush_provider_outbox_with_observer(
        &store,
        &account,
        &mut submission,
        &context(),
        &mut observer,
    )
    .await
    .expect("second flush");
    assert_eq!(
        observer.accepted, 1,
        "a completed acceptance must never train the same submission twice"
    );
    assert_eq!(mock.snapshot(|state| state.send_count), 1);
    drop(dir);
}

#[tokio::test]
async fn reply_reference_is_attached_to_draft() {
    let me = generate_key(SENDER);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let mut req = request(message(vec!["bob@proton.example"], vec![], vec![]));
    req.message.in_reply_to = Some("parent-remote-id".to_string());
    req.message.references = vec!["<first@example.com>".to_string()];
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));

    let drafts = mock.snapshot(MockState::draft_bodies);
    assert_eq!(drafts[0]["ParentID"], json!("parent-remote-id"));
    assert_eq!(drafts[0]["Action"], json!(0));
}

#[tokio::test]
async fn required_key_verification_is_distinctly_blocked() {
    let me = generate_key(SENDER);
    let external = generate_key("verify@example.com");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.contacts = vec![contact(
        "contact-verify",
        "verify@example.com",
        vec![clear_card(
            "BEGIN:VCARD\nVERSION:4.0\nEMAIL:verify@example.com\nX-PM-ENCRYPT:true\nX-PM-VERIFY:true\nEND:VCARD",
        )],
    )];
    // Only an unverified published key exists; it cannot satisfy an explicit
    // key-verification requirement.
    state.recipient_keys.insert(
        "verify@example.com".to_string(),
        external_keys("verify@example.com", &[(&external.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let req = request(message(vec!["verify@example.com"], vec![], vec![]));

    let error = submission
        .prepare(&req, &context())
        .await
        .expect_err("unverified key must not satisfy mandatory verification");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(error.safe_message.contains("verified"));
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
}

#[tokio::test]
async fn mandatory_verification_rejects_unsigned_card_key() {
    let me = generate_key(SENDER);
    let external = generate_key("verify@example.com");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.contacts = vec![contact(
        "contact-unsigned",
        "verify@example.com",
        vec![pinned_key_policy_card(&external.public, None)],
    )];
    state.recipient_keys.insert(
        "verify@example.com".to_string(),
        external_keys("verify@example.com", &[]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let req = request(message(vec!["verify@example.com"], vec![], vec![]));

    let error = submission
        .prepare(&req, &context())
        .await
        .expect_err("an unsigned card must not satisfy mandatory verification");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(
        error.safe_message.contains("authenticated"),
        "the safe error should name authenticated provenance: {error:?}"
    );
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
    assert_eq!(mock.snapshot(|state| state.send_count), 0);
}

#[tokio::test]
async fn mandatory_verification_accepts_authenticated_card_provenance() {
    let me = generate_key(SENDER);
    let external = generate_key("verify@example.com");
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    state.contacts = vec![contact(
        "contact-signed",
        "verify@example.com",
        vec![pinned_key_policy_card(&external.public, Some(&me))],
    )];
    state.recipient_keys.insert(
        "verify@example.com".to_string(),
        external_keys("verify@example.com", &[]),
    );
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let mut req = request(message(vec!["verify@example.com"], vec![], vec![]));
    req.prepared = Some(submission.prepare(&req, &context()).await.expect("prepare"));
    let outcome = submission.submit(&req, &context()).await.expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));

    let packages = mock.snapshot(MockState::send_packages);
    let pgp = package_of_type(&packages, 8);
    assert!(pgp["Addresses"].get("verify@example.com").is_some());
}

#[tokio::test]
async fn recipient_key_lookup_failure_blocks_submission() {
    let me = generate_key(SENDER);
    let keys = key_store(vec![address_keys("address-1", SENDER, &[&me])]);
    let mut state = MockState::default();
    // A successful contact lookup with no policy is not enough if the
    // recipient-key lookup fails: that must never degrade to cleartext.
    state.fail_keys = Some(503);
    let mock = ProtonMock::start(state).await;
    let mut submission = ProtonSubmission::new(mock.client(keys), account());
    let req = request(message(vec!["external@example.com"], vec![], vec![]));

    let error = submission
        .prepare(&req, &context())
        .await
        .expect_err("a failed recipient-key lookup must block submission");
    assert!(matches!(
        error.kind,
        ErrorKind::Transient | ErrorKind::Offline | ErrorKind::RateLimited
    ));
    assert_eq!(mock.snapshot(|state| state.draft_create_count), 0);
    assert!(
        mock.snapshot(MockState::send_packages).is_empty(),
        "no package may be built after a failed key lookup"
    );
}

#[tokio::test]
async fn prepared_alias_survives_restart() {
    let me = generate_key(SENDER);
    let alias = "alias@example.com";
    let alias_key = generate_key(alias);
    let bob = generate_key("bob@proton.example");
    let keys = key_store(vec![
        address_keys("address-1", SENDER, &[&me]),
        address_keys("address-2", alias, &[&alias_key]),
    ]);
    let mut state = MockState::default();
    state.recipient_keys.insert(
        "bob@proton.example".to_string(),
        internal_keys("bob@proton.example", &[(&bob.public, 3)]),
    );
    let mock = ProtonMock::start(state).await;
    let mut req = request(message(vec!["bob@proton.example"], vec![], vec![]));
    let mut first =
        ProtonSubmission::new(mock.client(keys.clone()), account()).with_sender_alias(alias);
    req.prepared = Some(first.prepare(&req, &context()).await.expect("prepare"));

    // A restarted service has no `with_sender_alias`; the prepared evidence is
    // the durable record of the chosen alias and must still submit.
    let mut restarted = ProtonSubmission::new(mock.client(keys), account());
    let outcome = restarted.submit(&req, &context()).await.expect("submit");
    assert!(matches!(outcome, SubmissionOutcome::Accepted(_)));
    let drafts = mock.snapshot(MockState::draft_bodies);
    assert_eq!(drafts[0]["Message"]["Sender"]["Address"], json!(alias));
    assert_eq!(mock.snapshot(|state| state.send_count), 1);
}

#[derive(Default)]
struct CountingObserver {
    accepted: usize,
}

impl SubmissionObserver for CountingObserver {
    fn accepted(&mut self, _entry: &pige_core::send::OutboxEntry, _receipt: &SubmissionReceipt) {
        self.accepted += 1;
    }
}
