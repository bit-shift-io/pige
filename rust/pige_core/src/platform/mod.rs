//! Platform boundary: the OS services the core needs but cannot implement
//! itself. Desktop, Android, and test adapters implement these traits and are
//! injected through [`crate::api::Config`]; nothing here depends on a UI.

pub mod fake;

use std::path::Path;

use futures::future::BoxFuture;

use crate::account::{Account, AccountId};
use crate::sync::NewMailSummary;
use crate::transport::{MailTransport, SmtpSender};

/// Platform operations surface failures as user-facing strings so callers can
/// forward them to the UI without downcasting adapter-specific errors.
pub type Result<T> = std::result::Result<T, String>;

/// Password storage. Secrets live in the OS keychain (or an in-memory fake in
/// tests) and are deliberately kept out of the SQLite cache.
pub trait CredentialStore: Send + Sync {
    fn get(&self, id: &AccountId) -> Result<Option<String>>;
    fn set(&self, id: &AccountId, secret: &str) -> Result<()>;
    fn delete(&self, id: &AccountId) -> Result<()>;
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

    fn smtp<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn SmtpSender>>>;
}
