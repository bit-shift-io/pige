//! The durable outbox and its state machine.
//!
//! The authoritative lifecycle is [`SubmissionState`]:
//! - `Draft`: saved locally, never sent.
//! - `Queued`: ready to send, persisted before any network call.
//! - `Preparing`: claimed by a flush while provider-side artifacts are built.
//! - `Submitting`: the prepared artifact is durable; delivery may be in flight.
//! - `Accepted`: the provider accepted delivery. Only a client-appended Sent
//!   copy remains.
//! - `Complete`: delivery and the Sent-copy policy are both finished.
//! - `Rejected`: an evidenced, permanent refusal.
//! - `Uncertain`: the write may or may not have reached the provider. Such rows
//!   are never retried automatically; the user must decide.
//!
//! Flush is deliberately conservative: a row is claimed atomically *before* any
//! network work, an interrupted `Submitting` row is reconciled rather than
//! resent, and acceptance is recorded separately from the Sent copy. The small
//! window between delivery and the Sent copy can therefore cause a missing
//! copy, never a duplicate delivery.
//!
//! The legacy names `Sending`, `SentLocal`, `Sent`, and `Failed` remain as
//! compatibility aliases for the pre-provider vocabulary.

use async_trait::async_trait;
use rusqlite::{Row, params};

use crate::account::{Account, AccountId};
use crate::contacts::{EvidenceId, LearningOrigin, Observation};
use crate::mail::{Address, AttachmentDisposition, Flags, Uid};
use crate::providers::{
    ErrorKind, OperationContext, OperationId, PreparedSubmission, ProviderError, ProviderId,
    SubmissionAttachment, SubmissionOutcome, SubmissionReceipt, SubmissionRequest,
    SubmissionService, SentCopyPolicy,
};
use crate::store::{Store, StoreError, check_generation_in, record_observations_in};
use crate::transport::{
    Capabilities, FlagOp, FolderState, MailTransport, RemoteFolder, RemoteMessage, SendError,
    SmtpSender, TransportError, UidRange,
};

use super::OutgoingMessage;
use super::compose::build_mime;
use super::submission::SubmissionState;

/// Compatibility name for the durable submission lifecycle. New code should
/// use [`SubmissionState`]; the alias keeps older callers and the UI compiling.
pub type OutboxState = SubmissionState;

/// Stable identifier for one outbox row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutboxId(pub String);

impl OutboxId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for OutboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for OutboxId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One outbox row, including the compose fields and the exact raw bytes once
/// they have been generated. Provider-neutral submission metadata is kept in
/// separate fields so a receipt, a prepared artifact, and retry diagnostics can
/// never overwrite one another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    pub id: OutboxId,
    pub message: OutgoingMessage,
    pub state: SubmissionState,
    pub error: Option<String>,
    pub raw: Option<Vec<u8>>,
    pub operation_id: OperationId,
    pub backend: Option<ProviderId>,
    pub prepared: Option<PreparedSubmission>,
    pub receipt: Option<SubmissionReceipt>,
    pub retry_count: u32,
    pub retry_after: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Result of one flush. Per-message failures are reported here rather than
/// aborting the whole sync; only an unrecoverable store failure returns `Err`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxReport {
    /// Fully delivered and, when possible, the Sent copy is complete.
    pub sent: u32,
    /// Accepted, but the Sent copy still has to be retried.
    pub sent_local: u32,
    /// Permanently rejected this pass.
    pub failed: u32,
    /// Still queued or interrupted and waiting for a later attempt.
    pub queued: u32,
    /// Rows whose outcome cannot be proven; never retried automatically.
    pub uncertain: u32,
    pub errors: Vec<String>,
}

const COLUMNS: &str = "id, account_id, to_json, cc_json, bcc_json, subject, body, \
     in_reply_to, references_json, raw, state, error, created_at, updated_at, \
     operation_id, backend, prepared_version, prepared_backend, prepared_raw, \
     prepared_reference, receipt_operation, receipt_remote_message, \
     receipt_remote_thread, retry_count, retry_after";

/// Optional learning payload committed with the once-only acceptance
/// transition. Keeping it in the same transaction is what guarantees a replay
/// cannot train the same submission twice.
struct AcceptanceLearning<'a> {
    ctx: &'a OperationContext,
    fingerprint: Option<&'a str>,
    observations: &'a [Observation],
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

fn decode_json<T: serde::de::DeserializeOwned>(
    raw: &str,
    field: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(raw).map_err(|_| StoreError::corrupt(field, raw))
}

impl Store {
    /// Insert a new row in `state`, returning its id.
    pub(crate) fn insert_outbox(
        &self,
        message: &OutgoingMessage,
        state: OutboxState,
    ) -> Result<OutboxId, StoreError> {
        self.insert_outbox_with_parts(message, state, &[])
    }

    /// Insert a row and its structured parts in one transaction.
    ///
    /// Validation runs before the transaction so an oversized submission never
    /// creates an outbox row. Parts keep the caller's stable ids and are deleted
    /// with the row by the foreign-key cascade.
    pub(crate) fn insert_outbox_with_parts(
        &self,
        message: &OutgoingMessage,
        state: OutboxState,
        attachments: &[SubmissionAttachment],
    ) -> Result<OutboxId, StoreError> {
        super::submission::validate_attachments(attachments)?;

        let id = OutboxId::new();
        let operation = OperationId::new(uuid::Uuid::new_v4().to_string());
        let backend = self
            .accounts()?
            .into_iter()
            .find(|account| account.id == message.account_id)
            .map(|account| account.provider_id());
        let now = now_unix();

        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO outbox (
                 id, account_id, to_json, cc_json, bcc_json, subject, body,
                 in_reply_to, references_json, raw, state, error, created_at,
                 updated_at, operation_id, backend
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, NULL, ?11,
                       ?11, ?12, ?13)",
            params![
                id.0,
                message.account_id.0,
                encode_json(&message.to)?,
                encode_json(&message.cc)?,
                encode_json(&message.bcc)?,
                message.subject,
                message.body,
                message.in_reply_to,
                encode_json(&message.references)?,
                state.as_str(),
                now,
                operation.0,
                backend.as_ref().map(|backend| backend.0.clone()),
            ],
        )?;
        for (position, attachment) in attachments.iter().enumerate() {
            tx.execute(
                "INSERT INTO outbox_parts (
                     outbox_id, part_id, position, filename, content_type,
                     content_id, disposition, bytes
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id.0,
                    &attachment.id,
                    position as i64,
                    &attachment.filename,
                    &attachment.content_type,
                    attachment.content_id.as_deref(),
                    attachment.disposition.as_str(),
                    &attachment.bytes,
                ],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn outbox_entry(&self, id: &OutboxId) -> Result<Option<OutboxEntry>, StoreError> {
        let sql = format!("SELECT {COLUMNS} FROM outbox WHERE id = ?1");
        let conn = self.connection();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([&id.0])?;
        match rows.next()? {
            Some(row) => Ok(Some(entry_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Every outbox row, newest first, optionally scoped to one account.
    pub fn outbox_items(
        &self,
        account: Option<&AccountId>,
    ) -> Result<Vec<OutboxEntry>, StoreError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM outbox
             WHERE (?1 IS NULL OR account_id = ?1)
             ORDER BY updated_at DESC, id"
        );
        let conn = self.connection();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([account.map(|account| account.0.as_str())])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(entry_from_row(row)?);
        }
        Ok(items)
    }

    pub fn outbox_drafts(
        &self,
        account: Option<&AccountId>,
    ) -> Result<Vec<OutboxEntry>, StoreError> {
        Ok(self
            .outbox_items(account)?
            .into_iter()
            .filter(|entry| entry.state == OutboxState::Draft)
            .collect())
    }

    /// Rows a flush should inspect: queued, accepted with a Sent copy pending,
    /// and interrupted preparations/submissions. Terminal and uncertain rows are
    /// deliberately excluded so they are never retried automatically.
    pub fn outbox_pending(&self, account: &AccountId) -> Result<Vec<OutboxEntry>, StoreError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM outbox
             WHERE account_id = ?1 AND state IN
                 ('queued', 'preparing', 'submitting', 'accepted',
                  'sent_local', 'sending')
             ORDER BY created_at, id"
        );
        let conn = self.connection();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([&account.0])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(entry_from_row(row)?);
        }
        Ok(items)
    }

    pub fn set_outbox_state(
        &self,
        id: &OutboxId,
        state: OutboxState,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox SET state = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
            params![state.as_str(), error, now_unix(), id.0],
        )?;
        Ok(())
    }

    pub fn set_outbox_raw(&self, id: &OutboxId, raw: &[u8]) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox SET raw = ?1, updated_at = ?2 WHERE id = ?3",
            params![raw, now_unix(), id.0],
        )?;
        Ok(())
    }

    /// Atomically claim a queued row for preparation, persisting the stable
    /// operation id and backend before any network work. Only one flush can win
    /// the transition out of `queued`.
    pub fn claim_outbox_for_prepare(
        &self,
        id: &OutboxId,
        operation: &OperationId,
        backend: &ProviderId,
    ) -> Result<bool, StoreError> {
        let changed = self.connection().execute(
            "UPDATE outbox
             SET state = 'preparing',
                 operation_id = COALESCE(operation_id, ?1),
                 backend = COALESCE(backend, ?2),
                 error = NULL,
                 updated_at = ?3
             WHERE id = ?4 AND state = 'queued'",
            params![operation.0, backend.0, now_unix(), id.0],
        )?;
        Ok(changed == 1)
    }

    /// Persist the prepared artifact and move to `submitting` in one write. The
    /// prepared bytes are also mirrored into `raw` for the legacy Sent-append
    /// path and diagnostics.
    pub fn set_outbox_prepared_submitting(
        &self,
        id: &OutboxId,
        prepared: &PreparedSubmission,
        raw: Option<&[u8]>,
    ) -> Result<bool, StoreError> {
        let changed = self.connection().execute(
            "UPDATE outbox
             SET state = 'submitting',
                 prepared_version = ?1,
                 prepared_backend = ?2,
                 prepared_raw = ?3,
                 prepared_reference = ?4,
                 raw = COALESCE(?3, raw),
                 error = NULL,
                 updated_at = ?5
             WHERE id = ?6 AND state = 'preparing'",
            params![
                prepared.version,
                prepared.backend.0,
                raw,
                prepared.reference,
                now_unix(),
                id.0,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Record acceptance, the provider receipt, and the once-only learning
    /// marker in one transaction. Returns true only for the transition that
    /// actually recorded the acceptance, so a future learning hook cannot train
    /// the same submission twice.
    pub fn record_outbox_acceptance(
        &self,
        id: &OutboxId,
        receipt: &SubmissionReceipt,
    ) -> Result<bool, StoreError> {
        self.record_outbox_acceptance_in(id, receipt, None)
    }

    /// Record acceptance, the receipt, and accepted-send correspondent
    /// observations in one transaction.
    ///
    /// `fingerprint` is the normalized full-header fingerprint of the exact
    /// prepared bytes when available, so a later Sent-copy history replay
    /// converges on this evidence instead of counting the recipients twice.
    /// Returns true only for the transition that actually recorded acceptance.
    pub fn record_outbox_acceptance_with_learning(
        &self,
        id: &OutboxId,
        receipt: &SubmissionReceipt,
        ctx: &OperationContext,
        fingerprint: Option<&str>,
        observations: &[Observation],
    ) -> Result<bool, StoreError> {
        self.record_outbox_acceptance_in(
            id,
            receipt,
            Some(AcceptanceLearning {
                ctx,
                fingerprint,
                observations,
            }),
        )
    }

    fn record_outbox_acceptance_in(
        &self,
        id: &OutboxId,
        receipt: &SubmissionReceipt,
        learning: Option<AcceptanceLearning<'_>>,
    ) -> Result<bool, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        if let Some(learning) = &learning
            && learning.ctx.generation != 0
        {
            // The legacy SMTP/IMAP entry point has no account-session context
            // and uses generation 0 as its sentinel; every real provider/history
            // context carries a positive durable generation and is fenced here.
            check_generation_in(&tx, &learning.ctx.account, learning.ctx.generation)?;
        }
        let changed = tx.execute(
            "UPDATE outbox
             SET state = 'accepted',
                 receipt_operation = ?1,
                 receipt_remote_message = ?2,
                 receipt_remote_thread = ?3,
                 acceptance_recorded = 1,
                 retry_after = NULL,
                 error = NULL,
                 updated_at = ?4
             WHERE id = ?5
               AND acceptance_recorded = 0
               AND state IN ('preparing', 'submitting', 'queued')",
            params![
                receipt.operation.0,
                receipt.remote_message.as_ref().map(|id| id.0.clone()),
                receipt.remote_thread.as_ref().map(|id| id.0.clone()),
                now_unix(),
                id.0,
            ],
        )?;
        if changed == 1
            && let Some(learning) = &learning
        {
            let evidence = EvidenceId::for_operation(&receipt.operation);
            record_observations_in(
                &tx,
                &learning.ctx.account,
                &evidence,
                LearningOrigin::AcceptedSend,
                learning.fingerprint,
                learning.observations,
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn mark_outbox_complete(&self, id: &OutboxId) -> Result<bool, StoreError> {
        let changed = self.connection().execute(
            "UPDATE outbox SET state = 'complete', error = NULL, updated_at = ?1
             WHERE id = ?2 AND state = 'accepted'",
            params![now_unix(), id.0],
        )?;
        Ok(changed == 1)
    }

    pub fn mark_outbox_rejected(&self, id: &OutboxId, error: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox
             SET state = 'rejected', error = ?1, retry_count = retry_count + 1,
                 updated_at = ?2
             WHERE id = ?3",
            params![error, now_unix(), id.0],
        )?;
        Ok(())
    }

    /// Mark an unresolved write as terminal. The compare-and-swap only applies
    /// to states that were still in flight or waiting, so a late recovery
    /// result can never overwrite an acceptance that already won.
    pub fn mark_outbox_uncertain(&self, id: &OutboxId, error: &str) -> Result<bool, StoreError> {
        let changed = self.connection().execute(
            "UPDATE outbox
             SET state = 'uncertain', error = ?1, retry_count = retry_count + 1,
                 updated_at = ?2
             WHERE id = ?3 AND state IN ('queued', 'preparing', 'submitting')",
            params![error, now_unix(), id.0],
        )?;
        Ok(changed == 1)
    }

    /// Return a row to `queued` with retry metadata. Safe only when the caller
    /// has evidence that no delivery was accepted.
    pub fn requeue_outbox(
        &self,
        id: &OutboxId,
        error: Option<&str>,
        retry_after: Option<i64>,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox
             SET state = 'queued', error = ?1, retry_count = retry_count + 1,
                 retry_after = ?2, updated_at = ?3
             WHERE id = ?4",
            params![error, retry_after, now_unix(), id.0],
        )?;
        Ok(())
    }

    /// Structured parts in their original order. Bytes are stored in the row so
    /// they survive a process restart without relying on a cache file.
    pub fn outbox_parts(&self, id: &OutboxId) -> Result<Vec<SubmissionAttachment>, StoreError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT part_id, filename, content_type, bytes, content_id, disposition
             FROM outbox_parts WHERE outbox_id = ?1 ORDER BY position, part_id",
        )?;
        let mut rows = stmt.query([&id.0])?;
        let mut parts = Vec::new();
        while let Some(row) = rows.next()? {
            let disposition: String = row.get(5)?;
            parts.push(SubmissionAttachment {
                id: row.get(0)?,
                filename: row.get(1)?,
                content_type: row.get(2)?,
                bytes: row.get(3)?,
                content_id: row.get(4)?,
                disposition: AttachmentDisposition::from_db(&disposition),
            });
        }
        Ok(parts)
    }

    /// Persist the generated bytes and claim the row atomically. A row can only
    /// leave `queued` once, so overlapping flushes cannot both send it.
    pub fn claim_outbox(&self, id: &OutboxId, raw: &[u8]) -> Result<bool, StoreError> {
        let changed = self.connection().execute(
            "UPDATE outbox
             SET state = 'sending', raw = ?1, error = NULL, updated_at = ?2
             WHERE id = ?3 AND state = 'queued'",
            params![raw, now_unix(), id.0],
        )?;
        Ok(changed == 1)
    }

    pub fn set_outbox_error(&self, id: &OutboxId, error: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox SET error = ?1, updated_at = ?2 WHERE id = ?3",
            params![error, now_unix(), id.0],
        )?;
        Ok(())
    }

    pub fn delete_outbox(&self, id: &OutboxId) -> Result<bool, StoreError> {
        let changed = self
            .connection()
            .execute("DELETE FROM outbox WHERE id = ?1", [&id.0])?;
        Ok(changed == 1)
    }

    pub fn count_outbox_state(
        &self,
        account: &AccountId,
        state: OutboxState,
    ) -> Result<u32, StoreError> {
        Ok(self.connection().query_row(
            "SELECT COUNT(*) FROM outbox WHERE account_id = ?1 AND state = ?2",
            params![account.0, state.as_str()],
            |row| row.get(0),
        )?)
    }
}

fn entry_from_row(row: &Row<'_>) -> Result<OutboxEntry, StoreError> {
    let id: String = row.get(0)?;
    let to_json: String = row.get(2)?;
    let cc_json: String = row.get(3)?;
    let bcc_json: String = row.get(4)?;
    let references_json: String = row.get(8)?;
    let state: String = row.get(10)?;
    let operation_id: Option<String> = row.get(14)?;
    let backend: Option<String> = row.get(15)?;
    let prepared_version: Option<u32> = row.get(16)?;
    let prepared_backend: Option<String> = row.get(17)?;
    let prepared_raw: Option<Vec<u8>> = row.get(18)?;
    let prepared_reference: Option<String> = row.get(19)?;
    let receipt_operation: Option<String> = row.get(20)?;
    let receipt_remote_message: Option<String> = row.get(21)?;
    let receipt_remote_thread: Option<String> = row.get(22)?;

    let prepared = match (prepared_version, prepared_backend) {
        (Some(version), Some(backend)) => Some(PreparedSubmission {
            version,
            backend: ProviderId::from(backend),
            raw: prepared_raw,
            reference: prepared_reference,
        }),
        _ => None,
    };
    let receipt = receipt_operation.map(|operation| SubmissionReceipt {
        operation: OperationId::new(operation),
        remote_message: receipt_remote_message.map(crate::providers::RemoteMessageId::new),
        remote_thread: receipt_remote_thread.map(crate::providers::RemoteThreadId::new),
    });

    Ok(OutboxEntry {
        id: OutboxId(id.clone()),
        message: OutgoingMessage {
            account_id: AccountId(row.get(1)?),
            to: decode_json(&to_json, "outbox.to_json")?,
            cc: decode_json(&cc_json, "outbox.cc_json")?,
            bcc: decode_json(&bcc_json, "outbox.bcc_json")?,
            subject: row.get(5)?,
            body: row.get(6)?,
            in_reply_to: row.get(7)?,
            references: decode_json(&references_json, "outbox.references_json")?,
        },
        state: SubmissionState::from_db(&state)
            .ok_or_else(|| StoreError::corrupt("outbox.state", state))?,
        error: row.get(11)?,
        raw: row.get(9)?,
        operation_id: operation_id
            .map(OperationId::new)
            .unwrap_or_else(|| super::submission::legacy_operation_id(&id)),
        backend: backend.map(ProviderId::from),
        prepared,
        receipt,
        retry_count: row.get(23)?,
        retry_after: row.get(24)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

/// Save a new draft locally. Drafts never touch the network.
pub fn save_draft(store: &Store, message: OutgoingMessage) -> Result<OutboxId, StoreError> {
    store.insert_outbox(&message, OutboxState::Draft)
}

/// Replace an existing draft's fields. The row keeps its id so the Drafts list
/// stays stable while the user edits.
pub fn update_draft(
    store: &Store,
    id: &OutboxId,
    message: &OutgoingMessage,
) -> Result<(), StoreError> {
    store.connection().execute(
        "UPDATE outbox SET
             account_id = ?1, to_json = ?2, cc_json = ?3, bcc_json = ?4,
             subject = ?5, body = ?6, in_reply_to = ?7, references_json = ?8,
             updated_at = ?9
         WHERE id = ?10",
        params![
            message.account_id.0,
            encode_json(&message.to)?,
            encode_json(&message.cc)?,
            encode_json(&message.bcc)?,
            message.subject,
            message.body,
            message.in_reply_to,
            encode_json(&message.references)?,
            now_unix(),
            id.0,
        ],
    )?;
    Ok(())
}

pub fn discard_draft(store: &Store, id: &OutboxId) -> Result<bool, StoreError> {
    store.delete_outbox(id)
}

/// Drafts, newest first, optionally for one account.
pub fn drafts(store: &Store, account: Option<&AccountId>) -> Result<Vec<OutboxEntry>, StoreError> {
    store.outbox_drafts(account)
}

/// Every outbox entry, used by the UI for queued/failed status and tests.
pub fn list_outbox(
    store: &Store,
    account: Option<&AccountId>,
) -> Result<Vec<OutboxEntry>, StoreError> {
    store.outbox_items(account)
}

pub fn outbox_entry(store: &Store, id: &OutboxId) -> Result<Option<OutboxEntry>, StoreError> {
    store.outbox_entry(id)
}

/// Persist a message as `Queued` before any network work. The existing compose
/// UI has no attachment picker, so it always passes an empty parts vector.
pub async fn queue_send(store: &Store, message: OutgoingMessage) -> Result<OutboxId, StoreError> {
    super::submission::queue_submission(store, message, Vec::new())
}

/// Deliver every pending row for one account through the provider-neutral state
/// machine, using the legacy SMTP + IMAP transports behind a local
/// `SubmissionService`. Kept for callers and tests that still drive the protocol
/// transports directly; the API uses [`super::submission::flush_provider_outbox`]
/// with a provider session instead.
pub async fn flush_outbox(
    store: &Store,
    account: &Account,
    smtp: &mut dyn SmtpSender,
    imap: &mut dyn MailTransport,
) -> Result<OutboxReport, SendError> {
    let mut service = ProtocolSubmission {
        store,
        account,
        smtp,
        imap,
    };
    let ctx = OperationContext::with_timeout(
        account.id.clone(),
        0,
        std::time::Duration::from_secs(30),
    );
    super::submission::flush_provider_outbox(store, account, &mut service, &ctx)
        .await
        .map_err(store_error)
}

/// Deliver every pending row with SMTP only. The Sent APPEND is best-effort, so
/// an unreachable IMAP server must not block delivery; accepted rows remain
/// `Accepted` until a later flush can append them.
pub async fn flush_outbox_smtp_only(
    store: &Store,
    account: &Account,
    smtp: &mut dyn SmtpSender,
) -> Result<OutboxReport, SendError> {
    let mut imap = UnavailableImap;
    flush_outbox(store, account, smtp, &mut imap).await
}

/// The original SMTP + IMAP transports expressed through the submission
/// contract. This keeps the old direct-transport entry points working while the
/// durable state machine, crash recovery, and Sent-copy retry logic stay shared
/// with provider adapters.
struct ProtocolSubmission<'a> {
    store: &'a Store,
    account: &'a Account,
    smtp: &'a mut dyn SmtpSender,
    imap: &'a mut dyn MailTransport,
}

#[async_trait]
impl SubmissionService for ProtocolSubmission<'_> {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        SentCopyPolicy::ClientAppend
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        if let Some(prepared) = &request.prepared
            && prepared.raw.is_some()
        {
            return Ok(prepared.clone());
        }
        let from = Address::new(
            Some(self.account.display_name.clone())
                .filter(|name| !name.trim().is_empty()),
            self.account.email.clone(),
        );
        let raw = build_mime(&from, &request.message)
            .map_err(|error| ProviderError::new(ErrorKind::Forbidden, error.to_string()))?;
        Ok(PreparedSubmission {
            version: 1,
            backend: self.account.provider_id(),
            raw: Some(raw),
            reference: None,
        })
    }

    async fn submit(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        let raw = request
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.raw.clone())
            .ok_or_else(|| ProviderError::new(ErrorKind::Forbidden, "no prepared message"))?;
        let from = Address::new(
            Some(self.account.display_name.clone())
                .filter(|name| !name.trim().is_empty()),
            self.account.email.clone(),
        );
        let recipients = request.message.recipients();
        if recipients.is_empty() {
            return Ok(SubmissionOutcome::Rejected(ProviderError::new(
                ErrorKind::Forbidden,
                "message has no recipients",
            )));
        }
        match self.smtp.send_raw(&from, &recipients, &raw).await {
            Ok(()) => Ok(SubmissionOutcome::Accepted(SubmissionReceipt {
                operation: request.operation.clone(),
                remote_message: None,
                remote_thread: None,
            })),
            // A protocol send that failed after connecting is ambiguous: the
            // server may have accepted DATA before the connection dropped.
            Err(error) => Err(
                ProviderError::unknown_write_outcome("SMTP delivery did not complete")
                    .with_source_detail(error.to_string()),
            ),
        }
    }

    async fn reconcile(
        &mut self,
        _request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<crate::providers::ReconcileOutcome, ProviderError> {
        // The legacy transports expose no delivery evidence. Treat a reopened
        // write as unknown rather than guessing.
        Ok(crate::providers::ReconcileOutcome::Unknown)
    }

    async fn finalize_sent(
        &mut self,
        request: &SubmissionRequest,
        _receipt: &SubmissionReceipt,
        _ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        let raw = request
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.raw.as_deref())
            .ok_or_else(|| ProviderError::transient("the sent message has no raw bytes"))?;
        let folders = self
            .store
            .folders(&self.account.id)
            .map_err(|error| ProviderError::transient(error.to_string()))?;
        let Some(folder) = folders
            .iter()
            .find(|folder| folder.kind == crate::mail::FolderKind::Sent)
        else {
            return Ok(());
        };
        let flags = Flags {
            seen: true,
            ..Flags::default()
        };
        self.imap
            .append(&folder.name, raw, flags)
            .await
            .map_err(|error| {
                ProviderError::transient(format!("append to {}: {error}", folder.name))
            })
    }
}

/// A transport that fails every operation. Used by SMTP-only flushes so the
/// shared outbox loop can keep the same `&mut dyn MailTransport` signature while
/// leaving Sent APPENDs pending for a later sync.
struct UnavailableImap;

const IMAP_UNAVAILABLE: &str =
    "IMAP is unavailable; the Sent copy will be appended on the next sync";

fn imap_unavailable() -> TransportError {
    TransportError(IMAP_UNAVAILABLE.to_string())
}

#[async_trait]
impl MailTransport for UnavailableImap {
    async fn capabilities(&mut self) -> Result<Capabilities, TransportError> {
        Err(imap_unavailable())
    }

    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>, TransportError> {
        Err(imap_unavailable())
    }

    async fn select(&mut self, _folder: &RemoteFolder) -> Result<FolderState, TransportError> {
        Err(imap_unavailable())
    }

    async fn fetch_headers(
        &mut self,
        _range: UidRange,
    ) -> Result<Vec<RemoteMessage>, TransportError> {
        Err(imap_unavailable())
    }

    async fn list_uids(&mut self) -> Result<Vec<Uid>, TransportError> {
        Err(imap_unavailable())
    }

    async fn fetch_learning_headers(
        &mut self,
        _uids: &[Uid],
    ) -> Result<Vec<RemoteMessage>, TransportError> {
        Err(imap_unavailable())
    }

    async fn fetch_texts(
        &mut self,
        _uids: &[Uid],
        _max_bytes: usize,
    ) -> Result<Vec<(Uid, String)>, TransportError> {
        Err(imap_unavailable())
    }

    async fn fetch_body(&mut self, _uid: Uid) -> Result<Vec<u8>, TransportError> {
        Err(imap_unavailable())
    }

    async fn fetch_flags(&mut self, _uids: &[Uid]) -> Result<Vec<(Uid, Flags)>, TransportError> {
        Err(imap_unavailable())
    }

    async fn search(&mut self, _query: &str) -> Result<Vec<Uid>, TransportError> {
        Err(imap_unavailable())
    }

    async fn store_flags(&mut self, _uids: &[Uid], _flags: FlagOp) -> Result<(), TransportError> {
        Err(imap_unavailable())
    }

    async fn move_messages(&mut self, _uids: &[Uid], _dest: &str) -> Result<(), TransportError> {
        Err(imap_unavailable())
    }

    async fn copy_messages(&mut self, _uids: &[Uid], _dest: &str) -> Result<(), TransportError> {
        Err(imap_unavailable())
    }

    async fn delete_messages(&mut self, _uids: &[Uid]) -> Result<(), TransportError> {
        Err(imap_unavailable())
    }

    async fn append(
        &mut self,
        _folder: &str,
        _message: &[u8],
        _flags: Flags,
    ) -> Result<(), TransportError> {
        Err(imap_unavailable())
    }
}

fn store_error(error: StoreError) -> SendError {
    SendError(format!("outbox store: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_send_is_not_a_retryable_outbox_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pige.db")).unwrap();
        store.migrate().unwrap();
        let account = Account {
            id: AccountId("a".to_string()),
            label: "A".to_string(),
            email: "a@example.com".to_string(),
            display_name: "A".to_string(),
            backend: crate::account::MailBackend::ImapSmtp {
                username: "a@example.com".to_string(),
                imap: crate::account::ServerConfig {
                    host: "imap.example.com".to_string(),
                    port: 993,
                    security: crate::account::Security::Tls,
                },
                smtp: crate::account::ServerConfig {
                    host: "smtp.example.com".to_string(),
                    port: 465,
                    security: crate::account::Security::Tls,
                },
                auth: crate::account::AuthKind::Password,
            },
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        };
        store.upsert_account(&account).unwrap();
        let id = save_draft(
            &store,
            OutgoingMessage {
                account_id: account.id.clone(),
                to: vec![Address::new(None, "b@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "s".to_string(),
                body: "b".to_string(),
                in_reply_to: None,
                references: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            store.outbox_entry(&id).unwrap().unwrap().state,
            OutboxState::Draft
        );
        assert_eq!(store.outbox_pending(&account.id).unwrap().len(), 0);
    }
}
