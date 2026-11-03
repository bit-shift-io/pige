//! Attachment metadata in SQLite. Bytes live in the platform cache directory;
//! this table records what the reader should offer and where the cached copy
//! was written.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::mail::{AttachmentDisposition, AttachmentPart, MessageId, ThreadId};

use super::{Store, StoreError, messages::recompute_thread_on};

/// One attachment row: the display/fetch metadata plus the cached file, when
/// the user has opened it at least once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    pub part: AttachmentPart,
    pub cached_path: Option<PathBuf>,
}

impl Store {
    /// Metadata for every attachment on one message, in MIME order.
    pub fn attachments(&self, message_id: &MessageId) -> Result<Vec<AttachmentPart>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT part_id, filename, content_type, size, content_id, disposition
             FROM attachments WHERE message_id = ?1 ORDER BY rowid",
        )?;
        let mut rows = statement.query([&message_id.0])?;
        let mut parts = Vec::new();
        while let Some(row) = rows.next()? {
            parts.push(part_from_row(row)?);
        }
        Ok(parts)
    }

    /// One attachment and its cache path, if it has been downloaded.
    pub fn attachment(
        &self,
        message_id: &MessageId,
        part_id: &str,
    ) -> Result<Option<StoredAttachment>, StoreError> {
        let conn = self.connection();
        conn.query_row(
            "SELECT part_id, filename, content_type, size, content_id, disposition, cached_path
             FROM attachments WHERE message_id = ?1 AND part_id = ?2",
            params![message_id.0, part_id],
            |row| {
                Ok(StoredAttachment {
                    part: part_from_row(row)?,
                    cached_path: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Insert or refresh attachment metadata for one message. Existing cache
    /// paths are preserved, and rows for parts that disappeared from the MIME
    /// tree are removed.
    pub fn store_attachments(
        &self,
        message_id: &MessageId,
        parts: &[AttachmentPart],
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;

        let mut existing = Vec::new();
        {
            let mut statement =
                tx.prepare("SELECT part_id FROM attachments WHERE message_id = ?1")?;
            let mut rows = statement.query([&message_id.0])?;
            while let Some(row) = rows.next()? {
                existing.push(row.get::<_, String>(0)?);
            }
        }

        for part in parts {
            tx.execute(
                "INSERT INTO attachments (
                     id, message_id, part_id, filename, content_type, size,
                     content_id, disposition
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(message_id, part_id) DO UPDATE SET
                     filename = excluded.filename,
                     content_type = excluded.content_type,
                     size = excluded.size,
                     content_id = excluded.content_id,
                     disposition = excluded.disposition",
                params![
                    Uuid::new_v4().to_string(),
                    message_id.0,
                    part.part_id,
                    part.filename,
                    part.content_type,
                    i64::try_from(part.size).unwrap_or(i64::MAX),
                    part.content_id,
                    part.disposition.as_str(),
                ],
            )?;
        }

        for part_id in existing {
            if !parts.iter().any(|part| part.part_id == part_id) {
                tx.execute(
                    "DELETE FROM attachments WHERE message_id = ?1 AND part_id = ?2",
                    params![message_id.0, part_id],
                )?;
            }
        }

        // Sync only writes headers, so it cannot know whether a message has
        // user-visible attachments. Recompute the flag from the metadata we
        // just stored, and refresh the thread aggregate that the list reads.
        let thread_id: Option<String> = tx
            .query_row(
                "SELECT thread_id FROM messages WHERE id = ?1",
                [&message_id.0],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute(
            "UPDATE messages SET has_attachments = ?1 WHERE id = ?2",
            params![!parts.is_empty(), message_id.0],
        )?;
        if let Some(thread_id) = thread_id {
            recompute_thread_on(&tx, &ThreadId(thread_id))?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Remember where an opened attachment was cached.
    pub fn set_attachment_cached_path(
        &self,
        message_id: &MessageId,
        part_id: &str,
        path: &Path,
    ) -> Result<(), StoreError> {
        let path = path.to_string_lossy();
        self.connection().execute(
            "UPDATE attachments SET cached_path = ?1
             WHERE message_id = ?2 AND part_id = ?3",
            params![path.as_ref(), message_id.0, part_id],
        )?;
        Ok(())
    }
}

fn part_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttachmentPart> {
    let size: i64 = row.get(3)?;
    Ok(AttachmentPart {
        part_id: row.get(0)?,
        filename: row.get(1)?,
        content_type: row.get(2)?,
        size: size.max(0) as u64,
        content_id: row.get(4)?,
        disposition: AttachmentDisposition::from_db(&row.get::<_, String>(5)?),
    })
}
