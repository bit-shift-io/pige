//! Migration v15: durable learning controls.
//!
//! Clearing or rebuilding learned recipients is an explicit user action, so its
//! effect must survive a restart. These additive columns record:
//!
//! - `learning_epoch`, bumped by clear, rebuild, pause, and resume. A history
//!   page carries the epoch it was produced under and cannot commit after the
//!   epoch moved, even when the crawl checkpoint it started from was deleted.
//! - `learning_paused`, the user's persistent pause switch. Existing suggestions
//!   remain visible; every ingestion path is stopped.
//! - `history_disabled` and `cache_backfill_disabled`, set by clear so an
//!   ordinary resync cannot silently recreate the history the user just forgot.
//!   An explicit rebuild clears both.
//!
//! Each column is added only when absent so replaying the migration pipeline on
//! an already-upgraded schema is harmless (the store tests deliberately rewind
//! `meta.schema_version`).

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    for (column, definition) in [
        ("learning_epoch", "INTEGER NOT NULL DEFAULT 0"),
        ("learning_paused", "INTEGER NOT NULL DEFAULT 0"),
        ("history_disabled", "INTEGER NOT NULL DEFAULT 0"),
        ("cache_backfill_disabled", "INTEGER NOT NULL DEFAULT 0"),
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
