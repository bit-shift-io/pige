//! Native Proton authentication contract.
//!
//! These are synthetic HTTP fixtures: a stateful mock Proton API performs a real
//! SRP-6a exchange (signed test modulus, real `proton-srp` client/server proofs)
//! and serves synthetic key rings. No live account, device, or send is used.
//!
//! The named cases are the P1 RED/GREEN contract: server proof/modulus
//! verification, attempt-bound TOTP and human verification, two-password key
//! unlock, secondary address keys, FIDO refusal, refresh rotation across a
//! restart, logout locking/cancellation, a late refresh after removal, and
//! secret redaction from Debug and the SQLite cache.

#![cfg(feature = "provider-proton")]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use pige_core::account::{Account, AccountId, MailBackend};
use pige_core::auth::{
    AccountSessionManager, AuthAnswer, AuthChallenge, AuthProgress, AuthRequest,
    AuthenticationService, CredentialSecretRepository, SecretEnvelope, SecretPersistence,
    SecretRepository, StagedSecretRef, StoredCredentials, TokenRefresher,
};
use pige_core::platform::{CredentialStore, fake::FakeCredentials};
use pige_core::providers::proton::proton_core;
use pige_core::providers::proton::proton_crypto;
use pige_core::providers::proton::proton_srp::{
    RPGPVerifier, SRPAuth, SRPProofB64, SRPVerifierB64, ServerClientProof, ServerClientVerifier,
    ServerInteraction,
};
use pige_core::providers::proton::{
    ProtonAuthentication, ProtonClient, ProtonEndpoints, ProtonKeyStores, ProtonProviderFactory,
};
use pige_core::providers::{
    AuthAttemptId, ErrorKind, OperationContext, PROTON_PROVIDER, ProviderId,
};
use pige_core::store::Store;
use tempfile::TempDir;

/// The MIT Proton test modulus shipped with `proton-srp`'s own example/test
/// suite. It is the real server-signed SRP group used to exercise modulus
/// verification; it is public test data, not an account credential.
const TEST_MODULUS: &str = "-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA256

W2z5HBi8RvsfYzZTS7qBaUxxPhsfHJFZpu3Kd6s1JafNrCCH9rfvPLrfuqocxWPgWDH2R8neK7PkNvjxto9TStuY5z7jAzWRvFWN9cQhAKkdWgy0JY6ywVn22+HFpF4cYesHrqFIKUPDMSSIlWjBVmEJZ/MusD44ZT29xcPrOqeZvwtCffKtGAIjLYPZIEbZKnDM1Dm3q2K/xS5h+xdhjnndhsrkwm9U9oyA2wxzSXFL+pdfj2fOdRwuR5nW0J2NFrq3kJjkRmpO/Genq1UW+TEknIWAb6VzJJJA244K/H8cnSx2+nSNZO3bbo6Ys228ruV9A8m6DhxmS+bihN3ttQ==
-----BEGIN PGP SIGNATURE-----
Version: ProtonMail
Comment: https://protonmail.com

wl4EARYIABAFAlwB1j0JEDUFhcTpUY8mAAD8CgEAnsFnF4cF0uSHKkXa1GIa
GO86yMV4zDZEZcDSJo0fgr8A/AlupGN9EdHlsrZLmTA1vhIx+rOgxdEff28N
kvNM7qIK
=q6vu
-----END PGP SIGNATURE-----";

/// Unlocked MIT test key from `proton-crypto`'s own test fixtures. It is
/// re-locked in-test with the mailbox-derived passphrase so the real unlock path
/// is exercised.
const TEST_UNLOCKED_KEY: &str = "-----BEGIN PGP PRIVATE KEY BLOCK-----

xVgEaG0obxYJKwYBBAHaRw8BAQdArqDUqTY0fiMUpDOCMFPaPTBI4g7Y07MT+vnZ
VpFi/2kAAP4urYm3uXCEf+XDKoHUmc305p2kKroHDay3lFiD72/3MhM+zR9ydXN0
LXRlc3QgPHJ1c3QtdGVzdEB0ZXN0LnRlc3Q+wsAQBBMWCgCCBYJobShvAwsJBwkQ
T67+j/83wedFFAAAAAAAHAAgc2FsdEBub3RhdGlvbnMub3BlbnBncGpzLm9yZ2F7
6Et8juHQYp5P2AzmxisCl1Y6GYmQaeKJ/urC0W91AxUKCAMWAAICGQECmwMCHgEW
IQTI50ut9NIiFxkhL5lPrv6P/zfB5wAA4xkBAKC+MF20C0uWLgNAenHXv/l54IoE
0mqB2V0XCEnRq9gPAP9frokcQthm2TBHjYon9UmZBKmZxiHNgfBswzNuVI9ACsdd
BGhtKG8SCisGAQQBl1UBBQEBB0Cjj1BQ7ZclM6NGgLbhX3wEjklEzLwWIGpmHto2
Ae3hOAMBCgkAAP9NVRO5Vp0DOV7ztXe9ToEcVJ1ElofgbOoS6UVZVrIgsA+6wr4E
GBYKAHAFgmhtKG8JEE+u/o//N8HnRRQAAAAAABwAIHNhbHRAbm90YXRpb25zLm9w
ZW5wZ3Bqcy5vcmfTXVWm+eSp/BfE8dVZCmJvqbxt498GPLk4oUvGpMJwLAKbDBYh
BMjnS6300iIXGSEvmU+u/o//N8HnAADl7AEA7WF18t1EWxymtsfcQ2YtJHKtOFB9
Fv55Ii/IaWXhKPcA/1a+TJ3yxeJgBnl8W71/FykzGtECjlD6N7rkXZwIqsYO
=CpSA
-----END PGP PRIVATE KEY BLOCK-----";

struct MockConfig {
    username: &'static str,
    password: &'static str,
    mailbox_password: &'static str,
    password_mode: u8,
    two_fa: u32,
    require_hv: bool,
    invalid_server_proof: bool,
    invalid_modulus: bool,
    addresses: Vec<(&'static str, u32)>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            username: "alice@proton.me",
            password: "account-password",
            mailbox_password: "account-password",
            password_mode: 1,
            two_fa: 0,
            require_hv: false,
            invalid_server_proof: false,
            invalid_modulus: false,
            addresses: vec![("alice@proton.me", 1)],
        }
    }
}

struct MockState {
    username: String,
    password_mode: u8,
    two_fa: u32,
    require_hv: bool,
    invalid_server_proof: bool,
    modulus: String,
    salt: String,
    challenge: String,
    verifier: Option<SRPVerifierB64>,
    interaction: Option<ServerInteraction>,
    hv_token: String,
    hv_headers: Vec<Option<String>>,
    user: Value,
    addresses: Value,
    salts: Value,
    uid: String,
    access: String,
    refresh: String,
    refresh_tokens: Vec<String>,
    logout_calls: usize,
}

struct MockProton {
    server: MockServer,
    state: Arc<Mutex<MockState>>,
}

impl MockProton {
    async fn start(config: MockConfig) -> Self {
        let modulus = if config.invalid_modulus {
            tamper_modulus(TEST_MODULUS)
        } else {
            TEST_MODULUS.to_string()
        };

        let (salt, challenge, verifier, interaction) = if config.invalid_modulus {
            (
                base64::engine::general_purpose::STANDARD.encode([9u8; 10]),
                base64::engine::general_purpose::STANDARD.encode([8u8; 256]),
                None,
                None,
            )
        } else {
            let verifier = SRPAuth::generate_verifier(
                &RPGPVerifier::default(),
                config.password,
                None,
                &modulus,
            )
            .expect("test SRP verifier");
            let verifier_b64: SRPVerifierB64 = verifier.into();
            let server_verifier =
                ServerClientVerifier::try_from(&verifier_b64).expect("server verifier");
            let mut interaction = ServerInteraction::new_with_modulus_extractor(
                &RPGPVerifier::default(),
                &modulus,
                &server_verifier,
            )
            .expect("server interaction");
            let challenge = interaction.generate_challenge().encode_b64();
            (
                verifier_b64.salt.clone(),
                challenge,
                Some(verifier_b64),
                Some(interaction),
            )
        };

        // Proton's SRP salt (10 bytes) and its per-key mailbox salt (16 bytes,
        // bcrypt) are separate values. Reuse of the SRP salt is not valid.
        let key_salt = base64::engine::general_purpose::STANDARD.encode([7u8; 16]);
        let derived = proton_core::crypto::kdf::derive_key_passphrase(
            &SecretString::from(config.mailbox_password.to_string()),
            &key_salt,
        )
        .expect("derive key passphrase");
        let locked = lock_test_key(&derived);

        let mut address_json = Vec::new();
        let mut salt_json = vec![json!({ "ID": "user-key", "KeySalt": key_salt.clone() })];
        for (index, (email, order)) in config.addresses.iter().enumerate() {
            address_json.push(json!({
                "ID": format!("addr-{index}"),
                "Email": email,
                "Order": order,
                "Keys": [{
                    "ID": format!("addr-key-{index}"),
                    "PrivateKey": locked,
                    "Active": 1,
                    "Primary": 1,
                }],
            }));
            salt_json.push(json!({
                "ID": format!("addr-key-{index}"),
                "KeySalt": key_salt.clone(),
            }));
        }
        let user = json!({
            "User": {
                "Keys": [{
                    "ID": "user-key",
                    "PrivateKey": locked,
                    "Active": 1,
                    "Primary": 1,
                }],
            },
        });

        let state = Arc::new(Mutex::new(MockState {
            username: config.username.to_string(),
            password_mode: config.password_mode,
            two_fa: config.two_fa,
            require_hv: config.require_hv,
            invalid_server_proof: config.invalid_server_proof,
            modulus,
            salt,
            challenge,
            verifier,
            interaction,
            hv_token: "human-token".to_string(),
            hv_headers: Vec::new(),
            user,
            addresses: json!({ "Addresses": address_json }),
            salts: json!({ "KeySalts": salt_json }),
            uid: "session-uid".to_string(),
            access: "access-1".to_string(),
            refresh: "refresh-1".to_string(),
            refresh_tokens: Vec::new(),
            logout_calls: 0,
        }));

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/auth/v4/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Code": 1000,
                "UID": "unauth-uid",
                "AccessToken": "unauth-access",
                "RefreshToken": "unauth-refresh",
            })))
            .mount(&server)
            .await;

        let info_state = state.clone();
        Mock::given(method("POST"))
            .and(path("/core/v4/auth/info"))
            .respond_with(move |request: &Request| {
                let mut state = info_state.lock().unwrap();
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                assert_eq!(
                    body["Username"].as_str(),
                    Some(state.username.as_str()),
                    "the adapter must send the account email"
                );
                // A retried login (for example after human verification) starts
                // a fresh SRP exchange against the same stored verifier.
                if let Some(verifier) = state.verifier.clone() {
                    let server_verifier =
                        ServerClientVerifier::try_from(&verifier).expect("server verifier");
                    let mut interaction = ServerInteraction::new_with_modulus_extractor(
                        &RPGPVerifier::default(),
                        &state.modulus,
                        &server_verifier,
                    )
                    .expect("server interaction");
                    state.challenge = interaction.generate_challenge().encode_b64();
                    state.interaction = Some(interaction);
                }
                let challenge = state.challenge.clone();
                let response = json!({
                    "Code": 1000,
                    "Modulus": state.modulus,
                    "ServerEphemeral": challenge,
                    "Version": 4,
                    "Salt": state.salt,
                    "SRPSession": "srp-session",
                });
                drop(state);
                ResponseTemplate::new(200).set_body_json(response)
            })
            .mount(&server)
            .await;

        let auth_state = state.clone();
        Mock::given(method("POST"))
            .and(path("/core/v4/auth"))
            .respond_with(move |request: &Request| {
                let mut state = auth_state.lock().unwrap();
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                let hv = request
                    .headers
                    .get("x-pm-human-verification-token")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);

                let client = SRPProofB64 {
                    client_ephemeral: body["ClientEphemeral"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    client_proof: body["ClientProof"].as_str().unwrap_or_default().to_string(),
                    expected_server_proof: String::new(),
                };

                let Some(interaction) = state.interaction.as_mut() else {
                    return ResponseTemplate::new(401)
                        .set_body_json(json!({ "Code": 401, "Error": "bad SRP session" }));
                };
                let server_client = match ServerClientProof::try_from(&client) {
                    Ok(proof) => proof,
                    Err(_) => {
                        return ResponseTemplate::new(400)
                            .set_body_json(json!({ "Code": 400, "Error": "bad proof" }));
                    }
                };
                let server_proof = match interaction.verify_proof(&server_client) {
                    Ok(proof) => proof.encode_b64(),
                    Err(_) => {
                        return ResponseTemplate::new(400)
                            .set_body_json(json!({ "Code": 400, "Error": "bad proof" }));
                    }
                };
                state.hv_headers.push(hv.clone());

                if state.require_hv && hv.as_deref() != Some(state.hv_token.as_str()) {
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "Code": 9001,
                        "Error": "Human verification required",
                        "Details": {
                            "HumanVerificationMethods": ["captcha"],
                            "HumanVerificationToken": "hv-session",
                            "WebUrl": "https://verify.example.test/hv",
                        },
                    }));
                }

                let proof = if state.invalid_server_proof {
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()
                } else {
                    server_proof
                };
                success_response(&state, proof)
            })
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/core/v4/auth/2fa"))
            .respond_with(|request: &Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                let code = body["TwoFactorCode"].as_str().unwrap_or_default();
                if code.is_empty() {
                    ResponseTemplate::new(400)
                        .set_body_json(json!({ "Code": 400, "Error": "missing code" }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({ "Code": 1000 }))
                }
            })
            .mount(&server)
            .await;

        let salts_state = state.clone();
        Mock::given(method("GET"))
            .and(path("/core/v4/keys/salts"))
            .respond_with(move |_request: &Request| {
                let state = salts_state.lock().unwrap();
                ResponseTemplate::new(200).set_body_json(state.salts.clone())
            })
            .mount(&server)
            .await;

        let user_state = state.clone();
        Mock::given(method("GET"))
            .and(path("/core/v4/users"))
            .respond_with(move |_request: &Request| {
                let state = user_state.lock().unwrap();
                ResponseTemplate::new(200).set_body_json(state.user.clone())
            })
            .mount(&server)
            .await;

        let addresses_state = state.clone();
        Mock::given(method("GET"))
            .and(path("/core/v4/addresses"))
            .respond_with(move |_request: &Request| {
                let state = addresses_state.lock().unwrap();
                ResponseTemplate::new(200).set_body_json(state.addresses.clone())
            })
            .mount(&server)
            .await;

        let refresh_state = state.clone();
        Mock::given(method("POST"))
            .and(path("/auth/v4/refresh"))
            .respond_with(move |request: &Request| {
                let mut state = refresh_state.lock().unwrap();
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                let token = body["RefreshToken"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                state.refresh_tokens.push(token.clone());
                if token.is_empty() {
                    return ResponseTemplate::new(401)
                        .set_body_json(json!({ "Code": 401, "Error": "bad refresh token" }));
                }
                let next = state.refresh_tokens.len() + 1;
                ResponseTemplate::new(200).set_body_json(json!({
                    "Code": 1000,
                    "AccessToken": format!("access-{next}"),
                    "RefreshToken": format!("refresh-{next}"),
                }))
            })
            .mount(&server)
            .await;

        let logout_state = state.clone();
        Mock::given(method("DELETE"))
            .and(path("/core/v4/auth"))
            .respond_with(move |_request: &Request| {
                logout_state.lock().unwrap().logout_calls += 1;
                ResponseTemplate::new(200).set_body_json(json!({ "Code": 1000 }))
            })
            .mount(&server)
            .await;

        Self { server, state }
    }

    fn endpoints(&self) -> ProtonEndpoints {
        ProtonEndpoints::new(self.server.uri()).expect("mock endpoint")
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().unwrap()
    }
}

fn success_response(state: &MockState, proof: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "Code": 1000,
        "UID": state.uid,
        "AccessToken": state.access,
        "RefreshToken": state.refresh,
        "ServerProof": proof,
        "PasswordMode": state.password_mode,
        "2FA": { "Enabled": state.two_fa },
    }))
}

fn tamper_modulus(modulus: &str) -> String {
    let mut tampered = modulus.to_string();
    if let Some(index) = tampered.find("W2z5") {
        tampered.replace_range(index..index + 1, "X");
    }
    tampered
}

fn lock_test_key(passphrase: &SecretString) -> String {
    use proton_crypto::crypto::{DataEncoding, PGPProviderSync};

    let provider = proton_crypto::new_pgp_provider();
    let unlocked = provider
        .private_key_import_unlocked(TEST_UNLOCKED_KEY.as_bytes(), DataEncoding::Armor)
        .expect("import unlocked test key");
    let locked = provider
        .private_key_export(&unlocked, passphrase.expose_secret(), DataEncoding::Armor)
        .expect("lock test key");
    String::from_utf8(locked.as_ref().to_vec()).expect("utf8 armored key")
}

struct Harness {
    dir: TempDir,
    store: Arc<Store>,
    secrets: Arc<CredentialSecretRepository>,
    sessions: Arc<AccountSessionManager>,
    keys: Arc<ProtonKeyStores>,
    endpoints: ProtonEndpoints,
    auth: ProtonAuthentication,
    account: Account,
    mock: MockProton,
}

impl Harness {
    async fn new(config: MockConfig) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(Store::open(dir.path().join("pige.db")).expect("open store"));
        store.migrate().expect("migrate store");
        let credentials = Arc::new(FakeCredentials::default());
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials.clone() as Arc<dyn CredentialStore>,
            store.clone(),
        ));
        let sessions = Arc::new(AccountSessionManager::new(store.clone()));
        let keys = Arc::new(ProtonKeyStores::new());
        let mock = MockProton::start(config).await;
        let endpoints = mock.endpoints();
        let auth = ProtonAuthentication::new(
            secrets.clone(),
            sessions.clone(),
            store.clone(),
            endpoints.clone(),
            keys.clone(),
        );
        let account = Account {
            id: AccountId::new(),
            label: "alice".to_string(),
            email: "alice@proton.me".to_string(),
            display_name: "Alice".to_string(),
            backend: MailBackend::proton(None),
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        };
        store.upsert_account(&account).expect("seed account");
        Self {
            dir,
            store,
            secrets,
            sessions,
            keys,
            endpoints,
            auth,
            account,
            mock,
        }
    }

    fn request(&self) -> AuthRequest {
        AuthRequest {
            provider: ProviderId::from(PROTON_PROVIDER),
            account: Some(self.account.id.clone()),
            enable_contacts: false,
        }
    }

    async fn begin(&mut self) -> AuthProgress {
        self.auth.begin(self.request()).await.expect("begin auth")
    }

    fn account_id(&self) -> AccountId {
        self.account.id.clone()
    }

    fn staged_present(&self, attempt: &AuthAttemptId) -> bool {
        let reference = StagedSecretRef::for_attempt(attempt).expect("staged ref");
        self.secrets
            .load_staged(&reference)
            .expect("load staged")
            .is_some()
    }

    fn keys_unlocked(&self) -> bool {
        self.keys
            .get(&self.account.id)
            .is_some_and(|store| store.is_unlocked())
    }

    fn context(&self) -> OperationContext {
        self.sessions
            .context(&self.account.id, Duration::from_secs(30))
            .expect("context")
    }

    fn save_proton_secret(&self, uid: &str, access: &str, refresh: &str, skp: &str) {
        let ctx = self.context();
        let envelope = SecretEnvelope::proton(pige_core::auth::ProtonSessionSecret {
            uid: Some(uid.to_string()),
            access_token: Some(SecretString::from(access.to_string())),
            refresh_token: Some(SecretString::from(refresh.to_string())),
            skp: Some(SecretString::from(skp.to_string())),
        });
        self.secrets
            .save(&ctx, &envelope)
            .expect("save proton secret");
    }

    fn proton_secret(&self) -> pige_core::auth::ProtonSessionSecret {
        let envelope = self
            .secrets
            .load(&self.account.id)
            .expect("load secret")
            .expect("secret present");
        match envelope.credentials {
            StoredCredentials::Proton(secret) => secret,
            other => panic!("expected Proton secret, got {other:?}"),
        }
    }
}

fn challenge_attempt(progress: AuthProgress) -> (AuthAttemptId, AuthChallenge) {
    match progress {
        AuthProgress::Challenge { attempt, challenge } => (attempt, challenge),
        other => panic!("expected a challenge, got {other:?}"),
    }
}

fn complete(progress: AuthProgress) -> pige_core::auth::AuthenticatedAccount {
    match progress {
        AuthProgress::Complete(account) => account,
        other => panic!("expected completion, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_server_proof_or_modulus_rejected() {
    // 1. A valid SRP exchange followed by a wrong server proof must abort.
    let mut harness = Harness::new(MockConfig {
        invalid_server_proof: true,
        ..MockConfig::default()
    })
    .await;
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let error = harness
        .auth
        .respond(
            &attempt,
            AuthAnswer::Password(SecretString::from("account-password")),
        )
        .await
        .expect_err("a bad server proof must be rejected");
    assert_eq!(error.kind, ErrorKind::Reauthenticate);
    assert!(!harness.staged_present(&attempt), "no secret may be staged");
    assert!(!harness.keys_unlocked());

    // 2. A tampered signed modulus must fail before any proof is submitted.
    let mut harness = Harness::new(MockConfig {
        invalid_modulus: true,
        ..MockConfig::default()
    })
    .await;
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let error = harness
        .auth
        .respond(
            &attempt,
            AuthAnswer::Password(SecretString::from("account-password")),
        )
        .await
        .expect_err("a tampered modulus must be rejected");
    assert_eq!(error.kind, ErrorKind::Reauthenticate);
    assert!(!harness.staged_present(&attempt));
}

#[tokio::test]
async fn totp_challenge_requires_same_attempt() {
    let mut harness = Harness::new(MockConfig {
        two_fa: 1,
        ..MockConfig::default()
    })
    .await;
    let (attempt, challenge) = challenge_attempt(harness.begin().await);
    assert!(matches!(challenge, AuthChallenge::Password { .. }));

    let (totp_attempt, challenge) = challenge_attempt(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::Password(SecretString::from("account-password")),
            )
            .await
            .expect("password accepted"),
    );
    assert_eq!(totp_attempt, attempt);
    assert_eq!(challenge, AuthChallenge::Totp);

    // A TOTP answer for an unknown attempt is refused.
    let unknown = AuthAttemptId::new(uuid::Uuid::new_v4().to_string());
    let error = harness
        .auth
        .respond(&unknown, AuthAnswer::Totp(SecretString::from("123456")))
        .await
        .expect_err("unknown attempt");
    assert_eq!(error.kind, ErrorKind::NotFound);

    // A cancelled attempt cannot redeem a TOTP code.
    harness.auth.cancel(&attempt).await.expect("cancel");
    let error = harness
        .auth
        .respond(&attempt, AuthAnswer::Totp(SecretString::from("123456")))
        .await
        .expect_err("cancelled attempt");
    assert_eq!(error.kind, ErrorKind::NotFound);
    assert!(!harness.staged_present(&attempt));

    // A live attempt rejects the wrong answer kind, then accepts the code.
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let _ = challenge_attempt(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::Password(SecretString::from("account-password")),
            )
            .await
            .expect("password accepted"),
    );
    let error = harness
        .auth
        .respond(
            &attempt,
            AuthAnswer::Password(SecretString::from("not-a-totp")),
        )
        .await
        .expect_err("wrong answer kind");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    let authenticated = complete(
        harness
            .auth
            .respond(&attempt, AuthAnswer::Totp(SecretString::from("123456")))
            .await
            .expect("correct TOTP completes"),
    );
    assert_eq!(authenticated.persistence, SecretPersistence::Persistent);
}

#[tokio::test]
async fn two_password_login_unlocks_mailbox() {
    let mut harness = Harness::new(MockConfig {
        password_mode: 2,
        mailbox_password: "mailbox-password",
        addresses: vec![("alice@proton.me", 1), ("alias@pm.me", 2)],
        ..MockConfig::default()
    })
    .await;
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let (attempt, challenge) = challenge_attempt(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::Password(SecretString::from("account-password")),
            )
            .await
            .expect("account password accepted"),
    );
    assert_eq!(challenge, AuthChallenge::MailboxPassword);
    assert!(
        !harness.keys_unlocked(),
        "keys stay locked until the mailbox password is supplied"
    );

    // A wrong mailbox password fails and leaves the mailbox-password step
    // answerable again.
    let error = harness
        .auth
        .respond(
            &attempt,
            AuthAnswer::MailboxPassword(SecretString::from("wrong-mailbox")),
        )
        .await
        .expect_err("wrong mailbox password");
    assert_eq!(error.kind, ErrorKind::Reauthenticate);
    assert!(!harness.staged_present(&attempt));

    let authenticated = complete(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::MailboxPassword(SecretString::from("mailbox-password")),
            )
            .await
            .expect("mailbox password accepted"),
    );
    assert_eq!(authenticated.account.email, "alice@proton.me");
    let keys = harness.keys.get(&harness.account_id()).expect("key store");
    assert!(keys.is_unlocked());
    let addresses = keys.addresses().expect("addresses");
    let emails: Vec<&str> = addresses
        .iter()
        .map(|address| address.email.as_str())
        .collect();
    assert_eq!(emails, vec!["alice@proton.me", "alias@pm.me"]);

    let staged_envelope = harness
        .secrets
        .load_staged(&authenticated.credential_ref)
        .expect("load staged")
        .expect("staged secret");
    let secret = match staged_envelope.credentials {
        StoredCredentials::Proton(secret) => secret,
        other => panic!("expected a Proton secret, got {other:?}"),
    };
    assert!(
        secret
            .skp
            .as_ref()
            .is_some_and(|skp| !skp.expose_secret().is_empty())
    );
    assert!(secret.refresh_token.is_some());
    assert!(
        secret.access_token.is_some(),
        "the session tokens were captured from the same attempt"
    );
    // The account password itself is deliberately never persisted.
    assert!(!format!("{secret:?}").contains("account-password"));
    assert!(!format!("{secret:?}").contains("mailbox-password"));
}

#[tokio::test]
async fn secondary_address_keys_are_available() {
    let mut harness = Harness::new(MockConfig {
        addresses: vec![
            ("alice@proton.me", 2),
            ("alias@pm.me", 1),
            ("second@proton.me", 3),
        ],
        ..MockConfig::default()
    })
    .await;
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let authenticated = complete(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::Password(SecretString::from("account-password")),
            )
            .await
            .expect("login"),
    );
    assert_eq!(authenticated.account.email, "alice@proton.me");

    let keys = harness.keys.get(&harness.account_id()).expect("key store");
    let addresses = keys.addresses().expect("addresses");
    // Proton's `Order` decides the default address, not API order.
    assert_eq!(addresses[0].email, "alias@pm.me");
    assert_eq!(
        addresses
            .iter()
            .map(|address| address.email.as_str())
            .collect::<Vec<_>>(),
        vec!["alias@pm.me", "alice@proton.me", "second@proton.me"]
    );
    assert!(addresses.iter().all(|address| address.key_count >= 1));
    assert_eq!(keys.primary_email().as_deref(), Some("alias@pm.me"));
    // Debug exposes only counts, never key armor or passphrases.
    let debug = format!("{keys:?}");
    assert!(debug.contains("addresses: 3"));
    assert!(!debug.contains("BEGIN PGP"));
}

#[tokio::test]
async fn captcha_can_resume_bound_attempt() {
    let mut harness = Harness::new(MockConfig {
        require_hv: true,
        ..MockConfig::default()
    })
    .await;
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let (attempt, challenge) = challenge_attempt(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::Password(SecretString::from("account-password")),
            )
            .await
            .expect("human verification challenge"),
    );
    let AuthChallenge::HumanVerification(human) = challenge else {
        panic!("expected human verification, got {challenge:?}");
    };
    assert!(human.instructions.contains("verify.example.test"));
    assert!(!harness.staged_present(&attempt));

    // A token delivered to another attempt cannot complete this one.
    let unknown = AuthAttemptId::new(uuid::Uuid::new_v4().to_string());
    let error = harness
        .auth
        .respond(
            &unknown,
            AuthAnswer::HumanVerification(pige_core::auth::HumanVerificationAnswer {
                token: "human-token".to_string(),
            }),
        )
        .await
        .expect_err("unbound token");
    assert_eq!(error.kind, ErrorKind::NotFound);

    let authenticated = complete(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::HumanVerification(pige_core::auth::HumanVerificationAnswer {
                    token: "human-token".to_string(),
                }),
            )
            .await
            .expect("bound token completes the same attempt"),
    );
    assert_eq!(authenticated.account.email, "alice@proton.me");
    assert_eq!(
        harness.mock.state().hv_headers.last(),
        Some(&Some("human-token".to_string())),
        "the verification token was attached to the retried request"
    );
}

#[tokio::test]
async fn fido_only_returns_unsupported() {
    let mut harness = Harness::new(MockConfig {
        two_fa: 2,
        ..MockConfig::default()
    })
    .await;
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let (attempt, challenge) = challenge_attempt(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::Password(SecretString::from("account-password")),
            )
            .await
            .expect("FIDO-only account"),
    );
    let AuthChallenge::Unsupported { reason } = challenge else {
        panic!("expected unsupported, got {challenge:?}");
    };
    assert!(reason.contains("FIDO2"));
    assert!(!harness.staged_present(&attempt));
    assert!(!harness.keys_unlocked());
    let error = harness
        .auth
        .respond(&attempt, AuthAnswer::Totp(SecretString::from("123456")))
        .await
        .expect_err("unsupported attempt is terminal");
    assert_eq!(error.kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn refresh_rotation_survives_restart() {
    let harness = Harness::new(MockConfig::default()).await;
    harness.save_proton_secret("session-uid", "access-1", "refresh-1", "skp-secret");

    // First process refresh persists the rotated pair before returning.
    let refresher = pige_core::auth::proton::ProtonTokenRefresher::new(
        harness.secrets.clone(),
        harness.endpoints.clone(),
    );
    let ctx = harness.context();
    let token = refresher.refresh(&ctx).await.expect("first refresh");
    assert_eq!(token.expose_secret(), "access-2");
    let rotated = harness.proton_secret();
    assert_eq!(
        rotated
            .access_token
            .as_ref()
            .map(|value| value.expose_secret()),
        Some("access-2")
    );
    assert_eq!(
        rotated
            .refresh_token
            .as_ref()
            .map(|value| value.expose_secret()),
        Some("refresh-2")
    );

    // Simulated restart: a different process/refresher only has the stored
    // envelope, so the newly rotated refresh token must be used.
    let restart_context = harness.context();
    let restarted = pige_core::auth::proton::ProtonTokenRefresher::new(
        harness.secrets.clone(),
        harness.endpoints.clone(),
    );
    let token = restarted
        .refresh(&restart_context)
        .await
        .expect("refresh after restart");
    assert_eq!(token.expose_secret(), "access-3");
    let rotated_again = harness.proton_secret();
    assert_eq!(
        rotated_again
            .refresh_token
            .as_ref()
            .map(|value| value.expose_secret()),
        Some("refresh-3")
    );
    assert_eq!(
        harness.mock.state().refresh_tokens,
        vec!["refresh-1".to_string(), "refresh-2".to_string()]
    );
}

#[tokio::test]
async fn logout_locks_keys_and_cancels_work() {
    let mut harness = Harness::new(MockConfig::default()).await;
    let (attempt, _) = challenge_attempt(harness.begin().await);
    let authenticated = complete(
        harness
            .auth
            .respond(
                &attempt,
                AuthAnswer::Password(SecretString::from("account-password")),
            )
            .await
            .expect("login"),
    );
    // Simulate F10 committing the staged candidate to the active credential.
    let staged = harness
        .secrets
        .load_staged(&authenticated.credential_ref)
        .expect("load staged")
        .expect("staged credential");
    let commit_ctx = harness.context();
    harness.secrets.save(&commit_ctx, &staged).expect("commit");
    let key_store = harness.keys.get(&harness.account_id()).expect("key store");
    assert!(key_store.is_unlocked());

    let live_context = harness.context();
    harness
        .auth
        .revoke(&harness.account_id())
        .await
        .expect("logout");

    assert!(
        live_context.cancellation.is_cancelled(),
        "logout must cancel live account work"
    );
    assert!(
        !key_store.is_unlocked(),
        "logout must wipe the unlocked key material"
    );
    assert!(
        harness.keys.get(&harness.account_id()).is_none(),
        "the registry entry is dropped"
    );
    assert!(
        harness
            .secrets
            .load(&harness.account_id())
            .expect("load")
            .is_none(),
        "the stored credential is removed"
    );
    assert_eq!(harness.mock.state().logout_calls, 1);
}

#[tokio::test]
async fn late_refresh_after_removal_cannot_save() {
    let harness = Harness::new(MockConfig::default()).await;
    harness.save_proton_secret("session-uid", "access-1", "refresh-1", "skp-secret");
    let old_generation = harness
        .store
        .generation(&harness.account_id())
        .expect("generation");
    let live_context = harness.context();

    harness
        .sessions
        .invalidate(&harness.account_id())
        .expect("invalidate");

    // A context minted before removal is cancelled outright.
    assert!(live_context.cancellation.is_cancelled());
    let refresher = pige_core::auth::proton::ProtonTokenRefresher::new(
        harness.secrets.clone(),
        harness.endpoints.clone(),
    );
    let error = refresher
        .refresh(&live_context)
        .await
        .expect_err("a stale refresh must not proceed");
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(harness.mock.state().refresh_tokens.len(), 0);

    // A context that is not cancelled but carries the old generation still
    // cannot persist the rotated token: the durable write fence rejects it.
    harness.save_proton_secret("session-uid", "access-1", "refresh-1", "skp-secret");
    let stale_generation = OperationContext::new(
        harness.account_id(),
        old_generation,
        tokio::time::Instant::now() + Duration::from_secs(30),
    );
    let error = refresher
        .refresh(&stale_generation)
        .await
        .expect_err("a stale-generation save must be fenced");
    assert_eq!(error.kind, ErrorKind::Conflict);
    let stored = harness.proton_secret();
    assert_eq!(
        stored
            .refresh_token
            .as_ref()
            .map(|value| value.expose_secret()),
        Some("refresh-1"),
        "the rotated token was not persisted"
    );
}

#[tokio::test]
async fn secrets_never_enter_debug_or_cache() {
    let harness = Harness::new(MockConfig {
        two_fa: 1,
        ..MockConfig::default()
    })
    .await;

    let request = harness.request();
    let account_id = harness.account_id();
    let mut auth = harness.auth;
    let (attempt, _) = challenge_attempt(auth.begin(request).await.expect("begin"));
    let (attempt, _) = challenge_attempt(
        auth.respond(
            &attempt,
            AuthAnswer::Password(SecretString::from("account-password")),
        )
        .await
        .expect("password accepted and TOTP requested"),
    );
    // The pending attempt holds the account password in memory.
    let trace = format!("{auth:?}");
    for secret in ["account-password", "refresh-1", "skp-secret"] {
        assert!(!trace.contains(secret), "Debug leaked {secret}: {trace}");
    }

    let authenticated = complete(
        auth.respond(&attempt, AuthAnswer::Totp(SecretString::from("123456")))
            .await
            .expect("complete"),
    );
    let keys = harness.keys.get(&account_id).expect("keys");
    let factory = ProtonProviderFactory::new(
        harness.sessions.clone(),
        harness.secrets.clone(),
        harness.endpoints.clone(),
        harness.keys.clone(),
    );
    let client = ProtonClient::new(
        factory.token_source(&account_id),
        keys.clone(),
        harness.endpoints.clone(),
    );
    let refresher = pige_core::auth::proton::ProtonTokenRefresher::new(
        harness.secrets.clone(),
        harness.endpoints.clone(),
    );

    let debug_forms = format!(
        "{auth:?}\n{authenticated:?}\n{keys:?}\n{factory:?}\n{client:?}\n{refresher:?}\n{:?}\n{:?}",
        harness.endpoints, harness.keys,
    );
    let envelope = harness
        .secrets
        .load_staged(&authenticated.credential_ref)
        .expect("load staged")
        .expect("staged present");
    for secret in [
        "account-password",
        "access-1",
        "refresh-1",
        "session-uid",
        "skp-secret",
        "BEGIN PGP PRIVATE KEY",
    ] {
        assert!(
            !debug_forms.contains(secret),
            "Debug leaked {secret}: {debug_forms}"
        );
    }
    assert!(!format!("{envelope:?}").contains("access-1"));
    assert!(!format!("{envelope:?}").contains("refresh-1"));

    // The protected envelope lives in memory; the SQLite cache and the data
    // directory must not contain any session secret.
    let cached = std::fs::read(harness.dir.path().join("pige.db")).expect("read db");
    for secret in ["access-1", "refresh-1", "skp-secret", "account-password"] {
        assert!(
            !contains_bytes(&cached, secret.as_bytes()),
            "SQLite cache contains {secret}"
        );
    }
    assert_no_bytes(harness.dir.path(), b"refresh-1");
    assert!(
        !harness.dir.path().join("sessions").exists(),
        "the vendored session-file module must never be used"
    );
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn assert_no_bytes(root: &Path, needle: &[u8]) {
    for entry in std::fs::read_dir(root).expect("read dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.is_dir() {
            assert_no_bytes(&path, needle);
        } else if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !contains_bytes(&bytes, needle),
                "{} contains forbidden secret bytes",
                path.display()
            );
        }
    }
}
