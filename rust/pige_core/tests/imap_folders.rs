//! Folder classification and (opt-in) live IMAP integration tests.
//!
//! The classification tests run in the default suite. The live test is
//! `#[ignore]`d so `cargo test` never touches the network; run it explicitly
//! with `PIGE_TEST_IMAP_HOST`, `PIGE_TEST_IMAP_USER`, and
//! `PIGE_TEST_IMAP_PASSWORD` set.

use std::env;

use pige_core::{
    Account, AccountId, AuthKind, FolderKind, Security, ServerConfig,
    transport::{
        MailTransport,
        imap::{classify_folder, connect},
    },
};

#[test]
fn special_use_attributes_win() {
    assert_eq!(classify_folder("Anything", &["\\Inbox"]), FolderKind::Inbox);
    assert_eq!(classify_folder("Sent", &["\\Sent"]), FolderKind::Sent);
    assert_eq!(classify_folder("Bin", &["\\Trash"]), FolderKind::Trash);
}

#[test]
fn name_fallbacks_cover_gmail_and_generic_servers() {
    assert_eq!(
        classify_folder("[Gmail]/All Mail", &[]),
        FolderKind::Archive
    );
    assert_eq!(classify_folder("[Gmail]/Sent Mail", &[]), FolderKind::Sent);
    assert_eq!(classify_folder("Junk", &[]), FolderKind::Junk);
    assert_eq!(classify_folder("Projects", &[]), FolderKind::Other);
}

/// Live smoke test: connect, list folders, and require at least one result.
/// Skipped unless the three `PIGE_TEST_IMAP_*` variables are set.
#[tokio::test]
#[ignore]
async fn lists_folders_against_a_real_server() {
    let host = env::var("PIGE_TEST_IMAP_HOST").expect("PIGE_TEST_IMAP_HOST");
    let user = env::var("PIGE_TEST_IMAP_USER").expect("PIGE_TEST_IMAP_USER");
    let password = env::var("PIGE_TEST_IMAP_PASSWORD").expect("PIGE_TEST_IMAP_PASSWORD");

    let account = Account {
        id: AccountId("imap-live".to_string()),
        label: "Live".to_string(),
        email: user.clone(),
        display_name: String::new(),
        username: user.clone(),
        imap: ServerConfig {
            host,
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerConfig {
            host: String::new(),
            port: 465,
            security: Security::Tls,
        },
        auth: AuthKind::Password,
        sort_order: 0,
    };

    let mut transport = connect(&account, &password)
        .await
        .expect("connect to server");

    let folders = transport.list_folders().await.expect("list folders");
    assert!(!folders.is_empty(), "expected at least one folder");
}
