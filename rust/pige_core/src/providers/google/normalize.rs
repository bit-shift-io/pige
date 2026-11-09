//! Normalize Gmail wire values into Pige's provider-neutral domain types.
//!
//! Gmail is label-based: one message has one id and a set of label ids. This
//! module keeps that identity flat (no fake per-label copies and no invented
//! UIDs), maps system labels to [`FolderKind`], represents All Mail as a
//! provider-owned virtual collection, and decodes MIME payloads into the same
//! [`BodyContent`] shape every other adapter produces.

use std::collections::BTreeSet;

use base64::Engine;

use crate::mail::{
    AttachmentDisposition, Flags, FolderKind, MAX_ATTACHMENT_BYTES, MessageHeader,
    parse_header_block,
};
use crate::providers::{
    AttachmentDescriptor, BodyContent, CollectionRecord, MessageRecord, ProviderError,
    RemoteAttachmentId, RemoteCollectionId, RemoteMessageId, RemoteThreadId, SignatureStatus,
};

use super::client::{Label, Message, Payload};

/// Provider-owned virtual collection for "All Mail". This is deliberately not
/// Gmail's `ALL` label id: the adapter owns the collection and decides which
/// messages belong to it (accessible, non-Spam, non-Trash mail).
pub(crate) const ALL_MAIL_COLLECTION: &str = "pige:all-mail";

pub(crate) const INBOX_LABEL: &str = "INBOX";
pub(crate) const SENT_LABEL: &str = "SENT";
pub(crate) const DRAFTS_LABEL: &str = "DRAFTS";
pub(crate) const TRASH_LABEL: &str = "TRASH";
pub(crate) const SPAM_LABEL: &str = "SPAM";
pub(crate) const STARRED_LABEL: &str = "STARRED";
pub(crate) const IMPORTANT_LABEL: &str = "IMPORTANT";
pub(crate) const UNREAD_LABEL: &str = "UNREAD";

/// The metadata header field mask requested for every list/history record.
pub(crate) const METADATA_HEADERS: &[&str] = &[
    "From",
    "Reply-To",
    "To",
    "Cc",
    "Bcc",
    "Message-ID",
    "In-Reply-To",
    "References",
    "Date",
    "Subject",
];

/// System collections swept during a bounded bootstrap or reconcile.
pub(crate) const SYNCABLE_SYSTEM_LABELS: &[&str] = &[
    INBOX_LABEL,
    SENT_LABEL,
    DRAFTS_LABEL,
    TRASH_LABEL,
    SPAM_LABEL,
];

/// Labels that are message state rather than a user-visible collection.
fn is_state_label(id: &str) -> bool {
    matches!(
        id,
        UNREAD_LABEL | STARRED_LABEL | IMPORTANT_LABEL | "ALL" | "CHATS"
    ) || id.starts_with("CATEGORY_")
}

pub(crate) fn folder_kind(id: &str) -> FolderKind {
    match id {
        INBOX_LABEL => FolderKind::Inbox,
        SENT_LABEL => FolderKind::Sent,
        DRAFTS_LABEL => FolderKind::Drafts,
        TRASH_LABEL => FolderKind::Trash,
        SPAM_LABEL => FolderKind::Junk,
        _ => FolderKind::Other,
    }
}

/// Map one label to a collection, or `None` when it is not a mailbox-like
/// collection (state labels) or is Gmail's real All Mail label, which is
/// replaced by [`ALL_MAIL_COLLECTION`].
pub(crate) fn collection_record(label: &Label) -> Option<CollectionRecord> {
    if label.id == "ALL" || label.id == "CHATS" || label.id == "UNREAD" {
        return None;
    }
    Some(CollectionRecord {
        remote_id: RemoteCollectionId::new(label.id.clone()),
        name: label.name.clone(),
        kind: folder_kind(&label.id),
        // Gmail labels have no hierarchy delimiter; a user label name can
        // contain `/`, but it is a name, not a path.
        delimiter: None,
        total: label.messages_total,
        unread: label.messages_unread,
    })
}

pub(crate) fn all_mail_collection() -> CollectionRecord {
    CollectionRecord {
        remote_id: RemoteCollectionId::new(ALL_MAIL_COLLECTION),
        name: "All Mail".to_string(),
        kind: FolderKind::Archive,
        delimiter: None,
        total: None,
        unread: None,
    }
}

/// The present syncable system collections in stable engine order.
pub(crate) fn syncable_collections(labels: &[Label]) -> Vec<RemoteCollectionId> {
    let present: BTreeSet<&str> = labels.iter().map(|label| label.id.as_str()).collect();
    SYNCABLE_SYSTEM_LABELS
        .iter()
        .filter(|label| present.contains(**label))
        .map(|label| RemoteCollectionId::new(*label))
        .collect()
}

/// Whether a label id is a mailbox-like collection that history membership
/// events should surface. Read/star/important state is a flag update, not a
/// collection membership.
pub(crate) fn is_membership_label(id: &str) -> bool {
    !is_state_label(id)
}

pub(crate) fn is_all_mail_member(label_ids: &[String]) -> bool {
    !label_ids
        .iter()
        .any(|label| label == SPAM_LABEL || label == TRASH_LABEL)
}

/// Build a full message record from one metadata response.
pub(crate) fn message_record(message: &Message) -> MessageRecord {
    let mut collections: Vec<RemoteCollectionId> = message
        .label_ids
        .iter()
        .filter(|label| !is_state_label(label))
        .map(|label| RemoteCollectionId::new(label.clone()))
        .collect();
    if is_all_mail_member(&message.label_ids) {
        collections.push(RemoteCollectionId::new(ALL_MAIL_COLLECTION));
    }
    collections.sort();
    collections.dedup();

    MessageRecord {
        remote_id: RemoteMessageId::new(message.id.clone()),
        remote_thread: message.thread_id.clone().map(RemoteThreadId::new),
        header: header(message),
        flags: flags(&message.label_ids),
        collections,
        snippet: message.snippet.clone(),
        // Metadata responses do not carry MIME parts, so attachment presence
        // is only known after a full body fetch. It is never invented.
        has_attachments: false,
    }
}

fn header(message: &Message) -> MessageHeader {
    let block = message
        .payload
        .as_ref()
        .map(|payload| header_block(&payload.headers))
        .unwrap_or_default();
    let mut header = parse_header_block(block.as_bytes()).unwrap_or_default();

    // Gmail's internalDate is the authoritative arrival timestamp; the Date
    // header is user-supplied. sizeEstimate is the server's size (upper bound),
    // not a client guess from the truncated metadata payload.
    if let Some(date) = message
        .internal_date
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
    {
        header.date = date.div_euclid(1000);
    }
    header.size = message.size_estimate.unwrap_or(0).min(u64::from(u32::MAX)) as u32;
    header
}

fn header_block(headers: &[super::client::Header]) -> String {
    let mut block = String::new();
    for header in headers {
        let name = header.name.trim();
        if name.is_empty() {
            continue;
        }
        block.push_str(name);
        block.push_str(": ");
        block.push_str(header.value.trim());
        block.push_str("\r\n");
    }
    block.push_str("\r\n");
    block
}

fn flags(label_ids: &[String]) -> Flags {
    let has = |label: &str| label_ids.iter().any(|value| value == label);
    Flags {
        seen: !has(UNREAD_LABEL),
        answered: false,
        flagged: has(STARRED_LABEL),
        deleted: false,
        draft: has(DRAFTS_LABEL),
    }
}

/// Normalize a full Gmail message into a displayable body.
pub(crate) fn body_content(message: &Message) -> BodyContent {
    let mut body = BodyContent {
        plain_text: None,
        html: None,
        raw: None,
        attachments: Vec::new(),
        // Gmail's API does not expose signature trust.
        signature: SignatureStatus::NotApplicable,
    };
    if let Some(payload) = &message.payload {
        walk_payload(payload, &mut body);
    }
    body
}

fn walk_payload(payload: &Payload, body: &mut BodyContent) {
    let mime = payload.mime_type.as_deref().unwrap_or_default();
    let data = payload
        .body
        .as_ref()
        .and_then(|part| part.data.as_deref())
        .and_then(decode_base64url);
    let attachment = is_attachment(payload);

    if !attachment && mime.eq_ignore_ascii_case("text/plain") {
        if body.plain_text.is_none() {
            body.plain_text = data.clone().and_then(|bytes| String::from_utf8(bytes).ok());
        }
    } else if !attachment && mime.eq_ignore_ascii_case("text/html") && body.html.is_none() {
        body.html = data.clone().and_then(|bytes| String::from_utf8(bytes).ok());
    }

    if attachment {
        let part_id = payload.part_id.clone().unwrap_or_default();
        let remote_id = payload
            .body
            .as_ref()
            .and_then(|part| part.attachment_id.clone())
            .unwrap_or_else(|| part_id.clone());
        let (filename, content_type, size, content_id, disposition) = descriptor_fields(payload);
        body.attachments.push(AttachmentDescriptor {
            remote_id: RemoteAttachmentId::new(remote_id),
            part_id,
            filename,
            content_type,
            size,
            content_id,
            disposition,
        });
    }

    for part in &payload.parts {
        walk_payload(part, body);
    }
}

fn is_attachment(payload: &Payload) -> bool {
    payload
        .body
        .as_ref()
        .is_some_and(|part| part.attachment_id.is_some())
        || payload
            .filename
            .as_deref()
            .is_some_and(|name| !name.is_empty())
}

fn descriptor_fields(
    payload: &Payload,
) -> (String, String, u64, Option<String>, AttachmentDisposition) {
    let filename = payload
        .filename
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "attachment".to_string());
    let content_type = payload
        .mime_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let size = payload
        .body
        .as_ref()
        .and_then(|part| part.size)
        .unwrap_or(0);
    let content_id = payload.headers.iter().find_map(|header| {
        header
            .name
            .eq_ignore_ascii_case("content-id")
            .then(|| header.value.trim().trim_matches(['<', '>']).to_string())
            .filter(|value| !value.is_empty())
    });
    let disposition = payload
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-disposition"))
        .map(|header| header.value.to_ascii_lowercase())
        .filter(|value| value.starts_with("inline"))
        .map_or(AttachmentDisposition::Attachment, |_| {
            AttachmentDisposition::Inline
        });
    (filename, content_type, size, content_id, disposition)
}

/// Decode an inline part by its Gmail part id when the attachment endpoint did
/// not name a separate `attachmentId`.
pub(crate) fn attachment_data(message: &Message, part_id: &str) -> Option<Vec<u8>> {
    fn walk(payload: &Payload, part_id: &str) -> Option<Vec<u8>> {
        if payload.part_id.as_deref() == Some(part_id) {
            return payload
                .body
                .as_ref()
                .and_then(|part| part.data.as_deref())
                .and_then(decode_base64url);
        }
        payload.parts.iter().find_map(|part| walk(part, part_id))
    }
    message
        .payload
        .as_ref()
        .and_then(|root| walk(root, part_id))
}

pub(crate) fn decode_attachment(
    response: &super::client::AttachmentResponse,
) -> Result<Vec<u8>, ProviderError> {
    let bytes = response
        .data
        .as_deref()
        .and_then(decode_base64url)
        .ok_or_else(|| ProviderError::not_found("the attachment data was not present"))?;
    let reported = response.size.unwrap_or(0);
    if reported > MAX_ATTACHMENT_BYTES || bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(ProviderError::unsupported(
            "the attachment is larger than the client accepts",
        ));
    }
    Ok(bytes)
}

pub(crate) fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    const URL_SAFE_NO_PAD: base64::engine::GeneralPurpose =
        base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value))
        .ok()
}
