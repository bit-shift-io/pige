//! Contract tests for the provider-neutral boundary.
//!
//! These exercise the application-owned [`ProviderRegistry`]: dispatching by
//! `Account::provider_id()`, rejecting duplicate registrations, keeping
//! [`ProviderCursor`] values opaque, short-circuiting an already-cancelled
//! context before any network work, and never leaking adapter wire payloads
//! through a [`ProviderError`]'s `Display`/`Debug`.
//!
//! The shared `common::provider_contract::run_suite` additionally runs the
//! provider-neutral conformance checks against the scripted fake, the IMAP
//! adapter over a scripted transport, and (with the matching feature) the
//! native Gmail/Proton adapters with injected credentials and no live network.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pige_core::FolderKind;
use pige_core::account::{Account, AccountId, AuthKind, MailBackend, Security, ServerConfig};
use pige_core::auth::CredentialSecretRepository;
use pige_core::platform::CredentialStore;
use pige_core::platform::fake::{FakeCredentials, FakeTransportFactory};
use pige_core::providers::{
    CollectionRecord, ContactCapabilities, ErrorKind, MutationCapabilities, OperationContext,
    ProviderCapabilities, ProviderCursor, ProviderError, ProviderFactory, ProviderId,
    ProviderRegistry, ProviderSession, RemoteCollectionId, SentCopyPolicy, SyncRequest,
    fake::{FakeProviderFactory, FakeProviderSession},
    imap::{ImapProviderFactory, imap_capabilities},
};
use pige_core::transport::{Capabilities, FakeTransport, FolderState, RemoteFolder};

use common::provider_contract::{ContractFixture, ProviderTestHarness, run_suite};

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

// ---------------------------------------------------------------------------
// Shared conformance suite drivers
// ---------------------------------------------------------------------------

fn full_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        mailbox: true,
        sync: true,
        mutations: MutationCapabilities {
            read: true,
            star: true,
            labels: true,
            move_messages: true,
            archive: true,
            trash: true,
            permanent_delete: true,
        },
        submission: true,
        contacts: ContactCapabilities {
            read: true,
            incremental_sync: true,
            remote_search: false,
            write: false,
            groups: false,
        },
        server_search: false,
    }
}

fn collection(remote: &str, name: &str, kind: FolderKind) -> CollectionRecord {
    CollectionRecord {
        remote_id: RemoteCollectionId::new(remote),
        name: name.to_string(),
        kind,
        delimiter: Some("/".to_string()),
        total: None,
        unread: None,
    }
}

struct FakeHarness {
    factory: Arc<FakeProviderFactory>,
    account: Account,
    collections: Vec<CollectionRecord>,
}

impl FakeHarness {
    fn new() -> Self {
        let collections = vec![
            collection("remote-inbox", "INBOX", FolderKind::Inbox),
            collection("remote-sent", "Sent", FolderKind::Sent),
        ];
        let factory = Arc::new(
            FakeProviderFactory::new()
                .session(FakeProviderSession::new().collections(collections.clone())),
        );
        Self {
            factory,
            account: account(),
            collections,
        }
    }
}

#[async_trait]
impl ProviderTestHarness for FakeHarness {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn account(&self) -> Account {
        self.account.clone()
    }

    fn expected_capabilities(&self) -> ProviderCapabilities {
        full_capabilities()
    }

    async fn open(&self, ctx: &OperationContext) -> Result<ProviderSession, ProviderError> {
        self.factory.open(&self.account, ctx).await
    }

    fn fixture(&self) -> ContractFixture {
        ContractFixture {
            collections: Some(self.collections.clone()),
            sent_copy_policy: Some(SentCopyPolicy::ClientAppend),
        }
    }

    fn reset_events(&self) {
        self.factory.recorder().clear();
    }
}

struct ImapHarness {
    factory: ImapProviderFactory,
    account: Account,
    collections: Vec<CollectionRecord>,
}

impl ImapHarness {
    fn new() -> Self {
        let account = account();
        let folders = vec![
            RemoteFolder {
                name: "INBOX".to_string(),
                delimiter: Some("/".to_string()),
                kind: FolderKind::Inbox,
                selectable: true,
            },
            RemoteFolder {
                name: "Sent".to_string(),
                delimiter: Some("/".to_string()),
                kind: FolderKind::Sent,
                selectable: true,
            },
        ];
        let states = [
            (
                "INBOX".to_string(),
                FolderState {
                    uidvalidity: 1,
                    uidnext: 2,
                    exists: 1,
                },
            ),
            (
                "Sent".to_string(),
                FolderState {
                    uidvalidity: 1,
                    uidnext: 1,
                    exists: 0,
                },
            ),
        ]
        .into_iter()
        .collect();
        let transports = Arc::new(FakeTransportFactory::default());
        transports.add_transport(
            account.id.clone(),
            FakeTransport {
                folders: folders.clone(),
                states,
                ..FakeTransport::default()
            },
        );
        let credentials: Arc<dyn CredentialStore> = Arc::new(FakeCredentials::new());
        let factory = ImapProviderFactory::new(
            transports,
            Arc::new(CredentialSecretRepository::new(credentials)),
        );
        let collections = folders
            .into_iter()
            .map(|folder| CollectionRecord {
                remote_id: pige_core::providers::imap::identity::collection_remote_id(&folder.name),
                name: folder.name,
                kind: folder.kind,
                delimiter: folder.delimiter,
                total: None,
                unread: None,
            })
            .collect();
        Self {
            factory,
            account,
            collections,
        }
    }
}

#[async_trait]
impl ProviderTestHarness for ImapHarness {
    fn name(&self) -> &'static str {
        "imap"
    }

    fn account(&self) -> Account {
        self.account.clone()
    }

    fn expected_capabilities(&self) -> ProviderCapabilities {
        imap_capabilities(Capabilities::default())
    }

    async fn open(&self, ctx: &OperationContext) -> Result<ProviderSession, ProviderError> {
        self.factory.open(&self.account, ctx).await
    }

    fn fixture(&self) -> ContractFixture {
        ContractFixture {
            collections: Some(self.collections.clone()),
            sent_copy_policy: Some(SentCopyPolicy::ClientAppend),
        }
    }
}

#[tokio::test]
async fn fake_provider_matches_the_shared_contract() {
    run_suite(FakeHarness::new()).await;
}

#[tokio::test]
async fn imap_provider_matches_the_shared_contract() {
    run_suite(ImapHarness::new()).await;
}

#[cfg(feature = "provider-google")]
mod google_contract {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use secrecy::SecretString;
    use tempfile::TempDir;

    use pige_core::auth::{AccountSessionManager, TokenSource};
    use pige_core::providers::google::{
        GMAIL_MODIFY_SCOPE, GoogleEndpoints, GoogleProviderFactory, GoogleTokenSourceProvider,
    };
    use pige_core::providers::{GMAIL_PROVIDER, ProviderError};
    use pige_core::store::Store;
    use pige_core::{AccountId, MailBackend};

    struct FixedTokenSource;

    #[async_trait]
    impl TokenSource for FixedTokenSource {
        async fn access_token(
            &self,
            _ctx: &OperationContext,
        ) -> Result<SecretString, ProviderError> {
            Ok(SecretString::from("synthetic-access-token"))
        }
    }

    struct FixedSources;

    impl GoogleTokenSourceProvider for FixedSources {
        fn token_source(
            &self,
            _account: &AccountId,
        ) -> Result<Arc<dyn TokenSource>, ProviderError> {
            Ok(Arc::new(FixedTokenSource))
        }
    }

    struct GoogleHarness {
        _dir: TempDir,
        factory: GoogleProviderFactory,
        account: Account,
    }

    impl GoogleHarness {
        fn new() -> Self {
            let dir = TempDir::new().expect("temp dir");
            let store = Arc::new(Store::open(dir.path().join("pige.db")).expect("store"));
            store.migrate().expect("migrate");
            let sessions = Arc::new(AccountSessionManager::new(store));
            let endpoints = GoogleEndpoints {
                gmail_base: url::Url::parse("https://gmail.invalid").expect("gmail url"),
                people_base: url::Url::parse("https://people.invalid").expect("people url"),
            };
            let factory = GoogleProviderFactory::new(sessions, endpoints)
                .with_token_source_provider(Arc::new(FixedSources));
            let account = Account {
                id: AccountId::new(),
                label: "Gmail".to_string(),
                email: "person@example.com".to_string(),
                display_name: "Person".to_string(),
                backend: MailBackend::gmail("google-user-contract"),
                contact_sources: Vec::new(),
                sort_order: 0,
                color: None,
            };
            Self {
                _dir: dir,
                factory,
                account,
            }
        }
    }

    #[async_trait]
    impl ProviderTestHarness for GoogleHarness {
        fn name(&self) -> &'static str {
            "gmail"
        }

        fn account(&self) -> Account {
            self.account.clone()
        }

        fn expected_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                mailbox: true,
                sync: true,
                mutations: MutationCapabilities {
                    read: true,
                    star: true,
                    labels: true,
                    move_messages: true,
                    archive: true,
                    trash: true,
                    permanent_delete: false,
                },
                submission: true,
                contacts: ContactCapabilities {
                    read: true,
                    incremental_sync: true,
                    remote_search: false,
                    write: false,
                    groups: false,
                },
                server_search: false,
            }
        }

        async fn open(&self, ctx: &OperationContext) -> Result<ProviderSession, ProviderError> {
            self.factory.open(&self.account, ctx).await
        }

        fn fixture(&self) -> ContractFixture {
            ContractFixture {
                collections: None,
                sent_copy_policy: Some(SentCopyPolicy::ServerManaged),
            }
        }
    }

    #[tokio::test]
    async fn gmail_provider_matches_the_shared_contract() {
        let harness = GoogleHarness::new();
        assert_eq!(
            harness.account.provider_id(),
            ProviderId::from(GMAIL_PROVIDER)
        );
        run_suite(harness).await;
    }

    #[test]
    fn gmail_requests_the_mail_scope_without_contacts_by_default() {
        // Check the scopes the adapter actually requests, not just the
        // constant: a mail-only session must ask for the modify scope and no
        // contact scope, while enabling contacts adds only the read-only pair.
        let mail = pige_core::providers::google::requested_scopes(false);
        assert_eq!(mail.first().map(String::as_str), Some(GMAIL_MODIFY_SCOPE));
        assert!(
            !mail.iter().any(|scope| scope.contains("contacts")),
            "mail-only sync must not request contacts: {mail:?}"
        );
        let with_contacts = pige_core::providers::google::requested_scopes(true);
        assert!(
            with_contacts
                .contains(&pige_core::providers::google::CONTACTS_READONLY_SCOPE.to_string()),
            "{with_contacts:?}"
        );
    }
}

#[cfg(feature = "provider-proton")]
mod proton_contract {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::TempDir;

    use pige_core::auth::{AccountSessionManager, CredentialSecretRepository};
    use pige_core::platform::fake::FakeCredentials;
    use pige_core::providers::proton::{ProtonEndpoints, ProtonKeyStores, ProtonProviderFactory};
    use pige_core::providers::{PROTON_PROVIDER, ProviderError};
    use pige_core::store::Store;
    use pige_core::{AccountId, MailBackend};

    struct ProtonHarness {
        _dir: TempDir,
        factory: ProtonProviderFactory,
        account: Account,
    }

    impl ProtonHarness {
        fn new() -> Self {
            let dir = TempDir::new().expect("temp dir");
            let store = Arc::new(Store::open(dir.path().join("pige.db")).expect("store"));
            store.migrate().expect("migrate");
            let sessions = Arc::new(AccountSessionManager::new(store));
            let credentials = Arc::new(FakeCredentials::new());
            let secrets = Arc::new(CredentialSecretRepository::new(credentials));
            let endpoints = ProtonEndpoints::new_insecure_for_tests("http://127.0.0.1:9")
                .expect("synthetic Proton endpoint");
            let factory = ProtonProviderFactory::new(
                sessions,
                secrets,
                endpoints,
                Arc::new(ProtonKeyStores::new()),
            );
            let account = Account {
                id: AccountId::new(),
                label: "Proton".to_string(),
                email: "person@proton.example".to_string(),
                display_name: "Person".to_string(),
                backend: MailBackend::proton(Some("proton-user-contract".to_string())),
                contact_sources: Vec::new(),
                sort_order: 0,
                color: None,
            };
            Self {
                _dir: dir,
                factory,
                account,
            }
        }
    }

    #[async_trait]
    impl ProviderTestHarness for ProtonHarness {
        fn name(&self) -> &'static str {
            "proton"
        }

        fn account(&self) -> Account {
            self.account.clone()
        }

        fn expected_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                mailbox: true,
                sync: true,
                mutations: MutationCapabilities {
                    read: true,
                    star: true,
                    labels: true,
                    move_messages: true,
                    archive: true,
                    trash: true,
                    permanent_delete: true,
                },
                submission: true,
                contacts: ContactCapabilities {
                    read: true,
                    incremental_sync: false,
                    remote_search: false,
                    write: false,
                    groups: false,
                },
                server_search: false,
            }
        }

        async fn open(&self, ctx: &OperationContext) -> Result<ProviderSession, ProviderError> {
            self.factory.open(&self.account, ctx).await
        }

        fn fixture(&self) -> ContractFixture {
            ContractFixture {
                collections: None,
                sent_copy_policy: Some(SentCopyPolicy::ServerManaged),
            }
        }
    }

    #[tokio::test]
    async fn proton_provider_matches_the_shared_contract() {
        let harness = ProtonHarness::new();
        assert_eq!(
            harness.account.provider_id(),
            ProviderId::from(PROTON_PROVIDER)
        );
        run_suite(harness).await;
    }
}
