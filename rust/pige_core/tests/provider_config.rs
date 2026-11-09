//! Versioned backend account configuration.
//!
//! The configuration document moved from v1 (protocol fields inline on each
//! account) to v2 (a typed [`MailBackend`] plus contact sources). These tests
//! pin the migration contract:
//!
//! - a v1 document round-trips its ids and endpoints exactly;
//! - a native account needs no IMAP endpoint and is never given a fake one;
//! - a document from a newer build is rejected before anything is saved;
//! - restoring a v1 document yields no contacts or tokens;
//! - a Gmail-over-IMAP account stays an IMAP account (no silent conversion);
//! - the SQLite account mirror matches the JSON backend.

use std::sync::Arc;

use pige_core::Store;
use pige_core::account::{
    Account, AccountId, AuthKind, ContactSourceConfig, MailBackend, Security, ServerConfig,
};
use pige_core::api::{self, Config, DB_FILE_NAME};
use pige_core::config::{self, CURRENT_VERSION, ConfigDocument, ConfigError};
use pige_core::platform::BackupStore;
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::providers::{ContactSourceId, ProviderId};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Serializes the tests that install the process-wide API state.
static TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// A v1 document with a custom host, security, username, and colour, so the
/// migration cannot pass by falling back to defaults.
const V1_DOCUMENT: &str = r##"{
  "version": 1,
  "accounts": [
    {
      "id": "acct-1",
      "label": "Personal",
      "email": "ada@example.com",
      "display_name": "Ada Lovelace",
      "username": "ada-user",
      "imap": { "host": "imap.custom.example", "port": 1993, "security": "StartTls" },
      "smtp": { "host": "smtp.custom.example", "port": 2587, "security": "Tls" },
      "auth": "Password",
      "sort_order": 4,
      "color": "#c2185b"
    }
  ],
  "settings": {},
  "blocked_senders": []
}"##;

/// A v1 Gmail-over-IMAP document: the servers are Gmail's, but the account was
/// configured manually and must not become a native Gmail account.
const V1_GMAIL_IMAP: &str = r#"{
  "version": 1,
  "accounts": [
    {
      "id": "gmail-imap-1",
      "label": "Gmail",
      "email": "ada@gmail.com",
      "display_name": "Ada",
      "username": "ada@gmail.com",
      "imap": { "host": "imap.gmail.com", "port": 993, "security": "Tls" },
      "smtp": { "host": "smtp.gmail.com", "port": 465, "security": "Tls" },
      "auth": "Password",
      "sort_order": 0
    }
  ],
  "settings": {}
}"#;

fn server(host: &str, port: u16, security: Security) -> ServerConfig {
    ServerConfig {
        host: host.to_string(),
        port,
        security,
    }
}

fn imap_account(id: &str, host: &str, smtp_host: &str, username: &str, sort_order: i64) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: "Personal".to_string(),
        email: "ada@example.com".to_string(),
        display_name: "Ada".to_string(),
        backend: MailBackend::ImapSmtp {
            username: username.to_string(),
            imap: server(host, 993, Security::Tls),
            smtp: server(smtp_host, 465, Security::Tls),
            auth: AuthKind::Password,
        },
        contact_sources: Vec::new(),
        sort_order,
        color: None,
    }
}

fn gmail_account(id: &str, sort_order: i64) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: "Gmail".to_string(),
        email: "ada@gmail.com".to_string(),
        display_name: "Ada".to_string(),
        backend: MailBackend::gmail("google-uid-123"),
        contact_sources: Vec::new(),
        sort_order,
        color: None,
    }
}

#[test]
fn v1_round_trip_preserves_account_ids_and_endpoints() {
    let dir = TempDir::new().unwrap();
    let path = config::document_path(dir.path());
    std::fs::write(&path, V1_DOCUMENT).unwrap();

    let loaded = config::load(&path).unwrap().expect("document");
    assert_eq!(loaded.version, CURRENT_VERSION);
    assert_eq!(loaded.accounts.len(), 1);
    let account = loaded.accounts[0].clone();

    assert_eq!(account.id, AccountId("acct-1".to_string()));
    assert_eq!(account.label, "Personal");
    assert_eq!(account.email, "ada@example.com");
    assert_eq!(account.display_name, "Ada Lovelace");
    assert_eq!(account.sort_order, 4);
    assert_eq!(account.color.as_deref(), Some("#c2185b"));

    let (username, imap, smtp, auth) = account.imap_smtp().expect("migrated to IMAP/SMTP");
    assert_eq!(username, "ada-user");
    assert_eq!(auth, AuthKind::Password);
    assert_eq!(imap.host, "imap.custom.example");
    assert_eq!(imap.port, 1993);
    assert_eq!(imap.security, Security::StartTls);
    assert_eq!(smtp.host, "smtp.custom.example");
    assert_eq!(smtp.port, 2587);
    assert_eq!(smtp.security, Security::Tls);
    assert_eq!(account.provider_id(), ProviderId::from("imap-smtp"));

    // Saving writes v2; loading it back must reproduce the account exactly.
    config::save(&path, &loaded).unwrap();
    let reloaded = config::load(&path).unwrap().expect("document");
    assert_eq!(reloaded.version, CURRENT_VERSION);
    assert_eq!(reloaded.accounts, vec![account]);
}

#[test]
fn native_account_needs_no_imap_endpoint() {
    let dir = TempDir::new().unwrap();
    let path = config::document_path(dir.path());
    let account = gmail_account("native-1", 0);
    let mut doc = ConfigDocument::default();
    doc.accounts.push(account.clone());
    config::save(&path, &doc).unwrap();

    // The document must not invent an IMAP or SMTP endpoint for a native
    // backend.
    let text = std::fs::read_to_string(&path).unwrap();
    for forbidden in [
        "imap_host",
        "imap.gmail.com",
        "smtp_host",
        "\"imap\"",
        "\"smtp\"",
    ] {
        assert!(
            !text.contains(forbidden),
            "document contains {forbidden}:\n{text}"
        );
    }

    assert!(account.imap_smtp().is_none());
    assert_eq!(account.provider_id(), ProviderId::from("gmail"));

    let loaded = config::load(&path).unwrap().expect("document");
    assert_eq!(loaded.accounts, vec![account]);
}

#[test]
fn newer_config_is_rejected_before_save() {
    let dir = TempDir::new().unwrap();
    let path = config::document_path(dir.path());
    let newer = r#"{"version": 3, "accounts": [], "settings": {}, "blocked_senders": []}"#;
    std::fs::write(&path, newer).unwrap();

    let error = config::load(&path).expect_err("a newer format is rejected");
    match error {
        ConfigError::TooNew { found, supported } => {
            assert_eq!(found, 3);
            assert_eq!(supported, CURRENT_VERSION);
        }
        other => panic!("expected ConfigError::TooNew, got {other:?}"),
    }

    // Rejected, not reinterpreted: the newer file is left byte-for-byte intact.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
}

#[tokio::test]
async fn restore_v1_has_no_contacts_or_tokens() {
    let _guard = TEST_LOCK.lock().await;
    let dir = TempDir::new().unwrap();
    let backups = Arc::new(FakeBackupStore::new());
    let name = "pige-backup-2026-01-01-000000.json";
    backups.write(name, V1_DOCUMENT.as_bytes()).unwrap();

    let credentials = Arc::new(FakeCredentials::default());
    let transport: Arc<dyn pige_core::platform::TransportFactory> =
        Arc::new(FakeTransportFactory::default());
    api::reset_for_tests();
    api::init(Config {
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
        config_dir: dir.path().join("config"),
        providers: Arc::new(pige_core::providers::imap::imap_registry(
            transport.clone(),
            credentials.clone(),
        )),
        credentials,
        notifier: Arc::new(FakeNotifier::default()),
        opener: Arc::new(FakeOpener::default()),
        transport,
        backups: backups.clone(),
    })
    .expect("init api");

    api::restore_config(name.to_string())
        .await
        .expect("restore v1 backup");

    let account = api::account(&AccountId("acct-1".to_string())).expect("restored account");
    assert_eq!(account.provider_id(), ProviderId::from("imap-smtp"));
    assert!(account.imap_smtp().is_some());
    assert!(
        account.contact_sources.is_empty(),
        "a restored v1 account must have no contacts"
    );
    assert!(matches!(account.backend, MailBackend::ImapSmtp { .. }));

    // The document written after restore is v2 and still carries no contacts
    // or token material for the account.
    let path = config::document_path(&dir.path().join("config"));
    let text = std::fs::read_to_string(path).unwrap();
    assert!(
        text.contains("\"contact_sources\": []"),
        "no contacts expected: {text}"
    );
    assert!(
        !text.contains("\"enabled\""),
        "no contact sources expected: {text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("token"),
        "no tokens expected: {text}"
    );

    api::reset_for_tests();
}

#[test]
fn gmail_imap_stays_imap() {
    let dir = TempDir::new().unwrap();
    let path = config::document_path(dir.path());
    std::fs::write(&path, V1_GMAIL_IMAP).unwrap();

    let loaded = config::load(&path).unwrap().expect("document");
    let account = &loaded.accounts[0];

    assert_eq!(account.provider_id(), ProviderId::from("imap-smtp"));
    assert!(matches!(account.backend, MailBackend::ImapSmtp { .. }));
    let (username, imap, smtp, _auth) = account.imap_smtp().expect("stays IMAP");
    assert_eq!(username, "ada@gmail.com");
    assert_eq!(imap.host, "imap.gmail.com");
    assert_eq!(smtp.host, "smtp.gmail.com");
    assert!(account.contact_sources.is_empty());
}

#[test]
fn store_mirror_matches_json_backend() {
    let dir = TempDir::new().unwrap();
    let path = config::document_path(dir.path());

    let mut proton = gmail_account("mirror-proton", 2);
    proton.email = "ada@proton.me".to_string();
    proton.backend = MailBackend::proton(None);
    proton.contact_sources = vec![ContactSourceConfig {
        id: ContactSourceId::from("src-1"),
        provider: ProviderId::from("proton"),
        kind: "personal".to_string(),
        enabled: true,
    }];

    let mut doc = ConfigDocument::default();
    doc.accounts.push(imap_account(
        "mirror-imap",
        "imap.custom.example",
        "smtp.custom.example",
        "ada-user",
        0,
    ));
    doc.accounts.push(gmail_account("mirror-gmail", 1));
    doc.accounts.push(proton);
    config::save(&path, &doc).unwrap();
    let loaded = config::load(&path).unwrap().expect("document");

    let store = Store::open(dir.path().join(DB_FILE_NAME)).expect("open store");
    store.migrate().expect("migrate store");
    store
        .sync_accounts(&loaded.accounts)
        .expect("mirror accounts");

    // The mirror reconstructs the same typed backend and contacts the JSON has.
    assert_eq!(store.accounts().unwrap(), loaded.accounts);
    let gmail = store
        .accounts()
        .unwrap()
        .into_iter()
        .find(|account| account.id == AccountId("mirror-gmail".to_string()))
        .expect("mirrored gmail account");
    assert!(gmail.imap_smtp().is_none());
    assert_eq!(gmail.provider_id(), ProviderId::from("gmail"));
}

#[test]
fn store_reads_pre_v5_account_rows_as_imap() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join(DB_FILE_NAME);
    let store = Store::open(&db).expect("open store");
    store.migrate().expect("migrate store");

    // Simulate a row written before v5: the legacy columns are populated and
    // there is no `backend` JSON. Reopening must reconstruct the IMAP backend
    // from those columns rather than losing it.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO accounts (
                 id, label, email, display_name, username,
                 imap_host, imap_port, imap_security,
                 smtp_host, smtp_port, smtp_security, auth_kind, sort_order
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                "legacy-1",
                "Legacy",
                "legacy@example.com",
                "Legacy",
                "legacy-user",
                "imap.legacy.example",
                143,
                "starttls",
                "smtp.legacy.example",
                587,
                "starttls",
                "password",
                0,
            ],
        )
        .unwrap();
    }

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    let account = &accounts[0];
    assert_eq!(account.provider_id(), ProviderId::from("imap-smtp"));
    assert!(account.contact_sources.is_empty());
    let (username, imap, smtp, auth) = account.imap_smtp().expect("legacy IMAP backend");
    assert_eq!(username, "legacy-user");
    assert_eq!(imap.host, "imap.legacy.example");
    assert_eq!(imap.port, 143);
    assert_eq!(imap.security, Security::StartTls);
    assert_eq!(smtp.host, "smtp.legacy.example");
    assert_eq!(smtp.port, 587);
    assert_eq!(auth, AuthKind::Password);
}
