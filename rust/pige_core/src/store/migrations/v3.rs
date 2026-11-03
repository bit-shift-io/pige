//! Migration v3: attachment disposition.
//!
//! The v1 schema already had the attachment table. v3 records whether a part
//! was presented inline or as a download so the reader can make the same
//! choice the sender did.

use rusqlite::Connection;

use crate::store::StoreError;

pub const V3_SQL: &str = r#"
ALTER TABLE attachments
    ADD COLUMN disposition TEXT NOT NULL DEFAULT 'attachment';
"#;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    // Some databases are rewound or partially rebuilt by tests and recovery
    // tools. Only add the column when it is genuinely absent.
    let has_disposition = {
        let mut statement = conn.prepare("PRAGMA table_info(attachments)")?;
        let mut rows = statement.query([])?;
        let mut found = false;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == "disposition" {
                found = true;
                break;
            }
        }
        found
    };
    if !has_disposition {
        conn.execute_batch(V3_SQL)?;
    }
    Ok(())
}
