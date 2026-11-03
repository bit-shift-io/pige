//! Migration v2: the full-text index and its initial backfill.
//!
//! The virtual table mirrors message headers plus cached plain-text bodies.
//! Delete triggers keep the index honest for every existing deletion path
//! (UIDVALIDITY resets, paging trims, and account cascades).

use rusqlite::Connection;

use crate::store::{StoreError, search};

pub const V2_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
    message_id UNINDEXED,
    thread_id UNINDEXED,
    account_id UNINDEXED,
    folder_id UNINDEXED,
    subject,
    sender,
    recipients,
    body
);

CREATE TRIGGER IF NOT EXISTS message_fts_message_delete
AFTER DELETE ON messages
BEGIN
    DELETE FROM message_fts WHERE message_id = old.id;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_thread_update
AFTER UPDATE OF thread_id ON messages
WHEN old.thread_id <> new.thread_id
BEGIN
    UPDATE message_fts SET thread_id = new.thread_id WHERE message_id = new.id;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_body_delete
AFTER DELETE ON bodies
BEGIN
    UPDATE message_fts SET body = NULL WHERE message_id = old.message_id;
END;
"#;

/// Create the index, then index every message that predates it.
pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(V2_SQL)?;
    search::backfill_on(conn)
}
