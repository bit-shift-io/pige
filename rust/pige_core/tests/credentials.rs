//! Account onboarding: provider presets, input validation, credential storage,
//! and account removal. The API is process-global, so the tests share one async
//! lock and re-initialize the API for each case.

use std::sync::Arc;

use pige_core::{
    AccountId, Address, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId, Security,
    ServerConfig, Store, StoredMessage, ThreadId, Uid,
    account::{
        Account, AuthKind,
        presets::{ProviderPreset, preset},
    },
    api::{self, AccountInput, Config, DB_FILE_NAME},
    platform::{
        CredentialStore, credentials_unavailable,
        fake::{FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    },
    transport::{MailTransport, imap},
};
use tempfile::TempDir;
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

type ClearField = fn(&mut AccountInput);

struct Harness {
    dir: TempDir,
    credentials: Arc<FakeCredentials>,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let credentials = Arc::new(FakeCredentials::default());

        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            credentials: credentials.clone(),
            notifier: Arc::new(FakeNotifier::default()),
            opener: Arc::new(FakeOpener::default()),
            transport: Arc::new(FakeTransportFactory::default()),
        })
        .expect("init api");

        Self { dir, credentials }
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.dir.path().join("data").join(DB_FILE_NAME)
    }

    fn open_store(&self) -> Store {
        Store::open(self.db_path()).expect("open store")
    }
}

fn input(label: &str, email: &str) -> AccountInput {
    AccountInput {
        label: label.to_string(),
        email: email.to_string(),
        display_name: label.to_string(),
        username: email.to_string(),
        password: format!("secret-{label}"),
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
    }
}

fn seed_account_data(store: &Store, owner: &AccountId, tag: &str) {
    let folder = Folder {
        id: FolderId(format!("{tag}-inbox")),
        account_id: owner.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 1,
        total: 1,
        uidvalidity: Some(1),
        uidnext: Some(2),
    };
    store.upsert_folder(&folder).unwrap();

    let message = StoredMessage {
        id: MessageId(format!("{tag}-message")),
        account_id: owner.clone(),
        folder_id: folder.id.clone(),
        uid: Uid(1),
        uidvalidity: 1,
        header: MessageHeader {
            message_id: Some(format!("<{tag}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: format!("{tag} subject"),
            from: vec![Address::new(None, format!("{tag}@example.com"))],
            to: Vec::new(),
            cc: Vec::new(),
            date: 1,
            size: 64,
        },
        flags: Flags::default(),
        thread_id: ThreadId(format!("{tag}-thread")),
        snippet: format!("{tag} snippet"),
        has_attachments: false,
    };
    store.upsert_message(&message).unwrap();
    store
        .upsert_body(
            &message.id,
            &pige_core::store::Body {
                plain_text: Some(format!("{tag} searchable body")),
                html: None,
                fetched_at: 1,
            },
        )
        .unwrap();
}

#[test]
fn gmail_preset_uses_993_and_465() {
    let defaults = preset(ProviderPreset::Gmail);
    assert_eq!(
        (defaults.imap_host, defaults.imap_port),
        ("imap.gmail.com", 993)
    );
    assert_eq!(
        (defaults.smtp_host, defaults.smtp_port),
        ("smtp.gmail.com", 465)
    );
    assert_eq!(defaults.security, Security::Tls);
}

#[test]
fn fastmail_and_manual_presets_are_available() {
    let fastmail = preset(ProviderPreset::Fastmail);
    assert_eq!(
        (fastmail.imap_host, fastmail.imap_port),
        ("imap.fastmail.com", 993)
    );
    assert_eq!(
        (fastmail.smtp_host, fastmail.smtp_port),
        ("smtp.fastmail.com", 465)
    );
    assert_eq!(fastmail.security, Security::Tls);

    let manual = preset(ProviderPreset::Manual);
    assert!(manual.imap_host.is_empty());
    assert!(manual.smtp_host.is_empty());
    assert_eq!((manual.imap_port, manual.smtp_port), (0, 0));
}

#[test]
fn account_validation_requires_hosts_user_and_password() {
    let mut valid = input("Personal", "ada@example.com");
    assert!(valid.validate().is_ok());

    let cases: [(&str, ClearField); 7] = [
        ("label", |value| value.label.clear()),
        ("email", |value| value.email.clear()),
        ("username", |value| value.username.clear()),
        ("password", |value| value.password.clear()),
        ("IMAP host", |value| value.imap.host.clear()),
        ("SMTP host", |value| value.smtp.host.clear()),
        ("IMAP port", |value| value.imap.port = 0),
    ];

    for (field, clear) in cases {
        let mut value = valid.clone();
        clear(&mut value);
        let error = value.validate().expect_err("invalid input must fail");
        assert!(
            error
                .to_ascii_lowercase()
                .contains(&field.to_ascii_lowercase()),
            "error for {field:?} should name the field, got {error:?}"
        );
    }

    valid.email = "not-an-address".to_string();
    assert!(valid.validate().is_err());
}

#[test]
fn fake_credentials_round_trip_and_delete() {
    let credentials = FakeCredentials::new();
    let id = AccountId::new();

    assert_eq!(credentials.get(&id).unwrap(), None);
    credentials.set(&id, "hunter2").unwrap();
    assert_eq!(credentials.get(&id).unwrap().as_deref(), Some("hunter2"));
    credentials.delete(&id).unwrap();
    assert_eq!(credentials.get(&id).unwrap(), None);
    assert_eq!(credentials.deleted(), vec![id]);
}

#[tokio::test]
async fn removing_an_account_cascades_mail_and_credentials() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();

    let first = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("first account");
    let first_id = first.accounts[0].id.clone();
    let second = api::add_account(input("Work", "grace@example.com"))
        .await
        .expect("second account");
    let second_id = second.accounts[1].id.clone();

    let store = harness.open_store();
    seed_account_data(&store, &first_id, "personal");
    seed_account_data(&store, &second_id, "work");

    let snapshot = api::remove_account(&first_id)
        .await
        .expect("remove account");

    assert_eq!(snapshot.accounts.len(), 1);
    assert_eq!(snapshot.accounts[0].id, second_id);
    assert!(snapshot.sections.iter().all(|s| s.account_id != first_id));

    // Credentials are gone, and the removal is reported to the store.
    assert_eq!(harness.credentials.get(&first_id).unwrap(), None);
    assert!(harness.credentials.deleted().contains(&first_id));
    assert_eq!(
        harness.credentials.get(&second_id).unwrap().as_deref(),
        Some("secret-Work")
    );

    let store = harness.open_store();
    assert!(store.folders(&first_id).unwrap().is_empty());
    assert!(
        store
            .messages(&FolderId("personal-inbox".into()), 10, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .messages(&FolderId("work-inbox".into()), 10, None)
            .unwrap()
            .len()
            == 1
    );
    assert!(
        store
            .body(&MessageId("personal-message".into()))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .body(&MessageId("work-message".into()))
            .unwrap()
            .is_some()
    );

    // The full-text index cascaded with the messages.
    let hits = store
        .search("personal", &pige_core::search::SearchScope::AllAccounts, 10)
        .unwrap();
    assert!(hits.is_empty(), "removed mail must leave the search index");
    let hits = store
        .search("work", &pige_core::search::SearchScope::AllAccounts, 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn remove_unknown_account_is_an_error_and_keeps_other_credentials() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("account");
    let id = added.accounts[0].id.clone();

    let error = api::remove_account(&AccountId("missing".into()))
        .await
        .expect_err("unknown account");
    assert!(error.contains("unknown account"));
    assert!(harness.credentials.secret(&id).is_some());
}

#[tokio::test]
async fn keyring_unavailable_uses_a_session_only_password() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    harness.credentials.fail_unavailable();

    let error = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect_err("persistent credential store is unavailable");
    assert!(
        credentials_unavailable(&error),
        "expected a typed unavailable error, got {error:?}"
    );
    assert!(api::snapshot().accounts.is_empty());

    let snapshot = api::add_account_with_session(input("Personal", "ada@example.com"))
        .await
        .expect("session-only account");
    let id = snapshot.accounts[0].id.clone();
    assert_eq!(
        harness.credentials.get(&id).unwrap().as_deref(),
        Some("secret-Personal"),
        "the session store must still authenticate this process"
    );
    assert!(
        !contains_bytes(
            &std::fs::read(harness.db_path()).unwrap(),
            b"secret-Personal"
        ),
        "session passwords must never reach SQLite"
    );

    api::remove_account(&id).await.expect("remove account");
    assert_eq!(harness.credentials.get(&id).unwrap(), None);
}

#[tokio::test]
async fn edit_and_rename_keep_the_account_identity_and_password() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let added = api::add_account(input("Personal", "ada@example.com"))
        .await
        .expect("account");
    let id = added.accounts[0].id.clone();

    let renamed = api::rename_account(&id, "Home")
        .await
        .expect("rename account");
    assert_eq!(renamed.accounts[0].label, "Home");

    let mut changed = input("Home", "ada@example.com");
    changed.password.clear();
    changed.imap.host = "mail.example.com".to_string();
    let snapshot = api::update_account(&id, changed)
        .await
        .expect("update account");
    assert_eq!(snapshot.accounts[0].id, id);
    assert_eq!(snapshot.accounts[0].label, "Home");
    assert_eq!(
        harness.credentials.get(&id).unwrap().as_deref(),
        Some("secret-Personal"),
        "an empty edit password keeps the stored secret"
    );
}

#[tokio::test]
async fn test_account_rejects_invalid_input_before_connecting() {
    let _guard = TEST_LOCK.lock().await;
    let _harness = Harness::new();

    let mut value = input("Personal", "ada@example.com");
    value.smtp.host.clear();
    let error = api::test_account(&value)
        .await
        .expect_err("invalid input must not connect");
    assert!(error.to_ascii_lowercase().contains("smtp host"));
}

/// Real-server smoke test. Set `PIGE_TEST_IMAP_HOST`, `PIGE_TEST_IMAP_USER`,
/// and `PIGE_TEST_IMAP_PASSWORD` (optionally `PIGE_TEST_IMAP_PORT`, default 993)
/// to exercise TLS plus password authentication against a live server.
#[tokio::test]
async fn gated_real_imap_connection() {
    let (Ok(host), Ok(user), Ok(password)) = (
        std::env::var("PIGE_TEST_IMAP_HOST"),
        std::env::var("PIGE_TEST_IMAP_USER"),
        std::env::var("PIGE_TEST_IMAP_PASSWORD"),
    ) else {
        return;
    };
    let port = std::env::var("PIGE_TEST_IMAP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(993);

    let account = Account {
        id: AccountId::new(),
        label: "Live".to_string(),
        email: user.clone(),
        display_name: "Live".to_string(),
        username: user,
        imap: ServerConfig {
            host,
            port,
            security: Security::Tls,
        },
        smtp: ServerConfig {
            host: "smtp.invalid".to_string(),
            port: 465,
            security: Security::Tls,
        },
        auth: AuthKind::Password,
        sort_order: 0,
    };

    let mut transport = imap::connect(&account, &password)
        .await
        .expect("real IMAP login");
    transport
        .capabilities()
        .await
        .expect("real IMAP capabilities");
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
