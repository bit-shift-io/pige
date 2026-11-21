//! Cached message bodies. The raw HTML and plain text are stored once a body
//! is fetched so later reads never hit the network; the AST is re-derived from
//! these on demand. `ast_json` is reserved for a richer cache and left null.

use rusqlite::{OptionalExtension, params};

use crate::mail::MessageId;
use crate::providers::SignatureStatus;

use super::{Store, StoreError};

/// The raw body text cached for one message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Body {
    pub plain_text: Option<String>,
    pub html: Option<String>,
    pub fetched_at: i64,
}

impl Store {
    /// Record a successful fetch with no readable text so prefetch skips it.
    pub fn set_body_state(&self, id: &MessageId, state: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE messages SET body_state = ?1 WHERE id = ?2",
            params![state, id.0],
        )?;
        Ok(())
    }

    /// Read a message's body state ('headers', 'none'), if the message
    /// exists. Lets prefetch skip a message whose fetch already happened and
    /// produced no readable text.
    pub fn body_state(&self, id: &MessageId) -> Result<Option<String>, StoreError> {
        self.connection()
            .query_row(
                "SELECT body_state FROM messages WHERE id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Read the cached body for a message, if one has been stored.
    pub fn body(&self, message_id: &MessageId) -> Result<Option<Body>, StoreError> {
        let conn = self.connection();
        conn.query_row(
            "SELECT plain_text, html, fetched_at FROM bodies WHERE message_id = ?1",
            [&message_id.0],
            |row| {
                Ok(Body {
                    plain_text: row.get(0)?,
                    html: row.get(1)?,
                    fetched_at: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Record the signature trust state a provider reported with a body fetch.
    /// Provider-neutral; the IMAP adapter reports `NotApplicable`.
    pub fn set_signature_status(
        &self,
        id: &MessageId,
        status: SignatureStatus,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE messages SET signature = ?1 WHERE id = ?2",
            params![signature_to_db(status), id.0],
        )?;
        Ok(())
    }

    /// The last signature trust state recorded for a message. Missing rows and
    /// rows written before v8 read back as `NotApplicable`.
    pub fn signature_status(&self, id: &MessageId) -> Result<SignatureStatus, StoreError> {
        let raw: Option<String> = self
            .connection()
            .query_row(
                "SELECT signature FROM messages WHERE id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw
            .as_deref()
            .map(signature_from_db)
            .unwrap_or(SignatureStatus::NotApplicable))
    }

    /// Store (or replace) the cached body for a message and refresh its search
    /// entry with the plain-text half.
    pub fn upsert_body(&self, message_id: &MessageId, body: &Body) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO bodies (message_id, plain_text, html, fetched_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(message_id) DO UPDATE SET
                 plain_text = excluded.plain_text,
                 html = excluded.html,
                 fetched_at = excluded.fetched_at",
            params![message_id.0, body.plain_text, body.html, body.fetched_at,],
        )?;
        super::search::index_message_on(&tx, message_id)?;
        tx.commit()?;
        Ok(())
    }
}

/// Stable string form of a signature status. Kept explicit (not derived) so a
/// future variant cannot silently change the meaning of a stored value.
fn signature_to_db(status: SignatureStatus) -> &'static str {
    match status {
        SignatureStatus::NotApplicable => "not_applicable",
        SignatureStatus::Verified => "verified",
        SignatureStatus::Unsigned => "unsigned",
        SignatureStatus::Unverified => "unverified",
        SignatureStatus::Invalid => "invalid",
        SignatureStatus::Unsupported => "unsupported",
    }
}

/// Parse a stored signature status. An unknown legacy value degrades to
/// `NotApplicable` rather than failing a read.
fn signature_from_db(raw: &str) -> SignatureStatus {
    match raw {
        "verified" => SignatureStatus::Verified,
        "unsigned" => SignatureStatus::Unsigned,
        "unverified" => SignatureStatus::Unverified,
        "invalid" => SignatureStatus::Invalid,
        "unsupported" => SignatureStatus::Unsupported,
        _ => SignatureStatus::NotApplicable,
    }
}
