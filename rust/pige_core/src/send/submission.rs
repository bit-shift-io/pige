//! Provider-independent durable submission.
//!
//! The outbox is the source of truth for every outgoing message. A submission
//! is persisted with a stable operation id *before* any network call, then moves
//! through this state machine:
//!
//! `Draft -> Queued -> Preparing -> Submitting -> Accepted -> Complete`
//!
//! `Rejected` is a terminal, evidenced refusal. `Uncertain` means the durable
//! outcome cannot be proven (a crash or timeout during a non-idempotent write);
//! it is never retried automatically. The only network work a later flush may
//! repeat is a Sent-copy finalize, which is safe because acceptance is recorded
//! separately from the copy and the adapter must prove the copy is absent before
//! appending it.
//!
//! No secrets are persisted here. A [`PreparedSubmission`] contains only a
//! version, backend id, optional prepared bytes, and an opaque adapter reference.

use crate::account::Account;
use crate::mail::{Address, MAX_ATTACHMENT_BYTES};
use crate::providers::{
    ErrorKind, OperationContext, OperationId, PreparedSubmission, ProviderError, ProviderId,
    ReconcileOutcome, SentCopyPolicy, SubmissionAttachment, SubmissionOutcome, SubmissionReceipt,
    SubmissionRequest, SubmissionService,
};
use crate::store::{Store, StoreError};

use super::compose::build_mime_with_parts;
use super::outbox::{OutboxEntry, OutboxId, OutboxReport};

/// The durable lifecycle of one outgoing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionState {
    /// Saved locally; never sent.
    Draft,
    /// Persisted before network work, waiting for a flush.
    Queued,
    /// Claimed and preparing provider-side artifacts (for example a draft).
    Preparing,
    /// The prepared artifact is durable and the provider submission is in
    /// flight. If this state survives a crash the outcome is ambiguous.
    Submitting,
    /// The provider accepted delivery. A client-appended Sent copy may remain.
    Accepted,
    /// Delivery and the Sent-copy policy are both finished.
    Complete,
    /// The provider or MIME construction refused the message permanently.
    Rejected,
    /// The write may or may not have reached the provider. Never auto-retried.
    Uncertain,
}

impl SubmissionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Submitting => "submitting",
            Self::Accepted => "accepted",
            Self::Complete => "complete",
            Self::Rejected => "rejected",
            Self::Uncertain => "uncertain",
        }
    }

    /// Decode both the current names and the legacy v4 outbox states.
    ///
    /// A legacy `sending` row may already have been accepted, so it maps to
    /// `Uncertain` (never resent). A legacy `sent_local` row was accepted by the
    /// SMTP transport and only its Sent copy remains; it maps to `Accepted`.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "draft" => Some(Self::Draft),
            "queued" => Some(Self::Queued),
            "preparing" => Some(Self::Preparing),
            "submitting" => Some(Self::Submitting),
            "accepted" => Some(Self::Accepted),
            "complete" => Some(Self::Complete),
            "rejected" => Some(Self::Rejected),
            "uncertain" => Some(Self::Uncertain),
            // Legacy v4 states.
            "sending" => Some(Self::Uncertain),
            "sent_local" => Some(Self::Accepted),
            "sent" => Some(Self::Complete),
            "failed" => Some(Self::Uncertain),
            _ => None,
        }
    }

    /// Whether a flush must inspect this row.
    pub fn is_pending(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Preparing | Self::Submitting | Self::Accepted
        )
    }

    /// Compatibility aliases for the pre-F9 outbox vocabulary. They keep the
    /// local draft/status UI and older callers compiling without resurrecting the
    /// unsafe "retry `sending`" behaviour.
    #[allow(non_upper_case_globals)]
    pub const Sending: Self = Self::Submitting;
    #[allow(non_upper_case_globals)]
    pub const SentLocal: Self = Self::Accepted;
    #[allow(non_upper_case_globals)]
    pub const Sent: Self = Self::Complete;
    #[allow(non_upper_case_globals)]
    pub const Failed: Self = Self::Rejected;
}

impl std::str::FromStr for SubmissionState {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_db(value).ok_or(())
    }
}

/// The maximum total attachment payload for one submission.
///
/// The per-part cap is the existing [`MAX_ATTACHMENT_BYTES`] reader cap; the
/// total cap prevents a message that is individually valid from exhausting
/// memory, disk, or an SMTP transaction.
pub const MAX_SUBMISSION_ATTACHMENT_BYTES: u64 = MAX_ATTACHMENT_BYTES;

/// Observe a first-time acceptance so a future learning task (R1) can train on
/// successful outgoing mail. The callback is invoked *after* the acceptance and
/// receipt have committed in one transaction, and only for the transition that
/// first records the acceptance.
pub trait SubmissionObserver: Send {
    fn accepted(&mut self, entry: &OutboxEntry, receipt: &SubmissionReceipt);
}

/// Default observer used by the production flush: acceptance is recorded
/// durably now, but no learning is performed until R1 installs a real hook.
struct NoopObserver;

impl SubmissionObserver for NoopObserver {
    fn accepted(&mut self, _entry: &OutboxEntry, _receipt: &SubmissionReceipt) {}
}

/// Validate structured parts before anything is persisted or prepared.
pub fn validate_attachments(attachments: &[SubmissionAttachment]) -> Result<(), StoreError> {
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for attachment in attachments {
        if attachment.id.trim().is_empty() {
            return Err(StoreError::InvalidAttachment {
                reason: "attachment id must not be empty".to_string(),
            });
        }
        if !seen.insert(attachment.id.as_str()) {
            return Err(StoreError::InvalidAttachment {
                reason: format!("duplicate attachment id {:?}", attachment.id),
            });
        }
        let size = attachment.bytes.len() as u64;
        if size > MAX_ATTACHMENT_BYTES {
            return Err(StoreError::AttachmentTooLarge {
                size,
                limit: MAX_ATTACHMENT_BYTES,
            });
        }
        total = total.saturating_add(size);
        if total > MAX_SUBMISSION_ATTACHMENT_BYTES {
            return Err(StoreError::AttachmentsTooLarge {
                size: total,
                limit: MAX_SUBMISSION_ATTACHMENT_BYTES,
            });
        }
    }
    Ok(())
}

/// Atomically persist a queued submission and its structured parts.
///
/// Validation happens before the transaction, so an oversized part never
/// creates an outbox row (and therefore can never reach delivery).
pub fn queue_submission(
    store: &Store,
    message: super::OutgoingMessage,
    attachments: Vec<SubmissionAttachment>,
) -> Result<OutboxId, StoreError> {
    store.insert_outbox_with_parts(&message, SubmissionState::Queued, &attachments)
}

/// Deliver every pending row for one account through a provider submission
/// service. Per-message failures are recorded on the row and in the report;
/// only an unrecoverable store failure returns `Err`.
pub async fn flush_provider_outbox(
    store: &Store,
    account: &Account,
    service: &mut dyn SubmissionService,
    ctx: &OperationContext,
) -> Result<OutboxReport, StoreError> {
    let mut observer = NoopObserver;
    flush_provider_outbox_with_observer(store, account, service, ctx, &mut observer).await
}

/// Observable variant used by tests and, later, R1 learning.
pub async fn flush_provider_outbox_with_observer(
    store: &Store,
    account: &Account,
    service: &mut dyn SubmissionService,
    ctx: &OperationContext,
    observer: &mut dyn SubmissionObserver,
) -> Result<OutboxReport, StoreError> {
    let from = Address::new(
        Some(account.display_name.clone()).filter(|name| !name.trim().is_empty()),
        account.email.clone(),
    );
    let pending = store.outbox_pending(&account.id)?;
    let mut report = OutboxReport::default();

    for entry in pending {
        // A cancelled or expired context must not start new network work. Rows
        // that were already accepted are left exactly where they are.
        if ctx.ensure_active().is_err() {
            if entry.state == SubmissionState::Accepted {
                report.sent_local += 1;
            }
            continue;
        }

        match entry.state {
            SubmissionState::Queued => {
                process_queued(store, account, &from, service, ctx, &entry, observer, &mut report)
                    .await?;
            }
            SubmissionState::Preparing => {
                recover_preparing(store, account, &from, service, ctx, &entry, observer, &mut report)
                    .await?;
            }
            SubmissionState::Submitting => {
                recover_submitting(store, account, &from, service, ctx, &entry, observer, &mut report)
                    .await?;
            }
            SubmissionState::Accepted => {
                finalize_accepted(store, account, service, ctx, &entry, &mut report).await?;
            }
            SubmissionState::Uncertain => {}
            SubmissionState::Draft
            | SubmissionState::Complete
            | SubmissionState::Rejected => {}
        }
    }

    // `queued` keeps its original meaning: rows still waiting for *delivery*.
    // Accepted rows are reported through `sent_local` because only their Sent
    // copy is outstanding.
    report.queued = store.count_outbox_state(&account.id, SubmissionState::Queued)?
        + store.count_outbox_state(&account.id, SubmissionState::Preparing)?
        + store.count_outbox_state(&account.id, SubmissionState::Submitting)?;
    report.uncertain = store.count_outbox_state(&account.id, SubmissionState::Uncertain)?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn process_queued(
    store: &Store,
    account: &Account,
    from: &Address,
    service: &mut dyn SubmissionService,
    ctx: &OperationContext,
    entry: &OutboxEntry,
    observer: &mut dyn SubmissionObserver,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    let attachments = store.outbox_parts(&entry.id)?;
    // Re-check limits at use time; a database edited outside this process must
    // not be able to make the outbox deliver an oversized message.
    if let Err(error) = validate_attachments(&attachments) {
        reject(store, entry, &error.to_string(), report)?;
        return Ok(());
    }

    let raw = match &entry.raw {
        Some(raw) => raw.clone(),
        None => match build_mime_with_parts(from, &entry.message, &attachments) {
            Ok(raw) => raw,
            Err(error) => {
                reject(store, entry, &error.to_string(), report)?;
                return Ok(());
            }
        },
    };
    if entry.message.recipients().is_empty() {
        reject(store, entry, "message has no recipients", report)?;
        return Ok(());
    }

    // Persist the stable operation id and backend, then claim the row exactly
    // once. Building MIME above is deterministic and has no network side effect.
    if !store.claim_outbox_for_prepare(&entry.id, &entry.operation_id, &account.provider_id())? {
        return Ok(());
    }

    // The raw MIME built here is the exact artifact offered to prepare, so a
    // retry reuses it instead of generating a second Message-ID. It is persisted
    // as the prepared artifact once prepare succeeds.
    let request = SubmissionRequest {
        operation: entry.operation_id.clone(),
        message: entry.message.clone(),
        attachments,
        prepared: entry.prepared.clone().or_else(|| {
            Some(PreparedSubmission {
                version: 0,
                backend: account.provider_id(),
                raw: Some(raw),
                reference: None,
            })
        }),
    };

    let prepared = match service.prepare(&request, ctx).await {
        Ok(prepared) => prepared,
        Err(error) => {
            handle_prepare_error(store, entry, &error, report)?;
            return Ok(());
        }
    };
    let request = SubmissionRequest {
        prepared: Some(prepared.clone()),
        ..request
    };
    if !store.set_outbox_prepared_submitting(&entry.id, &prepared, prepared.raw.as_deref())? {
        return Ok(());
    }
    if ctx.ensure_active().is_err() {
        store.requeue_outbox(&entry.id, None, None)?;
        report.queued += 1;
        return Ok(());
    }

    match service.submit(&request, ctx).await {
        Ok(SubmissionOutcome::Accepted(receipt)) => {
            finish_accepted(store, entry, service, ctx, &request, receipt, observer, report).await?;
        }
        Ok(SubmissionOutcome::Rejected(error)) => {
            reject(store, entry, &error.safe_message, report)?;
        }
        Ok(SubmissionOutcome::Unknown) => {
            mark_uncertain(
                store,
                entry,
                "the provider could not confirm whether the message was accepted",
                report,
            )?;
        }
        Err(error) => handle_submit_error(store, entry, &error, report)?,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn recover_preparing(
    store: &Store,
    account: &Account,
    from: &Address,
    service: &mut dyn SubmissionService,
    ctx: &OperationContext,
    entry: &OutboxEntry,
    observer: &mut dyn SubmissionObserver,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    let request = request_from_entry(store, entry)?;
    match service.reconcile(&request, ctx).await {
        Ok(ReconcileOutcome::Accepted(receipt)) => {
            finish_accepted(store, entry, service, ctx, &request, receipt, observer, report).await?;
        }
        Ok(ReconcileOutcome::NotAccepted) => {
            // Preparation may have created a remote draft, but no delivery was
            // accepted. Record the evidence and retry from the beginning.
            store.requeue_outbox(&entry.id, None, None)?;
            if let Some(fresh) = store.outbox_entry(&entry.id)? {
                process_queued(store, account, from, service, ctx, &fresh, observer, report)
                    .await?;
            }
        }
        Ok(ReconcileOutcome::Unknown) | Err(_) => {
            mark_uncertain(
                store,
                entry,
                "interrupted preparation could not be reconciled",
                report,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn recover_submitting(
    store: &Store,
    account: &Account,
    from: &Address,
    service: &mut dyn SubmissionService,
    ctx: &OperationContext,
    entry: &OutboxEntry,
    observer: &mut dyn SubmissionObserver,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    let request = request_from_entry(store, entry)?;
    match service.reconcile(&request, ctx).await {
        Ok(ReconcileOutcome::Accepted(receipt)) => {
            finish_accepted(store, entry, service, ctx, &request, receipt, observer, report).await?;
        }
        Ok(ReconcileOutcome::NotAccepted) => {
            // The provider explicitly proved the write did not happen, so a
            // fresh submission is safe.
            store.requeue_outbox(&entry.id, None, None)?;
            if let Some(fresh) = store.outbox_entry(&entry.id)? {
                process_queued(store, account, from, service, ctx, &fresh, observer, report)
                    .await?;
            }
        }
        Ok(ReconcileOutcome::Unknown) | Err(_) => {
            mark_uncertain(
                store,
                entry,
                "the submission was interrupted and its outcome is unknown",
                report,
            )?;
        }
    }
    Ok(())
}

async fn finalize_accepted(
    store: &Store,
    account: &Account,
    service: &mut dyn SubmissionService,
    ctx: &OperationContext,
    entry: &OutboxEntry,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    // A row accepted by one backend must not be completed by a different one.
    // This is the legacy `sent_local` guarantee: it stays on the SMTP backend
    // whose delivery it represents, even if the account later gains a native
    // provider.
    if let Some(backend) = &entry.backend
        && backend != &account.provider_id()
    {
        let message = format!("the Sent copy for this message belongs to backend {backend}");
        store.set_outbox_error(&entry.id, &message)?;
        report.sent_local += 1;
        report.errors.push(message);
        return Ok(());
    }

    if service.sent_copy_policy() == SentCopyPolicy::ServerManaged {
        store.mark_outbox_complete(&entry.id)?;
        report.sent += 1;
        return Ok(());
    }

    let request = match request_from_entry(store, entry) {
        Ok(request) => request,
        Err(error) => {
            let message = error.to_string();
            store.set_outbox_error(&entry.id, &message)?;
            report.sent_local += 1;
            report.errors.push(message);
            return Ok(());
        }
    };

    // A deadline may have expired after acceptance; never submit again, and
    // leave the copy for a later pass.
    if ctx.ensure_active().is_err() {
        report.sent_local += 1;
        return Ok(());
    }

    match service.finalize_sent(&request, &receipt_of(entry, &request), ctx).await {
        Ok(()) => {
            store.mark_outbox_complete(&entry.id)?;
            report.sent += 1;
        }
        Err(error) => {
            let message = format!("Sent copy: {}", error.safe_message);
            store.set_outbox_error(&entry.id, &message)?;
            report.sent_local += 1;
            report.errors.push(message);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn finish_accepted(
    store: &Store,
    entry: &OutboxEntry,
    service: &mut dyn SubmissionService,
    ctx: &OperationContext,
    request: &SubmissionRequest,
    receipt: SubmissionReceipt,
    observer: &mut dyn SubmissionObserver,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    // Acceptance, receipt, and the once-only learning marker share a single
    // transaction. The observer fires only for the transition that wins it.
    let first = store.record_outbox_acceptance(&entry.id, &receipt)?;
    if first {
        observer.accepted(entry, &receipt);
    }

    if service.sent_copy_policy() == SentCopyPolicy::ServerManaged {
        store.mark_outbox_complete(&entry.id)?;
        report.sent += 1;
        return Ok(());
    }

    if ctx.ensure_active().is_err() {
        report.sent_local += 1;
        return Ok(());
    }
    match service.finalize_sent(request, &receipt, ctx).await {
        Ok(()) => {
            store.mark_outbox_complete(&entry.id)?;
            report.sent += 1;
        }
        Err(error) => {
            let message = format!("Sent copy: {}", error.safe_message);
            store.set_outbox_error(&entry.id, &message)?;
            report.sent_local += 1;
            report.errors.push(message);
        }
    }
    Ok(())
}

fn request_from_entry(
    store: &Store,
    entry: &OutboxEntry,
) -> Result<SubmissionRequest, StoreError> {
    let attachments = store.outbox_parts(&entry.id)?;
    let prepared = entry.prepared.clone().or_else(|| {
        entry.raw.as_ref().map(|raw| PreparedSubmission {
            version: 0,
            backend: entry
                .backend
                .clone()
                .unwrap_or_else(|| ProviderId::from(crate::providers::IMAP_SMTP_PROVIDER)),
            raw: Some(raw.clone()),
            reference: None,
        })
    });
    Ok(SubmissionRequest {
        operation: entry.operation_id.clone(),
        message: entry.message.clone(),
        attachments,
        prepared,
    })
}

fn receipt_of(entry: &OutboxEntry, request: &SubmissionRequest) -> SubmissionReceipt {
    entry
        .receipt
        .clone()
        .unwrap_or_else(|| SubmissionReceipt {
            operation: request.operation.clone(),
            remote_message: None,
            remote_thread: None,
        })
}

fn handle_prepare_error(
    store: &Store,
    entry: &OutboxEntry,
    error: &ProviderError,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    match error.kind {
        ErrorKind::UnknownWriteOutcome | ErrorKind::Cancelled => {
            // A draft may exist. Keep `Preparing` so the next flush reconciles
            // the provider evidence before repeating preparation.
            store.set_outbox_error(&entry.id, &error.safe_message)?;
            report.errors.push(error.safe_message.clone());
        }
        error_kind if is_permanent(error_kind) => {
            reject(store, entry, &error.safe_message, report)?;
        }
        _ => {
            store.requeue_outbox(
                &entry.id,
                Some(&error.safe_message),
                error.retry_after.map(|after| after.as_secs() as i64),
            )?;
            report.errors.push(error.safe_message.clone());
        }
    }
    Ok(())
}

fn handle_submit_error(
    store: &Store,
    entry: &OutboxEntry,
    error: &ProviderError,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    match error.kind {
        ErrorKind::UnknownWriteOutcome | ErrorKind::Cancelled => {
            mark_uncertain(store, entry, &error.safe_message, report)?;
        }
        error_kind if is_permanent(error_kind) => {
            reject(store, entry, &error.safe_message, report)?;
        }
        _ => {
            store.requeue_outbox(
                &entry.id,
                Some(&error.safe_message),
                error.retry_after.map(|after| after.as_secs() as i64),
            )?;
            report.errors.push(error.safe_message.clone());
        }
    }
    Ok(())
}

fn is_permanent(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Unsupported
            | ErrorKind::Forbidden
            | ErrorKind::NotFound
            | ErrorKind::Conflict
            | ErrorKind::CursorInvalid
    )
}

fn reject(
    store: &Store,
    entry: &OutboxEntry,
    message: &str,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    store.mark_outbox_rejected(&entry.id, message)?;
    report.failed += 1;
    report.errors.push(message.to_string());
    Ok(())
}

fn mark_uncertain(
    store: &Store,
    entry: &OutboxEntry,
    message: &str,
    report: &mut OutboxReport,
) -> Result<(), StoreError> {
    store.mark_outbox_uncertain(&entry.id, message)?;
    report.errors.push(message.to_string());
    Ok(())
}

/// A stable operation id for a row that predates provider-neutral submission.
pub(crate) fn legacy_operation_id(outbox: &str) -> OperationId {
    OperationId::new(format!("legacy-{outbox}"))
}
