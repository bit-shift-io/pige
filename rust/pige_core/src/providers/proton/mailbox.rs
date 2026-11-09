//! Proton read-side mailbox access: collections and header-only history.
//!
//! Collections are Proton labels/folders keyed by their opaque remote id.
//! History crawls the account-wide listing newest first using header-only
//! metadata; it never fetches a full message and never decrypts a body. The
//! listing is offset-paged and therefore unstable under concurrent arrivals,
//! so the cursor carries an event anchor: when the anchor moves the adapter
//! re-fetches the previous page and deduplicates by opaque message id. A pass
//! is only marked complete after an end-of-listing anchor check, never after a
//! failed or partial page.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::mail::{MAX_ATTACHMENT_BYTES, safe_attachment_filename};
use crate::providers::{
    AttachmentDescriptor, BodyContent, CollectionRecord, ErrorKind, HistoryCoverage, HistoryPage,
    HistoryRequest, MailboxService, MessageRecord, OperationContext, ProviderCursor, ProviderError,
    RemoteAttachmentId, RemoteMessageId, SignatureStatus,
};

use proton_core::api;
use proton_core::crypto::keys::{AddressKeys, KeyStore};
use proton_core::crypto::Verdict;
use proton_core::model::enums::{label_type, message_flag};
use proton_core::model::label::Label;
use proton_core::model::message::{Attachment, Message, MessageMetadata};

use super::client::{ProtonClient, ProtonHttp};
use super::normalize;

const HISTORY_CURSOR_VERSION: u32 = 1;
const HISTORY_MIN_PAGE: u32 = 1;
/// Proton's listing accepts at most 150 messages per page.
pub(crate) const PROTON_MAX_PAGE: u32 = 150;

/// The existing reader body cap, applied to decrypted plaintext.
const MAX_BODY_BYTES: usize = crate::api::MAX_BODY_BYTES as usize;
/// Armored/base64 ciphertext expands over plaintext. This is a hard input bound,
/// not a substitute for the decrypted-output cap.
const MAX_ENCRYPTED_BODY_BYTES: usize = MAX_BODY_BYTES * 2;
/// Encrypted attachment data packets carry a small packet/signature overhead.
const MAX_ENCRYPTED_ATTACHMENT_BYTES: u64 = MAX_ATTACHMENT_BYTES + 1024 * 1024;

/// Proton mailbox reads.
pub struct ProtonMailbox {
    client: ProtonClient,
}

impl ProtonMailbox {
    pub fn new(client: ProtonClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl MailboxService for ProtonMailbox {
    async fn collections(
        &mut self,
        ctx: &OperationContext,
    ) -> Result<Vec<CollectionRecord>, ProviderError> {
        ctx.ensure_active()?;
        let http = self.client.request_http(ctx).await?;
        let labels = fetch_labels(&http).await?;
        let counts = fetch_counts(&http).await?;
        // Count failures are decorative, but a deadline/cancellation that
        // expired while fetching them must still fail the whole call: the
        // context owns whether this operation may return a result at all.
        ctx.ensure_active()?;
        let mut collections = collection_records(&labels, &counts);
        collections.sort_by(|left, right| {
            collection_order(&left.remote_id)
                .cmp(&collection_order(&right.remote_id))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(collections)
    }

    async fn body(
        &mut self,
        id: &RemoteMessageId,
        ctx: &OperationContext,
    ) -> Result<BodyContent, ProviderError> {
        ctx.ensure_active()?;
        let http = self.client.request_http(ctx).await?;
        let message = http.finish(api::messages::get_message(&http, id.as_str()).await)?;
        ctx.ensure_active()?;

        let attachments = message.attachments.iter().map(descriptor).collect();
        let kind = body_kind(&message)?;
        let (plain_text, html, signature) = match kind {
            BodyKind::Cleartext => {
                if message.body.len() > MAX_BODY_BYTES {
                    return Err(too_large("message body"));
                }
                let (plain, html) = split_body(&message.body, &message.mime_type);
                (plain, html, SignatureStatus::NotApplicable)
            }
            BodyKind::Encrypted => {
                if message.body.len() > MAX_ENCRYPTED_BODY_BYTES {
                    return Err(too_large("encrypted message body"));
                }
                let verify = sender_verification_keys(&http, &message.meta.sender.address).await;
                ctx.ensure_active()?;
                let (bytes, verdict) = decrypt_body(&self.client, &message, &verify)?;
                let (plain, html) = split_body(&String::from_utf8_lossy(&bytes), &message.mime_type);
                (plain, html, signature_status(verdict))
            }
        };
        ctx.ensure_active()?;
        Ok(BodyContent {
            plain_text,
            html,
            // Proton does not expose the original RFC822 message; never
            // fabricate one from decrypted MIME parts.
            raw: None,
            attachments,
            signature,
        })
    }

    async fn attachment(
        &mut self,
        message: &RemoteMessageId,
        id: &RemoteAttachmentId,
        ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError> {
        ctx.ensure_active()?;
        let http = self.client.request_http(ctx).await?;
        let message = http.finish(api::messages::get_message(&http, message.as_str()).await)?;
        let attachment = message
            .attachments
            .iter()
            .find(|attachment| attachment.id == id.as_str())
            .ok_or_else(|| ProviderError::not_found("the attachment is not in this message"))?;
        if attachment.size > MAX_ATTACHMENT_BYTES {
            return Err(too_large("attachment"));
        }
        let key_packets = attachment.key_packets.as_deref().ok_or_else(|| {
            ProviderError::unsupported("an unencrypted native Proton attachment")
        })?;
        let data = http.finish(api::attachments::get_attachment(&http, id.as_str()).await)?;
        if data.len() as u64 > MAX_ENCRYPTED_ATTACHMENT_BYTES {
            return Err(too_large("encrypted attachment"));
        }
        ctx.ensure_active()?;
        let plain = decrypt_attachment(&self.client, &message, key_packets, &data)?;
        if plain.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(too_large("attachment"));
        }
        Ok(plain)
    }

    async fn history(
        &mut self,
        request: HistoryRequest,
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        ctx.ensure_active()?;
        let requested_page_size = request.page_size.clamp(HISTORY_MIN_PAGE, PROTON_MAX_PAGE);
        let mut state = match &request.cursor {
            Some(cursor) => decode_history_cursor(cursor)?,
            None => HistoryCursor {
                v: HISTORY_CURSOR_VERSION,
                page: 0,
                scanned: 0,
                pass_scanned: 0,
                total_hint: None,
                anchor: String::new(),
                tail: Vec::new(),
                complete: false,
                churned: false,
                page_size: requested_page_size,
            },
        };
        // Offset pages are page-size-dependent: changing the size on resume
        // shifts every boundary with no event-anchor signal. Pin the size in
        // the cursor; a cursor written by an older build (0) adopts this
        // request's size once.
        if state.page_size == 0 {
            state.page_size = requested_page_size;
        }
        let page_size = state.page_size.clamp(HISTORY_MIN_PAGE, PROTON_MAX_PAGE);
        let http = self.client.request_http(ctx).await?;

        if state.anchor.is_empty() {
            state.anchor = latest_event(&http).await?;
        }
        let latest_before = latest_event(&http).await?;

        // A completed crawl resumes only while the account is unchanged. New
        // mail above the completed range starts a fresh (deduplicated) pass so
        // a completion marker can never hide an arrival.
        if state.complete {
            if latest_before == state.anchor {
                return Ok(HistoryPage {
                    messages: Vec::new(),
                    checkpoint: encode_history_cursor(&state)?,
                    has_more: false,
                    coverage: HistoryCoverage {
                        scanned: state.scanned,
                        total_hint: state.total_hint,
                    },
                });
            }
            state.complete = false;
            state.page = 0;
            state.scanned = 0;
            state.pass_scanned = 0;
            state.total_hint = None;
            state.tail.clear();
            state.churned = false;
            state.anchor = latest_before.clone();
        }

        let churn = latest_before != state.anchor;
        let page = state.page;
        // Offset listing can shift an unseen record backwards into the page we
        // just consumed. Re-reading that page (plus the current one) and
        // deduplicating against the previous page's ids closes that gap. The
        // same overlap runs while a churned pass has not yet gone clean.
        let first_page = if (churn || state.churned) && page > 0 {
            page - 1
        } else {
            page
        };
        let mut total = state.total_hint.unwrap_or(0);
        let mut ids: HashSet<String> = HashSet::new();
        let mut messages: Vec<MessageRecord> = Vec::new();
        let mut last_page_ids: Vec<String> = Vec::new();

        for number in first_page..=page {
            // A fenced/cancelled context must not fetch or emit another page.
            ctx.ensure_active()?;
            let (reported_total, page_messages) = list_page(&http, number, page_size).await?;
            total = total.max(reported_total as u64);
            let is_current = number == page;
            if is_current {
                last_page_ids = page_messages
                    .iter()
                    .map(|message| message.id.clone())
                    .collect();
            }
            for message in page_messages {
                if !ids.insert(message.id.clone()) {
                    continue;
                }
                // Items from the already-consumed page are returned again only
                // when churn shifted the listing; the previous page's ids stay
                // in `tail` so the ordinary overlap is not duplicated.
                if state.tail.iter().any(|id| id == &message.id) {
                    continue;
                }
                messages.push(normalize::message_record(&message));
            }
        }
        // `scanned` counts the current pass, never the sum of restart attempts:
        // a churn restart begins a fresh pass, and the reported value is the
        // maximum any pass reached so it cannot move backwards.
        state.pass_scanned = state.pass_scanned.saturating_add(messages.len() as u64);
        state.scanned = state.scanned.max(state.pass_scanned);
        state.total_hint = Some(total);
        state.page = page.saturating_add(1);
        state.tail = last_page_ids;
        if churn {
            state.anchor = latest_before;
            // The overlap closed the ordinary one-page shift, but a larger
            // concurrent change could still have moved an unseen record past
            // it. Require one clean, churn-free pass before claiming complete.
            state.churned = true;
        }

        let at_end = state.page as u64 * u64::from(page_size) >= total;
        let mut has_more = !at_end;
        if at_end {
            // Re-read the anchor *after* the listing. A deletion between the
            // pre-listing read and the final page can shift the boundary
            // record into the page already consumed, so completion is never
            // based only on the anchor sampled before the page.
            ctx.ensure_active()?;
            let latest_after = latest_event(&http).await?;
            if latest_after != state.anchor || state.churned {
                // The listing moved under the pass (or may have). Re-run from
                // the first page with the new anchor; durable learning is
                // keyed by native id, so the replay is safe and deduplicated.
                // A fresh pass starts a fresh scanned counter; the reported
                // coverage stays the maximum already reached.
                state.page = 0;
                state.pass_scanned = 0;
                state.total_hint = None;
                state.tail.clear();
                state.anchor = latest_after;
                state.churned = false;
                state.complete = false;
                has_more = true;
            } else {
                state.complete = true;
                has_more = false;
            }
        }

        ctx.ensure_active()?;
        Ok(HistoryPage {
            messages,
            checkpoint: encode_history_cursor(&state)?,
            has_more,
            coverage: HistoryCoverage {
                scanned: state.scanned,
                total_hint: state.total_hint,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Body / attachment crypto
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Cleartext,
    Encrypted,
}

/// Classify the wire body before touching crypto. A message that claims to be
/// encrypted but uses a format this adapter cannot decrypt fails closed; it is
/// never rendered as cleartext.
fn body_kind(message: &Message) -> Result<BodyKind, ProviderError> {
    let trimmed = message.body.trim_start();
    if trimmed.starts_with("-----BEGIN PGP MESSAGE-----") {
        return Ok(BodyKind::Encrypted);
    }
    if trimmed.starts_with("-----BEGIN PGP ") {
        return Err(ProviderError::unsupported(
            "this native Proton OpenPGP message format",
        ));
    }
    let mime = message.mime_type.trim().to_ascii_lowercase();
    let encrypted_form = mime.contains("encrypted")
        || mime.contains("application/pgp")
        || message.password.is_some()
        || message_flag::has(message.meta.flags, message_flag::E2E);
    if encrypted_form {
        return Err(ProviderError::unsupported(
            "this native Proton encrypted message format",
        ));
    }
    Ok(BodyKind::Cleartext)
}

/// Verified Proton sender keys only. External/WKD keys stay out of the
/// verifier set, so an external signature is `Unverified`, never `Verified`.
async fn sender_verification_keys(http: &ProtonHttp, email: &str) -> Vec<String> {
    if email.trim().is_empty() {
        return Vec::new();
    }
    match http
        .finish(api::keys::get_all_public_keys(http, email).await)
    {
        Ok(keys) => keys
            .keys
            .into_iter()
            .map(|key| key.public_key)
            .filter(|key| !key.is_empty())
            .collect(),
        // A key lookup failure must not fail the body; it degrades the verdict
        // to Unverified rather than pretending the signature is valid.
        Err(_) => Vec::new(),
    }
}

fn candidate_addresses<'a>(keys: &'a KeyStore, address_id: &str) -> Vec<&'a AddressKeys> {
    let mut candidates = Vec::new();
    if let Some(exact) = keys.address(address_id) {
        candidates.push(exact);
    }
    // Only when the exact address is absent or cannot decrypt do we try the
    // account's other address rings. This never silently substitutes the
    // primary address for a different alias's key.
    for address in &keys.addresses {
        if address.address_id != address_id {
            candidates.push(address);
        }
    }
    candidates
}

fn decrypt_body(
    client: &ProtonClient,
    message: &Message,
    verify: &[String],
) -> Result<(Vec<u8>, Verdict), ProviderError> {
    client.unlocked_keys().with_keys(|keys| {
        let candidates = candidate_addresses(keys, &message.meta.address_id);
        if candidates.is_empty() {
            return Err(ProviderError::reauthenticate(
                "unlock the Proton address keys to continue",
            ));
        }
        let provider = proton_core::crypto::provider();
        let mut last_error = None;
        for address in candidates {
            match proton_core::crypto::decrypt_body_bounded(
                &provider,
                address,
                verify,
                &message.body,
                MAX_BODY_BYTES,
            ) {
                Ok(result) => return Ok(result),
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        Err(
            ProviderError::new(ErrorKind::Unsupported, "decrypting this Proton message body")
                .with_source_detail(last_error.unwrap_or_default()),
        )
    })?
}

fn decrypt_attachment(
    client: &ProtonClient,
    message: &Message,
    key_packets: &str,
    data_packet: &[u8],
) -> Result<Vec<u8>, ProviderError> {
    client.unlocked_keys().with_keys(|keys| {
        let candidates = candidate_addresses(keys, &message.meta.address_id);
        if candidates.is_empty() {
            return Err(ProviderError::reauthenticate(
                "unlock the Proton address keys to continue",
            ));
        }
        let provider = proton_core::crypto::provider();
        let mut last_error = None;
        for address in candidates {
            match proton_core::crypto::decrypt_attachment_bounded(
                &provider,
                address,
                key_packets,
                data_packet,
                MAX_ATTACHMENT_BYTES as usize,
            ) {
                Ok(result) => return Ok(result),
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        Err(
            ProviderError::new(ErrorKind::Unsupported, "decrypting this Proton attachment")
                .with_source_detail(last_error.unwrap_or_default()),
        )
    })?
}

fn descriptor(attachment: &Attachment) -> AttachmentDescriptor {
    let content_type = attachment
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    let content_id = attachment
        .content_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let inline = attachment
        .disposition
        .as_deref()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("inline"));
    AttachmentDescriptor {
        remote_id: RemoteAttachmentId::new(attachment.id.clone()),
        part_id: attachment.id.clone(),
        filename: safe_attachment_filename(&attachment.name, "attachment"),
        content_type,
        size: attachment.size,
        content_id,
        disposition: if inline {
            crate::mail::AttachmentDisposition::Inline
        } else {
            crate::mail::AttachmentDisposition::Attachment
        },
    }
}

fn split_body(body: &str, mime_type: &str) -> (Option<String>, Option<String>) {
    let mime_type = mime_type.trim().to_ascii_lowercase();
    if mime_type.starts_with("text/html") {
        return (None, Some(body.to_string()));
    }
    if mime_type.starts_with("text/plain") || mime_type.is_empty() {
        return (Some(body.to_string()), None);
    }
    if mime_type.starts_with("multipart/")
        && let Some(parsed) = mail_parser::MessageParser::default().parse(body.as_bytes())
    {
        let html = parsed.body_html(0).map(|value| value.into_owned());
        let plain = parsed.body_text(0).map(|value| value.into_owned());
        if html.is_some() || plain.is_some() {
            return (plain, html);
        }
    }
    (Some(body.to_string()), None)
}

fn signature_status(verdict: Verdict) -> SignatureStatus {
    match verdict {
        Verdict::Verified => SignatureStatus::Verified,
        Verdict::Unsigned => SignatureStatus::Unsigned,
        Verdict::Unverified => SignatureStatus::Unverified,
        Verdict::Invalid => SignatureStatus::Invalid,
    }
}

fn too_large(what: &str) -> ProviderError {
    ProviderError::new(
        ErrorKind::Unsupported,
        format!("the {what} is larger than the client accepts"),
    )
}

// ---------------------------------------------------------------------------
// Shared wire helpers (also used by sync)
// ---------------------------------------------------------------------------

/// List every label/folder type. System folders arrive under `Type=4`; user
/// folders and labels under `Type=3`/`Type=1`. Duplicate ids are collapsed.
pub(crate) async fn fetch_labels(http: &ProtonHttp) -> Result<Vec<Label>, ProviderError> {
    let mut labels: Vec<Label> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for kind in [
        label_type::SYSTEM_FOLDER,
        label_type::MESSAGE_FOLDER,
        label_type::MESSAGE_LABEL,
    ] {
        let batch = http.finish(api::labels::list(http, kind).await)?;
        for label in batch {
            if !label.id.is_empty() && seen.insert(label.id.clone()) {
                labels.push(label);
            }
        }
    }
    Ok(labels)
}

/// Per-label total/unread counts. Decorative metadata: a transient failure
/// never fails a collection listing that already succeeded. Cancellation and
/// fatal authentication/authorization errors are still real failures and are
/// propagated so a fenced context cannot keep working.
pub(crate) async fn fetch_counts(
    http: &ProtonHttp,
) -> Result<HashMap<String, (i64, i64)>, ProviderError> {
    match http.finish(api::messages::counts(http).await) {
        Ok(counts) => Ok(counts
            .into_iter()
            .map(|count| (count.label_id, (count.total, count.unread)))
            .collect()),
        Err(error)
            if !matches!(
                error.kind,
                ErrorKind::Cancelled | ErrorKind::Reauthenticate | ErrorKind::Forbidden
            ) =>
        {
            Ok(HashMap::new())
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn collection_records(
    labels: &[Label],
    counts: &HashMap<String, (i64, i64)>,
) -> Vec<CollectionRecord> {
    labels
        .iter()
        .filter_map(|label| {
            let delimiter = match label.label_type {
                label_type::MESSAGE_FOLDER | label_type::SYSTEM_FOLDER => Some("/".to_string()),
                _ => None,
            };
            normalize::collection_record(label, counts.get(&label.id).copied(), delimiter)
        })
        .collect()
}

pub(crate) fn collection_changes(
    labels: &[Label],
    counts: &HashMap<String, (i64, i64)>,
) -> Vec<crate::providers::MailChange> {
    use crate::providers::MailChange;
    collection_records(labels, counts)
        .into_iter()
        .map(MailChange::UpsertCollection)
        .collect()
}

fn collection_order(id: &crate::providers::RemoteCollectionId) -> (u8, &str) {
    let rank = match id.as_str() {
        normalize::INBOX => 0,
        normalize::SENT => 1,
        normalize::DRAFTS => 2,
        normalize::TRASH => 3,
        normalize::SPAM => 4,
        normalize::ARCHIVE => 5,
        _ => 6,
    };
    (rank, id.as_str())
}

pub(crate) async fn latest_event(http: &ProtonHttp) -> Result<String, ProviderError> {
    http.finish(api::events::get_latest_event_id(http).await)
}

pub(crate) async fn list_page(
    http: &ProtonHttp,
    page: u32,
    page_size: u32,
) -> Result<(u32, Vec<MessageMetadata>), ProviderError> {
    let query = api::messages::ListQuery {
        page: Some(page),
        page_size: Some(page_size),
        ..api::messages::ListQuery::default()
    };
    http.finish(api::messages::list_messages(http, &query).await)
}

// ---------------------------------------------------------------------------
// History cursor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryCursor {
    v: u32,
    page: u32,
    /// Highest number of records any pass of this generation has reached.
    scanned: u64,
    /// Records emitted by the current pass; reset when a churned pass restarts.
    #[serde(default)]
    pass_scanned: u64,
    total_hint: Option<u64>,
    /// Event id captured at crawl start (or after a detected reset). A moved
    /// anchor means the offset listing may have shifted.
    anchor: String,
    /// Opaque ids returned by the most recent page, used to suppress ordinary
    /// overlap duplicates.
    tail: Vec<String>,
    complete: bool,
    /// The listing moved under a page in this pass. A clean pass then runs
    /// before coverage is reported complete.
    #[serde(default)]
    churned: bool,
    /// Pinned listing page size. Offset boundaries depend on it, so it must not
    /// change between pages of one crawl.
    #[serde(default)]
    page_size: u32,
}

fn encode_history_cursor(state: &HistoryCursor) -> Result<ProviderCursor, ProviderError> {
    let payload = serde_json::to_string(state).map_err(|error| {
        ProviderError::transient("could not encode the history cursor")
            .with_source_detail(error.to_string())
    })?;
    Ok(ProviderCursor::new(HISTORY_CURSOR_VERSION, payload))
}

fn decode_history_cursor(cursor: &ProviderCursor) -> Result<HistoryCursor, ProviderError> {
    if cursor.version != HISTORY_CURSOR_VERSION {
        return Err(ProviderError::cursor_invalid(
            "the history cursor was written by a different version",
        ));
    }
    let state: HistoryCursor = serde_json::from_str(&cursor.payload)
        .map_err(|_| ProviderError::cursor_invalid("the history cursor could not be read"))?;
    if state.v != HISTORY_CURSOR_VERSION {
        return Err(ProviderError::cursor_invalid(
            "the history cursor was written by a different version",
        ));
    }
    Ok(state)
}
