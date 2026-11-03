//! Message persistence, flag updates, paging, and conversation aggregation.

use rusqlite::{Row, params};

use crate::account::AccountId;
use crate::mail::{
    Address, Conversation, Flags, FolderId, MessageHeader, MessageId, StoredMessage, ThreadId, Uid,
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
        self.connection().execute(
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
        let sql = format!(
            "SELECT {COLUMNS} FROM messages
             WHERE folder_id = ?1 AND (?2 IS NULL OR date < ?2)
             ORDER BY date DESC, uid DESC
             LIMIT ?3"
        );
        let mut stmt = self.connection().prepare(&sql)?;
        let mut rows = stmt.query(params![folder.0, before_date, i64::from(limit)])?;

        let mut messages = Vec::new();
        while let Some(row) = rows.next()? {
            messages.push(message_from_row(row)?);
        }
        Ok(messages)
    }

    pub fn message(&self, id: &MessageId) -> Result<Option<StoredMessage>, StoreError> {
        let sql = format!("SELECT {COLUMNS} FROM messages WHERE id = ?1");
        let mut stmt = self.connection().prepare(&sql)?;
        let mut rows = stmt.query([&id.0])?;
        match rows.next()? {
            Some(row) => Ok(Some(message_from_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn set_flags(&self, id: &MessageId, flags: &Flags) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE messages SET flags = ?1 WHERE id = ?2",
            params![flags.to_bits(), id.0],
        )?;
        Ok(())
    }

    /// Remove messages older than `uid` in one folder, returning the number of
    /// rows deleted. Used when a server reports a UID high-water mark.
    pub fn delete_messages_below_uid(
        &self,
        folder: &FolderId,
        uid: Uid,
    ) -> Result<usize, StoreError> {
        let deleted = self.connection().execute(
            "DELETE FROM messages WHERE folder_id = ?1 AND uid < ?2",
            params![folder.0, uid.0],
        )?;
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
        let mut stmt = self.connection().prepare(
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
             ORDER BY date DESC
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
    Ok(addresses
        .first()
        .map(|address| {
            address
                .name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| address.email.clone())
        })
        .unwrap_or_default())
}
