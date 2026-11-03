//! Message envelopes, IMAP flags, and conversation aggregates as stored in
//! the local cache.

use serde::{Deserialize, Serialize};

use crate::account::AccountId;

use super::{Address, MessageId, ThreadId, Uid};

/// Envelope fields cached for the list and reader. Bodies live separately so a
/// header refresh never discards downloaded text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHeader {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub subject: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// Unix seconds, UTC.
    pub date: i64,
    pub size: u32,
}

/// IMAP system flags, packed into an integer column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flags {
    pub seen: bool,
    pub answered: bool,
    pub flagged: bool,
    pub deleted: bool,
    pub draft: bool,
}

impl Flags {
    const SEEN: i64 = 1 << 0;
    const ANSWERED: i64 = 1 << 1;
    const FLAGGED: i64 = 1 << 2;
    const DELETED: i64 = 1 << 3;
    const DRAFT: i64 = 1 << 4;

    pub fn to_bits(self) -> i64 {
        let mut bits = 0;
        if self.seen {
            bits |= Self::SEEN;
        }
        if self.answered {
            bits |= Self::ANSWERED;
        }
        if self.flagged {
            bits |= Self::FLAGGED;
        }
        if self.deleted {
            bits |= Self::DELETED;
        }
        if self.draft {
            bits |= Self::DRAFT;
        }
        bits
    }

    pub fn from_bits(bits: i64) -> Self {
        Self {
            seen: bits & Self::SEEN != 0,
            answered: bits & Self::ANSWERED != 0,
            flagged: bits & Self::FLAGGED != 0,
            deleted: bits & Self::DELETED != 0,
            draft: bits & Self::DRAFT != 0,
        }
    }
}

/// A cached message row. `thread_id` is assigned at insert time; until real
/// threading lands each message gets its own thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: MessageId,
    pub account_id: AccountId,
    pub folder_id: super::FolderId,
    pub uid: Uid,
    pub uidvalidity: u32,
    pub header: MessageHeader,
    pub flags: Flags,
    pub thread_id: ThreadId,
    pub snippet: String,
    pub has_attachments: bool,
}

/// One row in the conversation list: the newest message in a thread plus
/// aggregate counts for older copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub thread_id: ThreadId,
    pub account_id: AccountId,
    pub subject: String,
    pub sender: String,
    pub snippet: String,
    pub date: i64,
    pub message_count: u32,
    pub unread: u32,
    pub has_attachments: bool,
}
