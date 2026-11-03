//! Transport abstraction: the async surface the sync engine and mutations use
//! to talk to a mail server, plus the scripted in-memory fake used by tests.

pub mod fake;
pub mod imap;
pub mod smtp;
pub mod tls;

use async_trait::async_trait;

use crate::mail::{Address, Flags, FolderKind, MessageHeader, Uid};

pub use fake::FakeTransport;

/// A folder as reported by the server's LIST command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    pub name: String,
    pub delimiter: Option<String>,
    pub kind: FolderKind,
}

/// Server state for a selected folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderState {
    pub uidvalidity: u32,
    pub uidnext: u32,
    pub exists: u32,
}

/// One message fetched from the server: UID, parsed header, flags, and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMessage {
    pub uid: Uid,
    pub header: MessageHeader,
    pub flags: Flags,
    pub size: u32,
}

/// Server capability flags that change client behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub gmail: bool,
    pub move_ext: bool,
}

/// Inclusive UID range used by [`MailTransport::fetch_headers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UidRange {
    pub from: Uid,
    pub to: Uid,
}

/// A flag mutation for [`MailTransport::store_flags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagOp {
    Add(Flags),
    Remove(Flags),
    Replace(Flags),
}

/// A transport-level failure. Implementors map their own errors into a message.
#[derive(Debug, thiserror::Error)]
#[error("transport error: {0}")]
pub struct TransportError(pub String);

/// A send (SMTP) failure.
#[derive(Debug, thiserror::Error)]
#[error("send error: {0}")]
pub struct SendError(pub String);

/// Async surface for talking to a mail server. Kept object-safe so the sync
/// engine and mutations can be exercised against a scripted [`FakeTransport`]
/// without any network. `capabilities` and the mutation methods are unused by
/// the read path but keep the trait complete for the IMAP implementation and
/// the move/delete mutations.
///
/// Move/archive/trash are expressed as three primitives so the API can branch
/// on [`Capabilities`] (Gmail labels vs. the generic `MOVE`/`COPY` fallback):
/// - [`MailTransport::move_messages`] — a server-side `MOVE` (or a generic
///   COPY + delete + expunge fallback inside the transport).
/// - [`MailTransport::copy_messages`] — a bare `COPY`, which on Gmail adds a
///   label without removing the source.
/// - [`MailTransport::delete_messages`] — mark `\Deleted` and expunge, which on
///   Gmail removes the source label.
#[async_trait]
pub trait MailTransport: Send {
    async fn capabilities(&mut self) -> Result<Capabilities, TransportError>;
    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>, TransportError>;
    async fn select(&mut self, folder: &RemoteFolder) -> Result<FolderState, TransportError>;
    async fn fetch_headers(
        &mut self,
        range: UidRange,
    ) -> Result<Vec<RemoteMessage>, TransportError>;
    /// Fetch the first `max_bytes` of each message's text, used to fill snippets.
    async fn fetch_texts(
        &mut self,
        uids: &[Uid],
        max_bytes: usize,
    ) -> Result<Vec<(Uid, String)>, TransportError>;
    async fn fetch_body(&mut self, uid: Uid) -> Result<Vec<u8>, TransportError>;
    async fn fetch_flags(&mut self, uids: &[Uid]) -> Result<Vec<(Uid, Flags)>, TransportError>;
    async fn search(&mut self, query: &str) -> Result<Vec<Uid>, TransportError>;
    async fn store_flags(&mut self, uids: &[Uid], flags: FlagOp) -> Result<(), TransportError>;
    /// Move messages to `dest`, removing them from the currently selected
    /// mailbox. Implementors use `MOVE` when available and fall back to COPY +
    /// `\Deleted` + expunge otherwise.
    async fn move_messages(&mut self, uids: &[Uid], dest: &str) -> Result<(), TransportError>;
    /// Copy messages to `dest` without removing them from the source mailbox.
    async fn copy_messages(&mut self, uids: &[Uid], dest: &str) -> Result<(), TransportError>;
    /// Mark messages `\Deleted` and expunge them from the currently selected
    /// mailbox.
    async fn delete_messages(&mut self, uids: &[Uid]) -> Result<(), TransportError>;
    async fn append(
        &mut self,
        folder: &str,
        message: &[u8],
        flags: Flags,
    ) -> Result<(), TransportError>;
}

/// Async surface for submitting a composed message.
#[async_trait]
pub trait SmtpSender: Send {
    async fn send_raw(
        &mut self,
        from: &Address,
        recipients: &[Address],
        raw: &[u8],
    ) -> Result<(), SendError>;
}
