//! Task G5: explicitly requested live Google probes.
//!
//! Nothing here runs by default. Every case is `#[ignore]` and, when selected
//! with `--ignored`, requires its own environment variables. A missing value is
//! a hard failure: an explicitly requested live run must never report success
//! without touching Google. Send additionally requires `PIGE_TEST_ALLOW_SEND=1`.
//!
//! Run one case (never in CI):
//!
//! ```text
//! PIGE_TEST_GOOGLE_CLIENT_ID=... \
//! PIGE_TEST_GOOGLE_REFRESH_TOKEN=... \
//! PIGE_TEST_GOOGLE_ACCOUNT_EMAIL=person@example.com \
//! cargo test -p pige_core --features provider-google --test live_google \
//!     -- --ignored --nocapture live_gmail_read_and_sync
//! ```
//!
//! `PIGE_TEST_GOOGLE_CLIENT_ID` and `PIGE_TEST_GOOGLE_REFRESH_TOKEN` must belong
//! to a disposable test account. A token without the contacts scopes exercises
//! the denial path; `PIGE_TEST_ALLOW_SEND=1` is the only way the send case runs.
//! Access tokens and refresh tokens are never printed.

#![cfg(feature = "provider-google")]

use std::time::Duration;

use async_trait::async_trait;
use secrecy::SecretString;

use pige_core::account::{Account, AccountId, MailBackend};
use pige_core::auth::TokenSource;
use pige_core::mail::Address;
use pige_core::providers::google::{
    GOOGLE_SAVED_CONTACTS_SOURCE, GmailMailbox, GmailSubmission, GmailSync, GoogleAddressBook,
    GoogleClient, GoogleEndpoints,
};
use pige_core::providers::{
    AddressBookService, ErrorKind, MailboxService, OperationContext, OperationId, ProviderError,
    SubmissionOutcome, SubmissionRequest, SubmissionService, SyncRequest, SyncService,
};
use pige_core::send::OutgoingMessage;
use tokio::time::Instant;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

struct EnvTokenSource {
    access_token: SecretString,
}

#[async_trait]
impl TokenSource for EnvTokenSource {
    async fn access_token(&self, _ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        Ok(self.access_token.clone())
    }
}

fn required(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => panic!(
            "live Google test requested but {name} is not set. \
             Export the disposable test-account values, or do not run --ignored."
        ),
    }
}

fn context(account: &AccountId) -> OperationContext {
    OperationContext::new(
        account.clone(),
        1,
        Instant::now() + Duration::from_secs(120),
    )
}

/// Exchange a stored refresh token for a short-lived access token. The token
/// endpoint response is never logged; only a coarse error is reported.
async fn access_token() -> SecretString {
    let client_id = required("PIGE_TEST_GOOGLE_CLIENT_ID");
    let refresh_token = required("PIGE_TEST_GOOGLE_REFRESH_TOKEN");
    let response = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .expect("Google token endpoint is unreachable");
    let status = response.status();
    assert!(
        status.is_success(),
        "Google rejected the live refresh token (HTTP {status}); the response body is not printed"
    );
    let body: serde_json::Value = response
        .json()
        .await
        .expect("Google token response was not JSON");
    let token = body
        .get("access_token")
        .and_then(|value| value.as_str())
        .expect("Google token response carried no access token");
    SecretString::from(token.to_string())
}

async fn live_client(_account: &AccountId) -> GoogleClient {
    // The account email is needed for sendAs validation in the send case; the
    // read cases only need the token and the resolver identity `me`.
    let _ = required("PIGE_TEST_GOOGLE_ACCOUNT_EMAIL");
    GoogleClient::new(
        std::sync::Arc::new(EnvTokenSource {
            access_token: access_token().await,
        }),
        GoogleEndpoints::production(),
    )
}

#[tokio::test]
#[ignore = "live Google account: requires PIGE_TEST_GOOGLE_CLIENT_ID and PIGE_TEST_GOOGLE_REFRESH_TOKEN"]
async fn live_gmail_read_and_sync() {
    let account = AccountId::new();
    let client = live_client(&account).await;
    let ctx = context(&account);

    let mut mailbox = GmailMailbox::new(client.clone(), "me");
    let collections = mailbox.collections(&ctx).await.expect("read Gmail labels");
    assert!(
        collections
            .iter()
            .any(|collection| collection.kind == pige_core::FolderKind::Inbox),
        "a live Gmail account must expose INBOX"
    );

    let mut sync = GmailSync::new(client, "me");
    let page = sync
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 10,
            },
            &ctx,
        )
        .await
        .expect("read a Gmail sync page");
    assert!(
        page.has_more || !page.changes.is_empty(),
        "a live account must produce a resumable sync page"
    );
    println!(
        "live Gmail: {} collections, {} changes, has_more={}",
        collections.len(),
        page.changes.len(),
        page.has_more
    );
}

#[tokio::test]
#[ignore = "live Google account: requires PIGE_TEST_GOOGLE_CONTACTS_DENIED=1 and a token without contacts scopes"]
async fn live_contacts_denial_keeps_mail() {
    assert_eq!(
        required("PIGE_TEST_GOOGLE_CONTACTS_DENIED"),
        "1",
        "set PIGE_TEST_GOOGLE_CONTACTS_DENIED=1 only when the refresh token lacks contacts scopes"
    );
    let account = AccountId::new();
    let client = live_client(&account).await;
    let ctx = context(&account);

    // Mail remains readable under a mail-only grant.
    let mut mailbox = GmailMailbox::new(client.clone(), "me");
    mailbox
        .collections(&ctx)
        .await
        .expect("mail must keep working when contacts are denied");

    let mut people = GoogleAddressBook::new(client, "me");
    let source = pige_core::providers::ContactSourceId::from(GOOGLE_SAVED_CONTACTS_SOURCE);
    let error = people
        .next_page(&source, None, &ctx)
        .await
        .expect_err("a contacts-denied token must not read People");
    assert!(
        matches!(error.kind, ErrorKind::Forbidden | ErrorKind::Reauthenticate),
        "unexpected contacts-denial classification: {error:?}"
    );
    println!("live contacts denial classified as {:?}", error.kind);
}

#[tokio::test]
#[ignore = "live Google send: requires PIGE_TEST_ALLOW_SEND=1, PIGE_TEST_GOOGLE_CLIENT_ID, PIGE_TEST_GOOGLE_REFRESH_TOKEN and PIGE_TEST_GOOGLE_ACCOUNT_EMAIL"]
async fn live_gmail_send_requires_explicit_opt_in() {
    assert_eq!(
        required("PIGE_TEST_ALLOW_SEND"),
        "1",
        "sending mail is destructive; set PIGE_TEST_ALLOW_SEND=1 to authorize one live send"
    );
    let email = required("PIGE_TEST_GOOGLE_ACCOUNT_EMAIL");
    let account_id = AccountId::new();
    let client = live_client(&account_id).await;
    let ctx = context(&account_id);
    let account = Account {
        id: account_id.clone(),
        label: "Live Google".to_string(),
        email: email.clone(),
        display_name: email.clone(),
        backend: MailBackend::gmail("live"),
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    };
    let mut submission = GmailSubmission::new(client, account);
    let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
    let request = SubmissionRequest {
        operation: operation.clone(),
        message: OutgoingMessage {
            account_id,
            to: vec![Address::new(None, email.clone())],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Pige live send probe".to_string(),
            body: "This is a disposable live-test message sent by Pige.".to_string(),
            in_reply_to: None,
            references: Vec::new(),
        },
        attachments: Vec::new(),
        prepared: None,
    };
    submission
        .prepare(&request, &ctx)
        .await
        .expect("prepare the live message");
    match submission
        .submit(&request, &ctx)
        .await
        .expect("submit the live message")
    {
        SubmissionOutcome::Accepted(receipt) => {
            assert_eq!(receipt.operation, operation);
            println!(
                "live Gmail send accepted (remote message present: {})",
                receipt.remote_message.is_some()
            );
        }
        other => panic!("live Gmail send did not complete: {other:?}"),
    }
}
