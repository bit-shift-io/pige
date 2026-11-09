//! Normalize Proton wire values into Pige's provider-neutral domain types.
//!
//! Proton is label-based like Gmail: one account-wide opaque message id can
//! belong to several folders/labels at once. This module keeps that identity
//! flat, maps system label ids to [`FolderKind`], and builds a complete
//! [`MessageRecord`] from listing/event metadata only. It never fetches or
//! decrypts a body just to learn correspondents.

use std::collections::BTreeSet;

use crate::mail::{Address, Flags, FolderKind, MessageHeader};
use crate::providers::{
    CollectionRecord, MessageRecord, RemoteCollectionId, RemoteMessageId, RemoteThreadId,
};

use proton_core::model::enums::{label_ids, message_flag};
use proton_core::model::label::Label;
use proton_core::model::message::{MessageMetadata, Recipient};

/// Inbox system folder.
pub(crate) const INBOX: &str = label_ids::INBOX;
/// Trash system folder.
pub(crate) const TRASH: &str = label_ids::TRASH;
/// Spam system folder.
pub(crate) const SPAM: &str = label_ids::SPAM;
/// Archive system folder.
pub(crate) const ARCHIVE: &str = label_ids::ARCHIVE;
/// Sent system folder.
pub(crate) const SENT: &str = label_ids::SENT;
/// Drafts system folder.
pub(crate) const DRAFTS: &str = label_ids::DRAFTS;
/// Starred marker. This is message state, not a mailbox-like collection.
pub(crate) const STARRED: &str = label_ids::STARRED;
/// All Mail aggregate view.
pub(crate) const ALL_MAIL: &str = label_ids::ALL_MAIL;
/// All Drafts aggregate view.
pub(crate) const ALL_DRAFTS: &str = label_ids::ALL_DRAFTS;
/// All Sent aggregate view.
pub(crate) const ALL_SENT: &str = label_ids::ALL_SENT;
/// Outbox (queued submissions), not a user collection.
const OUTBOX: &str = label_ids::OUTBOX;
/// Scheduled-send state.
const SCHEDULED: &str = label_ids::SCHEDULED;
/// Snoozed state.
const SNOOZED: &str = label_ids::SNOOZED;

/// Proton system folders that a bounded bootstrap/sweep enumerates. Aggregate
/// views (All Mail / All Drafts / All Sent) are intentionally excluded: listing
/// them would duplicate every message. User folders and labels are appended.
const SYSTEM_SWEEP_ORDER: &[&str] = &[INBOX, SENT, DRAFTS, TRASH, SPAM, ARCHIVE];

/// Labels that describe message state rather than a mailbox membership.
pub(crate) fn is_state_label(id: &str) -> bool {
    matches!(id, STARRED | OUTBOX | SCHEDULED | SNOOZED)
}

/// Aggregate views expose the same messages under a broader label. They remain
/// visible collections but are not swept separately.
fn is_aggregate_collection(id: &str) -> bool {
    matches!(id, ALL_MAIL | ALL_DRAFTS | ALL_SENT)
}

pub(crate) fn folder_kind(id: &str) -> FolderKind {
    match id {
        INBOX => FolderKind::Inbox,
        SENT | ALL_SENT => FolderKind::Sent,
        DRAFTS | ALL_DRAFTS => FolderKind::Drafts,
        TRASH => FolderKind::Trash,
        SPAM => FolderKind::Junk,
        ARCHIVE | ALL_MAIL => FolderKind::Archive,
        _ => FolderKind::Other,
    }
}

/// Build one collection from a Proton label, with per-label counts when the
/// server supplied them.
pub(crate) fn collection_record(
    label: &Label,
    counts: Option<(i64, i64)>,
    delimiter: Option<String>,
) -> Option<CollectionRecord> {
    if is_state_label(&label.id) {
        return None;
    }
    let (total, unread) = match counts {
        Some((total, unread)) => (
            Some(u32::try_from(total.max(0)).unwrap_or(u32::MAX)),
            Some(u32::try_from(unread.max(0)).unwrap_or(u32::MAX)),
        ),
        None => (None, None),
    };
    Some(CollectionRecord {
        remote_id: RemoteCollectionId::new(label.id.clone()),
        name: if label.name.trim().is_empty() {
            label.path.clone()
        } else {
            label.name.clone()
        },
        kind: folder_kind(&label.id),
        delimiter,
        total,
        unread,
    })
}

/// The collections a bounded bootstrap/sweep enumerates, in stable order:
/// system folders first, then user folders/labels ordered by remote id.
pub(crate) fn syncable_collections(labels: &[Label]) -> Vec<RemoteCollectionId> {
    let present: BTreeSet<&str> = labels
        .iter()
        .filter(|label| !is_state_label(&label.id) && !is_aggregate_collection(&label.id))
        .map(|label| label.id.as_str())
        .collect();
    let mut collections: Vec<RemoteCollectionId> = SYSTEM_SWEEP_ORDER
        .iter()
        .filter(|id| present.contains(**id))
        .map(|id| RemoteCollectionId::new(*id))
        .collect();
    collections.extend(
        present
            .iter()
            .filter(|id| !SYSTEM_SWEEP_ORDER.contains(id))
            .map(|id| RemoteCollectionId::new(*id)),
    );
    collections
}

/// Build a complete message record from listing/event metadata.
///
/// Proton's label ids are account-wide, not folder-local copies, so every
/// folder and label listing yields the same content and membership.
pub(crate) fn message_record(meta: &MessageMetadata) -> MessageRecord {
    MessageRecord {
        remote_id: RemoteMessageId::new(meta.id.clone()),
        remote_thread: (!meta.conversation_id.is_empty())
            .then(|| RemoteThreadId::new(meta.conversation_id.clone())),
        header: header(meta),
        flags: flags(meta),
        collections: collections(&meta.label_ids),
        // Proton listing metadata carries no snippet; never invent one.
        snippet: String::new(),
        has_attachments: meta.num_attachments > 0,
    }
}

fn collections(label_ids: &[String]) -> Vec<RemoteCollectionId> {
    let mut collections: Vec<RemoteCollectionId> = label_ids
        .iter()
        .filter(|label| !is_state_label(label))
        .map(|label| RemoteCollectionId::new(label.clone()))
        .collect();
    collections.sort();
    collections.dedup();
    collections
}

fn header(meta: &MessageMetadata) -> MessageHeader {
    MessageHeader {
        message_id: meta.external_id.clone().filter(|value| !value.is_empty()),
        // Proton's listing metadata does not expose In-Reply-To/References; a
        // follow-up full-header fetch supplies them when a thread is opened.
        in_reply_to: None,
        references: Vec::new(),
        subject: meta.subject.clone(),
        from: addresses(std::slice::from_ref(&meta.sender)),
        to: addresses(&meta.to_list),
        cc: addresses(&meta.cc_list),
        // Bcc is only exposed on a Sent copy; never invented for received mail.
        bcc: addresses(&meta.bcc_list),
        reply_to: Vec::new(),
        date: meta.time,
        size: u32::try_from(meta.size).unwrap_or(u32::MAX),
    }
}

fn addresses(recipients: &[Recipient]) -> Vec<Address> {
    recipients
        .iter()
        .filter(|recipient| !recipient.address.trim().is_empty())
        .map(|recipient| {
            let name = recipient.name.trim();
            Address::new(
                (!name.is_empty() && name != recipient.address).then(|| name.to_string()),
                recipient.address.clone(),
            )
        })
        .collect()
}

fn flags(meta: &MessageMetadata) -> Flags {
    let has_label = |label: &str| meta.label_ids.iter().any(|value| value == label);
    Flags {
        seen: meta.unread == 0,
        answered: message_flag::has(meta.flags, message_flag::REPLIED)
            || message_flag::has(meta.flags, message_flag::REPLIED_ALL),
        flagged: has_label(STARRED),
        deleted: has_label(TRASH),
        draft: has_label(DRAFTS) || has_label(ALL_DRAFTS),
    }
}
