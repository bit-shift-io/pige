//! Contract tests for the provider-neutral boundary.
//!
//! These exercise the application-owned [`ProviderRegistry`]: dispatching by
//! `Account::provider_id()`, rejecting duplicate registrations, keeping
//! [`ProviderCursor`] values opaque, short-circuiting an already-cancelled
//! context before any network work, and never leaking adapter wire payloads
//! through a [`ProviderError`]'s `Display`/`Debug`.

use std::sync::Arc;
use std::time::Duration;

use pige_core::account::{Account, AccountId, AuthKind, MailBackend, Security, ServerConfig};
use pige_core::providers::{
    ErrorKind, OperationContext, ProviderCursor, ProviderError, ProviderFactory, ProviderId,
    ProviderRegistry, SentCopyPolicy, SyncRequest, fake::FakeProviderFactory,
};

/// The existing IMAP/SMTP account shape always reports the `imap-smtp` backend.
fn account() -> Account {
    let server = ServerConfig {
        host: "imap.example.com".to_string(),
        port: 993,
        security: Security::Tls,
    };
    Account {
        id: AccountId("acct-1".to_string()),
        label: "Example".to_string(),
        email: "me@example.com".to_string(),
        display_name: "Me".to_string(),
        backend: MailBackend::ImapSmtp {
            username: "me".to_string(),
            imap: server.clone(),
            smtp: server,
            auth: AuthKind::Password,
        },
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn context() -> OperationContext {
    OperationContext::with_timeout(account().id, 1, Duration::from_secs(30))
}

#[tokio::test]
async fn opens_submission_only_provider() {
    let factory = Arc::new(FakeProviderFactory::submission_only());
    let mut registry = ProviderRegistry::new();
    registry
        .register(account().provider_id(), factory.clone())
        .expect("register submission-only provider");

    let session = registry
        .open(&account(), &context())
        .await
        .expect("open session");

    assert!(session.capabilities.submission);
    assert!(!session.capabilities.mailbox);
    assert!(!session.capabilities.sync);
    assert!(session.mailbox.is_none());
    assert!(session.sync.is_none());
    assert!(session.mutations.is_none());
    assert!(session.submission.is_some());
    assert!(session.address_book.is_none());
}

#[tokio::test]
async fn missing_capability_is_unsupported() {
    let factory = Arc::new(FakeProviderFactory::submission_only());
    let mut registry = ProviderRegistry::new();
    registry
        .register(account().provider_id(), factory)
        .expect("register submission-only provider");

    let mut session = registry
        .open(&account(), &context())
        .await
        .expect("open session");

    let error = session
        .mailbox_service()
        .err()
        .expect("a submission-only provider has no mailbox service");
    assert_eq!(error.kind, ErrorKind::Unsupported);

    let error = session
        .sync_service()
        .err()
        .expect("a submission-only provider has no sync service");
    assert_eq!(error.kind, ErrorKind::Unsupported);

    // The capability that is present must still be reachable without panicking.
    let submission = session
        .submission_service()
        .expect("submission service is present");
    assert_eq!(submission.sent_copy_policy(), SentCopyPolicy::ClientAppend);
}

#[tokio::test]
async fn duplicate_provider_is_rejected() {
    let id = ProviderId::from("imap-smtp");
    let mut registry = ProviderRegistry::new();
    registry
        .register(id.clone(), Arc::new(FakeProviderFactory::new()))
        .expect("first registration succeeds");

    let error = registry
        .register(id, Arc::new(FakeProviderFactory::new()))
        .expect_err("a duplicate provider id is rejected");
    assert_eq!(error.kind, ErrorKind::Conflict);
}

#[tokio::test]
async fn cursor_is_not_interpreted_by_registry() {
    let factory = Arc::new(FakeProviderFactory::new());
    let mut registry = ProviderRegistry::new();
    registry
        .register(account().provider_id(), factory.clone())
        .expect("register provider");

    let mut session = registry
        .open(&account(), &context())
        .await
        .expect("open session");

    // A cursor whose payload would break any naive parsing: a delimiter, path
    // separators, a non-ASCII rune, and text that is not JSON.
    let cursor = ProviderCursor::new(7, "a:b/c\u{1f600}=not-json");
    let page = session
        .sync_service()
        .expect("sync service")
        .next_page(
            SyncRequest {
                cursor: Some(cursor.clone()),
                initial_limit: 25,
            },
            &context(),
        )
        .await
        .expect("sync page");

    assert_eq!(
        page.checkpoint, cursor,
        "the cursor must round-trip byte-for-byte"
    );
    assert_ne!(
        cursor,
        ProviderCursor::new(7, "a:b/c\u{1f600}=not-json "),
        "cursors compare on their whole payload, not an interpreted prefix"
    );
    assert_eq!(
        factory.recorder().sync_requests()[0].cursor.as_ref(),
        Some(&cursor)
    );
}

#[tokio::test]
async fn cancelled_context_opens_no_network() {
    let factory = Arc::new(FakeProviderFactory::new());
    let mut registry = ProviderRegistry::new();
    registry
        .register(account().provider_id(), factory.clone())
        .expect("register provider");

    let context = context();
    context.cancellation.cancel();

    let error = registry
        .open(&account(), &context)
        .await
        .expect_err("open must fail once the context is cancelled");
    assert_eq!(error.kind, ErrorKind::Cancelled);

    assert_eq!(
        factory.recorder().open_attempts(),
        0,
        "the registry must not even attempt to open a cancelled context"
    );
    assert_eq!(
        factory.recorder().opens(),
        0,
        "no network session may be created for a cancelled context"
    );
}

#[test]
fn errors_redact_wire_payloads() {
    const MARKER: &str = "super-secret-wire-token";

    let error = ProviderError::new(ErrorKind::Reauthenticate, "sign-in required")
        .with_source_detail(format!("server rejected Authorization: Bearer {MARKER}"));

    let debug = format!("{error:?}");
    let display = format!("{error}");
    assert!(
        !debug.contains(MARKER),
        "Debug leaked the wire payload: {debug}"
    );
    assert!(
        !display.contains(MARKER),
        "Display leaked the wire payload: {display}"
    );
    assert!(display.contains("sign-in required"));
}
