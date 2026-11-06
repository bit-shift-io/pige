//! Migration v4: repair cached list previews.
//!
//! Snippets cached before the transfer-encoding decoder landed were the raw
//! `BODY[TEXT]` bytes, so base64 and quoted-printable mail showed as encoded
//! gibberish in the list. Rebuild each message's snippet from its cached body
//! when one exists; clear the rest so a stale encoded preview can never be
//! shown. The sync path refills the cleared previews for the newest messages,
//! and any body fetched later (prefetch or open) rewrites its message snippet.

use rusqlite::{Connection, params};

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    // Collect first so the update statement does not fight the read cursor.
    let mut statement = conn.prepare(
        "SELECT m.id, b.plain_text, b.html
         FROM messages m
         LEFT JOIN bodies b ON b.message_id = m.id",
    )?;
    let rows: Vec<(String, Option<String>, Option<String>)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    drop(statement);

    let mut update = conn.prepare("UPDATE messages SET snippet = ?1 WHERE id = ?2")?;
    for (id, plain_text, html) in rows {
        let snippet = crate::mail::snippet_from_parts(plain_text.as_deref(), html.as_deref())
            .unwrap_or_default();
        update.execute(params![snippet, id])?;
    }
    Ok(())
}
