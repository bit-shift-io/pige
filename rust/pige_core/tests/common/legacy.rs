//! Synthetic **real** v4 fixtures for migration tests.
#![allow(dead_code)]
//!
//! The fixture is built by running the actual migration pipeline to v4 with
//! [`Store::migrate_to`], then inserting rows through the v4 schema itself.
//! Nothing here hand-rolls an approximation of the old tables, so a column that
//! drifts between the real schema and the fixture is caught immediately.

use std::path::{Path, PathBuf};

use pige_core::Store;
use rusqlite::{Connection, params};

/// A database migrated to the v4 schema and (optionally) seeded with rows.
pub struct LegacyV4 {
    conn: Connection,
    path: PathBuf,
}

impl LegacyV4 {
    /// Create an empty v4 database at `path` using the real migration pipeline.
    pub fn create(path: &Path) -> Self {
        let store = Store::open(path).expect("open store");
        store.migrate_to(4).expect("migrate to v4");
        drop(store);
        let conn = Connection::open(path).expect("open raw connection");
        conn.execute_batch("PRAGMA foreign_keys=OFF;").expect("fk off");
        Self {
            conn,
            path: path.to_path_buf(),
        }
    }

    pub fn account(&self, id: &str, email: &str, order: i64) -> &Self {
        self.conn
            .execute(
                "INSERT INTO accounts (
                     id, label, email, display_name, username,
                     imap_host, imap_port, imap_security,
                     smtp_host, smtp_port, smtp_security, auth_kind, sort_order
                 ) VALUES (?1, ?1, ?2, ?1, ?2, 'imap.example.com', 993, 'tls',
                           'smtp.example.com', 465, 'tls', 'password', ?3)",
                params![id, email, order],
            )
            .expect("insert account");
        self
    }

    pub fn folder(&self, id: &str, account: &str, name: &str, kind: &str, uidvalidity: u32) -> &Self {
        self.conn
            .execute(
                "INSERT INTO folders (
                     id, account_id, name, delimiter, kind, unread, total, uidvalidity, uidnext
                 ) VALUES (?1, ?2, ?3, '/', ?4, 0, 0, ?5, 1)",
                params![id, account, name, kind, uidvalidity],
            )
            .expect("insert folder");
        self
    }

    /// Insert a message with the v1/v4 mandatory folder and UID columns. A
    /// `None` RFC Message-ID models mail that arrived without one; callers can
    /// pass the same id twice to model a duplicate.
    #[allow(clippy::too_many_arguments)]
    pub fn message(
        &self,
        id: &str,
        account: &str,
        folder: &str,
        uid: u32,
        uidvalidity: u32,
        rfc_message_id: Option<&str>,
        subject: &str,
        thread_id: &str,
        date: i64,
    ) -> &Self {
        self.conn
            .execute(
                "INSERT INTO messages (
                     id, account_id, folder_id, uid, uidvalidity, message_id, in_reply_to,
                     references_json, thread_id, subject, from_json, to_json, cc_json,
                     date, size, flags, snippet, has_attachments, body_state
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, '[]', ?7, ?8,
                           '[{\"name\":null,\"email\":\"sender@example.com\"}]', '[]', '[]',
                           ?9, 128, 0, ?8, 0, 'headers')",
                params![
                    id,
                    account,
                    folder,
                    uid,
                    uidvalidity,
                    rfc_message_id,
                    thread_id,
                    subject,
                    date
                ],
            )
            .expect("insert message");
        self
    }

    pub fn body(&self, message_id: &str, plain_text: &str) -> &Self {
        self.conn
            .execute(
                "INSERT INTO bodies (message_id, plain_text, html, fetched_at)
                 VALUES (?1, ?2, NULL, 1)",
                params![message_id, plain_text],
            )
            .expect("insert body");
        self
    }

    pub fn attachment(&self, id: &str, message_id: &str, part_id: &str, filename: &str) -> &Self {
        self.conn
            .execute(
                "INSERT INTO attachments (
                     id, message_id, part_id, filename, content_type, size, content_id, disposition
                 ) VALUES (?1, ?2, ?3, ?4, 'application/pdf', 10, NULL, 'attachment')",
                params![id, message_id, part_id, filename],
            )
            .expect("insert attachment");
        self
    }

    pub fn outbox(&self, id: &str, account: &str, state: &str) -> &Self {
        self.conn
            .execute(
                "INSERT INTO outbox (
                     id, account_id, thread_id, to_json, cc_json, bcc_json, subject, body,
                     in_reply_to, references_json, state, created_at, updated_at
                 ) VALUES (?1, ?2, NULL, '[]', '[]', '[]', 'draft', 'body', NULL, '[]', ?3, 1, 1)",
                params![id, account, state],
            )
            .expect("insert outbox");
        self
    }

    /// Insert a pre-existing FTS row, as the v2 backfill would have.
    pub fn fts(&self, message_id: &str, account: &str, folder: &str, subject: &str, body: &str) -> &Self {
        self.conn
            .execute(
                "INSERT INTO message_fts (
                     message_id, thread_id, account_id, folder_id, subject, sender, recipients, body
                 ) VALUES (?1, '', ?2, ?3, ?4, '', '', ?5)",
                params![message_id, account, folder, subject, body],
            )
            .expect("insert fts row");
        self
    }

    /// Run one raw statement and return the row count (for outbox checks).
    pub fn count(&self, sql: &str) -> i64 {
        self.conn
            .query_row(sql, [], |row| row.get(0))
            .expect("count")
    }

    pub fn has_column(&self, table: &str, column: &str) -> bool {
        let mut statement = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("table_info");
        let mut rows = statement.query([]).expect("query");
        while let Some(row) = rows.next().expect("row") {
            if row.get::<_, String>(1).expect("name") == column {
                return true;
            }
        }
        false
    }

    /// Close the builder and reopen a `Store` over the fixture without
    /// migrating, so a test can drive `migrate`/`migrate_to` itself.
    pub fn into_store(self) -> Store {
        drop(self.conn);
        Store::open(&self.path).expect("reopen store")
    }

    /// Run `PRAGMA integrity_check` and `PRAGMA foreign_key_check` on the
    /// fixture, returning any reported problems.
    pub fn integrity_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        {
            let mut statement = self.conn.prepare("PRAGMA integrity_check").expect("prep");
            let mut rows = statement.query([]).expect("query");
            while let Some(row) = rows.next().expect("row") {
                let value: String = row.get(0).expect("value");
                if value != "ok" {
                    problems.push(value);
                }
            }
        }
        {
            let mut statement = self
                .conn
                .prepare("PRAGMA foreign_key_check")
                .expect("prep");
            let mut rows = statement.query([]).expect("query");
            while let Some(row) = rows.next().expect("row") {
                problems.push(format!("foreign key violation: {row:?}"));
            }
        }
        problems
    }
}
