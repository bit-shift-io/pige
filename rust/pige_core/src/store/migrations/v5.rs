//! Migration v5: version the account mirror by backend.
//!
//! v4 (and earlier) stored only the inline IMAP/SMTP columns, which cannot
//! describe a native Gmail or Proton account without inventing an endpoint. v5
//! adds two columns:
//!
//! - `backend`: the typed backend serialized as JSON. NULL means a pre-v5 row,
//!   which is read back from the legacy columns as IMAP/SMTP.
//! - `contact_sources`: the account's contact sources as JSON, defaulting to an
//!   empty list so migrated accounts have no contacts or tokens.
//!
//! The legacy columns are kept (they are `NOT NULL` from v1) and stay populated
//! for IMAP/SMTP accounts, so an older binary sharing the database still sees
//! the servers. For native accounts they hold inert defaults and are never read
//! once `backend` is present.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    // Some databases are rewound or partially rebuilt by tests and recovery
    // tools, so only add a column when it is genuinely absent.
    if !has_column(conn, "accounts", "backend")? {
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN backend TEXT;")?;
    }
    if !has_column(conn, "accounts", "contact_sources")? {
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN contact_sources TEXT NOT NULL DEFAULT '[]';",
        )?;
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
