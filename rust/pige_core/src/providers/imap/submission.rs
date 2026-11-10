//! SMTP submission behind the provider boundary.
//!
//! The adapter owns exactly two protocol details:
//!
//! - `prepare` renders the exact MIME bytes (including structured parts) that
//!   will be offered to SMTP, so a retry never generates a second Message-ID.
//! - `finalize_sent` appends those same bytes to the account's Sent folder,
//!   but only after proving (via the Message-ID) that the copy is not already
//!   there. If that proof cannot be obtained, the copy is left pending rather
//!   than blindly appended a second time.
//!
//! Nothing here persists credentials. The account and the platform transport
//! factory are the only state, and the factory resolves secrets internally.

use std::sync::Arc;

use async_trait::async_trait;

use crate::account::Account;
use crate::mail::{Address, Flags, FolderKind};
use crate::platform::TransportFactory;
use crate::providers::ErrorKind;
use crate::providers::error::ProviderError;
use crate::providers::traits::SubmissionService;
use crate::providers::types::{
    OperationContext, PreparedSubmission, ReconcileOutcome, SentCopyPolicy, SubmissionOutcome,
    SubmissionReceipt, SubmissionRequest,
};
use crate::send::compose::{build_mime_with_parts, raw_message_id};
use crate::transport::{RemoteFolder, TransportError};

/// Client-appended Sent copies over the existing IMAP/SMTP transports.
pub struct ImapSubmission {
    transports: Arc<dyn TransportFactory>,
    account: Account,
}

impl ImapSubmission {
    pub fn new(transports: Arc<dyn TransportFactory>, account: Account) -> Self {
        Self {
            transports,
            account,
        }
    }

    fn sender_address(&self) -> Address {
        Address::new(
            Some(self.account.display_name.clone()).filter(|name| !name.trim().is_empty()),
            self.account.email.clone(),
        )
    }
}

#[async_trait]
impl SubmissionService for ImapSubmission {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        SentCopyPolicy::ClientAppend
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        ctx.ensure_active()?;
        // A durable prepared artifact from an earlier attempt is reused
        // verbatim, preserving the exact MIME and Message-ID across retries.
        if let Some(prepared) = &request.prepared
            && prepared.raw.is_some()
        {
            return Ok(prepared.clone());
        }

        let raw = build_mime_with_parts(
            &self.sender_address(),
            &request.message,
            &request.attachments,
        )
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
        ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        ctx.ensure_active()?;
        let raw = request
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.raw.clone())
            .ok_or_else(|| ProviderError::new(ErrorKind::Forbidden, "no prepared message"))?;
        let from = self.sender_address();
        let recipients = request.message.recipients();
        if recipients.is_empty() {
            return Ok(SubmissionOutcome::Rejected(ProviderError::new(
                ErrorKind::Forbidden,
                "message has no recipients",
            )));
        }

        // A failure to open the SMTP connection means nothing was delivered, so
        // the queued row can be retried safely.
        let mut smtp = self.transports.smtp(&self.account).await.map_err(|error| {
            ProviderError::offline("the mail server is not reachable").with_source_detail(error)
        })?;

        match smtp.send_raw(&from, &recipients, &raw).await {
            Ok(()) => Ok(SubmissionOutcome::Accepted(SubmissionReceipt {
                operation: request.operation.clone(),
                remote_message: None,
                remote_thread: None,
            })),
            // The connection was established before `send_raw`, so the server
            // may have accepted the message before the failure. Never report a
            // definite rejection here; the durable state becomes uncertain.
            Err(error) => Err(ProviderError::unknown_write_outcome(
                "SMTP delivery did not complete",
            )
            .with_source_detail(error.to_string())),
        }
    }

    async fn reconcile(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        // IMAP preparation has no remote side effect. If no prepared artifact
        // exists the interrupted work was only local rendering, so repeating it
        // is safe. Once bytes were prepared, SMTP delivery may be ambiguous and
        // only provider evidence (unavailable over plain IMAP) could resolve it.
        if request.prepared.is_none() {
            Ok(ReconcileOutcome::NotAccepted)
        } else {
            Ok(ReconcileOutcome::Unknown)
        }
    }

    async fn finalize_sent(
        &mut self,
        request: &SubmissionRequest,
        _receipt: &SubmissionReceipt,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        ctx.ensure_active()?;
        let Some(raw) = request
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.raw.as_deref())
        else {
            return Err(ProviderError::transient(
                "the sent message has no raw bytes to append",
            ));
        };

        let mut imap = self.transports.imap(&self.account).await.map_err(|error| {
            ProviderError::transient("could not reach the Sent folder").with_source_detail(error)
        })?;
        let folders = imap
            .list_folders()
            .await
            .map_err(sent_copy_transport_error)?;
        let Some(folder) = folders
            .iter()
            .find(|folder| folder.kind == FolderKind::Sent)
        else {
            // No Sent collection means there is no copy to write or retry.
            return Ok(());
        };

        let remote = RemoteFolder {
            name: folder.name.clone(),
            delimiter: folder.delimiter.clone(),
            kind: folder.kind,
        };
        imap.select(&remote)
            .await
            .map_err(sent_copy_transport_error)?;

        // Prove the copy is absent before appending. If the search itself fails
        // the append is ambiguous; leave the row pending for a later pass
        // rather than risking a duplicate Sent copy.
        let Some(message_id) = raw_message_id(raw) else {
            return Err(ProviderError::unknown_write_outcome(
                "the Sent copy cannot be verified without a Message-ID",
            ));
        };
        let query = format!("HEADER Message-ID \"{}\"", message_id.replace('"', ""));
        let existing = imap.search(&query).await.map_err(|error| {
            ProviderError::unknown_write_outcome(
                "the Sent folder could not be checked before appending",
            )
            .with_source_detail(error.to_string())
        })?;
        if !existing.is_empty() {
            return Ok(());
        }

        let flags = Flags {
            seen: true,
            ..Flags::default()
        };
        imap.append(&folder.name, raw, flags)
            .await
            .map_err(|error| {
                ProviderError::unknown_write_outcome(
                    "the Sent copy may or may not have been appended",
                )
                .with_source_detail(error.to_string())
            })
    }
}

/// Map an IMAP failure while finalizing a Sent copy. These errors are never
/// converted into a transport error that the outbox could mistake for delivery;
/// the copy is simply retried later.
fn sent_copy_transport_error(error: TransportError) -> ProviderError {
    ProviderError::transient("the Sent copy could not be written")
        .with_source_detail(error.to_string())
}
