//! Outgoing mail: compose helpers, MIME generation, SMTP delivery, and the
//! persistent outbox.

pub mod compose;
pub mod outbox;

use crate::account::AccountId;
use crate::mail::Address;
use crate::transport::{SendError, SmtpSender};

pub use compose::{
    ComposeError, build_mime, forward_message, forward_subject, quote_body, reply_message,
    reply_subject,
};
pub use outbox::{
    OutboxEntry, OutboxId, OutboxReport, OutboxState, discard_draft, drafts, flush_outbox,
    list_outbox, queue_send, save_draft, update_draft,
};

/// A plain-text message ready to be sent. Attachments are intentionally absent
/// in v1; the reader still displays received attachments.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutgoingMessage {
    pub account_id: AccountId,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

impl OutgoingMessage {
    /// Every envelope recipient, in To, Cc, Bcc order.
    pub fn recipients(&self) -> Vec<Address> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .cloned()
            .collect()
    }
}

/// Build the raw message and submit it. The returned bytes are exactly what the
/// SMTP transport accepted, so callers can persist them for the Sent APPEND.
pub async fn deliver(
    smtp: &mut dyn SmtpSender,
    from: &Address,
    message: &OutgoingMessage,
) -> Result<Vec<u8>, SendError> {
    let raw = build_mime(from, message).map_err(|error| SendError(error.to_string()))?;
    let recipients = message.recipients();
    if recipients.is_empty() {
        return Err(SendError("message has no recipients".to_string()));
    }
    smtp.send_raw(from, &recipients, &raw).await?;
    Ok(raw)
}
