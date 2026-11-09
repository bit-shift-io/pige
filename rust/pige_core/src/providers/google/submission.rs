//! Native Gmail submission.
//!
//! Gmail owns the Sent copy, so [`GmailSubmission`] advertises
//! [`SentCopyPolicy::ServerManaged`] and `finalize_sent` never APPENDs.
//!
//! `prepare` validates the configured From address against the account's
//! send-as aliases and renders the exact RFC 5322 bytes the API will accept.
//! Those bytes keep the `Bcc` header because Gmail derives To/Cc/Bcc from the
//! message headers; Gmail removes Bcc from delivered copies. The Message-ID is
//! derived from the stable operation id and persisted with the prepared bytes,
//! so a retry reuses the same operation evidence instead of rendering a second
//! identity.
//!
//! `submit` calls `users.messages.send` once. The Gmail API has no idempotency
//! key, so a timeout or an unreadable success response is reported as
//! [`SubmissionOutcome::Unknown`]; the outbox turns that into `Uncertain` and
//! never resends automatically. `reconcile` can only upgrade an ambiguous row
//! to Accepted when an exact `rfc822msgid:` search in the authenticated
//! account's Sent mail returns the same Message-ID. A matching subject is not
//! evidence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::mail::{Address, parse_header_block};
use crate::providers::{
    ErrorKind, GMAIL_PROVIDER, OperationContext, PreparedSubmission, ProviderError, ProviderId,
    ReconcileOutcome, RemoteMessageId, RemoteThreadId, SentCopyPolicy, SubmissionOutcome,
    SubmissionReceipt, SubmissionRequest, SubmissionService,
};
use crate::send::compose::{build_mime_with_parts_for_api, raw_message_id};

use super::client::GmailApi;

/// Version of the prepared-artifact envelope owned by this adapter.
const GMAIL_SUBMISSION_VERSION: u32 = 1;

/// Non-secret operation evidence stored in [`PreparedSubmission::reference`].
///
/// It names the outbox operation and the exact RFC Message-ID rendered for it,
/// so reconciliation cannot accidentally accept a different message with the
/// same subject or recipients.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparedEvidence {
    v: u32,
    operation: String,
    message_id: String,
}

impl PreparedEvidence {
    fn decode(value: &str) -> Option<Self> {
        serde_json::from_str(value).ok()
    }
}

/// Gmail native submission with a server-managed Sent copy.
pub struct GmailSubmission {
    api: GmailApi,
    sender: Address,
    domain: String,
}

impl GmailSubmission {
    pub fn new(client: super::GoogleClient, account: Account) -> Self {
        let domain = account
            .email
            .rsplit_once('@')
            .map(|(_, domain)| domain.trim().to_string())
            .filter(|domain| !domain.is_empty())
            .unwrap_or_else(|| "pige.invalid".to_string());
        let sender = Address::new(
            Some(account.display_name.clone()).filter(|name| !name.trim().is_empty()),
            account.email,
        );
        Self {
            api: GmailApi::new(client, "me"),
            sender,
            domain,
        }
    }

    fn sender_address(&self) -> Address {
        self.sender.clone()
    }

    /// A stable RFC Message-ID derived from the outbox operation. It is not a
    /// server idempotency key; it is the identity reconciliation searches for
    /// after an ambiguous send.
    fn message_id_for(&self, operation: &crate::providers::OperationId) -> String {
        let token: String = operation
            .as_str()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        format!("<pige-{token}@{}>", self.domain)
    }

    /// Validate the account's From address against Gmail's send-as aliases.
    ///
    /// The primary address and any verified custom alias are allowed; an
    /// unknown or pending alias is rejected before any message is rendered or
    /// sent. A failed alias lookup is a provider error (not a silent allow), so
    /// a transient settings failure leaves the outbox queued instead of sending
    /// from an unverified identity.
    async fn validate_from_alias(&self, ctx: &OperationContext) -> Result<(), ProviderError> {
        ctx.ensure_active()?;
        let email = self.sender.email.trim();
        let aliases = self.api.send_as(ctx).await?;
        let allowed = aliases.iter().any(|alias| {
            let matches = alias
                .send_as_email
                .as_deref()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case(email));
            if !matches {
                return false;
            }
            alias.is_primary == Some(true)
                || alias
                    .verification_status
                    .as_deref()
                    .is_some_and(|status| status.eq_ignore_ascii_case("accepted"))
        });
        if allowed {
            Ok(())
        } else {
            Err(ProviderError::forbidden(format!(
                "the sender address {email} is not an allowed Gmail send-as alias"
            )))
        }
    }

    /// Reuse an artifact this adapter prepared earlier for the same operation.
    ///
    /// The outbox builds a generic fallback MIME before calling `prepare`; that
    /// value has version 0 and deliberately omits Bcc, so it is never reused
    /// here. Only the versioned, recipient-complete Gmail artifact is trusted.
    fn reusable(&self, request: &SubmissionRequest) -> Option<PreparedSubmission> {
        let prepared = request.prepared.as_ref()?;
        if prepared.version != GMAIL_SUBMISSION_VERSION
            || prepared.backend.as_str() != GMAIL_PROVIDER
        {
            return None;
        }
        let raw = prepared.raw.as_deref()?;
        let evidence = self.evidence(request, prepared)?;
        if raw_message_id(raw).as_deref() != Some(evidence.message_id.as_str()) {
            return None;
        }
        if !prepared_recipients_match(raw, request) {
            return None;
        }
        Some(prepared.clone())
    }

    /// Validate the prepared artifact against the request's operation, backend,
    /// and rendered Message-ID.
    fn evidence(
        &self,
        request: &SubmissionRequest,
        prepared: &PreparedSubmission,
    ) -> Option<PreparedEvidence> {
        if prepared.version != GMAIL_SUBMISSION_VERSION
            || prepared.backend.as_str() != GMAIL_PROVIDER
        {
            return None;
        }
        let evidence = PreparedEvidence::decode(prepared.reference.as_deref()?)?;
        if evidence.v != GMAIL_SUBMISSION_VERSION
            || evidence.operation != request.operation.as_str()
        {
            return None;
        }
        if raw_message_id(prepared.raw.as_deref()?).as_deref() != Some(evidence.message_id.as_str())
        {
            return None;
        }
        prepared_recipients_match(prepared.raw.as_deref()?, request).then_some(evidence)
    }

    /// Look for the exact Message-ID in the authenticated account's Sent mail.
    ///
    /// Both the exact RFC Message-ID and the `SENT` label are required. A
    /// missing result is not evidence that delivery did not happen: Gmail's
    /// search index can lag, so callers stay uncertain rather than resending.
    async fn find_sent(
        &self,
        request: &SubmissionRequest,
        message_id: &str,
        ctx: &OperationContext,
    ) -> Result<Option<SubmissionReceipt>, ProviderError> {
        let query = format!(
            "rfc822msgid:{}",
            message_id.trim().trim_matches(|c| c == '<' || c == '>')
        );
        let response = self.api.search_messages(&query, ctx).await?;
        for reference in response.messages {
            let message = match self
                .api
                .message_metadata_headers(&reference.id, &["Message-ID"], ctx)
                .await
            {
                Ok(message) => message,
                // The listing raced a deletion; it cannot be our Sent copy.
                Err(error) if error.kind == ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !message.label_ids.iter().any(|label| label == "SENT") {
                continue;
            }
            let Some(found) = header_value(&message, "Message-ID") else {
                continue;
            };
            if found.trim() != message_id.trim() {
                continue;
            }
            let id = if message.id.is_empty() {
                reference.id
            } else {
                message.id
            };
            return Ok(Some(SubmissionReceipt {
                operation: request.operation.clone(),
                remote_message: Some(RemoteMessageId::new(id)),
                remote_thread: message.thread_id.map(RemoteThreadId::new),
            }));
        }
        Ok(None)
    }
}

#[async_trait]
impl SubmissionService for GmailSubmission {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        SentCopyPolicy::ServerManaged
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        ctx.ensure_active()?;
        self.validate_from_alias(ctx).await?;

        if let Some(prepared) = self.reusable(request) {
            return Ok(prepared);
        }

        let message_id = self.message_id_for(&request.operation);
        let raw = build_mime_with_parts_for_api(
            &self.sender_address(),
            &request.message,
            &request.attachments,
            &message_id,
        )
        .map_err(|error| ProviderError::forbidden(error.to_string()))?;

        let evidence = PreparedEvidence {
            v: GMAIL_SUBMISSION_VERSION,
            operation: request.operation.as_str().to_string(),
            message_id,
        };
        let reference = serde_json::to_string(&evidence).map_err(|error| {
            ProviderError::transient("could not encode the Gmail submission evidence")
                .with_source_detail(error.to_string())
        })?;
        Ok(PreparedSubmission {
            version: GMAIL_SUBMISSION_VERSION,
            backend: ProviderId::from(GMAIL_PROVIDER),
            raw: Some(raw),
            reference: Some(reference),
        })
    }

    async fn submit(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        ctx.ensure_active()?;
        let Some(prepared) = request.prepared.as_ref() else {
            return Ok(SubmissionOutcome::Rejected(ProviderError::forbidden(
                "the Gmail submission has no prepared message",
            )));
        };
        let Some(_evidence) = self.evidence(request, prepared) else {
            return Ok(SubmissionOutcome::Rejected(ProviderError::forbidden(
                "the prepared Gmail submission is invalid for this operation",
            )));
        };
        let Some(raw) = prepared.raw.as_deref() else {
            return Ok(SubmissionOutcome::Rejected(ProviderError::forbidden(
                "the Gmail submission has no prepared bytes",
            )));
        };

        match self.api.send_message(raw, ctx).await {
            Ok(message) => {
                if message.id.trim().is_empty() {
                    // A success without a receipt cannot be recorded as
                    // accepted; never resend automatically.
                    return Ok(SubmissionOutcome::Unknown);
                }
                Ok(SubmissionOutcome::Accepted(SubmissionReceipt {
                    operation: request.operation.clone(),
                    remote_message: Some(RemoteMessageId::new(message.id)),
                    remote_thread: message.thread_id.map(RemoteThreadId::new),
                }))
            }
            // The request may have reached Gmail. The outbox owns the
            // Unknown -> Uncertain transition; there is no automatic retry.
            Err(error) if error.kind == ErrorKind::UnknownWriteOutcome => {
                Ok(SubmissionOutcome::Unknown)
            }
            // A definite provider refusal (bad recipient, permission, missing
            // user) is a terminal rejection, distinct from connection loss.
            Err(error)
                if matches!(
                    error.kind,
                    ErrorKind::Forbidden
                        | ErrorKind::NotFound
                        | ErrorKind::Conflict
                        | ErrorKind::Unsupported
                        | ErrorKind::CursorInvalid
                ) =>
            {
                Ok(SubmissionOutcome::Rejected(error))
            }
            Err(error) => Err(error),
        }
    }

    async fn reconcile(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        ctx.ensure_active()?;
        let Some(prepared) = request.prepared.as_ref() else {
            // Preparation never produced a durable artifact, so no send could
            // have followed it. Repeating preparation is safe.
            return Ok(ReconcileOutcome::NotAccepted);
        };
        let Some(evidence) = self.evidence(request, prepared) else {
            // A prepared artifact from another backend (or a damaged one)
            // cannot prove this Gmail operation. Stay conservative.
            return Ok(ReconcileOutcome::Unknown);
        };

        match self.find_sent(request, &evidence.message_id, ctx).await? {
            Some(receipt) => Ok(ReconcileOutcome::Accepted(receipt)),
            None => Ok(ReconcileOutcome::Unknown),
        }
    }

    async fn finalize_sent(
        &mut self,
        _request: &SubmissionRequest,
        _receipt: &SubmissionReceipt,
        _ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        // Gmail stores the Sent copy. Appending again would duplicate it.
        Ok(())
    }
}

fn prepared_recipients_match(raw: &[u8], request: &SubmissionRequest) -> bool {
    let Ok(header) = parse_header_block(raw) else {
        return false;
    };
    same_addresses(&header.to, &request.message.to)
        && same_addresses(&header.cc, &request.message.cc)
        && same_addresses(&header.bcc, &request.message.bcc)
}

fn same_addresses(left: &[Address], right: &[Address]) -> bool {
    let mut left: Vec<String> = left
        .iter()
        .map(|address| address.email.trim().to_ascii_lowercase())
        .collect();
    let mut right: Vec<String> = right
        .iter()
        .map(|address| address.email.trim().to_ascii_lowercase())
        .collect();
    left.sort();
    right.sort();
    left == right
}

fn header_value(message: &super::client::Message, name: &str) -> Option<String> {
    message
        .payload
        .as_ref()?
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.clone())
}
