//! Task P5: Proton account UX, security presentation and the live gate.
//!
//! These cases drive the real `api` lifecycle with a scripted authentication
//! service and a provider-neutral session. They are synthetic: no Proton
//! account, credential, key or network endpoint is used. The Proton wire
//! protocol itself is covered by `proton_auth`, `proton_crypto`,
//! `proton_mailbox` and `proton_submission`.

#![cfg(feature = "provider-proton")]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use pige_core::account::{Account, AccountId, MailBackend, Security, ServerConfig};
use pige_core::api::{self, Config, DB_FILE_NAME};
use pige_core::auth::{
    AuthChallenge, AuthProgress, AuthRouter, AuthenticationService, CredentialSecretRepository,
    FakeAuthScript, FakeAuthService, SecretRepository,
};
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::platform::{BackupStore, CredentialStore};
use pige_core::providers::proton::{ProtonEndpoints, ProtonKeyStores, ProtonProviderFactory};
use pige_core::providers::{
    OperationContext, PROTON_PROVIDER, ProviderCapabilities, ProviderFactory, ProviderId,
    ProviderRegistry, fake::FakeProviderFactory,
};
use pige_core::store::Store;
use tempfile::TempDir;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

type AuthHandle = Arc<Mutex<Box<dyn AuthenticationService>>>;

struct Harness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
    transport: Arc<FakeTransportFactory>,
    providers: Arc<dyn ProviderFactory>,
    backups: Arc<FakeBackupStore>,
    auth: Option<AuthHandle>,
}

impl Harness {
    fn new(script: FakeAuthScript) -> Self {
        let dir = TempDir::new().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("data")).expect("data dir");
        let credentials = Arc::new(FakeCredentials::default());
        let transport = Arc::new(FakeTransportFactory::default());
        let store =
            Arc::new(Store::open(dir.path().join("data").join(DB_FILE_NAME)).expect("auth store"));
        store.migrate().expect("migrate auth store");
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials.clone(),
            store.clone(),
        ));
        let sessions = Arc::new(pige_core::auth::AccountSessionManager::new(store.clone()));
        let proton =
            FakeAuthService::new(secrets.clone(), sessions.clone(), store.clone()).script(script);
        let google = FakeAuthService::new(secrets, sessions, store).script(
            FakeAuthScript::password("google-password")
                .provider(ProviderId::from(pige_core::providers::GMAIL_PROVIDER)),
        );
        let router = AuthRouter::new(vec![
            (
                ProviderId::from(PROTON_PROVIDER),
                Arc::new(Mutex::new(
                    Box::new(proton) as Box<dyn AuthenticationService>
                )),
            ),
            (
                ProviderId::from(pige_core::providers::GMAIL_PROVIDER),
                Arc::new(Mutex::new(
                    Box::new(google) as Box<dyn AuthenticationService>
                )),
            ),
        ])
        .expect("compose Proton and Google authentication");
        let mut registry = ProviderRegistry::new();
        for provider in [
            PROTON_PROVIDER,
            pige_core::providers::IMAP_SMTP_PROVIDER,
            pige_core::providers::GMAIL_PROVIDER,
        ] {
            registry
                .register(
                    ProviderId::from(provider),
                    Arc::new(FakeProviderFactory::new()),
                )
                .expect("register provider");
        }
        let harness = Self {
            dir,
            credentials,
            transport,
            providers: Arc::new(registry),
            backups: Arc::new(FakeBackupStore::new()),
            auth: Some(Arc::new(Mutex::new(
                Box::new(router) as Box<dyn AuthenticationService>
            ))),
        };
        harness.init();
        harness
    }

    fn config(&self) -> Config {
        Config {
            data_dir: self.dir.path().join("data"),
            cache_dir: self.dir.path().join("cache"),
            config_dir: self.dir.path().join("config"),
            credentials: self.credentials.clone(),
            notifier: Arc::new(FakeNotifier::new()),
            opener: Arc::new(FakeOpener::new()),
            providers: self.providers.clone(),
            transport: self.transport.clone(),
            backups: self.backups.clone(),
            auth: self.auth.clone(),
        }
    }

    fn init(&self) {
        api::reset_for_tests();
        api::init(self.config()).expect("init api");
    }

    fn store(&self) -> Arc<Store> {
        let store =
            Arc::new(Store::open(self.dir.path().join("data").join(DB_FILE_NAME)).expect("store"));
        store.migrate().expect("migrate");
        store
    }

    fn secrets(&self) -> Arc<CredentialSecretRepository> {
        Arc::new(CredentialSecretRepository::with_gate(
            self.credentials.clone(),
            self.store(),
        ))
    }
}

fn proton_script(challenge: AuthChallenge) -> FakeAuthScript {
    FakeAuthScript::password("proton-account-password")
        .provider(ProviderId::from(PROTON_PROVIDER))
        .challenge(challenge)
}

fn verification_prompt() -> AuthChallenge {
    AuthChallenge::HumanVerification(pige_core::auth::HumanChallenge {
        instructions: "https://verify.example/proton".to_string(),
        expires_at_ms: i64::MAX,
    })
}

fn provisional_proton(authenticated: &mut pige_core::auth::AuthenticatedAccount) {
    authenticated.account.email = "alice@proton.me".to_string();
    authenticated.account.label = "Proton".to_string();
    authenticated.account.display_name = "Alice".to_string();
    authenticated.account.backend = MailBackend::proton(Some("proton-user".to_string()));
}

async fn begin_proton(account: Option<AccountId>) -> AuthProgress {
    api::begin_auth_with_username(
        ProviderId::from(PROTON_PROVIDER),
        "alice@proton.me".to_string(),
        account,
        true,
    )
    .await
    .expect("begin Proton sign-in with a username")
}

#[tokio::test]
async fn cancel_verification_stores_no_account() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new(proton_script(verification_prompt()));

    let AuthProgress::Challenge {
        attempt,
        challenge: AuthChallenge::HumanVerification(_),
    } = begin_proton(None).await
    else {
        panic!("expected the human-verification challenge");
    };
    assert!(api::snapshot().accounts.is_empty());
    assert!(
        harness.store().pending_auth(&attempt).unwrap().is_some(),
        "the attempt reserves a pending-auth row without creating an account"
    );

    api::cancel_auth(&attempt)
        .await
        .expect("cancel verification");

    assert!(
        api::snapshot().accounts.is_empty(),
        "cancelling verification must not create an account"
    );
    assert!(
        harness.store().pending_auth(&attempt).unwrap().is_none(),
        "cancel removes the pending-auth row"
    );
    assert!(
        harness
            .secrets()
            .load_staged(&pige_core::auth::StagedSecretRef::for_attempt(&attempt).unwrap())
            .unwrap()
            .is_none(),
        "cancel discards the staged credential"
    );
}

#[tokio::test]
async fn config_restore_requires_secret_reauth() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new(proton_script(AuthChallenge::Password {
        username: Some("alice@proton.me".to_string()),
    }));

    let account = Account {
        id: AccountId::new(),
        label: "Restored Proton".to_string(),
        email: "restored@proton.me".to_string(),
        display_name: "Restored".to_string(),
        backend: MailBackend::proton(Some("proton-user".to_string())),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    };
    let document = pige_core::config::ConfigDocument {
        accounts: vec![account.clone()],
        ..pige_core::config::ConfigDocument::default()
    };
    let bytes = serde_json::to_vec_pretty(&document).expect("backup document");
    let backup = "pige-backup-2026-11-10-120000.json";
    harness.backups.write(backup, &bytes).expect("write backup");

    let outcome = api::restore_config(backup.to_string())
        .await
        .expect("restore Proton configuration");
    assert!(
        outcome
            .snapshot
            .accounts
            .iter()
            .any(|candidate| candidate.id == account.id)
    );

    assert!(
        harness
            .credentials
            .get(&account.id)
            .expect("read credential store")
            .is_none(),
        "a configuration backup never carries a Proton token"
    );
    assert!(
        harness.secrets().load(&account.id).unwrap().is_none(),
        "restore invents no active Proton credential"
    );
    let error = api::resume_auth(&account.id)
        .await
        .expect_err("restored Proton config needs a fresh sign-in");
    assert!(
        error.to_lowercase().contains("sign in")
            || error.to_lowercase().contains("sign-in")
            || error.to_lowercase().contains("session"),
        "the reauthentication error must be actionable: {error}"
    );
}

#[tokio::test]
async fn removed_account_late_login_is_discarded() {
    let _guard = TEST_LOCK.lock().await;
    let _harness = Harness::new(proton_script(AuthChallenge::Password {
        username: Some("alice@proton.me".to_string()),
    }));

    // Create the Proton account first, using the same username-first path a new
    // account uses in the app.
    let AuthProgress::Challenge { attempt, .. } = begin_proton(None).await else {
        panic!("expected a password challenge");
    };
    let AuthProgress::Complete(mut authenticated) = api::answer_auth(
        &attempt,
        pige_core::auth::AuthAnswer::Password(pige_core::auth::secrecy::SecretString::from(
            "proton-account-password",
        )),
    )
    .await
    .expect("complete Proton sign-in") else {
        panic!("expected completion");
    };
    provisional_proton(&mut authenticated);
    let (id, _) = api::finish_auth(authenticated)
        .await
        .expect("commit Proton account");

    // Start a re-login, then remove the account before its completion is
    // committed.
    let AuthProgress::Challenge { attempt, .. } = begin_proton(Some(id.clone())).await else {
        panic!("expected a password challenge");
    };
    let AuthProgress::Complete(authenticated) = api::answer_auth(
        &attempt,
        pige_core::auth::AuthAnswer::Password(pige_core::auth::secrecy::SecretString::from(
            "proton-account-password",
        )),
    )
    .await
    .expect("complete re-login") else {
        panic!("expected completion");
    };
    api::remove_account(&id).await.expect("remove account");
    let error = api::finish_auth(authenticated)
        .await
        .expect_err("a removed account's late login must be discarded");
    assert!(
        error.to_lowercase().contains("no longer")
            || error.to_lowercase().contains("removed")
            || error.to_lowercase().contains("start again"),
        "late login must report a discarded attempt: {error}"
    );
    assert!(
        api::snapshot().accounts.is_empty(),
        "a late login must not recreate the removed account"
    );
}

#[tokio::test]
async fn proton_contacts_are_registered_when_enabled() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new(proton_script(AuthChallenge::Password {
        username: Some("alice@proton.me".to_string()),
    }));

    let AuthProgress::Challenge { attempt, .. } = begin_proton(None).await else {
        panic!("expected a password challenge");
    };
    let AuthProgress::Complete(mut authenticated) = api::answer_auth(
        &attempt,
        pige_core::auth::AuthAnswer::Password(pige_core::auth::secrecy::SecretString::from(
            "proton-account-password",
        )),
    )
    .await
    .expect("complete Proton sign-in") else {
        panic!("expected completion");
    };
    provisional_proton(&mut authenticated);
    let (id, _) = api::finish_auth_with_contacts(authenticated, true)
        .await
        .expect("commit Proton account with contacts");

    let account = api::account(&id).expect("created account");
    assert_eq!(
        account.contact_sources,
        pige_core::account::proton_contact_sources(),
        "the Proton contact source is attached on opt-in"
    );
    let states = harness.store().contact_source_states(&id).expect("states");
    assert!(
        states.iter().any(|(source, state)| {
            source.as_str() == pige_core::account::PROTON_CONTACTS_SOURCE_ID && state.enabled
        }),
        "the durable contact source state is registered: {states:?}"
    );
}

#[tokio::test]
async fn proton_factory_advertises_only_proven_capabilities() {
    let dir = TempDir::new().expect("temp dir");
    let credentials = Arc::new(FakeCredentials::default());
    let store =
        Arc::new(Store::open(dir.path().join("data").join(DB_FILE_NAME)).expect("provider store"));
    store.migrate().expect("migrate provider store");
    let sessions = Arc::new(pige_core::auth::AccountSessionManager::new(store.clone()));
    let secrets = Arc::new(CredentialSecretRepository::with_gate(
        credentials,
        store.clone(),
    ));
    let factory = ProtonProviderFactory::new(
        sessions.clone(),
        secrets,
        ProtonEndpoints::production(),
        Arc::new(ProtonKeyStores::new()),
    );
    let account = Account {
        id: AccountId::new(),
        label: "Proton".to_string(),
        email: "alice@proton.me".to_string(),
        display_name: "Alice".to_string(),
        backend: MailBackend::proton(Some("proton-user".to_string())),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    };
    let ctx = sessions
        .context(&account.id, Duration::from_secs(10))
        .expect("operation context");
    let session = factory.open(&account, &ctx).await.expect("open session");
    let expected = ProviderCapabilities {
        mailbox: true,
        sync: true,
        mutations: pige_core::providers::MutationCapabilities {
            read: true,
            star: true,
            labels: true,
            move_messages: true,
            archive: true,
            trash: true,
            permanent_delete: true,
        },
        submission: true,
        contacts: pige_core::providers::ContactCapabilities {
            read: true,
            incremental_sync: false,
            remote_search: false,
            write: false,
            groups: false,
        },
        // Proton has no server-side search API used by this adapter.
        server_search: false,
    };
    assert_eq!(session.capabilities, expected);
    assert!(session.mailbox.is_some());
    assert!(session.sync.is_some());
    assert!(session.mutations.is_some());
    assert!(session.submission.is_some());
    assert!(session.address_book.is_some());

    // An IMAP-shaped account is never served by the Proton factory.
    let imap = Account {
        backend: MailBackend::imap_smtp(
            "alice@proton.me",
            ServerConfig {
                host: "imap.example".to_string(),
                port: 993,
                security: Security::Tls,
            },
            ServerConfig {
                host: "smtp.example".to_string(),
                port: 465,
                security: Security::Tls,
            },
        ),
        ..account
    };
    let ctx = OperationContext::with_timeout(imap.id.clone(), 1, Duration::from_secs(1));
    assert!(factory.open(&imap, &ctx).await.is_err());
}
