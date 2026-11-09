//! Task P3: normalized decrypted reading, attachment packets, signature
//! verdicts and contact metadata for the native Proton adapter.
//!
//! Every test runs against a local synthetic Proton API with runtime-generated
//! OpenPGP keys. No fixture contains real mail, keys, addresses or tokens; all
//! addresses are `example.com`. The named cases are the P3 RED/GREEN contract:
//! internal and external armored bodies, unsigned/unverified/invalid/
//! unsupported verdict separation, multiple-key and alias-address decryption,
//! attachment packet integrity, bounded plaintext, HTML sanitization, and
//! contact snapshot sync that never erases learned mail evidence.

#![cfg(feature = "provider-proton")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use pige_core::Store;
use pige_core::account::{Account, AccountId, MailBackend};
use pige_core::auth::TokenSource;
use pige_core::contacts::normalize_address;
use pige_core::contacts::sync::sync_slice;
use pige_core::mail::{Address, AttachmentDisposition, Flags, MessageHeader};
use pige_core::providers::proton::{
    ProtonAddressBook, ProtonClient, ProtonEndpoints, ProtonMailbox, UnlockedKeyStore, proton_core,
    proton_crypto,
};
use pige_core::providers::{
    AddressBookService, ContactSourceId, ErrorKind, HistoryCoverage, HistoryPage, MailboxService,
    MessageRecord, OperationContext, ProviderCursor, ProviderError, RemoteAttachmentId,
    RemoteMessageId, SignatureStatus,
};
use pige_core::sync::provider::WorkBudget;
use proton_core::crypto::keys::{AddressKeys, KeyStore, StoredKey};
use proton_core::model::enums::key_flag;
use proton_crypto::crypto::{
    DataEncoding, Encryptor, EncryptorSync, KeyGenerator, KeyGeneratorAlgorithm, KeyGeneratorSync,
    PGPMessage, PGPProviderSync,
};
use tempfile::TempDir;

const ACCESS: &str = "synthetic-access";
const BODY_CAP: usize = 10 * 1024 * 1024;
const SOURCE: &str = "proton.contacts";

// ---------------------------------------------------------------------------
// Synthetic Proton API
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct MockState {
    messages: HashMap<String, Value>,
    attachments: HashMap<String, Vec<u8>>,
    /// Armored public keys keyed by sender address; the body is the API
    /// `/core/v4/keys/all` response object.
    sender_keys: HashMap<String, Value>,
    contacts: Vec<Value>,
    /// Override the reported `Total`, to exercise an empty page that still
    /// claims more contacts remain.
    contact_total: Option<usize>,
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

    fn update(&self, change: impl FnOnce(&mut MockState)) {
        change(&mut self.state.lock().expect("mock state"));
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
    let authorized = request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {ACCESS}"));
    if !authorized {
        return ResponseTemplate::new(401).set_body_json(json!({"Code": 401}));
    }
    let path = request.url.path();
    let query = |name: &str| {
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };

    if let Some(id) = path.strip_prefix("/mail/v4/messages/") {
        return match state.messages.get(id) {
            Some(message) => ResponseTemplate::new(200).set_body_json(json!({
                "Code": 1000,
                "Message": message,
            })),
            None => ResponseTemplate::new(404).set_body_json(json!({"Code": 2501})),
        };
    }
    if let Some(id) = path.strip_prefix("/mail/v4/attachments/") {
        return match state.attachments.get(id) {
            Some(bytes) => ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(bytes.clone()),
            None => ResponseTemplate::new(404).set_body_json(json!({"Code": 2501})),
        };
    }
    if path == "/core/v4/keys/all" {
        let email = query("Email").unwrap_or_default();
        return ResponseTemplate::new(200).set_body_json(
            state
                .sender_keys
                .get(&email)
                .cloned()
                .unwrap_or_else(|| external_keys("", &[])),
        );
    }
    if path == "/contacts/v4/contacts" {
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
            "Total": state.contact_total.unwrap_or(state.contacts.len()),
            "Contacts": contacts,
        }));
    }
    ResponseTemplate::new(404).set_body_json(json!({"Code": 2501}))
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

// ---------------------------------------------------------------------------
// Runtime OpenPGP fixtures
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

/// Encrypt `text` to an armored public key. `signer` adds an inline signature;
/// `future` signs at a timestamp the verifier rejects as a bad signature.
fn encrypt_to(
    recipient_public: &str,
    signer: Option<&GeneratedKey>,
    text: &str,
    future: bool,
) -> String {
    let provider = provider();
    let public = provider
        .public_key_import(recipient_public.as_bytes(), DataEncoding::Armor)
        .expect("import recipient key");
    let signing_key = signer.map(|key| {
        provider
            .private_key_import(
                key.stored.armored.as_bytes(),
                "synthetic-passphrase".as_bytes(),
                DataEncoding::Armor,
            )
            .expect("import signing key")
    });

    let mut encryptor = provider
        .new_encryptor()
        .with_encryption_key(&public)
        .with_utf8();
    if let Some(key) = signing_key.as_ref() {
        encryptor = encryptor.with_signing_key(key);
    }
    if future {
        encryptor = encryptor.at_signing_time(proton_crypto::crypto::UnixTimestamp(4_000_000_000));
    }
    let message = encryptor.encrypt(text.as_bytes()).expect("encrypt body");
    String::from_utf8(message.armor().expect("armor body")).expect("armor utf8")
}

/// Encrypt `text` to an armored public key using compressed plaintext. This is
/// the shape of a compression bomb: a tiny ciphertext that expands past the
/// body cap only after decryption.
fn encrypt_to_compressed(recipient_public: &str, text: &str) -> String {
    let provider = provider();
    let public = provider
        .public_key_import(recipient_public.as_bytes(), DataEncoding::Armor)
        .expect("import recipient key");
    let message = provider
        .new_encryptor()
        .with_encryption_key(&public)
        .with_utf8()
        .with_compression()
        .encrypt(text.as_bytes())
        .expect("encrypt compressed body");
    String::from_utf8(message.armor().expect("armor body")).expect("armor utf8")
}

struct RawPacketStream(Vec<u8>);

impl proton_rpgp::pgp::ser::Serialize for RawPacketStream {
    fn to_writer<W: std::io::Write>(
        &self,
        writer: &mut W,
    ) -> proton_rpgp::pgp::errors::Result<()> {
        writer.write_all(&self.0)?;
        Ok(())
    }

    fn write_len(&self) -> usize {
        self.0.len()
    }
}

/// Build an encrypted message whose inline signature was produced over
/// `signed_plaintext` but whose encrypted literal contains `plaintext`. The
/// packet stream stays integrity-protected (valid SEIPD MDC), so decryption
/// succeeds and the failure is specifically a bad signature over the data.
fn encrypt_with_mismatched_signature(
    recipient_public: &str,
    signer: &GeneratedKey,
    plaintext: &str,
    signed_plaintext: &str,
) -> String {
    use proton_rpgp::pgp::{
        armor,
        bytes::Bytes,
        composed::{Deserializable, SignedPublicKey, SignedSecretKey},
        crypto::{hash::HashAlgorithm, sym::SymmetricKeyAlgorithm},
        packet::{
            LiteralData, OnePassSignature, PublicKeyEncryptedSessionKey, SignatureConfig,
            SignatureType, SymEncryptedProtectedData,
        },
        types::{KeyDetails, Password},
    };

    let (recipient, _) = SignedPublicKey::from_string(recipient_public).expect("recipient key");
    let (signer_key, _) =
        SignedSecretKey::from_string(&signer.stored.armored).expect("signer key");
    let profile = proton_rpgp::Profile::default();
    let session_key = SymmetricKeyAlgorithm::AES256.new_session_key(profile.rng());
    let signature = SignatureConfig::v4(
        SignatureType::Binary,
        signer_key.primary_key.algorithm(),
        HashAlgorithm::Sha256,
    )
    .sign(
        &signer_key.primary_key,
        &Password::from("synthetic-passphrase"),
        signed_plaintext.as_bytes(),
    )
    .expect("sign different data");
    let one_pass = OnePassSignature::v3(
        SignatureType::Binary,
        HashAlgorithm::Sha256,
        signer_key.primary_key.algorithm(),
        signer_key.primary_key.legacy_key_id(),
    );
    let literal = LiteralData::from_bytes("", Bytes::copy_from_slice(plaintext.as_bytes()))
        .expect("literal packet");
    use proton_rpgp::pgp::packet::PacketTrait;

    let mut inner = Vec::new();
    one_pass
        .to_writer_with_header(&mut inner)
        .expect("one-pass packet");
    literal
        .to_writer_with_header(&mut inner)
        .expect("literal packet");
    signature
        .to_writer_with_header(&mut inner)
        .expect("signature packet");
    proton_rpgp::pgp::composed::Message::from_bytes(&inner[..])
        .expect("inner signed packet stream parses");
    let data_packet = SymEncryptedProtectedData::encrypt_seipdv1(
        profile.rng(),
        SymmetricKeyAlgorithm::AES256,
        session_key.as_ref(),
        &inner,
    )
    .expect("encrypt packet stream");
    // The generated ECC identity signs with an EdDSA primary key and encrypts
    // with its bound subkey, so wrap the session key to the encryption subkey.
    let encryption_key = &recipient
        .public_subkeys
        .first()
        .expect("recipient encryption subkey")
        .key;
    let key_packet = PublicKeyEncryptedSessionKey::from_session_key_v3(
        profile.rng(),
        &session_key,
        SymmetricKeyAlgorithm::AES256,
        encryption_key,
    )
    .expect("wrap session key");
    let mut packets = Vec::new();
    key_packet
        .to_writer_with_header(&mut packets)
        .expect("key packet");
    data_packet
        .to_writer_with_header(&mut packets)
        .expect("data packet");
    let mut armored = Vec::new();
    armor::write(
        &RawPacketStream(packets),
        armor::BlockType::Message,
        &mut armored,
        None,
        false,
    )
    .expect("armor message");
    String::from_utf8(armored).expect("armored utf8")
}

fn message(
    id: &str,
    body: &str,
    mime_type: &str,
    address_id: &str,
    sender: &str,
    flags: u64,
    attachments: Vec<Value>,
) -> Value {
    json!({
        "ID": id,
        "ConversationID": format!("conv-{id}"),
        "Subject": "Synthetic",
        "Unread": 1,
        "Sender": {"Name": "Sender", "Address": sender},
        "ToList": [{"Name": "Me", "Address": "me@example.com"}],
        "Time": 1_700_000_000,
        "Size": body.len(),
        "AddressID": address_id,
        "LabelIDs": ["0"],
        "ExternalID": format!("<{id}@example.com>"),
        "NumAttachments": attachments.len(),
        "Flags": flags,
        "MIMEType": mime_type,
        "Body": body,
        "Attachments": attachments,
    })
}

fn attachment_value(
    id: &str,
    name: &str,
    mime_type: &str,
    content_id: Option<&str>,
    disposition: &str,
    key_packets: &str,
    size: u64,
) -> Value {
    json!({
        "ID": id,
        "Name": name,
        "Size": size,
        "KeyPackets": key_packets,
        "MIMEType": mime_type,
        "ContentID": content_id,
        "Disposition": disposition,
    })
}

fn context() -> OperationContext {
    OperationContext::with_timeout(AccountId::new(), 1, Duration::from_secs(60))
}

async fn body(mock: &ProtonMock, keys: &KeyStore, id: &str) -> Result<pige_core::providers::BodyContent, ProviderError> {
    let mut mailbox = ProtonMailbox::new(mock.client(keys.clone()));
    mailbox.body(&RemoteMessageId::new(id), &context()).await
}

// ---------------------------------------------------------------------------
// Named P3 tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn decrypt_internal_and_external_supported_formats() {
    let me = generate_key("me@example.com");
    let external = generate_key("external@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);

    let mut state = MockState::default();
    state.sender_keys.insert(
        "me@example.com".to_string(),
        internal_keys("me@example.com", &[(&me.public, 3)]),
    );
    state.sender_keys.insert(
        "external@example.com".to_string(),
        external_keys("external@example.com", &[(&external.public, 3)]),
    );
    state.messages.insert(
        "internal".to_string(),
        message(
            "internal",
            &encrypt_to(&me.public, Some(&me), "internal body", false),
            "text/plain",
            "address-1",
            "me@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    state.messages.insert(
        "external".to_string(),
        message(
            "external",
            &encrypt_to(&me.public, Some(&external), "external body", false),
            "text/plain",
            "address-1",
            "external@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let internal = body(&mock, &keys, "internal").await.expect("internal body");
    assert_eq!(internal.plain_text.as_deref(), Some("internal body"));
    assert_eq!(internal.signature, SignatureStatus::Verified);
    assert!(internal.raw.is_none(), "Proton exposes no original RFC822");

    let external = body(&mock, &keys, "external").await.expect("external body");
    assert_eq!(external.plain_text.as_deref(), Some("external body"));
    assert_eq!(
        external.signature,
        SignatureStatus::Unverified,
        "external/WKD keys are not verified Proton keys"
    );
    assert_ne!(external.signature, SignatureStatus::Verified);
}

#[tokio::test]
async fn unsigned_is_not_verified() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let mut state = MockState::default();
    state.sender_keys.insert(
        "me@example.com".to_string(),
        internal_keys("me@example.com", &[(&me.public, 3)]),
    );
    state.messages.insert(
        "unsigned".to_string(),
        message(
            "unsigned",
            &encrypt_to(&me.public, None, "unsigned body", false),
            "text/plain",
            "address-1",
            "me@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let content = body(&mock, &keys, "unsigned").await.expect("unsigned body");
    assert_eq!(content.plain_text.as_deref(), Some("unsigned body"));
    assert_eq!(content.signature, SignatureStatus::Unsigned);
    assert_ne!(content.signature, SignatureStatus::Verified);
}

#[tokio::test]
async fn flagged_sender_key_is_not_verified() {
    let me = generate_key("me@example.com");
    let compromised = generate_key("compromised@example.com");
    let no_sign = generate_key("no-sign@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);

    let mut state = MockState::default();
    // Proton marks one key as compromised (missing NOT_COMPROMISED) and the
    // other as no-sign. Neither may be used as a verifier.
    state.sender_keys.insert(
        "compromised@example.com".to_string(),
        internal_keys(
            "compromised@example.com",
            &[(&compromised.public, key_flag::NOT_OBSOLETE)],
        ),
    );
    state.sender_keys.insert(
        "no-sign@example.com".to_string(),
        internal_keys(
            "no-sign@example.com",
            &[(
                &no_sign.public,
                key_flag::NOT_COMPROMISED | key_flag::EMAIL_NO_SIGN,
            )],
        ),
    );
    for (id, sender, signer) in [
        ("compromised-key", "compromised@example.com", &compromised),
        ("no-sign-key", "no-sign@example.com", &no_sign),
    ] {
        state.messages.insert(
            id.to_string(),
            message(
                id,
                &encrypt_to(&me.public, Some(signer), "flagged signature body", false),
                "text/plain",
                "address-1",
                sender,
                1 << 3,
                Vec::new(),
            ),
        );
    }
    let mock = ProtonMock::start(state).await;

    for id in ["compromised-key", "no-sign-key"] {
        let content = body(&mock, &keys, id).await.expect("flagged body");
        assert_eq!(content.plain_text.as_deref(), Some("flagged signature body"));
        assert_ne!(
            content.signature,
            SignatureStatus::Verified,
            "{id} signed by an untrusted key must never verify"
        );
        assert_eq!(content.signature, SignatureStatus::Unverified);
    }
}

#[tokio::test]
async fn bad_signature_is_invalid() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let mut state = MockState::default();
    state.sender_keys.insert(
        "me@example.com".to_string(),
        internal_keys("me@example.com", &[(&me.public, 3)]),
    );
    state.messages.insert(
        "invalid".to_string(),
        message(
            "invalid",
            // The packet integrity is valid, but the signature covers different
            // data, so this is a real tampered-data signature failure.
            &encrypt_with_mismatched_signature(
                &me.public,
                &me,
                "tampered body",
                "original body",
            ),
            "text/plain",
            "address-1",
            "me@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let content = body(&mock, &keys, "invalid").await.expect("invalid body");
    assert_eq!(content.plain_text.as_deref(), Some("tampered body"));
    assert_eq!(content.signature, SignatureStatus::Invalid);
    assert_ne!(content.signature, SignatureStatus::Verified);
}

#[tokio::test]
async fn unknown_format_is_unsupported() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let mut state = MockState::default();
    state.messages.insert(
        "unknown".to_string(),
        message(
            "unknown",
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nnot supported\n-----BEGIN PGP SIGNATURE-----\n\nAAAA\n-----END PGP SIGNATURE-----",
            "multipart/encrypted",
            "address-1",
            "external@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let error = body(&mock, &keys, "unknown")
        .await
        .expect_err("unsupported encryption formats must not render as empty text");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_ne!(error.kind, ErrorKind::Transient);
}

#[tokio::test]
async fn multiple_keys_can_decrypt_old_mail() {
    let old = generate_key("me@example.com");
    let new = generate_key("me@example.com");
    let alias = generate_key("alias@example.com");
    let keys = key_store(vec![
        address_keys("address-primary", "me@example.com", &[&new, &old]),
        address_keys("address-alias", "alias@example.com", &[&alias]),
    ]);

    let mut state = MockState::default();
    // Old mail encrypted to a retired key in the same address ring.
    state.messages.insert(
        "old-key".to_string(),
        message(
            "old-key",
            &encrypt_to(&old.public, None, "old key body", false),
            "text/plain",
            "address-primary",
            "me@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    // Old mail belonging to a secondary address must use that address ring,
    // not silently fall back to the primary address key.
    state.messages.insert(
        "old-alias".to_string(),
        message(
            "old-alias",
            &encrypt_to(&alias.public, None, "alias body", false),
            "text/plain",
            "address-alias",
            "alias@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let old_key = body(&mock, &keys, "old-key").await.expect("old key body");
    assert_eq!(old_key.plain_text.as_deref(), Some("old key body"));
    let alias = body(&mock, &keys, "old-alias").await.expect("alias body");
    assert_eq!(alias.plain_text.as_deref(), Some("alias body"));
}

#[tokio::test]
async fn attachment_packet_integrity_failure_returns_error() {
    let me = generate_key("me@example.com");
    let address = address_keys("address-1", "me@example.com", &[&me]);
    let keys = key_store(vec![address.clone()]);

    let provider = provider();
    let uploaded = proton_core::crypto::encrypt_attachment(
        &provider,
        &address,
        b"attachment plaintext",
    )
    .expect("encrypt attachment");
    let key_packets = STANDARD.encode(&uploaded.key_packet);
    let mut corrupted = uploaded.data_packet.clone();
    let last = corrupted.last_mut().expect("data packet");
    *last ^= 0xff;

    let mut state = MockState::default();
    state.messages.insert(
        "with-attachment".to_string(),
        message(
            "with-attachment",
            &encrypt_to(&me.public, None, "body", false),
            "text/plain",
            "address-1",
            "me@example.com",
            1 << 3,
            vec![attachment_value(
                "att-1",
                "report.txt",
                "text/plain",
                Some("inline-report"),
                "inline",
                &key_packets,
                uploaded.data_packet.len() as u64,
            )],
        ),
    );
    state
        .attachments
        .insert("att-1".to_string(), corrupted);
    let mock = ProtonMock::start(state).await;

    let content = body(&mock, &keys, "with-attachment")
        .await
        .expect("body with attachment descriptor");
    assert_eq!(content.attachments.len(), 1);
    assert_eq!(content.attachments[0].content_id.as_deref(), Some("inline-report"));
    assert_eq!(
        content.attachments[0].disposition,
        AttachmentDisposition::Inline
    );

    let mut mailbox = ProtonMailbox::new(mock.client(keys));
    let error = mailbox
        .attachment(
            &RemoteMessageId::new("with-attachment"),
            &RemoteAttachmentId::new("att-1"),
            &context(),
        )
        .await
        .expect_err("a corrupted attachment packet must not return bytes");
    assert_eq!(
        error.kind,
        ErrorKind::Transient,
        "packet integrity failure must not be reported as an unsupported format"
    );
    assert_ne!(error.kind, ErrorKind::Unsupported);
}

#[tokio::test]
async fn oversize_plaintext_is_rejected_before_cache_growth() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let oversized = "x".repeat(BODY_CAP + 1);
    let mut state = MockState::default();
    state.messages.insert(
        "oversize".to_string(),
        message(
            "oversize",
            &encrypt_to(&me.public, None, &oversized, false),
            "text/plain",
            "address-1",
            "me@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let error = body(&mock, &keys, "oversize")
        .await
        .expect_err("plaintext above the body cap must be rejected");
    assert_eq!(error.kind, ErrorKind::Unsupported);
}

#[tokio::test]
async fn compressed_oversize_is_rejected_before_materialization() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    // Highly compressible data far above the cap: the armored ciphertext stays
    // small (well under the encrypted-input bound), so only a bounded
    // decrypt/decompress step can reject it without allocating the full text.
    let oversized = "A".repeat(BODY_CAP + 4 * 1024 * 1024);
    let compressed = encrypt_to_compressed(&me.public, &oversized);
    assert!(
        compressed.len() < BODY_CAP,
        "compressed ciphertext must be smaller than its expansion"
    );
    let mut state = MockState::default();
    state.messages.insert(
        "compressed-oversize".to_string(),
        message(
            "compressed-oversize",
            &compressed,
            "text/plain",
            "address-1",
            "me@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let error = body(&mock, &keys, "compressed-oversize")
        .await
        .expect_err("a decompression bomb must be rejected at the output cap");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(
        error.safe_message.contains("larger"),
        "the reader should see a size-limit failure: {}",
        error.safe_message
    );
}

#[tokio::test]
async fn html_still_passes_sanitizer() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let html = "<p>Hello</p><script>alert(1)</script><img src=\"https://tracker.example/pixel.png\">";
    let mut state = MockState::default();
    state.messages.insert(
        "html".to_string(),
        message(
            "html",
            &encrypt_to(&me.public, None, html, false),
            "text/html",
            "address-1",
            "me@example.com",
            1 << 3,
            Vec::new(),
        ),
    );
    let mock = ProtonMock::start(state).await;

    let content = body(&mock, &keys, "html").await.expect("html body");
    let html = content.html.expect("adapter returns normalized HTML");
    let document = pige_core::html::parse_html(&html, false);
    assert!(document.has_remote_images(), "the remote image must be retained as blocked content");
    assert!(document.blocks.iter().any(|block| matches!(
        block,
        pige_core::html::Block::Image {
            src: pige_core::html::ImageSource::Remote(url),
            ..
        } if url.contains("tracker.example")
    )));
    let rendered = format!("{document:?}");
    assert!(rendered.contains("Hello"));
    assert!(!rendered.contains("script"));
    assert!(!rendered.contains("alert"));
}

#[tokio::test]
async fn contact_names_and_emails_sync_offline() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let state = MockState {
        contacts: vec![
            json!({
                "ID": "contact-1",
                "Name": "Alice Example",
                "UID": "uid-1",
                "ContactEmails": [
                    {"ID": "email-1", "Email": "alice@example.com", "Name": "Alice", "ContactID": "contact-1"},
                    {"ID": "email-2", "Email": "a.alias@example.com", "Name": "", "ContactID": "contact-1"}
                ],
                "Cards": [
                    {"Type": "CERTIFICATE", "Data": "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nsynthetic\n-----END PGP PUBLIC KEY BLOCK-----", "Signature": "synthetic"}
                ]
            }),
            json!({
                "ID": "contact-2",
                "Name": "Bob Example",
                "ContactEmails": [
                    {"ID": "email-3", "Email": "bob@example.com", "Name": "", "ContactID": "contact-2"}
                ]
            }),
        ],
        ..MockState::default()
    };
    let mock = ProtonMock::start(state).await;
    let (dir, store, account) = open_store();
    let context = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(10));
    let source = ContactSourceId::new(SOURCE);
    store
        .upsert_contact_source(&account.id, &source, Some("Proton contacts"))
        .unwrap();

    let mut address_book = ProtonAddressBook::new(mock.client(keys));
    let progress = sync_slice(
        &store,
        &mut address_book,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(10)),
    )
    .await
    .expect("contact sync");
    assert!(progress.complete);

    let contacts = store.contact_emails(&account.id).unwrap();
    assert!(contacts.iter().any(|contact| {
        contact.address.email == "alice@example.com"
            && contact.name.as_deref() == Some("Alice")
    }));
    assert!(contacts.iter().any(|contact| {
        contact.address.email == "a.alias@example.com"
            && contact.name.as_deref() == Some("Alice Example")
    }));
    assert!(contacts.iter().any(|contact| {
        contact.address.email == "bob@example.com"
            && contact.name.as_deref() == Some("Bob Example")
    }));
    assert!(
        contacts.iter().all(|contact| contact.source.as_str() == SOURCE),
        "only the proton.contacts source is populated"
    );
    drop(dir);
}

#[tokio::test]
async fn empty_contact_page_before_total_is_transient() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let state = MockState {
        contacts: Vec::new(),
        // The server still claims 1000 contacts, but page zero is empty: this
        // is an offset shift, not a complete snapshot. Reporting completion
        // would let R6 sweep the whole address book.
        contact_total: Some(1000),
        ..MockState::default()
    };
    let mock = ProtonMock::start(state).await;
    let mut address_book = ProtonAddressBook::new(mock.client(keys));
    let error = address_book
        .next_page(
            &ContactSourceId::new(SOURCE),
            None,
            &context(),
        )
        .await
        .expect_err("an empty page before the reported total must not complete");
    assert_eq!(error.kind, ErrorKind::Transient);
}

#[tokio::test]
async fn contact_delete_does_not_erase_learned_address() {
    let me = generate_key("me@example.com");
    let keys = key_store(vec![address_keys("address-1", "me@example.com", &[&me])]);
    let state = MockState {
        contacts: vec![json!({
            "ID": "contact-1",
            "Name": "Shared Person",
            "ContactEmails": [
                {"ID": "email-1", "Email": "shared@example.com", "Name": "Shared Person", "ContactID": "contact-1"}
            ]
        })],
        ..MockState::default()
    };
    let mock = ProtonMock::start(state.clone()).await;
    let (dir, store, account) = open_store();
    let context = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(10));
    let source = ContactSourceId::new(SOURCE);
    store
        .upsert_contact_source(&account.id, &source, Some("Proton contacts"))
        .unwrap();

    // The address is independently known from mail history.
    let key = normalize_address(&Address::new(None, "shared@example.com")).unwrap();
    store
        .apply_history_page(&context, None, &history_page("shared@example.com", "h1"))
        .expect("learn history address");
    assert!(store.learned_address(&account.id, &key).unwrap().is_some());

    let mut first = ProtonAddressBook::new(mock.client(keys.clone()));
    sync_slice(
        &store,
        &mut first,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(10)),
    )
    .await
    .expect("first contact snapshot");
    assert!(
        store
            .contact_emails(&account.id)
            .unwrap()
            .iter()
            .any(|contact| contact.address.email == "shared@example.com")
    );

    // The provider now reports a full snapshot without the contact. The
    // completed snapshot sweeps the contact row, but never the learned mail
    // evidence for the same address.
    mock.update(|state| state.contacts.clear());
    let mut second = ProtonAddressBook::new(mock.client(keys));
    sync_slice(
        &store,
        &mut second,
        &source,
        &context,
        WorkBudget::with_timeout(100, 5, Duration::from_secs(10)),
    )
    .await
    .expect("second contact snapshot");

    assert!(
        !store
            .contact_emails(&account.id)
            .unwrap()
            .iter()
            .any(|contact| contact.address.email == "shared@example.com"),
        "the completed snapshot retires the deleted contact"
    );
    assert!(
        store.learned_address(&account.id, &key).unwrap().is_some(),
        "a contact delete must not erase learned mail evidence"
    );
    assert!(
        store
            .recipient_candidates(&account.id, "shared", &["shared@example.com".to_string()], 64)
            .unwrap()
            .iter()
            .any(|candidate| candidate.key == key),
        "the address stays suggestible from learned evidence"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// Test store / history helpers
// ---------------------------------------------------------------------------

fn open_store() -> (TempDir, Store, Account) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    let account = Account {
        id: AccountId("proton-p3".to_string()),
        label: "Proton".to_string(),
        email: "me@example.com".to_string(),
        display_name: "Me".to_string(),
        backend: MailBackend::proton(None),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    };
    store.upsert_account(&account).unwrap();
    (dir, store, account)
}

fn history_page(recipient: &str, checkpoint: &str) -> HistoryPage {
    let header = MessageHeader {
        message_id: Some("<proton-p3-history@example.com>".to_string()),
        in_reply_to: None,
        references: Vec::new(),
        subject: "History".to_string(),
        from: vec![Address::new(None, "sender@example.com")],
        to: vec![Address::new(None, recipient)],
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: 1_700_000_000,
        size: 64,
    };
    HistoryPage {
        messages: vec![MessageRecord {
            remote_id: RemoteMessageId::new("history-1"),
            remote_thread: None,
            header,
            flags: Flags::default(),
            collections: Vec::new(),
            snippet: String::new(),
            has_attachments: false,
        }],
        checkpoint: ProviderCursor::new(1, checkpoint),
        has_more: false,
        coverage: HistoryCoverage::default(),
    }
}
