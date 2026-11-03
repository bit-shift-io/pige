//! Folder persistence and UIDVALIDITY resets.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::account::AccountId;
use crate::mail::{Folder, FolderId, FolderKind};

use super::{Store, StoreError};

const COLUMNS: &str = "id, account_id, name, delimiter, kind, unread, total, uidvalidity, uidnext";

impl Store {
    pub fn upsert_folder(&self, folder: &Folder) -> Result<(), StoreError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO folders (
                 id, account_id, name, delimiter, kind, unread, total, uidvalidity, uidnext
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 account_id = excluded.account_id,
                 name = excluded.name,
                 delimiter = excluded.delimiter,
                 kind = excluded.kind,
                 unread = excluded.unread,
                 total = excluded.total,
                 uidvalidity = excluded.uidvalidity,
                 uidnext = excluded.uidnext",
            params![
                folder.id.0,
                folder.account_id.0,
                folder.name,
                folder.delimiter,
                folder.kind.as_str(),
                folder.unread,
                folder.total,
                folder.uidvalidity,
                folder.uidnext,
            ],
        )?;
        Ok(())
    }

    pub fn folders(&self, account: &AccountId) -> Result<Vec<Folder>, StoreError> {
        let conn = self.connection();
        folders_on(&conn, account)
    }

    /// Start a new UIDVALIDITY epoch for a folder: drop every cached message
    /// from the old epoch and clear the sync counters so the next sync refetches
    /// from scratch. Unknown folders are a no-op.
    pub fn reset_folder(
        &self,
        account: &AccountId,
        folder_name: &str,
        uidvalidity: u32,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;

        let folder_id: Option<String> = tx
            .query_row(
                "SELECT id FROM folders WHERE account_id = ?1 AND name = ?2",
                params![account.0, folder_name],
                |row| row.get(0),
            )
            .optional()?;

        let Some(folder_id) = folder_id else {
            tx.commit()?;
            return Ok(());
        };

        tx.execute("DELETE FROM messages WHERE folder_id = ?1", [&folder_id])?;
        tx.execute(
            "UPDATE folders
             SET uidvalidity = ?1, uidnext = NULL, unread = 0, total = 0, last_synced_at = NULL
             WHERE id = ?2",
            params![uidvalidity, folder_id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

pub(crate) fn folders_on(
    conn: &Connection,
    account: &AccountId,
) -> Result<Vec<Folder>, StoreError> {
    let sql =
        format!("SELECT {COLUMNS} FROM folders WHERE account_id = ?1 ORDER BY name COLLATE NOCASE");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([&account.0])?;

    let mut folders = Vec::new();
    while let Some(row) = rows.next()? {
        folders.push(folder_from_row(row)?);
    }
    Ok(folders)
}

pub(crate) fn folder_total_on(
    conn: &Connection,
    folder: &FolderId,
) -> Result<Option<u32>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT total FROM folders WHERE id = ?1",
            [&folder.0],
            |row| row.get(0),
        )
        .optional()?)
}

fn folder_from_row(row: &Row<'_>) -> Result<Folder, StoreError> {
    let kind: String = row.get(4)?;
    Ok(Folder {
        id: FolderId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        name: row.get(2)?,
        delimiter: row.get(3)?,
        kind: FolderKind::from_db(&kind)
            .ok_or_else(|| StoreError::corrupt("folders.kind", kind))?,
        unread: row.get(5)?,
        total: row.get(6)?,
        uidvalidity: row.get(7)?,
        uidnext: row.get(8)?,
    })
}
