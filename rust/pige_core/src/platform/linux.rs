//! Linux platform adapters: Secret Service credentials, `xdg-open`, and the
//! real IMAP transport. Notifications stay a no-op until Task 17 adds D-Bus.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use futures::future::BoxFuture;
use keyring::Entry;

use crate::account::{Account, AccountId};
use crate::platform::{
    CredentialStore, CredentialsUnavailable, Notifier, Opener, Result, TransportFactory,
};
use crate::sync::NewMailSummary;
use crate::transport::{MailTransport, SmtpSender, imap, smtp};

/// The Secret Service service name every pige secret is filed under.
pub const SERVICE: &str = "pige";

/// Secret Service-backed credential store.
///
/// Each password is stored as the keyring entry `(service = "pige",
/// username = "account:<id>")`. When the session has no reachable Secret
/// Service, persistent reads and writes fail with [`CredentialsUnavailable`]
/// rather than falling back to plaintext. The optional in-memory map is only
/// used through [`CredentialStore::set_session`], which the UI selects after
/// showing the user a warning.
pub struct LinuxCredentials {
    service: String,
    session: Mutex<HashMap<AccountId, String>>,
    /// Accounts this process has successfully read or written through Secret
    /// Service. They can never be treated as session-only.
    persistent: Mutex<HashSet<AccountId>>,
    /// Accounts whose only credential is the in-memory session password. This
    /// is deliberately separate from having a session string: an account can
    /// briefly hold both a persistent keyring secret and a session secret, and
    /// that must never make an unreachable keyring look successfully deleted.
    session_only: Mutex<HashSet<AccountId>>,
}

impl LinuxCredentials {
    pub fn new() -> Self {
        Self {
            service: SERVICE.to_string(),
            session: Mutex::new(HashMap::new()),
            persistent: Mutex::new(HashSet::new()),
            session_only: Mutex::new(HashSet::new()),
        }
    }

    fn session(&self) -> MutexGuard<'_, HashMap<AccountId, String>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn persistent(&self) -> MutexGuard<'_, HashSet<AccountId>> {
        self.persistent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session_only(&self) -> MutexGuard<'_, HashSet<AccountId>> {
        self.session_only
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn entry(&self, id: &AccountId) -> keyring::Result<Entry> {
        Entry::new(&self.service, &username(id))
    }
}

impl Default for LinuxCredentials {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for LinuxCredentials {
    fn get(&self, id: &AccountId) -> Result<Option<String>> {
        match self.entry(id) {
            Ok(entry) => match entry.get_password() {
                Ok(secret) => {
                    self.persistent().insert(id.clone());
                    Ok(Some(secret))
                }
                Err(keyring::Error::NoEntry) => Ok(self.session().get(id).cloned()),
                Err(keyring::Error::NoStorageAccess(cause)) => {
                    if let Some(secret) = self.session().get(id).cloned() {
                        Ok(Some(secret))
                    } else {
                        Err(CredentialsUnavailable(cause.to_string()).into_message())
                    }
                }
                Err(error) => Err(format!("read password from keyring: {error}")),
            },
            Err(keyring::Error::NoStorageAccess(cause)) => {
                if let Some(secret) = self.session().get(id).cloned() {
                    Ok(Some(secret))
                } else {
                    Err(CredentialsUnavailable(cause.to_string()).into_message())
                }
            }
            Err(error) => Err(format!("open keyring entry: {error}")),
        }
    }

    fn set(&self, id: &AccountId, secret: &str) -> Result<()> {
        let entry = self
            .entry(id)
            .map_err(|error| keyring_error("open keyring entry", error))?;
        entry
            .set_password(secret)
            .map_err(|error| keyring_error("store password in keyring", error))?;
        // A persistent write supersedes any session fallback for this account.
        self.persistent().insert(id.clone());
        self.session().remove(id);
        self.session_only().remove(id);
        Ok(())
    }

    fn delete(&self, id: &AccountId) -> Result<()> {
        let known_persistent = self.persistent().contains(id);
        let was_session_only = self.session_only().remove(id) && !known_persistent;
        self.session().remove(id);

        match self.entry(id) {
            Ok(entry) => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {
                    self.persistent().remove(id);
                    Ok(())
                }
                // Only an account known to have no persistent entry may be
                // treated as fully cleaned up while Secret Service is down.
                Err(keyring::Error::NoStorageAccess(_)) if was_session_only => {
                    self.persistent().remove(id);
                    Ok(())
                }
                Err(keyring::Error::NoStorageAccess(cause)) => {
                    Err(CredentialsUnavailable(cause.to_string()).into_message())
                }
                Err(error) => Err(format!("delete password from keyring: {error}")),
            },
            Err(keyring::Error::NoStorageAccess(_)) if was_session_only => {
                self.persistent().remove(id);
                Ok(())
            }
            Err(keyring::Error::NoStorageAccess(cause)) => {
                Err(CredentialsUnavailable(cause.to_string()).into_message())
            }
            Err(error) => Err(format!("open keyring entry: {error}")),
        }
    }

    fn set_session(&self, id: &AccountId, secret: &str) -> Result<()> {
        self.session().insert(id.clone(), secret.to_string());
        if !self.persistent().contains(id) {
            self.session_only().insert(id.clone());
        }
        Ok(())
    }

    fn set_transient(&self, id: &AccountId, secret: &str) -> Result<()> {
        self.session().insert(id.clone(), secret.to_string());
        Ok(())
    }

    fn delete_session(&self, id: &AccountId) -> Result<()> {
        self.session().remove(id);
        self.session_only().remove(id);
        Ok(())
    }
}

fn username(id: &AccountId) -> String {
    format!("account:{}", id.0)
}

fn keyring_error(context: &str, error: keyring::Error) -> String {
    match error {
        keyring::Error::NoStorageAccess(cause) => {
            CredentialsUnavailable(cause.to_string()).into_message()
        }
        other => format!("{context}: {other}"),
    }
}

/// Opens files and URLs with `xdg-open`, which honours the user's desktop
/// default applications.
#[derive(Debug, Default)]
pub struct LinuxOpener;

impl LinuxOpener {
    pub fn new() -> Self {
        Self
    }

    fn open(target: &std::ffi::OsStr) -> Result<()> {
        let mut child = Command::new("xdg-open")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("xdg-open: {error}"))?;
        // Reap the short-lived launcher off the UI thread. Dropping a Child
        // without waiting would leave a zombie on Unix.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

impl Opener for LinuxOpener {
    fn open_file(&self, path: &Path) -> Result<()> {
        Self::open(path.as_os_str())
    }

    fn open_url(&self, url: &str) -> Result<()> {
        Self::open(std::ffi::OsStr::new(url))
    }
}

/// Desktop notifications are deliberately inert until Task 17 wires
/// `notify-rust`; mail still arrives and syncs without them.
#[derive(Debug, Default)]
pub struct LinuxNotifier;

impl LinuxNotifier {
    pub fn new() -> Self {
        Self
    }
}

impl Notifier for LinuxNotifier {
    fn notify(&self, _mail: &NewMailSummary) {}
}

/// Builds real TLS IMAP connections with passwords from the credential store.
/// SMTP sending is completed by Task 13.
pub struct LinuxTransportFactory {
    credentials: std::sync::Arc<dyn CredentialStore>,
}

impl LinuxTransportFactory {
    pub fn new(credentials: std::sync::Arc<dyn CredentialStore>) -> Self {
        Self { credentials }
    }
}

impl TransportFactory for LinuxTransportFactory {
    fn imap<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn MailTransport>>> {
        Box::pin(async move {
            let password = self
                .credentials
                .get(&account.id)
                .map_err(|error| format!("{}: {error}", account.label))?
                .ok_or_else(|| {
                    format!(
                        "{}: no password is stored for this account; edit the account and enter it again",
                        account.label
                    )
                })?;
            let transport = imap::connect(account, &password)
                .await
                .map_err(|error| format!("{}: {error}", account.label))?;
            Ok(Box::new(transport) as Box<dyn MailTransport>)
        })
    }

    fn imap_with_password<'a>(
        &'a self,
        account: &'a Account,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn MailTransport>>> {
        Box::pin(async move {
            let transport = imap::connect(account, password)
                .await
                .map_err(|error| format!("{}: {error}", account.label))?;
            Ok(Box::new(transport) as Box<dyn MailTransport>)
        })
    }

    fn smtp<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn SmtpSender>>> {
        Box::pin(async move {
            let password = self
                .credentials
                .get(&account.id)
                .map_err(|error| format!("{}: {error}", account.label))?
                .ok_or_else(|| {
                    format!(
                        "{}: no password is stored for this account; edit the account and enter it again",
                        account.label
                    )
                })?;
            let transport = smtp::connect(account, &password)
                .await
                .map_err(|error| format!("{}: {error}", account.label))?;
            Ok(Box::new(transport) as Box<dyn SmtpSender>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_namespaced_by_account() {
        assert_eq!(username(&AccountId("abc".into())), "account:abc");
        assert_eq!(SERVICE, "pige");
    }
}
