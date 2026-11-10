//! Message persistence, folder membership, flag updates, paging, and
//! conversation aggregation.
//!
//! `messages` holds content identity only, keyed by the local [`MessageId`].
//! Folder membership is a separate many-to-many relation (`message_collections`)
//! and remote identity lives in `remote_messages`; neither is content. Queries
//! that need a folder project it in, so the same content can appear under many
//! labels without being duplicated.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::account::AccountId;
use crate::mail::{
    Address, Conversation, Flags, FolderId, MessageHeader, MessageId, StoredMessage,
    ThreadAssigner, ThreadId, Uid, thread::is_legacy_thread_id, thread::participants_of,
};
use crate::providers::RemoteMessageId;

use super::remote_ids::{IMAP_SMTP_BACKEND_KEY, ImapCoords};
use super::{Store, StoreError};

/// Content columns plus a projected, deterministic first folder membership.
/// `messages` no longer stores a folder, UID, or UIDVALIDITY: membership comes
/// from `message_collections` and remote identity from `remote_messages`.
///
/// Built by a function rather than a `const` so the IMAP projection can filter
/// on [`IMAP_SMTP_BACKEND_KEY`] instead of a duplicated string literal.
fn columns() -> String {
    format!(
        "m.id, m.account_id, m.message_id, m.in_reply_to, \
     m.references_json, m.thread_id, m.subject, \
     m.from_json, m.to_json, m.cc_json, m.date, m.size, m.flags, m.snippet, m.has_attachments, \
     (SELECT mc.folder_id FROM message_collections mc WHERE mc.message_id = m.id \
      ORDER BY mc.folder_id LIMIT 1), \
     (SELECT rm.remote_id FROM remote_messages rm WHERE rm.message_id = m.id \
      AND rm.backend_key = '{IMAP_SMTP_BACKEND_KEY}' ORDER BY rm.remote_id LIMIT 1)"
    )
}

/// The newest uncached Inbox message in a conversation.
#[derive(Debug, Clone)]
pub struct PrefetchCandidate {
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub message_id: MessageId,
    pub uid: Uid,
    pub uidvalidity: u32,
}

impl Store {
    /// Unread conversations first, then recent ones, bounded per account.
    pub fn prefetch_candidates(
        &self,
        max_bytes: u32,
        per_account: u32,
    ) -> Result<Vec<PrefetchCandidate>, StoreError> {
        prefetch_candidates_on(&self.connection(), max_bytes, per_account)
    }

    /// Insert or update one content row. When [`StoredMessage::folder_id`] is
    /// set, the row's membership of that folder is ensured as well; the
    /// authoritative membership may still be edited separately.
    pub fn upsert_message(&self, message: &StoredMessage) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        upsert_message_on(&tx, message)?;
        tx.commit()?;
        Ok(())
    }

    /// Replace one message's list preview, keeping its thread aggregate in
    /// step. Used when a decoded body lets the shell repair a preview that was
    /// cached before the transfer-encoding decoder landed.
    pub fn set_snippet(&self, id: &MessageId, snippet: &str) -> Result<(), StoreError> {
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
            "UPDATE messages SET snippet = ?1 WHERE id = ?2",
            params![snippet, id.0],
        )?;
        if let Some(thread) = thread {
            recompute_thread_on(&tx, &ThreadId(thread))?;
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

    /// The oldest IMAP UID cached for a folder, if any message there has an IMAP
    /// mapping. Used to page older headers.
    pub fn oldest_uid_in_folder(&self, folder: &FolderId) -> Result<Option<Uid>, StoreError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT rm.remote_id FROM message_collections mc
             JOIN remote_messages rm ON rm.message_id = mc.message_id
             WHERE mc.folder_id = ?1",
        )?;
        let mut rows = stmt.query([&folder.0])?;
        let mut oldest: Option<Uid> = None;
        while let Some(row) = rows.next()? {
            let remote: String = row.get(0)?;
            if let Some(coords) = ImapCoords::decode(&RemoteMessageId::new(remote)) {
                oldest = Some(oldest.map_or(coords.uid, |current| current.min(coords.uid)));
            }
        }
        Ok(oldest)
    }

    /// Every message in one conversation, newest first.
    pub fn messages_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.connection();
        let sql = format!(
            "SELECT {} FROM messages m
             WHERE m.thread_id = ?1
             ORDER BY m.date DESC, m.rowid DESC",
            columns()
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

    /// Remove cached content by local id, recomputing the threads they belonged
    /// to. Membership rows, remote mappings, bodies, attachments, and the FTS
    /// rows cascade away with the message.
    pub fn delete_messages(&self, ids: &[MessageId]) -> Result<usize, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let deleted = delete_messages_on(&tx, ids)?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Every cached header for one account, across folders. Bodies live in a
    /// separate table, so this is safe to use for rebuilding the thread index.
    pub fn messages_for_account(
        &self,
        account: &AccountId,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.connection();
        messages_for_account_on(&conn, account)
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
        merge_threads_on(&tx, account, from, into)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove messages older than `uid` in one folder, returning the number of
    /// rows deleted. The UID comes from each message's IMAP remote mapping, so
    /// it is decoded rather than split from a formatted key.
    pub fn delete_messages_below_uid(
        &self,
        folder: &FolderId,
        uid: Uid,
    ) -> Result<usize, StoreError> {
        let ids = {
            let conn = self.connection();
            let mut stmt = conn.prepare(
                "SELECT m.id, rm.remote_id FROM messages m
                 JOIN message_collections mc ON mc.message_id = m.id AND mc.folder_id = ?1
                 LEFT JOIN remote_messages rm ON rm.message_id = m.id",
            )?;
            let mut rows = stmt.query([&folder.0])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let remote: Option<String> = row.get(1)?;
                let older = remote
                    .as_deref()
                    .and_then(|raw| ImapCoords::decode(&RemoteMessageId::new(raw)))
                    .is_some_and(|coords| coords.uid < uid);
                if older {
                    ids.push(MessageId(id));
                }
            }
            ids
        };
        self.delete_messages(&ids)
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

/// Insert or update one message row and (when projected) its folder membership
/// inside a caller-owned transaction.
pub(crate) fn upsert_message_on(
    conn: &Connection,
    message: &StoredMessage,
) -> Result<(), StoreError> {
    let references_json = encode_json(&message.header.references)?;
    let from_json = encode_json(&message.header.from)?;
    let to_json = encode_json(&message.header.to)?;
    let cc_json = encode_json(&message.header.cc)?;

    // A header refresh may move a message to a different thread. Remember the
    // old thread so its aggregates are recomputed too.
    let previous_thread: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM messages WHERE id = ?1",
            [&message.id.0],
            |row| row.get(0),
        )
        .optional()?;

    conn.execute(
        "INSERT INTO messages (
             id, account_id, message_id, in_reply_to, references_json, thread_id, subject,
             from_json, to_json, cc_json, date, size, flags, snippet, has_attachments
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
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

    if let Some(folder) = &message.folder_id {
        conn.execute(
            "INSERT OR IGNORE INTO message_collections (message_id, folder_id)
             VALUES (?1, ?2)",
            params![message.id.0, folder.0],
        )?;
    }

    // A message carrying both a folder and IMAP UID coordinates records its
    // durable IMAP remote identity here, so a later re-sync resolves to the
    // same local id. The mapping is keyed by the *transport* that produced the
    // coordinates (`imap-smtp`), never by the account's configured backend: a
    // Gmail- or Proton-typed account may still be syncing over IMAP, and the
    // sync engine records the identical coordinate under `imap-smtp`. Keying by
    // the configured backend would split one message into two mapping rows.
    // A message with no UID (a native API message) writes no IMAP mapping.
    if let (Some(folder), Some(uid), Some(uidvalidity)) =
        (&message.folder_id, message.uid, message.uidvalidity)
        && let Some(mailbox) = conn
            .query_row(
                "SELECT name FROM folders WHERE id = ?1",
                [&folder.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
    {
        let remote = ImapCoords::new(mailbox, uidvalidity, uid).encode();
        conn.execute(
            "INSERT INTO remote_messages (account_id, backend_key, remote_id, message_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, backend_key, remote_id) DO UPDATE SET
                 message_id = excluded.message_id",
            params![
                message.account_id.0,
                IMAP_SMTP_BACKEND_KEY,
                remote.as_str(),
                message.id.0
            ],
        )?;
    }

    super::search::index_message_on(conn, &message.id)?;

    recompute_thread_on(conn, &message.thread_id)?;
    if let Some(previous) = previous_thread
        && previous != message.thread_id.0
    {
        recompute_thread_on(conn, &ThreadId(previous))?;
    }
    Ok(())
}

/// Delete content rows (and, through cascades, their memberships, remote
/// mappings, bodies, attachments, and FTS rows) inside a caller-owned
/// transaction, recomputing the affected threads.
pub(crate) fn delete_messages_on(
    conn: &Connection,
    ids: &[MessageId],
) -> Result<usize, StoreError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut affected: HashSet<ThreadId> = HashSet::new();
    let mut deleted = 0;
    for id in ids {
        let thread: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM messages WHERE id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()?;
        deleted += conn.execute("DELETE FROM messages WHERE id = ?1", [&id.0])?;
        if let Some(thread) = thread {
            affected.insert(ThreadId(thread));
        }
    }
    for thread in affected {
        recompute_thread_on(conn, &thread)?;
    }
    Ok(deleted)
}

/// Fold every message from `from` into `into` for one account inside a
/// caller-owned transaction.
pub(crate) fn merge_threads_on(
    conn: &Connection,
    account: &AccountId,
    from: &ThreadId,
    into: &ThreadId,
) -> Result<(), StoreError> {
    if from == into {
        return recompute_thread_on(conn, into);
    }
    conn.execute(
        "UPDATE messages SET thread_id = ?1 WHERE account_id = ?2 AND thread_id = ?3",
        params![into.0, account.0, from.0],
    )?;
    recompute_thread_on(conn, into)?;
    recompute_thread_on(conn, from)?;
    Ok(())
}

pub(crate) fn messages_on(
    conn: &Connection,
    folder: &FolderId,
    limit: u32,
    before_date: Option<i64>,
) -> Result<Vec<StoredMessage>, StoreError> {
    let sql = format!(
        "SELECT {} FROM messages m
         JOIN message_collections mc ON mc.message_id = m.id AND mc.folder_id = ?1
         WHERE (?2 IS NULL OR m.date < ?2)
         ORDER BY m.date DESC, m.rowid DESC
         LIMIT ?3",
        columns()
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
    let sql = format!("SELECT {} FROM messages m WHERE m.id = ?1", columns());
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([&id.0])?;
    match rows.next()? {
        Some(row) => Ok(Some(message_from_row(row)?)),
        None => Ok(None),
    }
}

pub(crate) fn messages_for_account_on(
    conn: &Connection,
    account: &AccountId,
) -> Result<Vec<StoredMessage>, StoreError> {
    let sql = format!(
        "SELECT {} FROM messages m
         WHERE m.account_id = ?1
         ORDER BY m.date ASC, m.rowid ASC",
        columns()
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([&account.0])?;

    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        messages.push(message_from_row(row)?);
    }
    Ok(messages)
}

/// One bounded page of cached messages for the local learning backfill,
/// ordered by the durable `messages.id` cursor. `after` is exclusive.
pub(crate) fn cached_messages_after_on(
    conn: &Connection,
    account: &AccountId,
    after: Option<&MessageId>,
    limit: u32,
) -> Result<Vec<StoredMessage>, StoreError> {
    let sql = format!(
        "SELECT {} FROM messages m
         WHERE m.account_id = ?1 AND (?2 IS NULL OR m.id > ?2)
         ORDER BY m.id ASC
         LIMIT ?3",
        columns()
    );
    let mut stmt = conn.prepare(&sql)?;
    let after = after.map(|id| id.0.as_str());
    let mut rows = stmt.query(params![account.0, after, i64::from(limit)])?;

    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        messages.push(message_from_row(row)?);
    }
    Ok(messages)
}

pub(crate) fn message_count_on(conn: &Connection, folder: &FolderId) -> Result<u32, StoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM message_collections WHERE folder_id = ?1",
        [&folder.0],
        |row| row.get(0),
    )?)
}

/// Rank Inbox messages into prefetch candidates. The work is done in Rust
/// because a message's IMAP UID is decoded from its opaque remote id rather
/// than stored in a column.
fn prefetch_candidates_on(
    conn: &Connection,
    max_bytes: u32,
    per_account: u32,
) -> Result<Vec<PrefetchCandidate>, StoreError> {
    struct RowData {
        account: AccountId,
        folder: FolderId,
        message: MessageId,
        thread: ThreadId,
        date: i64,
        seen: bool,
        size: i64,
        fetchable: bool,
        coords: Option<ImapCoords>,
    }

    let mut stmt = conn.prepare(
        "SELECT m.id, m.account_id, mc.folder_id, rm.remote_id, m.thread_id, m.date,
                (m.flags & 1) = 0, m.size, m.body_state = 'headers'
                    AND NOT EXISTS (SELECT 1 FROM bodies b WHERE b.message_id = m.id)
         FROM messages m
         JOIN message_collections mc ON mc.message_id = m.id
         JOIN folders f ON f.id = mc.folder_id AND f.kind = 'inbox'
         LEFT JOIN remote_messages rm ON rm.message_id = m.id
         ORDER BY m.account_id, m.thread_id, m.date DESC, m.rowid DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        let remote: Option<String> = row.get(3)?;
        Ok(RowData {
            message: MessageId(row.get(0)?),
            account: AccountId(row.get(1)?),
            folder: FolderId(row.get(2)?),
            coords: remote
                .as_deref()
                .and_then(|raw| ImapCoords::decode(&RemoteMessageId::new(raw))),
            thread: ThreadId(row.get(4)?),
            date: row.get(5)?,
            seen: !row.get::<_, bool>(6)?,
            size: row.get(7)?,
            fetchable: row.get(8)?,
        })
    })?;
    let mut data = Vec::new();
    for row in rows {
        data.push(row?);
    }

    // Newest message per (account, thread), plus whether the thread has unread.
    let mut threads: Vec<(AccountId, bool, usize)> = Vec::new();
    let mut index: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    for (position, row) in data.iter().enumerate() {
        let key = (row.account.0.clone(), row.thread.0.clone());
        match index.get(&key) {
            Some(&slot) => {
                if !row.seen {
                    threads[slot].1 = true;
                }
            }
            None => {
                index.insert(key, threads.len());
                threads.push((row.account.clone(), !row.seen, position));
            }
        }
    }

    #[derive(Debug)]
    struct Candidate {
        unread: bool,
        date: i64,
        uid: u32,
        account: AccountId,
        candidate: PrefetchCandidate,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    for (account, unread, newest) in threads {
        let row = &data[newest];
        let Some(coords) = &row.coords else { continue };
        if !row.fetchable || row.size > i64::from(max_bytes) {
            continue;
        }
        // Only the newest message of a thread is a candidate.
        candidates.push(Candidate {
            unread,
            date: row.date,
            uid: coords.uid.0,
            account: account.clone(),
            candidate: PrefetchCandidate {
                account_id: account,
                folder_id: row.folder.clone(),
                message_id: row.message.clone(),
                uid: coords.uid,
                uidvalidity: coords.uidvalidity,
            },
        });
    }

    let mut per_account_count: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    candidates.sort_by(|a, b| {
        a.account
            .0
            .cmp(&b.account.0)
            .then_with(|| b.unread.cmp(&a.unread))
            .then_with(|| b.date.cmp(&a.date))
            .then_with(|| b.uid.cmp(&a.uid))
    });
    let mut result = Vec::new();
    for candidate in candidates {
        let count = per_account_count
            .entry(candidate.account.0.clone())
            .or_insert(0);
        if *count >= per_account {
            continue;
        }
        *count += 1;
        result.push(candidate.candidate);
    }
    Ok(result)
}

/// Conversation list for one folder: one row per thread using its newest
/// message for display and aggregates over the copies in that folder.
pub(crate) fn conversations_on(
    conn: &Connection,
    folder: &FolderId,
    limit: u32,
    before_date: Option<i64>,
) -> Result<Vec<Conversation>, StoreError> {
    let mut stmt = conn.prepare(
        "WITH scoped AS (
             SELECT m.rowid AS rowid, m.thread_id AS thread_id, m.account_id AS account_id,
                    m.subject AS subject, m.from_json AS from_json, m.snippet AS snippet,
                    m.date AS date, m.has_attachments AS has_attachments, m.flags AS flags
             FROM messages m
             JOIN message_collections mc ON mc.message_id = m.id
             WHERE mc.folder_id = ?1
         ),
         ranked AS (
             SELECT
                 thread_id, account_id, subject, from_json, snippet, date, has_attachments,
                 COUNT(*) OVER (PARTITION BY thread_id) AS message_count,
                 SUM(CASE WHEN (flags & 1) = 0 THEN 1 ELSE 0 END)
                     OVER (PARTITION BY thread_id) AS unread,
                 MAX(has_attachments) OVER (PARTITION BY thread_id) AS any_attachments,
                 ROW_NUMBER() OVER (
                     PARTITION BY thread_id ORDER BY date DESC, rowid DESC
                 ) AS rank
             FROM scoped
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
        "WITH scoped AS (
             SELECT m.rowid AS rowid, m.account_id AS account_id, m.thread_id AS thread_id,
                    m.subject AS subject, m.from_json AS from_json, m.snippet AS snippet,
                    m.date AS date, m.has_attachments AS has_attachments, m.flags AS flags
             FROM messages m
             JOIN message_collections mc ON mc.message_id = m.id
             JOIN folders f ON f.id = mc.folder_id AND f.kind = 'inbox'
         ),
         ranked AS (
             SELECT
                 account_id, thread_id, subject, from_json, snippet, date, has_attachments,
                 COUNT(*) OVER (PARTITION BY account_id, thread_id) AS message_count,
                 SUM(CASE WHEN (flags & 1) = 0 THEN 1 ELSE 0 END)
                     OVER (PARTITION BY account_id, thread_id) AS unread,
                 MAX(has_attachments) OVER (PARTITION BY account_id, thread_id) AS any_attachments,
                 ROW_NUMBER() OVER (
                     PARTITION BY account_id, thread_id ORDER BY date DESC, rowid DESC
                 ) AS rank
             FROM scoped
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
             ORDER BY date DESC, rowid DESC
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
    let references_json: String = row.get(4)?;
    let from_json: String = row.get(7)?;
    let to_json: String = row.get(8)?;
    let cc_json: String = row.get(9)?;
    let flags: i64 = row.get(12)?;
    let folder_id: Option<String> = row.get(15)?;
    let remote: Option<String> = row.get(16)?;
    let coords = remote
        .as_deref()
        .and_then(|raw| ImapCoords::decode(&RemoteMessageId::new(raw)));

    Ok(StoredMessage {
        id: MessageId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        folder_id: folder_id.map(FolderId),
        uid: coords.as_ref().map(|coords| coords.uid),
        uidvalidity: coords.as_ref().map(|coords| coords.uidvalidity),
        header: MessageHeader {
            message_id: row.get(2)?,
            in_reply_to: row.get(3)?,
            references: decode_json(&references_json, "messages.references_json")?,
            subject: row.get(6)?,
            from: decode_json(&from_json, "messages.from_json")?,
            to: decode_json(&to_json, "messages.to_json")?,
            cc: decode_json(&cc_json, "messages.cc_json")?,
            // `Reply-To`/`Bcc` are learned from provider headers; the cache does
            // not persist columns for them yet, so a stored row reads them back
            // empty rather than inventing addresses.
            reply_to: Vec::new(),
            bcc: Vec::new(),
            date: row.get(10)?,
            size: row.get(11)?,
        },
        flags: Flags::from_bits(flags),
        thread_id: ThreadId(row.get(5)?),
        snippet: row.get(13)?,
        has_attachments: row.get(14)?,
    })
}

fn sender_from_json(raw: &str) -> Result<String, StoreError> {
    let addresses: Vec<Address> = decode_json(raw, "messages.from_json")?;
    Ok(crate::mail::sender_display(&addresses))
}
