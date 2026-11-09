//! Migration v8: provider body metadata on the cached message and attachment.
//!
//! A provider body fetch reports two things the reader needs that the old
//! IMAP-only path dropped:
//!
//! 1. a provider-neutral signature trust state ([`SignatureStatus`]) carried by
//!    the normalized body, and
//! 2. a stable *remote* attachment id ([`RemoteAttachmentId`]) distinct from the
//!    local MIME part id, so a later `attachment` fetch names the part the way
//!    the provider does.
//!
//! Both are additive columns with safe defaults, so an existing database keeps
//! every local id, body, attachment, and thread link.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    // Idempotent: a rewound/partially rebuilt database may already carry the
    // columns, and SQLite rejects a duplicate `ADD COLUMN`.
    if !has_column(conn, "messages", "signature")? {
        conn.execute_batch(
            "ALTER TABLE messages
                 ADD COLUMN signature TEXT NOT NULL DEFAULT 'not_applicable';",
        )?;
    }
    if !has_column(conn, "attachments", "remote_id")? {
        conn.execute_batch("ALTER TABLE attachments ADD COLUMN remote_id TEXT;")?;
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
