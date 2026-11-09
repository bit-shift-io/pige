//! The IMAP/SMTP provider adapter.
//!
//! This is the only place protocol code meets the provider contracts. The
//! factory consumes the existing low-level [`TransportFactory`] (which owns the
//! connection and TLS) plus the protected [`SecretRepository`] (which decodes
//! the stored credential envelope), and wraps the resulting transport in an
//! [`ImapMailbox`] `MailboxService`. Sync, mutations, and submission arrive in
//! later tasks; for now the session advertises mailbox access only.

pub mod identity;
pub mod mailbox;
pub mod mutations;
pub mod submission;
pub mod sync;

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::ExposeSecret;
use tokio::sync::Mutex as AsyncMutex;

use crate::account::Account;
use crate::auth::secrets::{
    CredentialSecretRepository, SecretEnvelope, SecretRepository, StoredCredentials,
};
use crate::mail::{Flags, Uid};
use crate::platform::{CredentialStore, TransportFactory};
use crate::providers::error::ProviderError;
use crate::providers::registry::ProviderRegistry;
use crate::providers::traits::{ProviderFactory, ProviderSession};
use crate::providers::types::{
    IMAP_SMTP_PROVIDER, MutationCapabilities, OperationContext, ProviderCapabilities,
};
use crate::transport::{
    Capabilities, FlagOp, FolderState, MailTransport, RemoteFolder, RemoteMessage, TransportError,
    UidRange,
};

pub use mailbox::ImapMailbox;
pub use mutations::ImapMutations;
pub use submission::ImapSubmission;
pub use sync::ImapSync;

/// Opens IMAP mailbox sessions for IMAP/SMTP accounts.
pub struct ImapProviderFactory {
    transports: Arc<dyn TransportFactory>,
    secrets: Arc<dyn SecretRepository>,
}

impl ImapProviderFactory {
    pub fn new(transports: Arc<dyn TransportFactory>, secrets: Arc<dyn SecretRepository>) -> Self {
        Self {
            transports,
            secrets,
        }
    }

    /// Open the underlying IMAP connection for `account`.
    ///
    /// When a password credential is stored through the protected envelope path
    /// it is decoded here and handed to the transport's explicit-password
    /// entry point (a legacy raw password is read back as-is). With no stored
    /// credential the transport resolves its own; a non-password credential on
    /// an IMAP account is a genuine misconfiguration, not a silent fallback.
    async fn open_transport(
        &self,
        account: &Account,
    ) -> Result<Box<dyn MailTransport>, ProviderError> {
        match self.secrets.load(&account.id)? {
            Some(SecretEnvelope {
                credentials: StoredCredentials::Password(password),
                ..
            }) => self
                .transports
                .imap_with_password(account, password.expose_secret())
                .await
                .map_err(connect_error),
            Some(_) => Err(ProviderError::unsupported(
                "this account does not use a password credential",
            )),
            None => self.transports.imap(account).await.map_err(connect_error),
        }
    }
}

/// Build a provider registry containing only the IMAP/SMTP adapter.
///
/// The shells (desktop, Android, and the test/temp fake) hand the application a
/// single `Arc<dyn ProviderFactory>`; until a native adapter lands, that registry
/// contains just this one entry. The protected-secret repository wraps the
/// platform credential store, so the adapter resolves passwords exactly as the
/// legacy transport factory did.
pub fn imap_registry(
    transports: Arc<dyn TransportFactory>,
    credentials: Arc<dyn CredentialStore>,
) -> ProviderRegistry {
    let secrets: Arc<dyn SecretRepository> = Arc::new(CredentialSecretRepository::new(credentials));
    let mut registry = ProviderRegistry::new();
    registry
        .register(
            crate::providers::types::ProviderId::from(IMAP_SMTP_PROVIDER),
            Arc::new(ImapProviderFactory::new(transports, secrets)),
        )
        .expect("the imap-smtp provider is registered exactly once");
    registry
}

/// Capabilities advertised by the IMAP adapter, derived from the server's own
/// capability set. Submission is SMTP delivery with a client-appended Sent copy.
/// The mutation booleans are honest
/// about what the adapter actually implements:
///
/// - `labels` means "label operations that are implemented". On Gmail a label
///   is a folder, and adding one is a `COPY` of the *source* mailbox's own UIDs
///   into the label folder — the same UID space, so it is genuine and correct:
///   `labels` is `true` for Gmail. It is `false` for plain IMAP, which has no
///   labels. Gmail *remove*-label is explicitly unsupported (it would need the
///   message's UID *inside that label's folder*, which a per-mailbox UID cannot
///   name); {@link ImapMutations} returns a per-item `Rejected(Unsupported)` for
///   it rather than acting on a foreign UID space.
/// - `permanent_delete` is true only where it is genuinely implemented: a
///   plain-IMAP expunge permanently deletes from the source mailbox. A Gmail
///   source UID addresses only that label, so Gmail stays false.
pub fn imap_capabilities(server: Capabilities) -> ProviderCapabilities {
    ProviderCapabilities {
        mailbox: true,
        sync: true,
        mutations: MutationCapabilities {
            read: true,
            star: true,
            labels: server.gmail,
            move_messages: true,
            archive: true,
            trash: true,
            permanent_delete: !server.gmail,
        },
        submission: true,
        contacts: crate::providers::types::ContactCapabilities::default(),
        server_search: false,
    }
}

#[async_trait]
impl ProviderFactory for ImapProviderFactory {
    async fn open(
        &self,
        account: &Account,
        ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        // A cancelled or expired context must never reach the network.
        ctx.ensure_active()?;
        if account.imap_smtp().is_none() {
            return Err(ProviderError::unsupported(
                "IMAP mailbox access for this backend",
            ));
        }

        // Authenticate once and probe the server's capability set with the same
        // connection the services will use. Opening three connections (one per
        // service) amplified every interaction into three logins — and, against
        // Gmail's per-account connection limits, made a body-prefetch batch
        // unusable. The services share this one connection through
        // [`SharedTransport`]; only one capability is ever driven per session,
        // so there is no borrow conflict between services.
        let mut transport = self.open_transport(account).await?;
        let server = transport.capabilities().await.map_err(transport_error)?;
        // Re-check after the connection attempt; the account may have been
        // invalidated while it was in flight.
        ctx.ensure_active()?;

        let shared = Arc::new(AsyncMutex::new(transport));

        let mut session = ProviderSession::new(imap_capabilities(server)).with_provider(
            crate::providers::types::ProviderId::from(crate::providers::IMAP_SMTP_PROVIDER),
        );
        session.mailbox = Some(Box::new(ImapMailbox::new(Box::new(SharedTransport::new(
            shared.clone(),
        )))));
        session.sync = Some(Box::new(ImapSync::new(Box::new(SharedTransport::new(
            shared.clone(),
        )))));
        session.mutations = Some(Box::new(ImapMutations::new(
            Box::new(SharedTransport::new(shared)),
            server,
        )));
        session.submission = Some(Box::new(ImapSubmission::new(
            self.transports.clone(),
            account.clone(),
        )));
        Ok(session)
    }
}

/// A [`MailTransport`] shared by every capability service of one session.
///
/// Opening a session authenticates exactly once; the mailbox, sync, and
/// mutation services each hold a handle to this same connection and take it
/// under the mutex for the duration of one call. A session is only ever driven
/// through a single capability at a time (a caller opens a session and uses one
/// service), so sharing the connection avoids three separate logins per open
/// without a borrow conflict between services.
struct SharedTransport(Arc<AsyncMutex<Box<dyn MailTransport>>>);

impl SharedTransport {
    fn new(transport: Arc<AsyncMutex<Box<dyn MailTransport>>>) -> Self {
        Self(transport)
    }
}

#[async_trait]
impl MailTransport for SharedTransport {
    async fn capabilities(&mut self) -> Result<Capabilities, TransportError> {
        self.0.lock().await.capabilities().await
    }

    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>, TransportError> {
        self.0.lock().await.list_folders().await
    }

    async fn select(&mut self, folder: &RemoteFolder) -> Result<FolderState, TransportError> {
        self.0.lock().await.select(folder).await
    }

    async fn fetch_headers(
        &mut self,
        range: UidRange,
    ) -> Result<Vec<RemoteMessage>, TransportError> {
        self.0.lock().await.fetch_headers(range).await
    }

    async fn list_uids(&mut self) -> Result<Vec<Uid>, TransportError> {
        self.0.lock().await.list_uids().await
    }

    async fn fetch_learning_headers(
        &mut self,
        uids: &[Uid],
    ) -> Result<Vec<RemoteMessage>, TransportError> {
        self.0.lock().await.fetch_learning_headers(uids).await
    }

    async fn fetch_texts(
        &mut self,
        uids: &[Uid],
        max_bytes: usize,
    ) -> Result<Vec<(Uid, String)>, TransportError> {
        self.0.lock().await.fetch_texts(uids, max_bytes).await
    }

    async fn fetch_body(&mut self, uid: Uid) -> Result<Vec<u8>, TransportError> {
        self.0.lock().await.fetch_body(uid).await
    }

    async fn fetch_flags(&mut self, uids: &[Uid]) -> Result<Vec<(Uid, Flags)>, TransportError> {
        self.0.lock().await.fetch_flags(uids).await
    }

    async fn search(&mut self, query: &str) -> Result<Vec<Uid>, TransportError> {
        self.0.lock().await.search(query).await
    }

    async fn store_flags(&mut self, uids: &[Uid], flags: FlagOp) -> Result<(), TransportError> {
        self.0.lock().await.store_flags(uids, flags).await
    }

    async fn move_messages(&mut self, uids: &[Uid], dest: &str) -> Result<(), TransportError> {
        self.0.lock().await.move_messages(uids, dest).await
    }

    async fn copy_messages(&mut self, uids: &[Uid], dest: &str) -> Result<(), TransportError> {
        self.0.lock().await.copy_messages(uids, dest).await
    }

    async fn delete_messages(&mut self, uids: &[Uid]) -> Result<(), TransportError> {
        self.0.lock().await.delete_messages(uids).await
    }

    async fn append(
        &mut self,
        folder: &str,
        message: &[u8],
        flags: Flags,
    ) -> Result<(), TransportError> {
        self.0.lock().await.append(folder, message, flags).await
    }
}

/// Map a transport failure to a redacted provider error. Authentication
/// failures become `Reauthenticate`; everything else is transient, with the
/// wire detail confined to `source_detail`.
pub(crate) fn transport_error(error: crate::transport::TransportError) -> ProviderError {
    let message = error.to_string();
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("login") || lowered.contains("auth") || lowered.contains("password") {
        ProviderError::reauthenticate("the mail server rejected the sign-in")
    } else {
        ProviderError::transient("the mail server request failed").with_source_detail(message)
    }
}

/// Map a connection failure (a platform string) to a redacted provider error.
fn connect_error(message: String) -> ProviderError {
    ProviderError::transient("could not connect to the mail server").with_source_detail(message)
}
