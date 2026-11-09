//! Provider-neutral identifiers and value types shared by every adapter.
//!
//! Local cache identity stays in [`crate::mail`]; the types here describe
//! remote objects and operation metadata. Adapters translate their own protocol
//! identifiers into these newtypes so the rest of the application never sees a
//! Gmail resource name, an IMAP UID, or a Proton reference.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::account::AccountId;
use crate::mail::{AttachmentDisposition, Flags, FolderKind, MessageHeader};

use super::error::ProviderError;

/// The backend id of the legacy IMAP/SMTP account shape. Every account written
/// before typed backends existed maps to this value, and it stays the dispatch
/// key for the IMAP/SMTP adapter.
pub const IMAP_SMTP_PROVIDER: &str = "imap-smtp";

/// The backend id of a native Gmail account. Only the account *shape* exists in
/// F2; the Gmail adapter arrives with the Google subplan.
pub const GMAIL_PROVIDER: &str = "gmail";

/// The backend id of a native Proton account. Only the account *shape* exists in
/// F2; the Proton adapter arrives with the Proton subplan.
pub const PROTON_PROVIDER: &str = "proton";

/// Define a provider-neutral string identifier. Remote identifiers are opaque
/// to the application and never compared against local ids.
macro_rules! remote_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

remote_id!(
    ProviderId,
    "Identifies a backend implementation, e.g. `imap-smtp`."
);
remote_id!(
    RemoteMessageId,
    "A message's identifier in its provider. Never a local `MessageId`."
);
remote_id!(
    RemoteCollectionId,
    "A collection (folder or label) identifier in its provider."
);
remote_id!(
    RemoteThreadId,
    "A provider thread/conversation identifier, when the provider has one."
);
remote_id!(
    RemoteAttachmentId,
    "A stable identifier for one attachment part within a provider message."
);
remote_id!(
    OperationId,
    "Caller-generated idempotency key for a durable mutation or submission."
);
remote_id!(
    ContactSourceId,
    "Identifies one contact source and its provider-side book."
);
remote_id!(
    RemoteContactId,
    "A contact's identifier within one provider contact source."
);
remote_id!(
    AuthAttemptId,
    "Identifies one in-flight interactive authentication attempt."
);

/// Opaque, versioned sync cursor. The payload is adapter-defined and must never
/// be inspected (or used as a credential) by the application or the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCursor {
    pub version: u32,
    pub payload: String,
}

impl ProviderCursor {
    pub fn new(version: u32, payload: impl Into<String>) -> Self {
        Self {
            version,
            payload: payload.into(),
        }
    }
}

/// What kind of durable job owns a cursor. Kept separate from the cursor so a
/// checkpoint is only ever applied to the job that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    MailSync,
    History,
    Contacts,
}

/// Identity of a resumable job: one account, one kind, one source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobKey {
    pub account: AccountId,
    pub kind: JobKind,
    pub source: String,
}

/// Everything a capability call needs to be generation- and cancellation-safe.
///
/// The generation is checked before network work and again inside every durable
/// write transaction; cancellation and the deadline bound how long a call may
/// hold an account gate.
#[derive(Debug, Clone)]
pub struct OperationContext {
    pub account: AccountId,
    pub generation: u64,
    pub cancellation: CancellationToken,
    pub deadline: tokio::time::Instant,
}

impl OperationContext {
    /// Build a context with an explicit deadline.
    pub fn new(account: AccountId, generation: u64, deadline: tokio::time::Instant) -> Self {
        Self {
            account,
            generation,
            cancellation: CancellationToken::new(),
            deadline,
        }
    }

    /// Build a context whose deadline is `timeout` from now.
    pub fn with_timeout(account: AccountId, generation: u64, timeout: Duration) -> Self {
        Self::new(account, generation, tokio::time::Instant::now() + timeout)
    }

    /// Fail fast when the context is already cancelled or past its deadline.
    pub fn ensure_active(&self) -> Result<(), ProviderError> {
        if self.cancellation.is_cancelled() {
            return Err(ProviderError::cancelled("operation cancelled"));
        }
        if tokio::time::Instant::now() >= self.deadline {
            return Err(ProviderError::transient("operation deadline exceeded"));
        }
        Ok(())
    }
}

/// Which message mutations a provider supports. Booleans are individual so a
/// partial provider (for example read-only mail) needs no new enum variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationCapabilities {
    pub read: bool,
    pub star: bool,
    pub labels: bool,
    pub move_messages: bool,
    pub archive: bool,
    pub trash: bool,
    pub permanent_delete: bool,
}

/// Which contact operations a provider supports. `remote_search`, `write` and
/// `groups` stay false in v1 until matching contract methods exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactCapabilities {
    pub read: bool,
    pub incremental_sync: bool,
    pub remote_search: bool,
    pub write: bool,
    pub groups: bool,
}

/// The coarse capability surface a session advertises, used by the application
/// to decide which services to route through and which UI actions exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub mailbox: bool,
    pub sync: bool,
    pub mutations: MutationCapabilities,
    pub submission: bool,
    pub contacts: ContactCapabilities,
    pub server_search: bool,
}

impl ProviderCapabilities {
    /// Capabilities for a provider that can only submit mail.
    pub fn submission_only() -> Self {
        Self {
            submission: true,
            ..Self::default()
        }
    }
}

/// A folder or label as reported by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionRecord {
    pub remote_id: RemoteCollectionId,
    pub name: String,
    pub kind: FolderKind,
    pub delimiter: Option<String>,
    pub total: Option<u32>,
    pub unread: Option<u32>,
}

/// A message as reported by the provider. Membership is a list because one
/// native message can belong to several labels/collections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    pub remote_id: RemoteMessageId,
    pub remote_thread: Option<RemoteThreadId>,
    pub header: MessageHeader,
    pub flags: Flags,
    pub collections: Vec<RemoteCollectionId>,
    pub snippet: String,
    pub has_attachments: bool,
}

/// One incremental change produced by a sync page.
///
/// The `UpsertMessage` variant carries a whole [`MessageRecord`], so the enum is
/// larger than its other variants. This shape is the cross-plan contract and a
/// sync page is not a hot path, so it is deliberately left unboxed (as
/// [`crate::auth::AuthProgress`] is).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailChange {
    UpsertCollection(CollectionRecord),
    RemoveCollection(RemoteCollectionId),
    UpsertMessage {
        message: MessageRecord,
        is_new_arrival: bool,
    },
    RemoveMessage(RemoteMessageId),
    SetMembership {
        message: RemoteMessageId,
        collection: RemoteCollectionId,
        present: bool,
    },
}

/// Ask the provider for the next page of mailbox changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRequest {
    pub cursor: Option<ProviderCursor>,
    pub initial_limit: u32,
}

/// One page of mailbox changes plus the checkpoint that resumes after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePage {
    pub changes: Vec<MailChange>,
    pub checkpoint: ProviderCursor,
    pub has_more: bool,
}

/// Ask the provider to page through older headers for recipient learning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    pub cursor: Option<ProviderCursor>,
    pub page_size: u32,
}

/// Progress of a history crawl; `scanned` is cumulative, not per-page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCoverage {
    pub scanned: u64,
    pub total_hint: Option<u64>,
}

/// Older messages learned from history. They are not inserted into the visible
/// mailbox cache automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub messages: Vec<MessageRecord>,
    pub checkpoint: ProviderCursor,
    pub has_more: bool,
    pub coverage: HistoryCoverage,
}

/// Whether a message carried a signature and, if so, its trust state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureStatus {
    NotApplicable,
    Verified,
    Unsigned,
    Unverified,
    Invalid,
    Unsupported,
}

/// One attachment offered by a fetched body, with the id used to fetch bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentDescriptor {
    pub remote_id: RemoteAttachmentId,
    pub part_id: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub disposition: AttachmentDisposition,
}

/// A normalized message body. Exactly which representations are present is
/// provider-dependent; `raw` carries the original bytes when available.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyContent {
    pub plain_text: Option<String>,
    pub html: Option<String>,
    pub raw: Option<Vec<u8>>,
    pub attachments: Vec<AttachmentDescriptor>,
    pub signature: SignatureStatus,
}

impl fmt::Debug for BodyContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BodyContent")
            .field("plain_text", &self.plain_text)
            .field("html", &self.html)
            .field("raw_len", &self.raw.as_ref().map(Vec::len))
            .field("attachments", &self.attachments)
            .field("signature", &self.signature)
            .finish()
    }
}

/// A semantic mutation to apply to one or more remote messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationRequest {
    pub operation: OperationId,
    pub targets: Vec<RemoteMessageId>,
    pub source: Option<RemoteCollectionId>,
    pub action: MutationAction,
}

/// Provider-neutral mutation actions. `Archive`/`Trash` map to provider
/// concepts inside the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationAction {
    SetRead(bool),
    SetStar(bool),
    AddLabel(RemoteCollectionId),
    RemoveLabel(RemoteCollectionId),
    Move(RemoteCollectionId),
    Archive,
    Trash,
    DeletePermanently,
}

/// Outcome of one target in a mutation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationOutcome {
    Applied,
    Rejected(ProviderError),
    /// The write may or may not have reached the server.
    Unknown,
}

/// Per-target mutation outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationItemResult {
    pub message: RemoteMessageId,
    pub outcome: MutationOutcome,
}

/// Result of a mutation request, one entry per target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationResult {
    pub items: Vec<MutationItemResult>,
}

/// One structured outgoing part. The existing plain-text compose path passes an
/// empty vector; these exist for provider APIs that encrypt/forward parts.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionAttachment {
    pub id: String,
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub content_id: Option<String>,
    pub disposition: AttachmentDisposition,
}

impl fmt::Debug for SubmissionAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubmissionAttachment")
            .field("id", &self.id)
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("bytes_len", &self.bytes.len())
            .field("content_id", &self.content_id)
            .field("disposition", &self.disposition)
            .finish()
    }
}

/// A backend artifact produced by `prepare`, reused by `submit` so a retry does
/// not repeat expensive or non-idempotent work. It never holds credentials.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedSubmission {
    pub version: u32,
    pub backend: ProviderId,
    pub raw: Option<Vec<u8>>,
    pub reference: Option<String>,
}

impl fmt::Debug for PreparedSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedSubmission")
            .field("version", &self.version)
            .field("backend", &self.backend)
            .field("raw_len", &self.raw.as_ref().map(Vec::len))
            .field("reference", &self.reference)
            .finish()
    }
}

/// A durable submission request. `operation` is the idempotency key.
///
/// Not serialized with the generic derives because [`crate::send::OutgoingMessage`]
/// owns its own wire representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionRequest {
    pub operation: OperationId,
    pub message: crate::send::OutgoingMessage,
    pub attachments: Vec<SubmissionAttachment>,
    pub prepared: Option<PreparedSubmission>,
}

/// Proof that a provider accepted a submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionReceipt {
    pub operation: OperationId,
    pub remote_message: Option<RemoteMessageId>,
    pub remote_thread: Option<RemoteThreadId>,
}

/// Result of attempting to submit: `Unknown` must never be retried blindly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionOutcome {
    Accepted(SubmissionReceipt),
    Rejected(ProviderError),
    Unknown,
}

/// Who writes the Sent copy: the provider, or the client appending it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SentCopyPolicy {
    ServerManaged,
    #[default]
    ClientAppend,
}

/// Result of reconciling an ambiguous submission with the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Accepted(SubmissionReceipt),
    NotAccepted,
    Unknown,
}

/// One contact as reported by a contact source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRecord {
    pub remote_id: RemoteContactId,
    pub version: Option<String>,
    pub names: Vec<String>,
    pub emails: Vec<crate::mail::Address>,
}

/// One contact change produced by a contact page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContactChange {
    Upsert(ContactRecord),
    Delete(RemoteContactId),
}

/// The role of one contact page in a source's sync stream.
///
/// The store needs this distinction because only a completed full snapshot may
/// retire contacts the provider no longer reports. A completed delta must leave
/// every cached contact untouched and keep its checkpoint for the next cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContactPageKind {
    /// A page of an ongoing full snapshot enumeration. Only a final page
    /// (`has_more == false`) completes the snapshot generation.
    Snapshot,
    /// The first page of a new full snapshot enumeration. The store allocates a
    /// fresh sweep generation so contacts absent from this snapshot are retired
    /// even if the previous cycle ended with an incremental delta.
    SnapshotStart,
    /// A page of an incremental delta. A delta never sweeps; its final page only
    /// advances the retained checkpoint.
    Delta,
}

impl ContactPageKind {
    /// Whether this page belongs to a full snapshot enumeration.
    pub fn is_snapshot(self) -> bool {
        matches!(self, Self::Snapshot | Self::SnapshotStart)
    }
}

/// One page of contact changes plus its resumable checkpoint.
///
/// `kind` distinguishes a full snapshot from an incremental delta. Only a
/// snapshot page with `has_more == false` completes a snapshot generation and
/// lets the store retire contacts the provider no longer reports. A completed
/// delta never sweeps; it only updates the retained checkpoint so the next
/// cycle can continue incrementally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPage {
    pub changes: Vec<ContactChange>,
    pub checkpoint: ProviderCursor,
    pub has_more: bool,
    pub kind: ContactPageKind,
}
