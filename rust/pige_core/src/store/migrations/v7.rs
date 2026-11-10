//! Migration v7: separate remote message identity from collection membership.
//!
//! Before v7 a `messages` row was one mailbox UID with a mandatory single
//! `folder_id`. REST providers need account-wide message identity plus
//! many-to-many label memberships, so this migration:
//!
//! 1. adds the additive mapping tables (`message_collections`,
//!    `remote_messages`, `remote_collections`) and backfills them one-to-one
//!    from the old columns, so no local id, body, attachment, or thread link
//!    changes;
//! 2. rebuilds `messages` as content only, dropping the mandatory
//!    `folder_id`/`uid`/`uidvalidity` columns;
//! 3. recreates the FTS triggers that the table drop removed and reindexes.
//!
//! The rebuild must run with foreign-key enforcement disabled (the migration
//! connection disables it before its transaction, see `migrations.rs`), because
//! dropping `messages` would otherwise perform an implicit `DELETE` that
//! cascades into `bodies` and `attachments`.

use rusqlite::{Connection, params};

use crate::mail::Uid;
use crate::store::remote_ids::{IMAP_SMTP_BACKEND_KEY, ImapCoords};
use crate::store::{StoreError, search};

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    // Some databases are rewound or partially rebuilt by tests and recovery
    // tools. If `messages` already lacks the old `folder_id` column the rebuild
    // has run; re-running it would fail, so treat v7 as already applied.
    if !has_column(conn, "messages", "folder_id")? {
        return Ok(());
    }

    // 1. Additive tables first, so a failure here leaves the old schema usable.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_collections (
             message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
             folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
             PRIMARY KEY (message_id, folder_id)
         );
         CREATE INDEX IF NOT EXISTS message_collections_folder
             ON message_collections(folder_id);

         CREATE TABLE IF NOT EXISTS remote_messages (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             backend_key TEXT NOT NULL,
             remote_id TEXT NOT NULL,
             message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
             PRIMARY KEY (account_id, backend_key, remote_id)
         );
         CREATE INDEX IF NOT EXISTS remote_messages_message
             ON remote_messages(message_id);

         CREATE TABLE IF NOT EXISTS remote_collections (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             backend_key TEXT NOT NULL,
             remote_id TEXT NOT NULL,
             folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
             PRIMARY KEY (account_id, backend_key, remote_id)
         );
         CREATE INDEX IF NOT EXISTS remote_collections_folder
             ON remote_collections(folder_id);",
    )?;

    backfill_memberships(conn)?;
    backfill_remote(conn)?;

    // Fault injection: fail before the table swap so tests can prove the whole
    // transaction (including the mapping backfill) rolls back to the old schema.
    if super::migration_fault() == 1 {
        return Err(StoreError::corrupt(
            "migration.fault",
            "injected before messages rebuild",
        ));
    }

    // 2. Rebuild the content table without the mandatory folder/UID columns.
    conn.execute_batch(
        "CREATE TABLE messages_new (
             id TEXT PRIMARY KEY,
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             message_id TEXT,
             in_reply_to TEXT,
             references_json TEXT NOT NULL DEFAULT '[]',
             thread_id TEXT NOT NULL,
             subject TEXT NOT NULL DEFAULT '',
             from_json TEXT NOT NULL DEFAULT '[]',
             to_json TEXT NOT NULL DEFAULT '[]',
             cc_json TEXT NOT NULL DEFAULT '[]',
             date INTEGER NOT NULL DEFAULT 0,
             size INTEGER NOT NULL DEFAULT 0,
             flags INTEGER NOT NULL DEFAULT 0,
             snippet TEXT NOT NULL DEFAULT '',
             has_attachments INTEGER NOT NULL DEFAULT 0,
             body_state TEXT NOT NULL DEFAULT 'headers',
             raw_path TEXT
         );
         INSERT INTO messages_new (
             id, account_id, message_id, in_reply_to, references_json, thread_id, subject,
             from_json, to_json, cc_json, date, size, flags, snippet, has_attachments,
             body_state, raw_path
         )
         SELECT id, account_id, message_id, in_reply_to, references_json, thread_id, subject,
                from_json, to_json, cc_json, date, size, flags, snippet, has_attachments,
                body_state, raw_path
         FROM messages;
         DROP TABLE messages;
         ALTER TABLE messages_new RENAME TO messages;
         CREATE INDEX IF NOT EXISTS messages_account_date ON messages(account_id, date DESC);
         CREATE INDEX IF NOT EXISTS messages_thread ON messages(thread_id);",
    )?;

    // 3. The table drop removed the v2 delete/thread triggers; recreate them
    // explicitly and reindex so FTS references the same MessageIds and content.
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS message_fts_message_delete
         AFTER DELETE ON messages
         BEGIN
             DELETE FROM message_fts WHERE message_id = old.id;
         END;

         CREATE TRIGGER IF NOT EXISTS message_fts_thread_update
         AFTER UPDATE OF thread_id ON messages
         WHEN old.thread_id <> new.thread_id
         BEGIN
             UPDATE message_fts SET thread_id = new.thread_id WHERE message_id = new.id;
         END;",
    )?;
    search::backfill_on(conn)?;
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

/// Copy each message's single folder into the new many-to-many relation.
fn backfill_memberships(conn: &Connection) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO message_collections (message_id, folder_id)
         SELECT id, folder_id FROM messages",
        [],
    )?;
    Ok(())
}

/// Record the legacy IMAP/SMTP remote identity for every cached folder and
/// message. The mailbox name is JSON-encoded inside the remote id, never joined
/// with the UID or UIDVALIDITY by a colon.
fn backfill_remote(conn: &Connection) -> Result<(), StoreError> {
    let folders: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, account_id, name FROM folders")?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    {
        let mut insert = conn.prepare(
            "INSERT OR IGNORE INTO remote_collections
                 (account_id, backend_key, remote_id, folder_id)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (folder, account, name) in &folders {
            insert.execute(params![account, IMAP_SMTP_BACKEND_KEY, name, folder])?;
        }
    }

    let messages: Vec<(String, String, i64, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT m.id, m.account_id, m.uid, m.uidvalidity, f.name
             FROM messages m JOIN folders f ON f.id = m.folder_id",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?
    };
    let mut insert = conn.prepare(
        "INSERT OR IGNORE INTO remote_messages
             (account_id, backend_key, remote_id, message_id)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (message, account, uid, uidvalidity, mailbox) in &messages {
        let coords = ImapCoords::new(mailbox.as_str(), *uidvalidity as u32, Uid(*uid as u32));
        insert.execute(params![
            account,
            IMAP_SMTP_BACKEND_KEY,
            coords.encode().as_str(),
            message
        ])?;
    }
    Ok(())
}
