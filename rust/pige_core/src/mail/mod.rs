//! Mail domain types shared by the store, sync engine, and API. No iced or
//! platform-specific code belongs in this module.

pub mod address;
pub mod message;
pub mod parse;

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use address::Address;
pub use message::{Conversation, Flags, MessageHeader, StoredMessage};
pub use parse::{ParseError, decode_rfc2047, normalized_subject, parse_flags, parse_header_block};

/// Typed wrapper so folder, message, and thread ids cannot be mixed up.
macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    FolderId,
    "Stable local identifier for a folder within one account."
);
id_type!(MessageId, "Stable local identifier for one cached message.");
id_type!(
    ThreadId,
    "Conversation identifier shared by every message in a conversation."
);

/// IMAP UID, scoped to a folder and UIDVALIDITY epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Uid(pub u32);

impl From<u32> for Uid {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderKind {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    Junk,
    Other,
}

impl FolderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Archive => "archive",
            Self::Junk => "junk",
            Self::Other => "other",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "inbox" => Some(Self::Inbox),
            "sent" => Some(Self::Sent),
            "drafts" => Some(Self::Drafts),
            "trash" => Some(Self::Trash),
            "archive" => Some(Self::Archive),
            "junk" => Some(Self::Junk),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// Remote folder plus its cached sync state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub account_id: crate::account::AccountId,
    pub name: String,
    /// Hierarchy delimiter reported by the server, if any.
    pub delimiter: Option<String>,
    pub kind: FolderKind,
    pub unread: u32,
    pub total: u32,
    pub uidvalidity: Option<u32>,
    pub uidnext: Option<u32>,
}
