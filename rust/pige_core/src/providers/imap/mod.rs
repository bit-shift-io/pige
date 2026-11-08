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

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::ExposeSecret;

use crate::account::Account;
use crate::auth::secrets::{SecretEnvelope, SecretRepository, StoredCredentials};
use crate::platform::TransportFactory;
use crate::providers::error::ProviderError;
use crate::providers::traits::{ProviderFactory, ProviderSession};
use crate::providers::types::{OperationContext, ProviderCapabilities};
use crate::transport::MailTransport;

pub use mailbox::ImapMailbox;

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

/// Capabilities advertised by the IMAP mailbox adapter. Sync, mutations, and
/// submission are added by later tasks; nothing is claimed before it exists.
fn imap_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        mailbox: true,
        ..ProviderCapabilities::default()
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
        let transport = self.open_transport(account).await?;
        // Re-check after the connection attempt; the account may have been
        // invalidated while it was in flight.
        ctx.ensure_active()?;

        let mut session = ProviderSession::new(imap_capabilities());
        session.mailbox = Some(Box::new(ImapMailbox::new(transport)));
        Ok(session)
    }
}

/// Map a connection failure (a platform string) to a redacted provider error.
fn connect_error(message: String) -> ProviderError {
    ProviderError::transient("could not connect to the mail server")
        .with_source_detail(message)
}
