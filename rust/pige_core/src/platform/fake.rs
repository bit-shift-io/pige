//! In-memory platform adapters for tests and for a desktop shell running
//! without a keychain. They store everything in process; nothing touches disk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;

use crate::account::{Account, AccountId};
use crate::mail::Address;
use crate::sync::NewMailSummary;
use crate::transport::{
    Capabilities, FakeTransport, FlagOp, FolderState, MailTransport, RemoteFolder, RemoteMessage,
    SendError, SmtpSender, TransportError, UidRange,
};

use super::{CredentialStore, Notifier, Opener, Result, TransportFactory};

/// In-memory password store.
#[derive(Debug, Default)]
pub struct FakeCredentials {
    secrets: Mutex<HashMap<AccountId, String>>,
    deleted: Mutex<Vec<AccountId>>,
    fail_next_set: AtomicBool,
    fail_delete: AtomicBool,
}

impl FakeCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience for tests that inspect what was persisted.
    pub fn secret(&self, id: &AccountId) -> Option<String> {
        self.secrets.lock().unwrap().get(id).cloned()
    }

    /// Make the next `set` call fail.
    pub fn fail_next_set(&self) {
        self.fail_next_set.store(true, Ordering::SeqCst);
    }

    /// Make every `delete` call fail.
    pub fn fail_delete(&self) {
        self.fail_delete.store(true, Ordering::SeqCst);
    }

    pub fn deleted(&self) -> Vec<AccountId> {
        self.deleted.lock().unwrap().clone()
    }
}

impl CredentialStore for FakeCredentials {
    fn get(&self, id: &AccountId) -> Result<Option<String>> {
        Ok(self.secrets.lock().unwrap().get(id).cloned())
    }

    fn set(&self, id: &AccountId, secret: &str) -> Result<()> {
        if self.fail_next_set.swap(false, Ordering::SeqCst) {
            return Err("fake credential set failed".to_string());
        }
        self.secrets
            .lock()
            .unwrap()
            .insert(id.clone(), secret.to_string());
        Ok(())
    }

    fn delete(&self, id: &AccountId) -> Result<()> {
        self.deleted.lock().unwrap().push(id.clone());
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err("fake credential delete failed".to_string());
        }
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

/// Shared handle to one scripted transport. The factory retains the same
/// instance so tests can inspect recorded calls after a sync.
#[derive(Debug, Clone)]
pub struct FakeTransportHandle(Arc<tokio::sync::Mutex<FakeTransport>>);

#[async_trait]
impl MailTransport for FakeTransportHandle {
    async fn capabilities(&mut self) -> std::result::Result<Capabilities, TransportError> {
        self.0.lock().await.capabilities().await
    }

    async fn list_folders(&mut self) -> std::result::Result<Vec<RemoteFolder>, TransportError> {
        self.0.lock().await.list_folders().await
    }

    async fn select(
        &mut self,
        folder: &RemoteFolder,
    ) -> std::result::Result<FolderState, TransportError> {
        self.0.lock().await.select(folder).await
    }

    async fn fetch_headers(
        &mut self,
        range: UidRange,
    ) -> std::result::Result<Vec<RemoteMessage>, TransportError> {
        self.0.lock().await.fetch_headers(range).await
    }

    async fn fetch_texts(
        &mut self,
        uids: &[crate::mail::Uid],
        max_bytes: usize,
    ) -> std::result::Result<Vec<(crate::mail::Uid, String)>, TransportError> {
        self.0.lock().await.fetch_texts(uids, max_bytes).await
    }

    async fn fetch_body(
        &mut self,
        uid: crate::mail::Uid,
    ) -> std::result::Result<Vec<u8>, TransportError> {
        self.0.lock().await.fetch_body(uid).await
    }

    async fn fetch_flags(
        &mut self,
        uids: &[crate::mail::Uid],
    ) -> std::result::Result<Vec<(crate::mail::Uid, crate::mail::Flags)>, TransportError> {
        self.0.lock().await.fetch_flags(uids).await
    }

    async fn search(
        &mut self,
        query: &str,
    ) -> std::result::Result<Vec<crate::mail::Uid>, TransportError> {
        self.0.lock().await.search(query).await
    }

    async fn store_flags(
        &mut self,
        uids: &[crate::mail::Uid],
        flags: FlagOp,
    ) -> std::result::Result<(), TransportError> {
        self.0.lock().await.store_flags(uids, flags).await
    }

    async fn move_messages(
        &mut self,
        uids: &[crate::mail::Uid],
        dest: &str,
    ) -> std::result::Result<(), TransportError> {
        self.0.lock().await.move_messages(uids, dest).await
    }

    async fn archive(
        &mut self,
        uids: &[crate::mail::Uid],
        folder: &RemoteFolder,
    ) -> std::result::Result<(), TransportError> {
        self.0.lock().await.archive(uids, folder).await
    }

    async fn trash(
        &mut self,
        uids: &[crate::mail::Uid],
        folder: &RemoteFolder,
    ) -> std::result::Result<(), TransportError> {
        self.0.lock().await.trash(uids, folder).await
    }

    async fn append(
        &mut self,
        folder: &str,
        message: &[u8],
        flags: crate::mail::Flags,
    ) -> std::result::Result<(), TransportError> {
        self.0.lock().await.append(folder, message, flags).await
    }
}

/// Scripted transport factory. Tests register one [`FakeTransport`] per account
/// and optionally mark accounts whose connection attempt should fail. The
/// factory returns handles to the same instances, so recorded calls remain
/// visible after sync.
#[derive(Debug, Default)]
pub struct FakeTransportFactory {
    transports: Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<FakeTransport>>>>,
    imap_failures: Mutex<HashSet<AccountId>>,
    smtp_failures: Mutex<HashSet<AccountId>>,
}

impl FakeTransportFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_transport(&self, id: AccountId, transport: FakeTransport) {
        self.transports
            .lock()
            .unwrap()
            .insert(id, Arc::new(tokio::sync::Mutex::new(transport)));
    }

    pub fn fail_imap(&self, id: AccountId) {
        self.imap_failures.lock().unwrap().insert(id);
    }

    pub fn fail_smtp(&self, id: AccountId) {
        self.smtp_failures.lock().unwrap().insert(id);
    }

    /// Snapshot of the scripted transport currently registered for `id`,
    /// including any calls recorded so far.
    pub async fn transport(&self, id: &AccountId) -> Option<FakeTransport> {
        let shared = { self.transports.lock().unwrap().get(id).map(Arc::clone) }?;
        Some(shared.lock().await.clone())
    }
}

impl TransportFactory for FakeTransportFactory {
    fn imap<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn MailTransport>>> {
        let id = account.id.clone();
        Box::pin(async move {
            if self.imap_failures.lock().unwrap().contains(&id) {
                return Err(format!("imap connection failed for {}", account.label));
            }

            let transport = { self.transports.lock().unwrap().get(&id).map(Arc::clone) };
            transport
                .map(|transport| Box::new(FakeTransportHandle(transport)) as Box<dyn MailTransport>)
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
