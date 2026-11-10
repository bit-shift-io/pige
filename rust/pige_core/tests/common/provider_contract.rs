//! Shared cross-provider contract suite.
//!
//! Every adapter (the scripted fake, the IMAP transport adapter, and the
//! feature-gated native Gmail/Proton adapters) can be driven through the same
//! [`ProviderTestHarness`] without exposing a protocol or SDK type to the
//! application-facing tests. The harness builds the account and session and may
//! supply fixture/event controls; [`run_suite`] then asserts the invariants the
//! rest of the application relies on.
//!
//! The suite is deliberately small. Adapter-specific wire behavior belongs in
//! that adapter's own tests; this is the cross-provider boundary contract.

#![allow(dead_code)]

use std::time::Duration;

use async_trait::async_trait;

use pige_core::account::Account;
use pige_core::providers::{
    CollectionRecord, ErrorKind, OperationContext, ProviderCapabilities, ProviderCursor,
    ProviderError, ProviderSession, SentCopyPolicy, SyncRequest,
};

/// Fixture expectations a harness may supply. Everything is optional so an
/// adapter that only opens a session can still run the common suite.
#[derive(Debug, Clone, Default)]
pub struct ContractFixture {
    /// Collections the adapter must report, including their opaque remote id.
    pub collections: Option<Vec<CollectionRecord>>,
    /// The Sent-copy policy the adapter advertises.
    pub sent_copy_policy: Option<SentCopyPolicy>,
}

/// One factory under contract. The harness owns the account and any synthetic
/// transport/secret fixture; the suite only sees provider-neutral values.
#[async_trait]
pub trait ProviderTestHarness: Send + Sync {
    /// Stable label for assertion messages.
    fn name(&self) -> &'static str;

    /// The account the factory will be asked to open.
    fn account(&self) -> Account;

    /// Capabilities the adapter is expected to advertise.
    fn expected_capabilities(&self) -> ProviderCapabilities;

    /// Open one session, exactly as the application's registry would.
    async fn open(&self, ctx: &OperationContext) -> Result<ProviderSession, ProviderError>;

    /// Fixture expectations, if this adapter supplies any.
    fn fixture(&self) -> ContractFixture {
        ContractFixture::default()
    }

    /// Reset any cross-open event/recorder state before the suite starts.
    fn reset_events(&self) {}
}

/// Run the provider-neutral conformance checks against one factory.
///
/// The checks cover:
/// - opening the account and reporting the expected provider/capabilities;
/// - exposing exactly the services the capabilities advertise, with an
///   `Unsupported` error for every absent one;
/// - refusing an already-cancelled context before any network work;
/// - treating cursor payloads as opaque (either round-tripping them unchanged
///   or rejecting the hostile value with `CursorInvalid`);
/// - matching any supplied collection and Sent-copy fixtures.
pub async fn run_suite(factory: impl ProviderTestHarness) {
    factory.reset_events();
    let name = factory.name();
    let account = factory.account();
    let expected = factory.expected_capabilities();
    let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));

    let mut session = factory
        .open(&ctx)
        .await
        .unwrap_or_else(|error| panic!("{name}: open failed: {error}"));

    assert_eq!(
        session.provider,
        account.provider_id(),
        "{name}: the session must name the account's provider"
    );
    assert_eq!(
        session.capabilities, expected,
        "{name}: advertised capabilities drifted from the harness expectation"
    );
    assert_eq!(
        session.mailbox.is_some(),
        expected.mailbox,
        "{name}: mailbox service presence must match capabilities"
    );
    assert_eq!(
        session.sync.is_some(),
        expected.sync,
        "{name}: sync service presence must match capabilities"
    );
    assert_eq!(
        session.mutations.is_some(),
        expected.mutations.read
            || expected.mutations.star
            || expected.mutations.labels
            || expected.mutations.move_messages
            || expected.mutations.archive
            || expected.mutations.trash
            || expected.mutations.permanent_delete,
        "{name}: mutation service presence must match capabilities"
    );
    assert_eq!(
        session.submission.is_some(),
        expected.submission,
        "{name}: submission service presence must match capabilities"
    );
    assert_eq!(
        session.address_book.is_some(),
        expected.contacts.read,
        "{name}: address-book service presence must match capabilities"
    );

    assert_absent_service(name, "mailbox", session.mailbox.is_none(), || {
        session.mailbox_service().map(|_| ())
    });
    assert_absent_service(name, "sync", session.sync.is_none(), || {
        session.sync_service().map(|_| ())
    });
    assert_absent_service(name, "mutations", session.mutations.is_none(), || {
        session.mutation_service().map(|_| ())
    });
    assert_absent_service(name, "submission", session.submission.is_none(), || {
        session.submission_service().map(|_| ())
    });
    assert_absent_service(name, "contacts", session.address_book.is_none(), || {
        session.address_book_service().map(|_| ())
    });

    // An already-cancelled context must never produce a session or touch the
    // network. Adapters that cache a client may still construct one, so we only
    // assert the typed cancellation result, not an internal call count.
    let cancelled = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_secs(30));
    cancelled.cancellation.cancel();
    match factory.open(&cancelled).await {
        Err(error) => assert_eq!(
            error.kind,
            ErrorKind::Cancelled,
            "{name}: a cancelled context must yield Cancelled, got {error:?}"
        ),
        Ok(_) => panic!("{name}: a cancelled context must not open a session"),
    }

    // Cursors are opaque to the application. A hostile, non-JSON payload must
    // either round-trip byte-for-byte (the fake's echo contract) or be rejected
    // with the typed cursor error; it must never be interpreted as anything
    // else or panic.
    if session.sync.is_some() {
        let cursor = ProviderCursor::new(7, "a:b/c\u{1f600}=not-json");
        let request = SyncRequest {
            cursor: Some(cursor.clone()),
            initial_limit: 5,
        };
        match session
            .sync_service()
            .expect("sync capability implies a sync service")
            .next_page(request, &ctx)
            .await
        {
            Ok(page) => assert_eq!(
                page.checkpoint, cursor,
                "{name}: an opaque cursor must round-trip unchanged"
            ),
            Err(error) => assert_eq!(
                error.kind,
                ErrorKind::CursorInvalid,
                "{name}: an unreadable opaque cursor must be CursorInvalid, got {error:?}"
            ),
        }
    }

    let fixture = factory.fixture();
    if let Some(expected_collections) = fixture.collections {
        assert!(
            expected.mailbox,
            "{name}: a collection fixture requires the mailbox capability"
        );
        let actual = session
            .mailbox_service()
            .expect("mailbox capability implies a mailbox service")
            .collections(&ctx)
            .await
            .unwrap_or_else(|error| panic!("{name}: collections failed: {error}"));
        assert_eq!(
            actual, expected_collections,
            "{name}: collections must match the supplied fixture"
        );
    }

    if let Some(expected_policy) = fixture.sent_copy_policy {
        assert!(
            expected.submission,
            "{name}: a Sent-copy fixture requires the submission capability"
        );
        let policy = session
            .submission_service()
            .expect("submission capability implies a submission service")
            .sent_copy_policy();
        assert_eq!(
            policy, expected_policy,
            "{name}: Sent-copy policy must match the supplied fixture"
        );
    }
}

fn assert_absent_service<F>(name: &str, service: &str, absent: bool, call: F)
where
    F: FnOnce() -> Result<(), ProviderError>,
{
    if !absent {
        return;
    }
    let error = call().expect_err(&format!(
        "{name}: an absent {service} service must return Unsupported"
    ));
    assert_eq!(
        error.kind,
        ErrorKind::Unsupported,
        "{name}: an absent {service} service must return Unsupported, got {error:?}"
    );
}
