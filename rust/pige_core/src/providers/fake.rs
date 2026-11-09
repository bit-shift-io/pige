//! A scripted, in-memory provider used by the contract and orchestration tests.
//!
//! Nothing here touches the network or an SDK. A [`FakeProviderFactory`] owns a
//! shared [`FakeRecorder`] (so submission attempts are recorded across every
//! opened session) and a [`FakeProviderSession`] script: what each service
//! returns, which calls fail, and how long each call waits. This lets later
//! tasks drive ordering and failure paths deterministically.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::account::Account;

use super::error::ProviderError;
use super::traits::{
    AddressBookService, MailboxService, MutationService, ProviderFactory, ProviderSession,
    SubmissionService, SyncService,
};
use super::types::{
    BodyContent, ChangePage, CollectionRecord, ContactPage, ContactSourceId, HistoryCoverage,
    HistoryPage, HistoryRequest, MutationItemResult, MutationOutcome, MutationRequest,
    MutationResult, OperationContext, PreparedSubmission, ProviderCapabilities, ProviderCursor,
    ProviderId, ReconcileOutcome, RemoteAttachmentId, RemoteMessageId, SentCopyPolicy,
    SignatureStatus, SubmissionOutcome, SubmissionReceipt, SubmissionRequest, SyncRequest,
};

/// A capability method a script can mark as failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FakeCall {
    Collections,
    Body,
    Attachment,
    History,
    Sync,
    Mutation,
    Prepare,
    Submit,
    Reconcile,
    FinalizeSent,
    Contacts,
}

/// Which services an opened fake session attaches. Capabilities are derived
/// from this unless a factory overrides them explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeServices {
    pub mailbox: bool,
    pub sync: bool,
    pub mutations: bool,
    pub submission: bool,
    pub address_book: bool,
}

impl FakeServices {
    pub const NONE: Self = Self {
        mailbox: false,
        sync: false,
        mutations: false,
        submission: false,
        address_book: false,
    };

    pub const ALL: Self = Self {
        mailbox: true,
        sync: true,
        mutations: true,
        submission: true,
        address_book: true,
    };

    pub fn submission_only() -> Self {
        Self {
            submission: true,
            ..Self::NONE
        }
    }
}

impl Default for FakeServices {
    fn default() -> Self {
        Self::ALL
    }
}

/// Everything a fake session will return. Builder methods on
/// [`FakeProviderSession`] fill this in; the factory clones it for each open so
/// every session replays the same script from the start.
#[derive(Clone, Default)]
pub struct FakeProviderSession {
    collections: Vec<CollectionRecord>,
    body: Option<BodyContent>,
    attachment: Option<Vec<u8>>,
    mutation_result: Option<MutationResult>,
    prepared: Option<PreparedSubmission>,
    submission_outcome: Option<SubmissionOutcome>,
    reconcile_outcome: Option<ReconcileOutcome>,
    sent_copy_policy: SentCopyPolicy,
    failures: HashMap<FakeCall, ProviderError>,
    delay: Option<Duration>,
    echo_sync_cursor: bool,
    change_pages: Vec<ChangePage>,
    history_pages: Vec<HistoryPage>,
    contact_pages: Vec<ContactPage>,
}

impl FakeProviderSession {
    pub fn new() -> Self {
        Self {
            sent_copy_policy: SentCopyPolicy::ClientAppend,
            echo_sync_cursor: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn collections(mut self, collections: Vec<CollectionRecord>) -> Self {
        self.collections = collections;
        self
    }

    #[must_use]
    pub fn body(mut self, body: BodyContent) -> Self {
        self.body = Some(body);
        self
    }

    #[must_use]
    pub fn attachment_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.attachment = Some(bytes);
        self
    }

    #[must_use]
    pub fn mutation_result(mut self, result: MutationResult) -> Self {
        self.mutation_result = Some(result);
        self
    }

    #[must_use]
    pub fn prepared(mut self, prepared: PreparedSubmission) -> Self {
        self.prepared = Some(prepared);
        self
    }

    #[must_use]
    pub fn submission_outcome(mut self, outcome: SubmissionOutcome) -> Self {
        self.submission_outcome = Some(outcome);
        self
    }

    #[must_use]
    pub fn reconcile_outcome(mut self, outcome: ReconcileOutcome) -> Self {
        self.reconcile_outcome = Some(outcome);
        self
    }

    #[must_use]
    pub fn sent_copy_policy(mut self, policy: SentCopyPolicy) -> Self {
        self.sent_copy_policy = policy;
        self
    }

    /// Queue one sync page; pages are returned in the order queued.
    #[must_use]
    pub fn change_page(mut self, page: ChangePage) -> Self {
        self.change_pages.push(page);
        self
    }

    #[must_use]
    pub fn history_page(mut self, page: HistoryPage) -> Self {
        self.history_pages.push(page);
        self
    }

    #[must_use]
    pub fn contact_page(mut self, page: ContactPage) -> Self {
        self.contact_pages.push(page);
        self
    }

    /// Delay every capability call by this long before it returns.
    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Make one capability method fail with `error`.
    #[must_use]
    pub fn fail(mut self, call: FakeCall, error: ProviderError) -> Self {
        self.failures.insert(call, error);
        self
    }

    /// When true (the default) an unscripted sync page echoes the request
    /// cursor as its checkpoint.
    #[must_use]
    pub fn echo_sync_cursor(mut self, echo: bool) -> Self {
        self.echo_sync_cursor = echo;
        self
    }

    async fn settle(&self) {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
    }

    fn failure(&self, call: FakeCall) -> Option<ProviderError> {
        self.failures.get(&call).cloned()
    }
}

/// Per-open mutable cursor queues. The script itself is immutable so openings
/// are independent; these deques advance as pages are consumed.
#[derive(Default)]
struct FakeQueues {
    change_pages: VecDeque<ChangePage>,
    history_pages: VecDeque<HistoryPage>,
    contact_pages: VecDeque<ContactPage>,
}

impl FakeQueues {
    fn from_script(script: &FakeProviderSession) -> Self {
        Self {
            change_pages: script.change_pages.iter().cloned().collect(),
            history_pages: script.history_pages.iter().cloned().collect(),
            contact_pages: script.contact_pages.iter().cloned().collect(),
        }
    }
}

/// Shared call log. Because the factory owns it and clones the handle into
/// every opened session, recorded submission attempts span reopens.
#[derive(Default)]
pub struct FakeRecorder {
    inner: Mutex<Recorded>,
}

#[derive(Default)]
struct Recorded {
    open_attempts: usize,
    opens: usize,
    collection_calls: usize,
    body_calls: Vec<RemoteMessageId>,
    attachment_calls: Vec<(RemoteMessageId, RemoteAttachmentId)>,
    history_requests: Vec<HistoryRequest>,
    sync_requests: Vec<SyncRequest>,
    mutation_requests: Vec<MutationRequest>,
    prepare_requests: Vec<SubmissionRequest>,
    submission_attempts: Vec<SubmissionRequest>,
    reconcile_requests: Vec<SubmissionRequest>,
    finalize_requests: Vec<(SubmissionRequest, SubmissionReceipt)>,
    contact_requests: Vec<(ContactSourceId, Option<ProviderCursor>)>,
}

impl FakeRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.inner.lock().expect("fake recorder poisoned")
    }

    fn record_open_attempt(&self) {
        self.lock().open_attempts += 1;
    }

    fn record_open(&self) {
        self.lock().opens += 1;
    }

    fn record_collections(&self) {
        self.lock().collection_calls += 1;
    }

    fn record_body(&self, id: RemoteMessageId) {
        self.lock().body_calls.push(id);
    }

    fn record_attachment(&self, message: RemoteMessageId, id: RemoteAttachmentId) {
        self.lock().attachment_calls.push((message, id));
    }

    fn record_history(&self, request: HistoryRequest) {
        self.lock().history_requests.push(request);
    }

    fn record_sync(&self, request: SyncRequest) {
        self.lock().sync_requests.push(request);
    }

    fn record_mutation(&self, request: MutationRequest) {
        self.lock().mutation_requests.push(request);
    }

    fn record_prepare(&self, request: SubmissionRequest) {
        self.lock().prepare_requests.push(request);
    }

    fn record_submission(&self, request: SubmissionRequest) {
        self.lock().submission_attempts.push(request);
    }

    fn record_reconcile(&self, request: SubmissionRequest) {
        self.lock().reconcile_requests.push(request);
    }

    fn record_finalize(&self, request: SubmissionRequest, receipt: SubmissionReceipt) {
        self.lock().finalize_requests.push((request, receipt));
    }

    fn record_contact(&self, source: ContactSourceId, cursor: Option<ProviderCursor>) {
        self.lock().contact_requests.push((source, cursor));
    }

    pub fn open_attempts(&self) -> usize {
        self.lock().open_attempts
    }

    /// Successful live opens (no cancellation or scripted open failure).
    pub fn opens(&self) -> usize {
        self.lock().opens
    }

    pub fn collection_calls(&self) -> usize {
        self.lock().collection_calls
    }

    pub fn body_calls(&self) -> Vec<RemoteMessageId> {
        self.lock().body_calls.clone()
    }

    pub fn attachment_calls(&self) -> Vec<(RemoteMessageId, RemoteAttachmentId)> {
        self.lock().attachment_calls.clone()
    }

    pub fn history_requests(&self) -> Vec<HistoryRequest> {
        self.lock().history_requests.clone()
    }

    pub fn sync_requests(&self) -> Vec<SyncRequest> {
        self.lock().sync_requests.clone()
    }

    pub fn mutation_requests(&self) -> Vec<MutationRequest> {
        self.lock().mutation_requests.clone()
    }

    pub fn prepare_requests(&self) -> Vec<SubmissionRequest> {
        self.lock().prepare_requests.clone()
    }

    /// Every `submit` attempt, shared across all sessions this factory opened.
    pub fn submission_attempts(&self) -> Vec<SubmissionRequest> {
        self.lock().submission_attempts.clone()
    }

    pub fn reconcile_requests(&self) -> Vec<SubmissionRequest> {
        self.lock().reconcile_requests.clone()
    }

    pub fn finalize_requests(&self) -> Vec<(SubmissionRequest, SubmissionReceipt)> {
        self.lock().finalize_requests.clone()
    }

    pub fn contact_requests(&self) -> Vec<(ContactSourceId, Option<ProviderCursor>)> {
        self.lock().contact_requests.clone()
    }

    /// Forget every recorded call without touching the scripted behavior.
    pub fn clear(&self) {
        *self.lock() = Recorded::default();
    }
}

/// Builds fake [`ProviderSession`]s and owns the shared [`FakeRecorder`].
pub struct FakeProviderFactory {
    recorder: Arc<FakeRecorder>,
    script: FakeProviderSession,
    capabilities: Option<ProviderCapabilities>,
    services: FakeServices,
    open_error: Option<ProviderError>,
    open_delay: Option<Duration>,
}

impl Default for FakeProviderFactory {
    fn default() -> Self {
        Self {
            recorder: Arc::new(FakeRecorder::new()),
            script: FakeProviderSession::new(),
            capabilities: None,
            services: FakeServices::ALL,
            open_error: None,
            open_delay: None,
        }
    }
}

impl FakeProviderFactory {
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider that only supports submission.
    pub fn submission_only() -> Self {
        Self::default().services(FakeServices::submission_only())
    }

    #[must_use]
    pub fn services(mut self, services: FakeServices) -> Self {
        self.services = services;
        self
    }

    /// Override the capabilities advertised by opened sessions.
    #[must_use]
    pub fn capabilities(mut self, capabilities: ProviderCapabilities) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    #[must_use]
    pub fn session(mut self, script: FakeProviderSession) -> Self {
        self.script = script;
        self
    }

    /// Make every `open` fail with this error after the cancellation/deadline
    /// check.
    #[must_use]
    pub fn fail_open(mut self, error: ProviderError) -> Self {
        self.open_error = Some(error);
        self
    }

    /// Delay every `open` by this long.
    #[must_use]
    pub fn open_delay(mut self, delay: Duration) -> Self {
        self.open_delay = Some(delay);
        self
    }

    /// The shared recorder, for inspecting calls made through any session.
    pub fn recorder(&self) -> &FakeRecorder {
        &self.recorder
    }

    fn derived_capabilities(services: FakeServices) -> ProviderCapabilities {
        ProviderCapabilities {
            mailbox: services.mailbox,
            sync: services.sync,
            mutations: super::types::MutationCapabilities {
                read: services.mutations,
                star: services.mutations,
                labels: services.mutations,
                move_messages: services.mutations,
                archive: services.mutations,
                trash: services.mutations,
                permanent_delete: services.mutations,
            },
            submission: services.submission,
            contacts: super::types::ContactCapabilities {
                read: services.address_book,
                incremental_sync: services.address_book,
                remote_search: false,
                write: false,
                groups: false,
            },
            server_search: false,
        }
    }
}

#[async_trait]
impl ProviderFactory for FakeProviderFactory {
    async fn open(
        &self,
        _account: &Account,
        ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        self.recorder.record_open_attempt();
        if let Some(delay) = self.open_delay {
            tokio::time::sleep(delay).await;
        }
        ctx.ensure_active()?;
        if let Some(error) = &self.open_error {
            return Err(error.clone());
        }
        self.recorder.record_open();

        let capabilities = self
            .capabilities
            .unwrap_or_else(|| Self::derived_capabilities(self.services));
        let script = Arc::new(self.script.clone());
        let queues = Arc::new(Mutex::new(FakeQueues::from_script(&script)));
        let recorder = self.recorder.clone();

        let mut session = ProviderSession::new(capabilities);
        if self.services.mailbox {
            session.mailbox = Some(Box::new(FakeMailbox {
                script: script.clone(),
                queues: queues.clone(),
                recorder: recorder.clone(),
            }));
        }
        if self.services.sync {
            session.sync = Some(Box::new(FakeSync {
                script: script.clone(),
                queues: queues.clone(),
                recorder: recorder.clone(),
            }));
        }
        if self.services.mutations {
            session.mutations = Some(Box::new(FakeMutations {
                script: script.clone(),
                recorder: recorder.clone(),
            }));
        }
        if self.services.submission {
            session.submission = Some(Box::new(FakeSubmission {
                script: script.clone(),
                recorder: recorder.clone(),
            }));
        }
        if self.services.address_book {
            session.address_book = Some(Box::new(FakeAddressBook {
                script: script.clone(),
                queues,
                recorder,
            }));
        }
        Ok(session)
    }
}

fn empty_body() -> BodyContent {
    BodyContent {
        plain_text: None,
        html: None,
        raw: None,
        attachments: Vec::new(),
        signature: SignatureStatus::NotApplicable,
    }
}

struct FakeMailbox {
    script: Arc<FakeProviderSession>,
    queues: Arc<Mutex<FakeQueues>>,
    recorder: Arc<FakeRecorder>,
}

#[async_trait]
impl MailboxService for FakeMailbox {
    async fn collections(
        &mut self,
        ctx: &OperationContext,
    ) -> Result<Vec<CollectionRecord>, ProviderError> {
        self.recorder.record_collections();
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Collections) {
            return Err(error);
        }
        Ok(self.script.collections.clone())
    }

    async fn body(
        &mut self,
        id: &RemoteMessageId,
        ctx: &OperationContext,
    ) -> Result<BodyContent, ProviderError> {
        self.recorder.record_body(id.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Body) {
            return Err(error);
        }
        Ok(self.script.body.clone().unwrap_or_else(empty_body))
    }

    async fn attachment(
        &mut self,
        message: &RemoteMessageId,
        id: &RemoteAttachmentId,
        ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError> {
        self.recorder.record_attachment(message.clone(), id.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Attachment) {
            return Err(error);
        }
        Ok(self.script.attachment.clone().unwrap_or_default())
    }

    async fn history(
        &mut self,
        request: HistoryRequest,
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        self.recorder.record_history(request.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::History) {
            return Err(error);
        }
        let checkpoint = request
            .cursor
            .clone()
            .unwrap_or_else(|| ProviderCursor::new(1, "history-start"));
        let page = self
            .queues
            .lock()
            .expect("fake queues poisoned")
            .history_pages
            .pop_front();
        Ok(page.unwrap_or(HistoryPage {
            messages: Vec::new(),
            checkpoint,
            has_more: false,
            coverage: HistoryCoverage::default(),
        }))
    }
}

struct FakeSync {
    script: Arc<FakeProviderSession>,
    queues: Arc<Mutex<FakeQueues>>,
    recorder: Arc<FakeRecorder>,
}

#[async_trait]
impl SyncService for FakeSync {
    async fn next_page(
        &mut self,
        request: SyncRequest,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        self.recorder.record_sync(request.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Sync) {
            return Err(error);
        }
        let page = self
            .queues
            .lock()
            .expect("fake queues poisoned")
            .change_pages
            .pop_front();
        if let Some(page) = page {
            return Ok(page);
        }
        let checkpoint = if self.script.echo_sync_cursor {
            request
                .cursor
                .clone()
                .unwrap_or_else(|| ProviderCursor::new(1, "sync-start"))
        } else {
            ProviderCursor::new(1, "sync-start")
        };
        Ok(ChangePage {
            changes: Vec::new(),
            checkpoint,
            has_more: false,
        })
    }
}

struct FakeMutations {
    script: Arc<FakeProviderSession>,
    recorder: Arc<FakeRecorder>,
}

#[async_trait]
impl MutationService for FakeMutations {
    async fn apply(
        &mut self,
        request: MutationRequest,
        ctx: &OperationContext,
    ) -> Result<MutationResult, ProviderError> {
        self.recorder.record_mutation(request.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Mutation) {
            return Err(error);
        }
        if let Some(result) = &self.script.mutation_result {
            return Ok(result.clone());
        }
        Ok(MutationResult {
            items: request
                .targets
                .iter()
                .map(|message| MutationItemResult {
                    message: message.clone(),
                    outcome: MutationOutcome::Applied,
                })
                .collect(),
        })
    }
}

struct FakeSubmission {
    script: Arc<FakeProviderSession>,
    recorder: Arc<FakeRecorder>,
}

#[async_trait]
impl SubmissionService for FakeSubmission {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        self.script.sent_copy_policy
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        self.recorder.record_prepare(request.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Prepare) {
            return Err(error);
        }
        Ok(self
            .script
            .prepared
            .clone()
            .unwrap_or_else(|| PreparedSubmission {
                version: 1,
                backend: ProviderId::from("fake"),
                raw: None,
                reference: Some(format!("prepared:{}", request.operation)),
            }))
    }

    async fn submit(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        self.recorder.record_submission(request.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Submit) {
            return Err(error);
        }
        Ok(self.script.submission_outcome.clone().unwrap_or_else(|| {
            SubmissionOutcome::Accepted(SubmissionReceipt {
                operation: request.operation.clone(),
                remote_message: None,
                remote_thread: None,
            })
        }))
    }

    async fn reconcile(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        self.recorder.record_reconcile(request.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Reconcile) {
            return Err(error);
        }
        Ok(self
            .script
            .reconcile_outcome
            .clone()
            .unwrap_or(ReconcileOutcome::NotAccepted))
    }

    async fn finalize_sent(
        &mut self,
        request: &SubmissionRequest,
        receipt: &SubmissionReceipt,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.recorder
            .record_finalize(request.clone(), receipt.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::FinalizeSent) {
            return Err(error);
        }
        Ok(())
    }
}

struct FakeAddressBook {
    script: Arc<FakeProviderSession>,
    queues: Arc<Mutex<FakeQueues>>,
    recorder: Arc<FakeRecorder>,
}

#[async_trait]
impl AddressBookService for FakeAddressBook {
    async fn next_page(
        &mut self,
        source: &ContactSourceId,
        cursor: Option<ProviderCursor>,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        self.recorder.record_contact(source.clone(), cursor.clone());
        self.script.settle().await;
        ctx.ensure_active()?;
        if let Some(error) = self.script.failure(FakeCall::Contacts) {
            return Err(error);
        }
        let page = self
            .queues
            .lock()
            .expect("fake queues poisoned")
            .contact_pages
            .pop_front();
        Ok(page.unwrap_or(ContactPage {
            changes: Vec::new(),
            checkpoint: cursor.unwrap_or_else(|| ProviderCursor::new(1, "contacts-start")),
            has_more: false,
            kind: crate::providers::ContactPageKind::Snapshot,
        }))
    }
}
