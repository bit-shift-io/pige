//! Cached message bodies. The raw HTML and plain text are stored once a body
//! is fetched so later reads never hit the network; the AST is re-derived from
//! these on demand. `ast_json` is reserved for a richer cache and left null.

use rusqlite::{OptionalExtension, params};

use crate::mail::MessageId;

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
