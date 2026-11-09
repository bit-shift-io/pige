//! Migration v12: the indexed recipient-search projection.
//!
//! Compose autocomplete must not scan the replay-safe evidence ledger on every
//! keystroke. This migration adds two derived tables:
//!
//! - `recipient_index`: one row per `(account, normalized address)` with the
//!   ranking signals merged from learned correspondents and source contacts.
//! - `recipient_tokens`: case-folded prefix tokens (full address, local part,
//!   domain, name words) with a token-first index for range prefix lookups.
//!
//! Both tables derive from `contact_addresses`/`contact_emails` and are kept in
//! sync by the store. `contact_sources.kind` records whether a source is a
//! saved address book or Google-style Other contacts; existing sources default
//! to `saved`.
//!
//! The migration backfills the projection from any rows that already exist, so
//! an upgraded database searches immediately without waiting for contact sync.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    if !has_column(conn, "contact_sources", "kind")? {
        conn.execute_batch(
            "ALTER TABLE contact_sources ADD COLUMN kind TEXT NOT NULL DEFAULT 'saved';",
        )?;
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS recipient_index (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             address_key TEXT NOT NULL,
             email TEXT NOT NULL,
             display_name TEXT,
             email_folded TEXT NOT NULL,
             name_folded TEXT NOT NULL DEFAULT '',
             saved_contact INTEGER NOT NULL DEFAULT 0,
             other_contact INTEGER NOT NULL DEFAULT 0,
             outgoing INTEGER NOT NULL DEFAULT 0,
             selected INTEGER NOT NULL DEFAULT 0,
             incoming INTEGER NOT NULL DEFAULT 0,
             low_trust_only INTEGER NOT NULL DEFAULT 0,
             last_seen INTEGER NOT NULL DEFAULT 0,
             frequency INTEGER NOT NULL DEFAULT 0,
             tier INTEGER NOT NULL DEFAULT 3,
             PRIMARY KEY (account_id, address_key)
         );

         CREATE TABLE IF NOT EXISTS recipient_tokens (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             address_key TEXT NOT NULL,
             token TEXT NOT NULL,
             PRIMARY KEY (account_id, address_key, token)
         );

         CREATE INDEX IF NOT EXISTS recipient_tokens_token
             ON recipient_tokens(token, account_id, address_key);",
    )?;

    super::super::correspondents::rebuild_recipient_index_in(conn)?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}
