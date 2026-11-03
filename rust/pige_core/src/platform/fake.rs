//! In-memory platform adapters for tests and for a desktop shell running
//! without a keychain. They store everything in process; nothing touches disk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use futures::future::BoxFuture;

use crate::account::{Account, AccountId};
use crate::mail::Address;
use crate::sync::NewMailSummary;
use crate::transport::{FakeTransport, MailTransport, SendError, SmtpSender};

use super::{CredentialStore, Notifier, Opener, Result, TransportFactory};

/// In-memory password store.
#[derive(Debug, Default)]
pub struct FakeCredentials {
    secrets: Mutex<HashMap<AccountId, String>>,
}

impl FakeCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience for tests that inspect what was persisted.
    pub fn secret(&self, id: &AccountId) -> Option<String> {
        self.secrets.lock().unwrap().get(id).cloned()
    }
}

impl CredentialStore for FakeCredentials {
    fn get(&self, id: &AccountId) -> Result<Option<String>> {
        Ok(self.secrets.lock().unwrap().get(id).cloned())
    }

    fn set(&self, id: &AccountId, secret: &str) -> Result<()> {
        self.secrets
            .lock()
            .unwrap()
            .insert(id.clone(), secret.to_string());
        Ok(())
    }

    fn delete(&self, id: &AccountId) -> Result<()> {
        self.secrets.lock().unwrap().remove(id);
        Ok(())
    }
}

/// Records notifications so tests can assert what the platform was asked to
/// post.
#[derive(Debug, Default)]
pub struct FakeNotifier {
    notifications: Mutex<Vec<NewMailSummary>>,
}

impl FakeNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notifications(&self) -> Vec<NewMailSummary> {
        self.notifications.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.notifications.lock().unwrap().clear();
    }
}

impl Notifier for FakeNotifier {
    fn notify(&self, mail: &NewMailSummary) {
        self.notifications.lock().unwrap().push(mail.clone());
    }
}

/// Records file/URL open requests.
#[derive(Debug, Default)]
pub struct FakeOpener {
    files: Mutex<Vec<PathBuf>>,
    urls: Mutex<Vec<String>>,
}

impl FakeOpener {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn opened_files(&self) -> Vec<PathBuf> {
        self.files.lock().unwrap().clone()
    }

    pub fn opened_urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }
}

impl Opener for FakeOpener {
    fn open_file(&self, path: &Path) -> Result<()> {
        self.files.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }

    fn open_url(&self, url: &str) -> Result<()> {
        self.urls.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

/// A transport for one account that always succeeds. Used by the factory's
/// default SMTP path; Task 13 may extend it to record sent messages.
#[derive(Debug, Default)]
pub struct FakeSmtp {
    pub sent: Vec<Vec<u8>>,
}

#[async_trait]
impl SmtpSender for FakeSmtp {
    async fn send_raw(
        &mut self,
        _from: &Address,
        _recipients: &[Address],
        raw: &[u8],
    ) -> std::result::Result<(), SendError> {
        self.sent.push(raw.to_vec());
        Ok(())
    }
}

/// Scripted transport factory. Tests register one [`FakeTransport`] per account
/// and optionally mark accounts whose connection attempt should fail.
#[derive(Debug, Default)]
pub struct FakeTransportFactory {
    transports: Mutex<HashMap<AccountId, FakeTransport>>,
    imap_failures: Mutex<HashSet<AccountId>>,
    smtp_failures: Mutex<HashSet<AccountId>>,
}

impl FakeTransportFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_transport(&self, id: AccountId, transport: FakeTransport) {
        self.transports.lock().unwrap().insert(id, transport);
    }

    pub fn fail_imap(&self, id: AccountId) {
        self.imap_failures.lock().unwrap().insert(id);
    }

    pub fn fail_smtp(&self, id: AccountId) {
        self.smtp_failures.lock().unwrap().insert(id);
    }

    /// The scripted transport currently registered for `id`, if any.
    pub fn transport(&self, id: &AccountId) -> Option<FakeTransport> {
        self.transports.lock().unwrap().get(id).cloned()
    }
}

impl TransportFactory for FakeTransportFactory {
    fn imap<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn MailTransport>>> {
        let id = account.id.clone();
        Box::pin(async move {
            if self.imap_failures.lock().unwrap().contains(&id) {
                return Err(format!("imap connection failed for {}", account.label));
            }

            let transport = self.transports.lock().unwrap().get(&id).cloned();
            transport
                .map(|transport| Box::new(transport) as Box<dyn MailTransport>)
                .ok_or_else(|| format!("no fake IMAP transport for {}", account.label))
        })
    }

    fn smtp<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn SmtpSender>>> {
        Box::pin(async move {
            if self.smtp_failures.lock().unwrap().contains(&account.id) {
                return Err(format!("smtp connection failed for {}", account.label));
            }
            Ok(Box::new(FakeSmtp::default()) as Box<dyn SmtpSender>)
        })
    }
}
