//! Message persistence, flag updates, paging, and conversation aggregation.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::account::AccountId;
use crate::mail::{
    Address, Conversation, Flags, FolderId, MessageHeader, MessageId, StoredMessage,
    ThreadAssigner, ThreadId, Uid, thread::is_legacy_thread_id, thread::participants_of,
};

use super::{Store, StoreError};

const COLUMNS: &str = "id, account_id, folder_id, uid, uidvalidity, \
     message_id, in_reply_to, references_json, thread_id, subject, \
     from_json, to_json, cc_json, date, size, flags, snippet, has_attachments";

impl Store {
    pub fn upsert_message(&self, message: &StoredMessage) -> Result<(), StoreError> {
        let references_json = encode_json(&message.header.references)?;
        let from_json = encode_json(&message.header.from)?;
        let to_json = encode_json(&message.header.to)?;
        let cc_json = encode_json(&message.header.cc)?;

        // The natural key is (account, folder, UIDVALIDITY, UID), so a header
        // refresh updates the cached row without changing its local id. Body
        // state and raw paths are left intact for the body fetch path.
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;

        // A header refresh may move a message to a different thread (for
        // example when threading first learns about a reply). Remember the old
        // thread so its aggregates are recomputed too.
        let previous_thread: Option<String> = tx
            .query_row(
                "SELECT thread_id FROM messages
                 WHERE account_id = ?1 AND folder_id = ?2 AND uidvalidity = ?3 AND uid = ?4",
                params![
                    message.account_id.0,
                    message.folder_id.0,
                    message.uidvalidity,
                    message.uid.0,
                ],
                |row| row.get(0),
            )
            .optional()?;

        tx.execute(
            "INSERT INTO messages (
                 id, account_id, folder_id, uid, uidvalidity,
                 message_id, in_reply_to, references_json, thread_id, subject,
                 from_json, to_json, cc_json, date, size, flags, snippet, has_attachments
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(account_id, folder_id, uidvalidity, uid) DO UPDATE SET
                 message_id = excluded.message_id,
                 in_reply_to = excluded.in_reply_to,
                 references_json = excluded.references_json,
                 thread_id = excluded.thread_id,
                 subject = excluded.subject,
                 from_json = excluded.from_json,
                 to_json = excluded.to_json,
                 cc_json = excluded.cc_json,
                 date = excluded.date,
                 size = excluded.size,
                 flags = excluded.flags,
                 snippet = excluded.snippet,
                 has_attachments = excluded.has_attachments",
            params![
                message.id.0,
                message.account_id.0,
                message.folder_id.0,
                message.uid.0,
                message.uidvalidity,
                message.header.message_id,
                message.header.in_reply_to,
                references_json,
                message.thread_id.0,
                message.header.subject,
                from_json,
                to_json,
                cc_json,
                message.header.date,
                message.header.size,
                message.flags.to_bits(),
                message.snippet,
                message.has_attachments,
            ],
        )?;

        // Resolve the natural-key row's id: an update keeps the original local
        // id, so index that row rather than a freshly generated one.
        let stored_id: String = tx.query_row(
            "SELECT id FROM messages
             WHERE account_id = ?1 AND folder_id = ?2 AND uidvalidity = ?3 AND uid = ?4",
            params![
                message.account_id.0,
                message.folder_id.0,
                message.uidvalidity,
                message.uid.0,
            ],
            |row| row.get(0),
        )?;
        super::search::index_message_on(&tx, &MessageId(stored_id))?;

        recompute_thread_on(&tx, &message.thread_id)?;
        if let Some(previous) = previous_thread
            && previous != message.thread_id.0
        {
            recompute_thread_on(&tx, &ThreadId(previous))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Newest-first page of a folder's messages. `before_date` is the exclusive
    /// Unix-seconds cursor used by "load older".
    pub fn messages(
        &self,
        folder: &FolderId,
        limit: u32,
        before_date: Option<i64>,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.connection();
        messages_on(&conn, folder, limit, before_date)
    }

    pub fn message(&self, id: &MessageId) -> Result<Option<StoredMessage>, StoreError> {
        let conn = self.connection();
        message_on(&conn, id)
    }

    /// Every message in one conversation, newest first.
    pub fn messages_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.connection();
        let sql = format!(
            "SELECT {COLUMNS} FROM messages
             WHERE thread_id = ?1
             ORDER BY date DESC, uid DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([&thread_id.0])?;

        let mut messages = Vec::new();
        while let Some(row) = rows.next()? {
            messages.push(message_from_row(row)?);
        }
        Ok(messages)
    }

    pub fn set_flags(&self, id: &MessageId, flags: &Flags) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let thread: Option<String> = tx
            .query_row(
                "SELECT thread_id FROM messages WHERE id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute(
            "UPDATE messages SET flags = ?1 WHERE id = ?2",
            params![flags.to_bits(), id.0],
        )?;
        if let Some(thread) = thread {
            recompute_thread_on(&tx, &ThreadId(thread))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every cached header for one account, across folders. Bodies live in a
    /// separate table, so this is safe to use for rebuilding the thread index.
    pub fn messages_for_account(
        &self,
        account: &AccountId,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.connection();
        let sql = format!(
            "SELECT {COLUMNS} FROM messages
             WHERE account_id = ?1
             ORDER BY date ASC, uid ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([&account.0])?;

        let mut messages = Vec::new();
        while let Some(row) = rows.next()? {
            messages.push(message_from_row(row)?);
        }
        Ok(messages)
    }

    /// Recompute the aggregate row for one conversation from its messages:
    /// newest display fields plus message count, unread count, and attachment
    /// flag. A thread with no messages is removed.
    pub fn recompute_thread(&self, id: &ThreadId) -> Result<(), StoreError> {
        let conn = self.connection();
        recompute_thread_on(&conn, id)
    }

    /// Reassign rows that still carry the pre-threading per-message id. Stable
    /// thread ids (UUIDs assigned by this task, or ids seeded by tests/API
    /// callers) are left exactly as they are.
    ///
    /// Returns the number of message rows whose thread id changed.
    pub fn rethread_legacy_threads(&self, account: &AccountId) -> Result<usize, StoreError> {
        let messages = self.messages_for_account(account)?;
        let (legacy, stable): (Vec<_>, Vec<_>) =
            messages.into_iter().partition(is_legacy_thread_id);
        if legacy.is_empty() {
            return Ok(0);
        }

        let mut assigner = ThreadAssigner::from_messages(&stable);
        let mut affected: HashSet<ThreadId> = HashSet::new();
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let mut changed = 0;

        // `messages_for_account` is oldest-first, so parents are assigned
        // before their replies and reference chains resolve correctly.
        for message in &legacy {
            let participants = participants_of(&message.header);
            let thread = assigner.assign(
                &message.account_id,
                &message.header,
                message.header.date,
                &participants,
            );
            for (from, into) in assigner.take_merges() {
                changed += tx.execute(
                    "UPDATE messages SET thread_id = ?1
                     WHERE account_id = ?2 AND thread_id = ?3",
                    params![into.0, account.0, from.0],
                )?;
                affected.insert(from);
                affected.insert(into);
            }
            if thread != message.thread_id {
                changed += tx.execute(
                    "UPDATE messages SET thread_id = ?1 WHERE id = ?2 AND account_id = ?3",
                    params![thread.0, message.id.0, account.0],
                )?;
                affected.insert(message.thread_id.clone());
                affected.insert(thread);
            }
        }

        for thread in affected {
            recompute_thread_on(&tx, &thread)?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Fold every message from `from` into `into` in one transaction, then
    /// refresh both aggregate rows. Only rows in `account` are touched.
    pub fn merge_threads(
        &self,
        account: &AccountId,
        from: &ThreadId,
        into: &ThreadId,
    ) -> Result<(), StoreError> {
        if from == into {
            return self.recompute_thread(into);
        }
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE messages SET thread_id = ?1 WHERE account_id = ?2 AND thread_id = ?3",
            params![into.0, account.0, from.0],
        )?;
        recompute_thread_on(&tx, into)?;
        recompute_thread_on(&tx, from)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove messages older than `uid` in one folder, returning the number of
    /// rows deleted. Used when a server reports a UID high-water mark.
    pub fn delete_messages_below_uid(
        &self,
        folder: &FolderId,
        uid: Uid,
    ) -> Result<usize, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let mut affected = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT thread_id FROM messages
                 WHERE folder_id = ?1 AND uid < ?2",
            )?;
            let mut rows = stmt.query(params![folder.0, uid.0])?;
            while let Some(row) = rows.next()? {
                affected.push(ThreadId(row.get(0)?));
            }
        }
        let deleted = tx.execute(
            "DELETE FROM messages WHERE folder_id = ?1 AND uid < ?2",
            params![folder.0, uid.0],
        )?;
        for thread in affected {
            recompute_thread_on(&tx, &thread)?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// Conversation list for one folder: one row per thread using its newest
    /// message for display and aggregates over the copies in that folder.
    pub fn conversations(
        &self,
        folder: &FolderId,
        limit: u32,
        before_date: Option<i64>,
    ) -> Result<Vec<Conversation>, StoreError> {
        let conn = self.connection();
        conversations_on(&conn, folder, limit, before_date)
    }

    /// Newest-first page of conversations across every account's Inbox.
    pub fn unified_conversations(&self, limit: u32) -> Result<Vec<Conversation>, StoreError> {
        let conn = self.connection();
        unified_conversations_on(&conn, limit)
    }
}

pub(crate) fn messages_on(
    conn: &Connection,
    folder: &FolderId,
    limit: u32,
    before_date: Option<i64>,
) -> Result<Vec<StoredMessage>, StoreError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM messages
         WHERE folder_id = ?1 AND (?2 IS NULL OR date < ?2)
         ORDER BY date DESC, uid DESC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![folder.0, before_date, i64::from(limit)])?;

    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        messages.push(message_from_row(row)?);
    }
    Ok(messages)
}

pub(crate) fn message_on(
    conn: &Connection,
    id: &MessageId,
) -> Result<Option<StoredMessage>, StoreError> {
    let sql = format!("SELECT {COLUMNS} FROM messages WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([&id.0])?;
    match rows.next()? {
        Some(row) => Ok(Some(message_from_row(row)?)),
        None => Ok(None),
    }
}

pub(crate) fn message_count_on(conn: &Connection, folder: &FolderId) -> Result<u32, StoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
        [&folder.0],
        |row| row.get(0),
    )?)
}

pub(crate) fn conversations_on(
    conn: &Connection,
    folder: &FolderId,
    limit: u32,
    before_date: Option<i64>,
) -> Result<Vec<Conversation>, StoreError> {
    let mut stmt = conn.prepare(
        "WITH ranked AS (
             SELECT
                 thread_id, account_id, subject, from_json, snippet, date, has_attachments,
                 COUNT(*) OVER (PARTITION BY thread_id) AS message_count,
                 SUM(CASE WHEN (flags & 1) = 0 THEN 1 ELSE 0 END)
                     OVER (PARTITION BY thread_id) AS unread,
                 MAX(has_attachments) OVER (PARTITION BY thread_id) AS any_attachments,
                 ROW_NUMBER() OVER (
                     PARTITION BY thread_id ORDER BY date DESC, uid DESC
                 ) AS rank
             FROM messages
             WHERE folder_id = ?1
         )
         SELECT thread_id, account_id, subject, from_json, snippet, date,
                message_count, unread, any_attachments
         FROM ranked
         WHERE rank = 1 AND (?2 IS NULL OR date < ?2)
         ORDER BY date DESC, thread_id
         LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![folder.0, before_date, i64::from(limit)])?;

    let mut conversations = Vec::new();
    while let Some(row) = rows.next()? {
        let from_json: String = row.get(3)?;
        conversations.push(Conversation {
            thread_id: ThreadId(row.get(0)?),
            account_id: AccountId(row.get(1)?),
            subject: row.get(2)?,
            sender: sender_from_json(&from_json)?,
            snippet: row.get(4)?,
            date: row.get(5)?,
            message_count: row.get(6)?,
            unread: row.get(7)?,
            has_attachments: row.get(8)?,
        });
    }
    Ok(conversations)
}

/// Conversations from every account's Inbox. Threads are aggregated per
/// account so a shared thread can never consume another account's message
/// counts; the outer ordering is date DESC with stable thread/account
/// tie-breakers so paging and tests are deterministic.
pub(crate) fn unified_conversations_on(
    conn: &Connection,
    limit: u32,
) -> Result<Vec<Conversation>, StoreError> {
    let mut stmt = conn.prepare(
        "WITH ranked AS (
             SELECT
                 account_id, thread_id, subject, from_json, snippet, date, has_attachments,
                 COUNT(*) OVER (PARTITION BY account_id, thread_id) AS message_count,
                 SUM(CASE WHEN (flags & 1) = 0 THEN 1 ELSE 0 END)
                     OVER (PARTITION BY account_id, thread_id) AS unread,
                 MAX(has_attachments) OVER (PARTITION BY account_id, thread_id) AS any_attachments,
                 ROW_NUMBER() OVER (
                     PARTITION BY account_id, thread_id ORDER BY date DESC, uid DESC
                 ) AS rank
             FROM messages
             WHERE folder_id IN (SELECT id FROM folders WHERE kind = 'inbox')
         )
         SELECT thread_id, account_id, subject, from_json, snippet, date,
                message_count, unread, any_attachments
         FROM ranked
         WHERE rank = 1
         ORDER BY date DESC, thread_id, account_id
         LIMIT ?1",
    )?;
    let mut rows = stmt.query(params![i64::from(limit)])?;

    let mut conversations = Vec::new();
    while let Some(row) = rows.next()? {
        let from_json: String = row.get(3)?;
        conversations.push(Conversation {
            thread_id: ThreadId(row.get(0)?),
            account_id: AccountId(row.get(1)?),
            subject: row.get(2)?,
            sender: sender_from_json(&from_json)?,
            snippet: row.get(4)?,
            date: row.get(5)?,
            message_count: row.get(6)?,
            unread: row.get(7)?,
            has_attachments: row.get(8)?,
        });
    }
    Ok(conversations)
}

/// Recompute one `threads` row inside a caller-owned transaction. The newest
/// message supplies the display subject/sender/snippet; the aggregate columns
/// cover every folder so a conversation moved between folders stays coherent.
pub(crate) fn recompute_thread_on(conn: &Connection, id: &ThreadId) -> Result<(), StoreError> {
    let newest = conn
        .query_row(
            "SELECT account_id, subject, from_json, snippet, date
             FROM messages
             WHERE thread_id = ?1
             ORDER BY date DESC, uid DESC
             LIMIT 1",
            [&id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;

    let Some((account_id, subject, from_json, snippet, date)) = newest else {
        conn.execute("DELETE FROM threads WHERE id = ?1", [&id.0])?;
        return Ok(());
    };

    let (message_count, unread, any_attachments): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN (flags & 1) = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(MAX(has_attachments), 0)
         FROM messages WHERE thread_id = ?1",
        [&id.0],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    conn.execute(
        "INSERT INTO threads (
             id, account_id, subject, snippet, sender, date,
             message_count, unread, has_attachments
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             subject = excluded.subject,
             snippet = excluded.snippet,
             sender = excluded.sender,
             date = excluded.date,
             message_count = excluded.message_count,
             unread = excluded.unread,
             has_attachments = excluded.has_attachments",
        params![
            id.0,
            account_id,
            subject,
            snippet,
            sender_from_json(&from_json)?,
            date,
            message_count,
            unread,
            any_attachments,
        ],
    )?;
    Ok(())
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

fn decode_json<T: serde::de::DeserializeOwned>(
    raw: &str,
    field: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(raw).map_err(|_| StoreError::corrupt(field, raw))
}

fn message_from_row(row: &Row<'_>) -> Result<StoredMessage, StoreError> {
    let references_json: String = row.get(7)?;
    let from_json: String = row.get(10)?;
    let to_json: String = row.get(11)?;
    let cc_json: String = row.get(12)?;
    let flags: i64 = row.get(15)?;

    Ok(StoredMessage {
        id: MessageId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        folder_id: FolderId(row.get(2)?),
        uid: Uid(row.get(3)?),
        uidvalidity: row.get(4)?,
        header: MessageHeader {
            message_id: row.get(5)?,
            in_reply_to: row.get(6)?,
            references: decode_json(&references_json, "messages.references_json")?,
            subject: row.get(9)?,
            from: decode_json(&from_json, "messages.from_json")?,
            to: decode_json(&to_json, "messages.to_json")?,
            cc: decode_json(&cc_json, "messages.cc_json")?,
            date: row.get(13)?,
            size: row.get(14)?,
        },
        flags: Flags::from_bits(flags),
        thread_id: ThreadId(row.get(8)?),
        snippet: row.get(16)?,
        has_attachments: row.get(17)?,
    })
}

fn sender_from_json(raw: &str) -> Result<String, StoreError> {
    let addresses: Vec<Address> = decode_json(raw, "messages.from_json")?;
    Ok(crate::mail::sender_display(&addresses))
}
