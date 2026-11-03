//! Full-text index maintenance and query execution.
//!
//! The index mirrors `messages` plus the plain-text half of `bodies`. Raw HTML
//! is never indexed: it is markup, not user-visible text. Every write path that
//! changes a searchable field calls [`index_message_on`] inside its existing
//! transaction, while SQL triggers in migration v2 remove rows when messages or
//! bodies disappear.

use rusqlite::{Connection, OptionalExtension, params};

use crate::mail::{Address, MessageId, sender_display};
use crate::store::{Store, StoreError};

pub use crate::search::{SearchHit, SearchScope, fts_query};

impl Store {
    /// Search cached mail, newest matching message per conversation first.
    pub fn search(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: u32,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let conn = self.connection();
        search_on(&conn, query, scope, limit)
    }
}

pub(crate) fn search_on(
    conn: &Connection,
    query: &str,
    scope: &SearchScope,
    limit: u32,
) -> Result<Vec<SearchHit>, StoreError> {
    let fts = fts_query(query);
    if fts.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    // Scope predicates bind their values; only the fixed SQL shape is built
    // here, so user input never reaches the statement text.
    let (account, folder) = match scope {
        SearchScope::AllAccounts => (None, None),
        SearchScope::Account(id) => (Some(id.0.as_str()), None),
        SearchScope::Folder(id) => (None, Some(id.0.as_str())),
    };

    let mut stmt = conn.prepare(
        "WITH matches AS (
             SELECT
                 f.thread_id AS thread_id,
                 f.account_id AS account_id,
                 m.subject AS subject,
                 m.from_json AS from_json,
                 m.date AS date,
                 ROW_NUMBER() OVER (
                     PARTITION BY f.thread_id ORDER BY m.date DESC, m.uid DESC
                 ) AS rank
             FROM message_fts f
             JOIN messages m ON m.id = f.message_id
             WHERE message_fts MATCH ?1
               AND (?2 IS NULL OR f.account_id = ?2)
               AND (?3 IS NULL OR f.folder_id = ?3)
         )
         SELECT thread_id, account_id, subject, from_json, date
         FROM matches
         WHERE rank = 1
         ORDER BY date DESC, thread_id
         LIMIT ?4",
    )?;
    let mut rows = stmt.query(params![fts, account, folder, i64::from(limit)])?;

    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        let from_json: String = row.get(3)?;
        hits.push(SearchHit {
            thread_id: crate::mail::ThreadId(row.get(0)?),
            account_id: crate::account::AccountId(row.get(1)?),
            subject: row.get(2)?,
            sender: sender_of(&from_json)?,
            date: row.get(4)?,
        });
    }
    Ok(hits)
}

/// Insert or replace one message's FTS row from the current `messages` and
/// `bodies` contents. Must run inside the caller's write transaction.
pub(crate) fn index_message_on(
    conn: &Connection,
    message_id: &MessageId,
) -> Result<(), StoreError> {
    let row = conn
        .query_row(
            "SELECT thread_id, account_id, folder_id, subject, from_json, to_json, cc_json
             FROM messages WHERE id = ?1",
            [&message_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;

    // A missing message must not leave a stale hit behind.
    conn.execute(
        "DELETE FROM message_fts WHERE message_id = ?1",
        [&message_id.0],
    )?;
    let Some((thread_id, account_id, folder_id, subject, from_json, to_json, cc_json)) = row else {
        return Ok(());
    };

    let from: Vec<Address> = decode_addresses(&from_json, "messages.from_json")?;
    let to: Vec<Address> = decode_addresses(&to_json, "messages.to_json")?;
    let cc: Vec<Address> = decode_addresses(&cc_json, "messages.cc_json")?;
    let body: Option<String> = conn
        .query_row(
            "SELECT plain_text FROM bodies WHERE message_id = ?1",
            [&message_id.0],
            |row| row.get(0),
        )
        .optional()?
        .flatten();

    conn.execute(
        "INSERT INTO message_fts (
             message_id, thread_id, account_id, folder_id,
             subject, sender, recipients, body
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            message_id.0,
            thread_id,
            account_id,
            folder_id,
            subject,
            address_terms(&from),
            address_terms(to.iter().chain(cc.iter())),
            body,
        ],
    )?;
    Ok(())
}

/// Rebuild the whole index from `messages` and cached bodies. Used by the v2
/// migration after the virtual table is created.
pub(crate) fn backfill_on(conn: &Connection) -> Result<(), StoreError> {
    conn.execute("DELETE FROM message_fts", [])?;
    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM messages")?;
        let mut rows = stmt.query([])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(row.get(0)?);
        }
        ids
    };
    for id in ids {
        index_message_on(conn, &MessageId(id))?;
    }
    Ok(())
}

fn sender_of(from_json: &str) -> Result<String, StoreError> {
    Ok(sender_display(&decode_addresses(
        from_json,
        "messages.from_json",
    )?))
}

fn decode_addresses(raw: &str, field: &'static str) -> Result<Vec<Address>, StoreError> {
    serde_json::from_str(raw).map_err(|_| StoreError::corrupt(field, raw))
}

/// Index every address name and mailbox so a search matches either the display
/// name ("Ada Lovelace") or the bare address ("ada@example.com").
fn address_terms<'a>(addresses: impl IntoIterator<Item = &'a Address>) -> String {
    addresses
        .into_iter()
        .map(Address::to_header_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_terms_include_names_and_emails() {
        let terms = address_terms(&[
            Address::new(Some("Ada Lovelace".into()), "ada@example.com"),
            Address::new(None, "charles@example.com"),
        ]);
        assert!(terms.contains("Ada Lovelace"));
        assert!(terms.contains("ada@example.com"));
        assert!(terms.contains("charles@example.com"));
    }
}
