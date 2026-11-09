//! Explicit, opt-in Proton live checks.
//!
//! These tests are `#[ignore]`d and never run in the default or feature suite.
//! They use the production Proton API and a real disposable account, so a run
//! is only valid when the operator deliberately supplies every required value:
//!
//! ```text
//! PIGE_TEST_PROTON_EMAIL=...
//! PIGE_TEST_PROTON_ACCOUNT_PASSWORD=...
//! PIGE_TEST_PROTON_MAILBOX_PASSWORD=...   # when Proton asks for it
//! PIGE_TEST_PROTON_TOTP=123456            # when two-step is enabled
//! PIGE_TEST_PROTON_HV_TOKEN=...           # when human verification is required
//! ```
//!
//! Sending additionally requires `PIGE_TEST_ALLOW_SEND=1`; the send test only
//! mails the account itself. Missing env is a hard failure when a live test is
//! explicitly requested, never a silent pass. Secrets are read from the process
//! environment and are never printed.

#![cfg(feature = "provider-proton")]

use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;

use pige_core::Store;
use pige_core::account::{Account, AccountId};
use pige_core::auth::{
    AccountSessionManager, AuthAnswer, AuthChallenge, AuthProgress, AuthenticationService,
    CredentialSecretRepository, HumanVerificationAnswer, SecretRepository,
};
use pige_core::mail::Address;
use pige_core::platform::CredentialStore;
use pige_core::platform::fake::FakeCredentials;
use pige_core::providers::proton::{
    ProtonAuthentication, ProtonEndpoints, ProtonKeyStores, ProtonProviderFactory, ProtonSubmission,
};
use pige_core::providers::{
    MailChange, OperationContext, OperationId, ProviderFactory, SubmissionOutcome,
    SubmissionRequest, SubmissionService, SyncRequest,
};
use pige_core::send::OutgoingMessage;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(60);

struct LiveEnv {
    email: String,
    account_password: SecretString,
    mailbox_password: Option<SecretString>,
    totp: Option<SecretString>,
    hv_token: Option<String>,
    allow_send: bool,
}

impl LiveEnv {
    fn from_env(require_send: bool) -> Self {
        let email = std::env::var("PIGE_TEST_PROTON_EMAIL").unwrap_or_else(|_| {
            panic!(
                "live Proton test requested but PIGE_TEST_PROTON_EMAIL is not set; \
                 see rust/pige_core/tests/live_proton.rs"
            )
        });
        let account_password: SecretString = SecretString::from(
            std::env::var("PIGE_TEST_PROTON_ACCOUNT_PASSWORD").unwrap_or_else(|_| {
                panic!(
                    "live Proton test requested but PIGE_TEST_PROTON_ACCOUNT_PASSWORD is not set"
                )
            }),
        );
        let mailbox_password = std::env::var("PIGE_TEST_PROTON_MAILBOX_PASSWORD")
            .ok()
            .map(SecretString::from);
        let totp = std::env::var("PIGE_TEST_PROTON_TOTP")
            .ok()
            .map(SecretString::from);
        let hv_token = std::env::var("PIGE_TEST_PROTON_HV_TOKEN").ok();
        let allow_send = std::env::var("PIGE_TEST_ALLOW_SEND").as_deref() == Ok("1");
        if require_send && !allow_send {
            panic!(
                "live Proton send requested without PIGE_TEST_ALLOW_SEND=1; \
                 this guard prevents an accidental real send"
            );
        }
        Self {
            email,
            account_password,
            mailbox_password,
            totp,
            hv_token,
            allow_send,
        }
    }
}

struct LiveHarness {
    _dir: TempDir,
    sessions: Arc<AccountSessionManager>,
    secrets: Arc<CredentialSecretRepository>,
    keys: Arc<ProtonKeyStores>,
    factory: ProtonProviderFactory,
    auth: ProtonAuthentication,
}

impl LiveHarness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(Store::open(dir.path().join("live.db")).expect("open store"));
        store.migrate().expect("migrate store");
        let credentials: Arc<dyn CredentialStore> = Arc::new(FakeCredentials::default());
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials,
            store.clone(),
        ));
        let sessions = Arc::new(AccountSessionManager::new(store.clone()));
        let endpoints = ProtonEndpoints::production();
        let keys = Arc::new(ProtonKeyStores::new());
        let factory = ProtonProviderFactory::new(
            sessions.clone(),
            secrets.clone(),
            endpoints.clone(),
            keys.clone(),
        );
        let auth = ProtonAuthentication::new(
            secrets.clone(),
            sessions.clone(),
            store,
            endpoints,
            keys.clone(),
        )
        .with_refresher(factory.refresher());
        Self {
            _dir: dir,
            sessions,
            secrets,
            keys,
            factory,
            auth,
        }
    }

    async fn login(&mut self, env: &LiveEnv) -> (Account, OperationContext) {
        let mut progress = self
            .auth
            .begin_with_username(&env.email, None, true)
            .await
            .expect("begin live Proton login");
        loop {
            match progress {
                AuthProgress::Challenge { attempt, challenge } => {
                    let answer = match challenge {
                        AuthChallenge::Password { .. } => {
                            AuthAnswer::Password(env.account_password.clone())
                        }
                        AuthChallenge::Totp => AuthAnswer::Totp(
                            env.totp.clone().unwrap_or_else(|| {
                                panic!("Proton asked for a two-step code but PIGE_TEST_PROTON_TOTP is not set")
                            }),
                        ),
                        AuthChallenge::MailboxPassword => {
                            AuthAnswer::MailboxPassword(env.mailbox_password.clone().unwrap_or_else(
                                || {
                                    panic!(
                                        "Proton asked for the mailbox password but \
                                         PIGE_TEST_PROTON_MAILBOX_PASSWORD is not set"
                                    )
                                },
                            ))
                        }
                        AuthChallenge::HumanVerification(_) => {
                            AuthAnswer::HumanVerification(HumanVerificationAnswer {
                                token: env.hv_token.clone().unwrap_or_else(|| {
                                    panic!(
                                        "Proton asked for human verification but \
                                         PIGE_TEST_PROTON_HV_TOKEN is not set"
                                    )
                                }),
                            })
                        }
                        AuthChallenge::Unsupported { reason } => {
                            panic!("Proton sign-in mode is unsupported: {reason}")
                        }
                        AuthChallenge::Browser(_) => {
                            panic!("Proton unexpectedly requested a browser sign-in")
                        }
                    };
                    progress = self
                        .auth
                        .respond(&attempt, answer)
                        .await
                        .expect("answer live Proton challenge");
                }
                AuthProgress::Complete(authenticated) => {
                    let account = authenticated.account;
                    let ctx = self
                        .sessions
                        .context(&account.id, TIMEOUT)
                        .expect("live operation context");
                    // Persist the staged session so the factory's token source
                    // can refresh it like the application's F10 commit would.
                    let staged = self
                        .secrets
                        .load_staged(&authenticated.credential_ref)
                        .expect("load staged live credential")
                        .expect("staged live credential present");
                    self.secrets
                        .save(&ctx, &staged)
                        .expect("save live Proton credential");
                    return (account, ctx);
                }
                AuthProgress::Cancelled => panic!("live Proton login was cancelled"),
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires a disposable Proton account and live credentials"]
async fn live_proton_read_and_sync() {
    let env = LiveEnv::from_env(false);
    let mut harness = LiveHarness::new();
    let (account, ctx) = harness.login(&env).await;
    assert!(
        harness
            .keys
            .get(&account.id)
            .is_some_and(|keys| keys.is_unlocked())
    );

    let mut session = harness
        .factory
        .open(&account, &ctx)
        .await
        .expect("open live Proton session");
    let collections = session
        .mailbox
        .as_mut()
        .expect("mailbox capability")
        .collections(&ctx)
        .await
        .expect("list live Proton collections");
    assert!(
        !collections.is_empty(),
        "a live Proton account must expose at least one collection"
    );

    let page = session
        .sync
        .as_mut()
        .expect("sync capability")
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 20,
            },
            &ctx,
        )
        .await
        .expect("first live Proton sync page");
    let first = page.changes.iter().find_map(|change| match change {
        MailChange::UpsertMessage { message, .. } => Some(message.remote_id.clone()),
        _ => None,
    });
    if let Some(remote) = first {
        let content = session
            .mailbox
            .as_mut()
            .expect("mailbox capability")
            .body(&remote, &ctx)
            .await
            .expect("read the first live Proton message");
        assert!(
            content.plain_text.is_some() || content.html.is_some(),
            "a supported live body must expose text or HTML"
        );
        eprintln!("live Proton signature: {:?}", content.signature);
    }
    eprintln!(
        "live Proton: {} collections, {} sync changes",
        collections.len(),
        page.changes.len()
    );
}

#[tokio::test]
#[ignore = "requires a disposable Proton account, live credentials and send opt-in"]
async fn live_proton_send_requires_explicit_opt_in() {
    let env = LiveEnv::from_env(true);
    let mut harness = LiveHarness::new();
    let (account, ctx) = harness.login(&env).await;

    let message = OutgoingMessage {
        account_id: account.id.clone(),
        to: vec![Address::new(None, env.email.clone())],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Pige live Proton encryption check".to_string(),
        body: "This message is sent by Pige's explicit live Proton test to itself.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    };
    let submission = ProtonSubmission::new(harness.factory.client(&account.id), account.clone());
    let mut submission = submission;
    let request = SubmissionRequest {
        operation: OperationId::new(format!("live-proton-{}", AccountId::new().0)),
        message,
        attachments: Vec::new(),
        prepared: None,
    };
    let prepared = submission
        .prepare(&request, &ctx)
        .await
        .expect("prepare live Proton submission");
    let request = SubmissionRequest {
        prepared: Some(prepared),
        ..request
    };
    match submission
        .submit(&request, &ctx)
        .await
        .expect("submit live Proton message")
    {
        SubmissionOutcome::Accepted(receipt) => {
            eprintln!("live Proton accepted: {:?}", receipt.remote_message);
        }
        SubmissionOutcome::Rejected(error) => {
            panic!("live Proton send rejected: {}", error.safe_message)
        }
        SubmissionOutcome::Unknown => {
            panic!("live Proton send outcome is unknown; inspect Sent before retrying")
        }
    }
    assert!(env.allow_send);
}

#[tokio::test]
#[ignore = "requires a disposable Proton account and live credentials"]
async fn live_proton_logout_locks_keys() {
    let env = LiveEnv::from_env(false);
    let mut harness = LiveHarness::new();
    let (account, _ctx) = harness.login(&env).await;
    assert!(
        harness
            .keys
            .get(&account.id)
            .is_some_and(|keys| keys.is_unlocked())
    );

    harness
        .auth
        .revoke(&account.id)
        .await
        .expect("live Proton logout");
    assert!(
        !harness
            .keys
            .get(&account.id)
            .is_some_and(|keys| keys.is_unlocked()),
        "logout must lock the in-memory Proton key hierarchy"
    );
    assert!(
        harness
            .secrets
            .load(&account.id)
            .expect("read credential store")
            .is_none(),
        "logout must remove the protected Proton session"
    );
}
