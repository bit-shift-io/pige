//! Contact and correspondent-observation value types.
//!
//! These types are the cross-task vocabulary for recipient autocomplete: a
//! normalized [`AddressKey`] is the durable identity of a correspondent, while
//! an [`Observation`] is one role in which that address appeared in one
//! message. Observations are stored as evidence, never as a mutable counter, so
//! replaying a page cannot multiply a suggestion's frequency.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::mail::Address;

/// Canonical identity of an email address.
///
/// The local part is preserved byte-for-byte (case, dots, and `+` tags are
/// meaningful routing decisions), while the domain is lowercased. Two
/// addresses that differ only in domain case therefore map to the same
/// correspondent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AddressKey(pub String);

impl AddressKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AddressKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for AddressKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Why an address could not be normalized.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("address is empty")]
    Empty,
    #[error("address {0:?} is missing an @ separator")]
    MissingAt(String),
    #[error("address {0:?} has more than one @ separator")]
    MultipleAt(String),
    #[error("address {0:?} has an empty local part")]
    EmptyLocal(String),
    #[error("address {0:?} has an empty domain")]
    EmptyDomain(String),
    #[error("address {0:?} contains whitespace or control characters")]
    Whitespace(String),
    #[error("address {0:?} has a malformed domain")]
    InvalidDomain(String),
}

/// Stable identity of one piece of correspondence evidence.
///
/// It is derived from a native message/operation identity, never from a
/// Message-ID alone. The store keeps it in `correspondence_evidence`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceId(pub String);

impl EvidenceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Evidence for a provider-native message id (IMAP UID epoch, Gmail id...).
    pub fn for_message(remote: &crate::providers::RemoteMessageId) -> Self {
        Self(format!("message:{}", remote.as_str()))
    }

    /// Evidence for a durable local message that has no remote id (the
    /// ExistingCache backfill path).
    pub fn for_local(message: &crate::mail::MessageId) -> Self {
        Self(format!("local:{}", message.0))
    }

    /// Evidence for one accepted submission, keyed by its idempotent operation.
    pub fn for_operation(operation: &crate::providers::OperationId) -> Self {
        Self(format!("operation:{}", operation.0))
    }
}

impl fmt::Display for EvidenceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for EvidenceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// The header in which an address appeared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HeaderRole {
    From,
    ReplyTo,
    To,
    Cc,
    Bcc,
}

impl HeaderRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::From => "from",
            Self::ReplyTo => "reply_to",
            Self::To => "to",
            Self::Cc => "cc",
            Self::Bcc => "bcc",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "from" => Some(Self::From),
            "reply_to" => Some(Self::ReplyTo),
            "to" => Some(Self::To),
            "cc" => Some(Self::Cc),
            "bcc" => Some(Self::Bcc),
            _ => None,
        }
    }
}

/// How much the source of an observation can be trusted for suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ObservationTrust {
    /// Ordinary received mail or a cached header.
    Ordinary,
    /// Mail in Junk or Trash is recorded but must not outrank real contacts.
    SpamOrTrash,
    /// A successful send or an explicit user selection.
    Deliberate,
}

impl ObservationTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::SpamOrTrash => "spam_or_trash",
            Self::Deliberate => "deliberate",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "ordinary" => Some(Self::Ordinary),
            "spam_or_trash" => Some(Self::SpamOrTrash),
            "deliberate" => Some(Self::Deliberate),
            _ => None,
        }
    }

    /// Larger is stronger. Used to preserve trusted display names.
    pub(crate) fn rank(self) -> u8 {
        match self {
            Self::Ordinary => 1,
            Self::SpamOrTrash => 0,
            Self::Deliberate => 2,
        }
    }
}

/// Where an observation came from. The origin is provenance, not identity: a
/// message can be seen first in history and later as live mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LearningOrigin {
    /// A restart-safe backfill of already-cached headers.
    ExistingCache,
    /// The provider paged older headers through the history API.
    History,
    /// An adapter-confirmed new arrival in the sync change page.
    LiveArrival,
    /// The provider accepted one of our outgoing submissions.
    AcceptedSend,
    /// The user explicitly picked this address in compose.
    Selection,
}

impl LearningOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExistingCache => "existing_cache",
            Self::History => "history",
            Self::LiveArrival => "live_arrival",
            Self::AcceptedSend => "accepted_send",
            Self::Selection => "selection",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "existing_cache" => Some(Self::ExistingCache),
            "history" => Some(Self::History),
            "live_arrival" => Some(Self::LiveArrival),
            "accepted_send" => Some(Self::AcceptedSend),
            "selection" => Some(Self::Selection),
            _ => None,
        }
    }
}

/// One address observed in one role in one message.
///
/// The store coalesces these into per-message role sets, so a repeated page or
/// a UID reset never increments a count twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub key: AddressKey,
    pub address: Address,
    pub role: HeaderRole,
    /// Unix seconds, from the message header.
    pub last_seen: i64,
    /// True for mail we sent (or a compose selection).
    pub outgoing: bool,
    pub trust: ObservationTrust,
}
