//! Platform boundary: the OS services the core needs but cannot implement
//! itself. Desktop, Android, and test adapters implement these traits and are
//! injected through [`crate::api::Config`]; nothing here depends on a UI.

pub mod fake;
#[cfg(target_os = "linux")]
pub mod linux;

use std::path::Path;

use futures::future::BoxFuture;

use crate::account::{Account, AccountId};
use crate::sync::NewMailSummary;
use crate::transport::{MailTransport, SmtpSender};

/// Platform operations surface failures as user-facing strings so callers can
/// forward them to the UI without downcasting adapter-specific errors.
pub type Result<T> = std::result::Result<T, String>;

/// Marker prefix for a credential store the OS cannot reach (for example no
/// Secret Service on a headless Linux session). Callers use
/// [`credentials_unavailable`] to offer an in-memory session password instead
/// of persisting anything.
pub const CREDENTIALS_UNAVAILABLE: &str = "credential store unavailable";

/// The typed error behind [`CREDENTIALS_UNAVAILABLE`]. The credential trait is
/// string-based at the platform boundary, so the prefix is the stable contract
/// while this type keeps adapter code honest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{CREDENTIALS_UNAVAILABLE}: {0}")]
pub struct CredentialsUnavailable(pub String);

impl CredentialsUnavailable {
    pub fn into_message(self) -> String {
        self.to_string()
    }
}

/// Whether `error` means the persistent credential store is unreachable, in
/// which case credentials may be kept in memory for this process only.
pub fn credentials_unavailable(error: &str) -> bool {
    error.contains(CREDENTIALS_UNAVAILABLE)
}

/// Password storage. Secrets live in the OS keychain (or an in-memory fake in
/// tests) and are deliberately kept out of the SQLite cache.
///
/// `set_session` is the fallback when the OS store reports
/// [`CredentialsUnavailable`]: the secret stays in process memory and is gone
/// when pige exits. It must never be written to SQLite.
pub trait CredentialStore: Send + Sync {
    fn get(&self, id: &AccountId) -> Result<Option<String>>;
    fn set(&self, id: &AccountId, secret: &str) -> Result<()>;
    fn delete(&self, id: &AccountId) -> Result<()>;

    fn set_session(&self, id: &AccountId, secret: &str) -> Result<()> {
        let _ = (id, secret);
        Err("this credential store does not support session-only passwords".to_string())
    }

    fn delete_session(&self, id: &AccountId) -> Result<()> {
        let _ = id;
        Ok(())
    }
}

/// Delivers a notification for newly-arrived inbox mail.
pub trait Notifier: Send + Sync {
    fn notify(&self, mail: &NewMailSummary);
}

/// Opens files and URLs with the platform's default handler.
pub trait Opener: Send + Sync {
    fn open_file(&self, path: &Path) -> Result<()>;
    fn open_url(&self, url: &str) -> Result<()>;
}

/// Builds network transports for one account. Kept in the platform layer so
/// password lookup, TLS setup, and future OAuth refresh stay out of the API
/// state; tests inject a scripted fake.
pub trait TransportFactory: Send + Sync {
    fn imap<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn MailTransport>>>;

    /// Open an IMAP connection with an explicitly supplied password. Account
    /// onboarding uses this so a connection test never has to persist the
    /// secret first. Adapters that do not keep a session password fall back to
    /// their normal lookup.
    fn imap_with_password<'a>(
        &'a self,
        account: &'a Account,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn MailTransport>>> {
        let _ = password;
        self.imap(account)
    }

    fn smtp<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn SmtpSender>>>;
}
