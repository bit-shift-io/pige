//! Migration v16: source-scoped contact snapshot generations.
//!
//! Remote address books are synchronized in full snapshots. Each cached contact
//! row carries the snapshot generation that observed it, so a completed
//! generation can sweep contacts the provider no longer reports without ever
//! mistaking a failed or partial page for a complete snapshot.
//!
//! `contact_source_state` holds only synchronization metadata: whether the
//! source is enabled, the current/last snapshot generation, whether that
//! generation is still in progress, and cumulative progress for the settings
//! screen. Cached contacts and learned correspondent evidence stay in their
//! existing tables; deleting a source's contacts never touches evidence.
//!
//! Columns are added only when absent so replaying the migration pipeline on an
//! already-upgraded schema is harmless.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    if !has_column(conn, "contact_emails", "snapshot_generation")? {
        conn.execute_batch(
            "ALTER TABLE contact_emails
                 ADD COLUMN snapshot_generation INTEGER NOT NULL DEFAULT 0;",
        )?;
    }

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS contact_emails_snapshot
             ON contact_emails(account_id, source_id, snapshot_generation);

         CREATE TABLE IF NOT EXISTS contact_source_state (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             source_id TEXT NOT NULL,
             enabled INTEGER NOT NULL DEFAULT 1,
             snapshot_generation INTEGER NOT NULL DEFAULT 0,
             snapshot_active INTEGER NOT NULL DEFAULT 0,
             scanned INTEGER NOT NULL DEFAULT 0,
             complete INTEGER NOT NULL DEFAULT 0,
             updated_at INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (account_id, source_id)
         );",
    )?;
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
