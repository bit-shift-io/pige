//! IMAP remote identity mapping.
//!
//! IMAP has no account-wide message id: a message is named by
//! `(mailbox, UIDVALIDITY, UID)`. The mailbox name is serialized as a *quoted
//! JSON string*, never a colon-joined path, so a name containing the hierarchy
//! delimiter, colons, quotes, or non-ASCII characters round-trips exactly.
//!
//! This module is a thin, provider-local façade over [`ImapCoords`], the
//! F4-owned serialization, so the adapter never invents its own id format and
//! the store can decode what the adapter produced.

use crate::mail::Uid;
use crate::providers::{RemoteCollectionId, RemoteMessageId};
use crate::store::ImapCoords;

/// The durable remote id for one IMAP message.
pub fn message_remote_id(mailbox: &str, uidvalidity: u32, uid: Uid) -> RemoteMessageId {
    ImapCoords::new(mailbox, uidvalidity, uid).encode()
}

/// Parse a remote id produced by [`message_remote_id`]. Returns `None` for a
/// value that is not this version of the IMAP serialization.
pub fn decode_message(remote: &RemoteMessageId) -> Option<ImapCoords> {
    ImapCoords::decode(remote)
}

/// The remote id for one collection. A collection *is* its mailbox, so the
/// name is the identity; it is stored opaque and never parsed apart.
pub fn collection_remote_id(mailbox: &str) -> RemoteCollectionId {
    RemoteCollectionId::from(mailbox.to_string())
}
