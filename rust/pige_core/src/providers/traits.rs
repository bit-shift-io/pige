//! The async capability traits and the session/factory boundary.
//!
//! All network services are object-safe, `Send`, and take `&mut self` so an
//! adapter can own a connection without a lock held over an await. A
//! [`ProviderSession`] advertises which services exist; a [`ProviderFactory`]
//! opens one for an account. [`ProviderRegistry`](super::ProviderRegistry) is
//! the application-owned [`ProviderFactory`] that dispatches by account.

use std::fmt;

use async_trait::async_trait;

use crate::account::Account;

use super::error::ProviderError;
use super::types::{
    BodyContent, ChangePage, CollectionRecord, ContactPage, ContactSourceId, HistoryPage,
    HistoryRequest, MutationRequest, MutationResult, OperationContext, PreparedSubmission,
    ProviderCapabilities, ProviderCursor, ReconcileOutcome, RemoteAttachmentId, RemoteMessageId,
    SentCopyPolicy, SubmissionOutcome, SubmissionReceipt, SubmissionRequest, SyncRequest,
};

/// Read-side mailbox access: collections, bodies, attachments, and history.
#[async_trait]
pub trait MailboxService: Send {
    async fn collections(
        &mut self,
        ctx: &OperationContext,
    ) -> Result<Vec<CollectionRecord>, ProviderError>;

    async fn body(
        &mut self,
        id: &RemoteMessageId,
        ctx: &OperationContext,
    ) -> Result<BodyContent, ProviderError>;

    async fn attachment(
        &mut self,
        message: &RemoteMessageId,
        id: &RemoteAttachmentId,
        ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError>;

    async fn history(
        &mut self,
        request: HistoryRequest,
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError>;
}

/// Incremental mailbox synchronization.
#[async_trait]
pub trait SyncService: Send {
    async fn next_page(
        &mut self,
        request: SyncRequest,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError>;
}

/// Semantic message mutations.
#[async_trait]
pub trait MutationService: Send {
    async fn apply(
        &mut self,
        request: MutationRequest,
        ctx: &OperationContext,
    ) -> Result<MutationResult, ProviderError>;
}

/// Durable, provider-neutral submission.
#[async_trait]
pub trait SubmissionService: Send {
    /// Whether the provider writes the Sent copy itself or the client must.
    fn sent_copy_policy(&self) -> SentCopyPolicy;

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError>;

    async fn submit(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError>;

    /// Reconcile an ambiguous submission with the provider.
    async fn reconcile(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError>;

    /// Finish a successful submission (for example, append the Sent copy).
    async fn finalize_sent(
        &mut self,
        request: &SubmissionRequest,
        receipt: &SubmissionReceipt,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError>;
}

/// Paged access to one contact source.
#[async_trait]
pub trait AddressBookService: Send {
    async fn next_page(
        &mut self,
        source: &ContactSourceId,
        cursor: Option<ProviderCursor>,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError>;
}

/// One opened account. Capabilities describe which services are present; the
/// services themselves are optional so a provider can implement only part of
/// the surface. Shared adapter state may live behind an internal `Arc`.
pub struct ProviderSession {
    pub capabilities: ProviderCapabilities,
    pub mailbox: Option<Box<dyn MailboxService>>,
    pub sync: Option<Box<dyn SyncService>>,
    pub mutations: Option<Box<dyn MutationService>>,
    pub submission: Option<Box<dyn SubmissionService>>,
    pub address_book: Option<Box<dyn AddressBookService>>,
}

impl ProviderSession {
    /// An empty session advertising `capabilities`.
    pub fn new(capabilities: ProviderCapabilities) -> Self {
        Self {
            capabilities,
            mailbox: None,
            sync: None,
            mutations: None,
            submission: None,
            address_book: None,
        }
    }

    pub fn mailbox_service(&mut self) -> Result<&mut Box<dyn MailboxService>, ProviderError> {
        self.mailbox
            .as_mut()
            .ok_or_else(|| ProviderError::unsupported("mailbox"))
    }

    pub fn sync_service(&mut self) -> Result<&mut Box<dyn SyncService>, ProviderError> {
        self.sync
            .as_mut()
            .ok_or_else(|| ProviderError::unsupported("sync"))
    }

    pub fn mutation_service(&mut self) -> Result<&mut Box<dyn MutationService>, ProviderError> {
        self.mutations
            .as_mut()
            .ok_or_else(|| ProviderError::unsupported("mutations"))
    }

    pub fn submission_service(&mut self) -> Result<&mut Box<dyn SubmissionService>, ProviderError> {
        self.submission
            .as_mut()
            .ok_or_else(|| ProviderError::unsupported("submission"))
    }

    pub fn address_book_service(
        &mut self,
    ) -> Result<&mut Box<dyn AddressBookService>, ProviderError> {
        self.address_book
            .as_mut()
            .ok_or_else(|| ProviderError::unsupported("contacts"))
    }
}

impl fmt::Debug for ProviderSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderSession")
            .field("capabilities", &self.capabilities)
            .field("mailbox", &self.mailbox.is_some())
            .field("sync", &self.sync.is_some())
            .field("mutations", &self.mutations.is_some())
            .field("submission", &self.submission.is_some())
            .field("address_book", &self.address_book.is_some())
            .finish()
    }
}

/// Opens a [`ProviderSession`] for one account.
///
/// Implemented by `ProviderRegistry` (application-owned dispatch) and by each
/// native adapter. `Send + Sync` so a single factory can be shared across
/// worker tasks.
#[async_trait]
pub trait ProviderFactory: Send + Sync {
    async fn open(
        &self,
        account: &Account,
        ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError>;
}
