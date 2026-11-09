//! Folder persistence and UIDVALIDITY resets.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::account::AccountId;
use crate::mail::{Folder, FolderId, FolderKind};

use super::messages::recompute_thread_on;
use super::{Store, StoreError};

const COLUMNS: &str = "id, account_id, name, delimiter, kind, unread, total, uidvalidity, uidnext";

impl Store {
    pub fn upsert_folder(&self, folder: &Folder) -> Result<(), StoreError> {
        upsert_folder_on(&self.connection(), folder)
    }

    pub fn folders(&self, account: &AccountId) -> Result<Vec<Folder>, StoreError> {
        let conn = self.connection();
        folders_on(&conn, account)
    }

    /// Recompute a folder's cached `unread` rollup from its memberships. `total`
    /// is deliberately left as the value the provider last reported (server
    /// `exists`), because paging (`has_more`) compares it against the cached
    /// count; recomputing it from the cache would hide older mail on the server.
    pub fn recompute_folder_unread(&self, folder: &FolderId) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE folders SET
                 unread = (
                     SELECT COUNT(*) FROM message_collections mc
                     JOIN messages m ON m.id = mc.message_id
                     WHERE mc.folder_id = ?1 AND (m.flags & 1) = 0
                 )
             WHERE id = ?1",
            [&folder.0],
        )?;
        Ok(())
    }

    /// Start a new UIDVALIDITY epoch for a folder: drop every cached message
    /// from the old epoch and clear the sync counters so the next sync refetches
    /// from scratch. Unknown folders are a no-op.
    ///
    /// Only messages whose *only* membership is this folder are deleted;
    /// content shared with another collection survives with its body intact.
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

        let mut affected = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT m.thread_id FROM messages m
                 JOIN message_collections mc ON mc.message_id = m.id
                 WHERE mc.folder_id = ?1",
            )?;
            let mut rows = stmt.query([&folder_id])?;
            while let Some(row) = rows.next()? {
                affected.push(crate::mail::ThreadId(row.get(0)?));
            }
        }

        // Delete content that exists only in this folder, then drop this
        // folder's remaining memberships. Shared content keeps its body,
        // attachments, and other collection memberships.
        tx.execute(
            "DELETE FROM messages
             WHERE id IN (SELECT message_id FROM message_collections WHERE folder_id = ?1)
               AND id NOT IN (SELECT message_id FROM message_collections WHERE folder_id != ?1)",
            [&folder_id],
        )?;
        tx.execute(
            "DELETE FROM message_collections WHERE folder_id = ?1",
            [&folder_id],
        )?;
        for thread in affected {
            recompute_thread_on(&tx, &thread)?;
        }
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

pub(crate) fn upsert_folder_on(conn: &Connection, folder: &Folder) -> Result<(), StoreError> {
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

/// A folder upsert requested by a provider collection.
///
/// Unlike a full [`Folder`] this carries *optional* counters, so the store can
/// tell "the provider reported zero" from "the provider said nothing", and it
/// deliberately has no `uidvalidity`/`uidnext` fields: those sync counters are
/// owned by the IMAP path and must never be rewritten from a collection record.
pub(crate) struct ProviderFolderUpsert<'a> {
    pub id: &'a FolderId,
    pub account_id: &'a AccountId,
    pub name: &'a str,
    pub delimiter: Option<&'a str>,
    pub kind: FolderKind,
    pub unread: Option<u32>,
    pub total: Option<u32>,
}

/// Insert or update a folder described by a provider collection.
///
/// Unlike [`upsert_folder_on`], this never writes the `uidvalidity`/`uidnext`
/// sync counters, because the IMAP sync path owns them and a collection upsert
/// must not start a spurious epoch reset. It also overwrites `unread`/`total`
/// only when the provider actually reported a value, so a record that omits
/// counters cannot zero a folder's rollups.
pub(crate) fn upsert_provider_folder_on(
    conn: &Connection,
    folder: &ProviderFolderUpsert<'_>,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO folders (id, account_id, name, delimiter, kind, unread, total)
         VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?6, 0), COALESCE(?7, 0))
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             name = excluded.name,
             delimiter = excluded.delimiter,
             kind = excluded.kind,
             unread = COALESCE(?6, folders.unread),
             total = COALESCE(?7, folders.total)",
        params![
            folder.id.0,
            folder.account_id.0,
            folder.name,
            folder.delimiter,
            folder.kind.as_str(),
            folder.unread,
            folder.total,
        ],
    )?;
    Ok(())
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
