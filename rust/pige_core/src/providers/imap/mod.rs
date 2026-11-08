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
pub mod sync;

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::ExposeSecret;

use crate::account::Account;
use crate::auth::secrets::{SecretEnvelope, SecretRepository, StoredCredentials};
use crate::platform::TransportFactory;
use crate::providers::error::ProviderError;
use crate::providers::traits::{ProviderFactory, ProviderSession};
use crate::providers::types::{MutationCapabilities, OperationContext, ProviderCapabilities};
use crate::transport::{Capabilities, MailTransport};

pub use mailbox::ImapMailbox;
pub use mutations::ImapMutations;
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
    async fn open_transport(&self, account: &Account) -> Result<Box<dyn MailTransport>, ProviderError> {
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

/// Capabilities advertised by the IMAP adapter, derived from the server's own
/// capability set. Submission arrives with F9. The mutation booleans are honest
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
fn imap_capabilities(server: Capabilities) -> ProviderCapabilities {
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
        submission: false,
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

        // Each service owns its own connection: a service never borrows another
        // service's mutable connection. The first connection also probes the
        // server's capability set, which now drives the advertised mutations.
        let mut sync_transport = self.open_transport(account).await?;
        let server = sync_transport
            .capabilities()
            .await
            .map_err(transport_error)?;
        // Re-check after the connection attempt; the account may have been
        // invalidated while it was in flight.
        ctx.ensure_active()?;

        let mailbox_transport = self.open_transport(account).await?;
        let mutation_transport = self.open_transport(account).await?;

        let mut session = ProviderSession::new(imap_capabilities(server));
        session.mailbox = Some(Box::new(ImapMailbox::new(mailbox_transport)));
        session.sync = Some(Box::new(ImapSync::new(sync_transport)));
        session.mutations = Some(Box::new(ImapMutations::new(mutation_transport, server)));
        Ok(session)
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
    ProviderError::transient("could not connect to the mail server")
        .with_source_detail(message)
}
