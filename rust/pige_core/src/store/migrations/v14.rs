//! Migration v14: durable header-history learning progress.
//!
//! The history crawl cursor already lives in `job_checkpoints` and is fenced
//! by the account generation. This migration adds only the *progress* the UI
//! reads without opening a provider: cumulative scanned records, the adapter's
//! total hint, end-of-enumeration, a paused marker, and the absolute time a
//! rate-limit delay expires. The retry timestamp is durable so a restart does
//! not immediately hit the same server limit.
//!
//! `contact_learning_state` already exists for the local ExistingCache
//! backfill; these are additive columns, so an upgraded database keeps that
//! cursor untouched. Each column is added only when absent so replaying the
//! migration pipeline on an already-upgraded schema is harmless (the store
//! tests deliberately rewind `meta.schema_version`).

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    for (column, definition) in [
        ("history_scanned", "INTEGER NOT NULL DEFAULT 0"),
        ("history_total_hint", "INTEGER"),
        ("history_complete", "INTEGER NOT NULL DEFAULT 0"),
        ("history_paused", "INTEGER NOT NULL DEFAULT 0"),
        ("history_retry_after", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !has_column(conn, "contact_learning_state", column)? {
            conn.execute_batch(&format!(
                "ALTER TABLE contact_learning_state ADD COLUMN {column} {definition};"
            ))?;
        }
    }
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
