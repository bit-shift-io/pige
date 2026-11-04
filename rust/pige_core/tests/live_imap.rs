//! Live mail smoke test: connect to a real IMAP server, list its folders, fetch
//! one header, and (opt-in) send one plain-text message over SMTP.
//!
//! This test is `#[ignore]`d so `cargo test` never touches the network. Run it
//! explicitly with the required environment variables set:
//!
//! ```text
//! PIGE_TEST_IMAP_HOST=imap.example.com
//! PIGE_TEST_IMAP_USER=ada@example.com
//! PIGE_TEST_IMAP_PASSWORD=app-password
//! PIGE_TEST_SMTP_HOST=smtp.example.com   # only required with PIGE_TEST_SEND=1
//! PIGE_TEST_TO=recipient@example.com      # only required with PIGE_TEST_SEND=1
//! PIGE_TEST_SEND=1                        # optional; enables the send step
//! ```
//!
//! Usage:
//! ```text
//! cd rust && cargo test -p pige_core --test live_imap -- --ignored --nocapture
//! ```
//!
//! When a required variable is missing the test fails with a message naming
//! every missing variable; it never silently passes.

use std::env;

use pige_core::{
    Account, AccountId, Address, AuthKind, Security, ServerConfig, Uid,
    send::{OutgoingMessage, deliver},
    transport::{
        MailTransport, UidRange,
        imap::connect as imap_connect,
        smtp::connect as smtp_connect,
    },
};

/// Return the values of `names`, or panic naming every variable that is unset
/// or empty. Collecting all of them means one run reports the whole set.
fn require(names: &[&str]) -> Vec<String> {
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| env::var(name).map(|value| value.trim().is_empty()).unwrap_or(true))
        .collect();
    assert!(
        missing.is_empty(),
        "missing required environment variable(s): {}",
        missing.join(", ")
    );
    names
        .iter()
        .map(|name| env::var(name).expect("checked above"))
        .collect()
}

fn live_account(user: &str, imap_host: &str, smtp_host: &str) -> Account {
    Account {
        id: AccountId("live-smoke".to_string()),
        label: "Live smoke".to_string(),
        email: user.to_string(),
        display_name: String::new(),
        username: user.to_string(),
        imap: ServerConfig {
            host: imap_host.to_string(),
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerConfig {
            host: smtp_host.to_string(),
            port: 465,
            security: Security::Tls,
        },
        auth: AuthKind::Password,
        sort_order: 0,
    }
}

#[tokio::test]
#[ignore = "live network test; requires PIGE_TEST_* env vars and --ignored"]
async fn live_imap_and_smtp_smoke() {
    let [imap_host, user, password] = require(&[
        "PIGE_TEST_IMAP_HOST",
        "PIGE_TEST_IMAP_USER",
        "PIGE_TEST_IMAP_PASSWORD",
    ])
    .try_into()
    .expect("three values");

    let account = live_account(&user, &imap_host, "");
    let mut transport = imap_connect(&account, &password)
        .await
        .expect("connect to IMAP server");

    let folders = transport.list_folders().await.expect("list folders");
    assert!(
        !folders.is_empty(),
        "server returned no folders; expected at least one"
    );
    println!("listed {} folders", folders.len());

    // Prefer INBOX; fall back to the first reported folder.
    let folder = folders
        .iter()
        .find(|folder| folder.kind == pige_core::FolderKind::Inbox)
        .unwrap_or(&folders[0]);
    let state = transport
        .select(folder)
        .await
        .expect("select folder for header fetch");
    println!(
        "selected {}: uidvalidity={} uidnext={} exists={}",
        folder.name, state.uidvalidity, state.uidnext, state.exists
    );

    if state.exists > 0 {
        // Fetch the newest few messages (bounded) so at least one header comes
        // back even when the mailbox has UID gaps.
        let newest = state.uidnext.saturating_sub(1).max(1);
        let range = UidRange {
            from: Uid(newest.saturating_sub(9).max(1)),
            to: Uid(newest),
        };
        let headers = transport
            .fetch_headers(range)
            .await
            .expect("fetch headers");
        assert!(
            !headers.is_empty(),
            "folder {} reports {} messages but returned no headers for UIDs {}:{}",
            folder.name,
            state.exists,
            range.from.0,
            range.to.0
        );
        let first = &headers[0];
        println!(
            "fetched {} header(s); first uid={} subject={:?}",
            headers.len(),
            first.uid.0,
            first.header.subject
        );
    } else {
        println!("folder {} is empty; skipping header fetch", folder.name);
    }

    // The send step is strictly opt-in.
    if env::var("PIGE_TEST_SEND").as_deref() != Ok("1") {
        println!("PIGE_TEST_SEND != 1; skipping SMTP send");
        return;
    }

    let [smtp_host, to] = require(&["PIGE_TEST_SMTP_HOST", "PIGE_TEST_TO"])
        .try_into()
        .expect("two values");
    let account = live_account(&user, &imap_host, &smtp_host);
    let mut smtp = smtp_connect(&account, &password)
        .await
        .expect("connect to SMTP server");

    let from = Address::new(None, user.clone());
    let message = OutgoingMessage {
        account_id: account.id.clone(),
        to: vec![Address::new(None, to.clone())],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "pige live smoke test".to_string(),
        body: "This is a one-off plain-text message from the pige live smoke test.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    };
    deliver(&mut smtp, &from, &message)
        .await
        .expect("send plain-text message");
    println!("sent a plain-text message to {to}");
}
