//! Durable remote identity mappings.
//!
//! A cached message's *content* identity is its local [`MessageId`]; its
//! *remote* identity is a provider-scoped opaque string. This module owns the
//! mapping between the two and the IMAP-specific serialization of that string.
//!
//! The mapping is deliberately scoped by `(account_id, backend_key, remote_id)`
//! so the same remote id seen in two accounts — or by two backends — never
//! collapses to one local message. A message may have several remote mappings
//! (for example a provider history id beside its message id); the reverse
//! lookup is by `message_id`.
//!
//! IMAP has no account-wide message id: identity is
//! `(mailbox, UIDVALIDITY, UID)`. It is serialized as a versioned JSON object,
//! **never** as a colon-joined string, because an IMAP mailbox name may itself
//! contain the delimiter, colons, quotes, and non-ASCII bytes.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::mail::{FolderId, MessageId, Uid};
use crate::providers::RemoteMessageId;

use super::{Store, StoreError};

/// Backend key used for the legacy IMAP/SMTP adapter's remote mappings. It
/// matches [`crate::providers::IMAP_SMTP_PROVIDER`].
pub const IMAP_SMTP_BACKEND_KEY: &str = crate::providers::IMAP_SMTP_PROVIDER;

/// Schema version of the serialized IMAP remote id.
const IMAP_REMOTE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ImapRemoteWire {
    v: u32,
    mailbox: String,
    uidvalidity: u32,
    uid: u32,
}

/// IMAP identity for one cached message: the mailbox it was cached from plus
/// the UIDVALIDITY epoch and UID that name it inside that mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapCoords {
    pub mailbox: String,
    pub uidvalidity: u32,
    pub uid: Uid,
}

impl ImapCoords {
    pub fn new(mailbox: impl Into<String>, uidvalidity: u32, uid: Uid) -> Self {
        Self {
            mailbox: mailbox.into(),
            uidvalidity,
            uid,
        }
    }

    /// Serialize to the durable remote id. The mailbox name is JSON-quoted, so
    /// arbitrary names (colons, delimiters, quotes, unicode) round-trip.
    pub fn encode(&self) -> RemoteMessageId {
        let wire = ImapRemoteWire {
            v: IMAP_REMOTE_VERSION,
            mailbox: self.mailbox.clone(),
            uidvalidity: self.uidvalidity,
            uid: self.uid.0,
        };
        // The value is fully owned and JSON-serializable; this cannot fail.
        RemoteMessageId::new(serde_json::to_string(&wire).expect("serialize imap remote id"))
    }

    /// Parse a remote id produced by [`Self::encode`]. Returns `None` for a
    /// value that is not this version of the IMAP serialization (for example a
    /// native-provider id), so callers can ignore non-IMAP mappings.
    pub fn decode(remote: &RemoteMessageId) -> Option<Self> {
        let wire: ImapRemoteWire = serde_json::from_str(remote.as_str()).ok()?;
        if wire.v != IMAP_REMOTE_VERSION {
            return None;
        }
        Some(Self {
            mailbox: wire.mailbox,
            uidvalidity: wire.uidvalidity,
            uid: Uid(wire.uid),
        })
    }
}

/// Folder membership and remote identity projected for one cached message,
/// used by the IMAP read path when it needs to locate a message in a mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLocation {
    pub folder_id: FolderId,
    /// The first remote mapping, if the message has one.
    pub remote: Option<RemoteMessageId>,
}

impl Store {
    /// The remote id mapped to a local message. Fails with
    /// [`StoreError::NoRemoteMessage`] when the message has no mapping.
    pub fn remote_message(
        &self,
        account: &AccountId,
        id: &MessageId,
    ) -> Result<RemoteMessageId, StoreError> {
        let remote: Option<String> = self
            .connection()
            .query_row(
                "SELECT remote_id FROM remote_messages
                 WHERE account_id = ?1 AND message_id = ?2
                 ORDER BY backend_key, remote_id LIMIT 1",
                rusqlite::params![account.0, id.0],
                |row| row.get(0),
            )
            .optional()?;
        remote
            .map(RemoteMessageId::new)
            .ok_or_else(|| StoreError::NoRemoteMessage(id.clone()))
    }

    /// Resolve `(account, backend, remote_id)` back to a local message, if the
    /// provider object is already cached.
    pub fn remote_message_for(
        &self,
        account: &AccountId,
        backend: &str,
        remote: &RemoteMessageId,
    ) -> Result<Option<MessageId>, StoreError> {
        let id: Option<String> = self
            .connection()
            .query_row(
                "SELECT message_id FROM remote_messages
                 WHERE account_id = ?1 AND backend_key = ?2 AND remote_id = ?3",
                rusqlite::params![account.0, backend, remote.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.map(MessageId))
    }

    /// Record (or move) the remote mapping for a local message. Re-mapping a
    /// remote id is an upsert so a refresh does not create a duplicate.
    pub fn set_remote_message(
        &self,
        account: &AccountId,
        backend: &str,
        remote: &RemoteMessageId,
        message: &MessageId,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO remote_messages (account_id, backend_key, remote_id, message_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, backend_key, remote_id) DO UPDATE SET
                 message_id = excluded.message_id",
            rusqlite::params![account.0, backend, remote.as_str(), message.0],
        )?;
        Ok(())
    }

    /// Remove one remote mapping without touching the message content.
    pub fn remove_remote_message(
        &self,
        account: &AccountId,
        backend: &str,
        remote: &RemoteMessageId,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "DELETE FROM remote_messages
             WHERE account_id = ?1 AND backend_key = ?2 AND remote_id = ?3",
            rusqlite::params![account.0, backend, remote.as_str()],
        )?;
        Ok(())
    }

    /// IMAP coordinates for a cached message, decoded from its remote mapping.
    /// `None` when the message has no IMAP mapping.
    pub fn imap_coords(&self, id: &MessageId) -> Result<Option<ImapCoords>, StoreError> {
        let remote: Option<String> = self
            .connection()
            .query_row(
                "SELECT remote_id FROM remote_messages
                 WHERE message_id = ?1 AND backend_key = ?2
                 ORDER BY remote_id LIMIT 1",
                rusqlite::params![id.0, IMAP_SMTP_BACKEND_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(remote
            .as_deref()
            .and_then(|raw| ImapCoords::decode(&RemoteMessageId::new(raw))))
    }

    /// The folder a message was read under plus its first remote mapping, for
    /// the IMAP read path. `None` when the message row does not exist.
    pub fn message_location(&self, id: &MessageId) -> Result<Option<MessageLocation>, StoreError> {
        let conn = self.connection();
        let exists: Option<String> = conn
            .query_row("SELECT id FROM messages WHERE id = ?1", [&id.0], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Ok(None);
        }
        let folder_id: Option<String> = conn
            .query_row(
                "SELECT folder_id FROM message_collections
                 WHERE message_id = ?1 ORDER BY folder_id LIMIT 1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()?;
        let remote: Option<String> = conn
            .query_row(
                "SELECT remote_id FROM remote_messages
                 WHERE message_id = ?1 ORDER BY backend_key, remote_id LIMIT 1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()?;
        Ok(folder_id.map(|folder_id| MessageLocation {
            folder_id: FolderId(folder_id),
            remote: remote.map(RemoteMessageId::new),
        }))
    }

    /// Resolve a provider collection id to a local folder, if it is cached.
    pub fn remote_collection_for(
        &self,
        account: &AccountId,
        backend: &str,
        remote: &crate::providers::RemoteCollectionId,
    ) -> Result<Option<FolderId>, StoreError> {
        let id: Option<String> = self
            .connection()
            .query_row(
                "SELECT folder_id FROM remote_collections
                 WHERE account_id = ?1 AND backend_key = ?2 AND remote_id = ?3",
                rusqlite::params![account.0, backend, remote.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.map(FolderId))
    }

    /// Record the mapping from a provider collection id to a local folder.
    pub fn set_remote_collection(
        &self,
        account: &AccountId,
        backend: &str,
        remote: &crate::providers::RemoteCollectionId,
        folder: &FolderId,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO remote_collections (account_id, backend_key, remote_id, folder_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, backend_key, remote_id) DO UPDATE SET
                 folder_id = excluded.folder_id",
            rusqlite::params![account.0, backend, remote.as_str(), folder.0],
        )?;
        Ok(())
    }

    /// Remove one collection mapping without deleting the folder or messages.
    pub fn remove_remote_collection(
        &self,
        account: &AccountId,
        backend: &str,
        remote: &crate::providers::RemoteCollectionId,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "DELETE FROM remote_collections
             WHERE account_id = ?1 AND backend_key = ?2 AND remote_id = ?3",
            rusqlite::params![account.0, backend, remote.as_str()],
        )?;
        Ok(())
    }
}
