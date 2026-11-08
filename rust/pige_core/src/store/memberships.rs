//! Folder membership (many-to-many) and transactional change-page application.
//!
//! `message_collections` is the authoritative relation between a cached content
//! row and the collections (folders or labels) it belongs to. Removing a
//! membership never deletes the message: label removal and deletion are
//! distinct operations. Content is deleted only through
//! [`Store::delete_messages`](crate::store::Store::delete_messages) and its
//! callers, which cascade the body, attachments, and FTS rows.
//!
//! [`Store::apply_change_page`] applies a whole provider change page — content
//! upserts, membership edits, and the resumable checkpoint — in one
//! generation- and cursor-fenced transaction, so a failure leaves neither a
//! half-applied page nor an advanced cursor.

use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::mail::{FolderId, MessageId, StoredMessage, ThreadAssigner, thread::participants_of};
use crate::providers::{
    ChangePage, JobKey, MailChange, MessageRecord, OperationContext, ProviderCursor,
    RemoteCollectionId, RemoteMessageId,
};

use super::messages::{delete_messages_on, merge_threads_on, messages_for_account_on, upsert_message_on};
use super::{Store, StoreError};

impl Store {
    /// The collections (folders or labels) a cached message belongs to.
    pub fn message_memberships(&self, id: &MessageId) -> Result<Vec<FolderId>, StoreError> {
        let conn = self.connection();
        message_memberships_on(&conn, id)
    }

    /// Add one membership. Idempotent; never duplicates a `(message, folder)`
    /// pair. The message and its body are untouched.
    pub fn add_membership(&self, id: &MessageId, folder: &FolderId) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT OR IGNORE INTO message_collections (message_id, folder_id) VALUES (?1, ?2)",
            params![id.0, folder.0],
        )?;
        self.bump_thread_for(id)?;
        Ok(())
    }

    /// Remove one membership. The message, its body, its attachments, and its
    /// other memberships are all preserved.
    pub fn remove_membership(&self, id: &MessageId, folder: &FolderId) -> Result<(), StoreError> {
        self.connection().execute(
            "DELETE FROM message_collections WHERE message_id = ?1 AND folder_id = ?2",
            params![id.0, folder.0],
        )?;
        self.bump_thread_for(id)?;
        Ok(())
    }

    fn bump_thread_for(&self, id: &MessageId) -> Result<(), StoreError> {
        let conn = self.connection();
        let thread: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM messages WHERE id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(thread) = thread {
            super::messages::recompute_thread_on(&conn, &crate::mail::ThreadId(thread))?;
        }
        Ok(())
    }

    /// Apply one provider change page atomically: every content upsert,
    /// membership edit, and collection change lands together with the page's
    /// checkpoint, and only while the account generation still matches and the
    /// job's stored cursor is still `expected_previous`.
    ///
    /// `transport` is the backend key of the session that produced the page, so
    /// a Gmail- or Proton-typed account synced over IMAP writes `imap-smtp`
    /// rows instead of rows under its configured backend. `expected_previous`
    /// is the cursor the page was produced against (`None` for a first page);
    /// a mismatch is [`StoreError::StalePage`], which the caller discards
    /// rather than applying.
    pub fn apply_change_page(
        &self,
        ctx: &OperationContext,
        key: &JobKey,
        transport: &str,
        expected_previous: Option<&ProviderCursor>,
        page: &ChangePage,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        super::jobs::check_generation_in(&tx, &ctx.account, ctx.generation)?;
        super::jobs::check_checkpoint_in(&tx, key, expected_previous)?;

        let mut threads =
            ThreadAssigner::from_messages(&messages_for_account_on(&tx, &ctx.account)?);

        for change in &page.changes {
            match change {
                MailChange::UpsertCollection(record) => {
                    upsert_collection_on(&tx, &ctx.account, transport, record)?;
                }
                MailChange::RemoveCollection(remote) => {
                    remove_collection_on(&tx, &ctx.account, transport, remote)?;
                }
                MailChange::UpsertMessage { message, .. } => {
                    upsert_remote_message_on(&tx, &ctx.account, transport, &mut threads, message)?;
                }
                MailChange::RemoveMessage(remote) => {
                    let id = remote_message_on(&tx, &ctx.account, transport, remote)?
                        .ok_or_else(|| StoreError::UnknownRemoteMessage(remote.clone()))?;
                    delete_messages_on(&tx, &[id])?;
                }
                MailChange::SetMembership {
                    message,
                    collection,
                    present,
                } => {
                    set_membership_on(&tx, &ctx.account, transport, message, collection, *present)?;
                }
            }
        }

        super::jobs::set_checkpoint_in(&tx, key, ctx.generation, &page.checkpoint)?;
        tx.commit()?;
        Ok(())
    }
}

pub(crate) fn message_memberships_on(
    conn: &Connection,
    id: &MessageId,
) -> Result<Vec<FolderId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT folder_id FROM message_collections WHERE message_id = ?1 ORDER BY folder_id",
    )?;
    let mut rows = stmt.query([&id.0])?;
    let mut memberships = Vec::new();
    while let Some(row) = rows.next()? {
        memberships.push(FolderId(row.get(0)?));
    }
    Ok(memberships)
}

fn collection_folder_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    remote: &RemoteCollectionId,
) -> Result<Option<FolderId>, StoreError> {
    let id: Option<String> = conn
        .query_row(
            "SELECT folder_id FROM remote_collections
             WHERE account_id = ?1 AND backend_key = ?2 AND remote_id = ?3",
            params![account.0, backend, remote.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id.map(FolderId))
}

fn set_remote_collection_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    remote: &RemoteCollectionId,
    folder: &FolderId,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO remote_collections (account_id, backend_key, remote_id, folder_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, backend_key, remote_id) DO UPDATE SET
             folder_id = excluded.folder_id",
        params![account.0, backend, remote.as_str(), folder.0],
    )?;
    Ok(())
}

fn remote_message_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    remote: &RemoteMessageId,
) -> Result<Option<MessageId>, StoreError> {
    let id: Option<String> = conn
        .query_row(
            "SELECT message_id FROM remote_messages
             WHERE account_id = ?1 AND backend_key = ?2 AND remote_id = ?3",
            params![account.0, backend, remote.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id.map(MessageId))
}

fn set_remote_message_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    remote: &RemoteMessageId,
    message: &MessageId,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO remote_messages (account_id, backend_key, remote_id, message_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, backend_key, remote_id) DO UPDATE SET
             message_id = excluded.message_id",
        params![account.0, backend, remote.as_str(), message.0],
    )?;
    Ok(())
}

fn upsert_collection_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    record: &crate::providers::CollectionRecord,
) -> Result<FolderId, StoreError> {
    let existing = collection_folder_on(conn, account, backend, &record.remote_id)?;
    let folder_id = match existing {
        Some(id) => id,
        None => {
            // Reuse a folder with the same name in this account if one exists
            // (for example a folder created before the mapping was recorded).
            let by_name: Option<String> = conn
                .query_row(
                    "SELECT id FROM folders WHERE account_id = ?1 AND name = ?2",
                    params![account.0, record.name],
                    |row| row.get(0),
                )
                .optional()?;
            by_name.map(FolderId).unwrap_or_default()
        }
    };
    // Never touch `uidvalidity`/`uidnext` here: the IMAP sync path owns those
    // counters, and overwriting them from a collection record would start a
    // spurious epoch reset. Counters are written only when the provider
    // actually reported them, so an upsert that omits `unread`/`total` cannot
    // zero an existing folder's rollups.
    super::folders::upsert_provider_folder_on(
        conn,
        &super::folders::ProviderFolderUpsert {
            id: &folder_id,
            account_id: account,
            name: &record.name,
            delimiter: record.delimiter.as_deref(),
            kind: record.kind,
            unread: record.unread,
            total: record.total,
        },
    )?;
    set_remote_collection_on(conn, account, backend, &record.remote_id, &folder_id)?;
    Ok(folder_id)
}

fn remove_collection_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    remote: &RemoteCollectionId,
) -> Result<(), StoreError> {
    // Removing a collection mapping must not delete cached content or the
    // folder itself; the folder may still be referenced by memberships until a
    // later reconciliation retires them.
    conn.execute(
        "DELETE FROM remote_collections
         WHERE account_id = ?1 AND backend_key = ?2 AND remote_id = ?3",
        params![account.0, backend, remote.as_str()],
    )?;
    Ok(())
}

fn upsert_remote_message_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    threads: &mut ThreadAssigner,
    record: &MessageRecord,
) -> Result<MessageId, StoreError> {
    let id = remote_message_on(conn, account, backend, &record.remote_id)?
        .unwrap_or_else(MessageId::new);
    let participants = participants_of(&record.header);
    let thread_id = threads.assign(account, &record.header, record.header.date, &participants);
    for (from, into) in threads.take_merges() {
        merge_threads_on(conn, account, &from, &into)?;
    }
    let stored = StoredMessage {
        id: id.clone(),
        account_id: account.clone(),
        folder_id: None,
        uid: None,
        uidvalidity: None,
        header: record.header.clone(),
        flags: record.flags,
        thread_id,
        snippet: record.snippet.clone(),
        has_attachments: record.has_attachments,
    };
    upsert_message_on(conn, &stored)?;
    set_remote_message_on(conn, account, backend, &record.remote_id, &id)?;

    // A message record is a full snapshot of membership, so replace it.
    conn.execute(
        "DELETE FROM message_collections WHERE message_id = ?1",
        [&id.0],
    )?;
    for collection in &record.collections {
        let folder = collection_folder_on(conn, account, backend, collection)?
            .ok_or_else(|| StoreError::UnknownRemoteCollection(collection.clone()))?;
        conn.execute(
            "INSERT OR IGNORE INTO message_collections (message_id, folder_id) VALUES (?1, ?2)",
            params![id.0, folder.0],
        )?;
    }
    Ok(id)
}

fn set_membership_on(
    conn: &Connection,
    account: &AccountId,
    backend: &str,
    message: &RemoteMessageId,
    collection: &RemoteCollectionId,
    present: bool,
) -> Result<(), StoreError> {
    let id = remote_message_on(conn, account, backend, message)?
        .ok_or_else(|| StoreError::UnknownRemoteMessage(message.clone()))?;
    let folder = collection_folder_on(conn, account, backend, collection)?
        .ok_or_else(|| StoreError::UnknownRemoteCollection(collection.clone()))?;
    if present {
        conn.execute(
            "INSERT OR IGNORE INTO message_collections (message_id, folder_id) VALUES (?1, ?2)",
            params![id.0, folder.0],
        )?;
    } else {
        conn.execute(
            "DELETE FROM message_collections WHERE message_id = ?1 AND folder_id = ?2",
            params![id.0, folder.0],
        )?;
    }
    Ok(())
}
