//! Migration v9: provider-independent durable submission.
//!
//! The v1 outbox was a single SMTP-centric row: state, raw bytes, and an error
//! string. Provider-neutral submission needs to keep four independent pieces of
//! evidence — the stable operation id, the backend that owns the submission, the
//! prepared artifact, and the provider receipt — plus retry metadata. Structured
//! attachment parts get their own table so they survive a restart and are
//! cascaded away with the outbox row.
//!
//! Legacy states are translated conservatively. A v4 `sending` row may already
//! have been accepted, so it becomes `uncertain` and is never automatically
//! resent. A v4 `sent_local` row is accepted delivery whose Sent copy is still
//! pending; it stays on the `imap-smtp` backend that accepted it. Every legacy
//! `failed` row becomes `uncertain` as well because the old error string cannot
//! prove a permanent rejection; retrying could duplicate a delivery.

use rusqlite::Connection;

use crate::providers::IMAP_SMTP_PROVIDER;
use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    add_column(conn, "operation_id", "TEXT")?;
    add_column(conn, "backend", "TEXT")?;
    add_column(conn, "prepared_version", "INTEGER")?;
    add_column(conn, "prepared_backend", "TEXT")?;
    add_column(conn, "prepared_raw", "BLOB")?;
    add_column(conn, "prepared_reference", "TEXT")?;
    add_column(conn, "receipt_operation", "TEXT")?;
    add_column(conn, "receipt_remote_message", "TEXT")?;
    add_column(conn, "receipt_remote_thread", "TEXT")?;
    add_column(conn, "retry_count", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "retry_after", "INTEGER")?;
    add_column(
        conn,
        "acceptance_recorded",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS outbox_parts (
             outbox_id TEXT NOT NULL REFERENCES outbox(id) ON DELETE CASCADE,
             part_id TEXT NOT NULL,
             position INTEGER NOT NULL DEFAULT 0,
             filename TEXT NOT NULL,
             content_type TEXT NOT NULL,
             content_id TEXT,
             disposition TEXT NOT NULL,
             bytes BLOB NOT NULL,
             PRIMARY KEY (outbox_id, part_id)
         );
         CREATE INDEX IF NOT EXISTS outbox_parts_outbox
             ON outbox_parts(outbox_id);",
    )?;

    // Translate every legacy state exactly once. The WHERE clauses make the
    // migration safe to re-run on a database that was only partially rebuilt.
    conn.execute(
        "UPDATE outbox SET state = 'uncertain' WHERE state IN ('sending', 'failed')",
        [],
    )?;
    conn.execute(
        "UPDATE outbox SET state = 'accepted' WHERE state = 'sent_local'",
        [],
    )?;
    conn.execute(
        "UPDATE outbox SET state = 'complete' WHERE state = 'sent'",
        [],
    )?;

    // Every row gets a stable operation id and an owning backend. Rows written
    // before providers existed were necessarily handled by the IMAP/SMTP
    // transport, including `sent_local` rows whose Sent copy still needs it.
    conn.execute(
        "UPDATE outbox SET operation_id = 'legacy-' || id WHERE operation_id IS NULL",
        [],
    )?;
    conn.execute(
        "UPDATE outbox SET backend = ?1 WHERE backend IS NULL",
        [IMAP_SMTP_PROVIDER],
    )?;

    Ok(())
}

fn add_column(conn: &Connection, column: &str, definition: &str) -> Result<(), StoreError> {
    if !has_column(conn, "outbox", column)? {
        conn.execute_batch(&format!(
            "ALTER TABLE outbox ADD COLUMN {column} {definition};"
        ))?;
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
