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

use crate::providers::{
    BodyContent, CollectionRecord, HistoryCoverage, HistoryPage, HistoryRequest, MailboxService,
    MessageRecord, OperationContext, ProviderCursor, ProviderError, RemoteAttachmentId,
    RemoteMessageId,
};

use proton_core::api;
use proton_core::model::enums::label_type;
use proton_core::model::label::Label;
use proton_core::model::message::MessageMetadata;

use super::client::{ProtonClient, ProtonHttp};
use super::normalize;

const HISTORY_CURSOR_VERSION: u32 = 1;
const HISTORY_MIN_PAGE: u32 = 1;
/// Proton's listing accepts at most 150 messages per page.
pub(crate) const PROTON_MAX_PAGE: u32 = 150;

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
        let counts = fetch_counts(&http).await;
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
        _id: &RemoteMessageId,
        _ctx: &OperationContext,
    ) -> Result<BodyContent, ProviderError> {
        // P3 owns body retrieval and OpenPGP verification. Until then this is a
        // fail-closed gap, never ciphertext presented as display text.
        Err(ProviderError::unsupported(
            "reading a native Proton message body in this build",
        ))
    }

    async fn attachment(
        &mut self,
        _message: &RemoteMessageId,
        _id: &RemoteAttachmentId,
        _ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::unsupported(
            "downloading a native Proton attachment in this build",
        ))
    }

    async fn history(
        &mut self,
        request: HistoryRequest,
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        ctx.ensure_active()?;
        let page_size = request.page_size.clamp(HISTORY_MIN_PAGE, PROTON_MAX_PAGE);
        let mut state = match &request.cursor {
            Some(cursor) => decode_history_cursor(cursor)?,
            None => HistoryCursor {
                v: HISTORY_CURSOR_VERSION,
                page: 0,
                scanned: 0,
                total_hint: None,
                anchor: String::new(),
                tail: Vec::new(),
                complete: false,
                churned: false,
            },
        };
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
            state.total_hint = None;
            state.tail.clear();
            state.churned = false;
            state.anchor = latest_before.clone();
        }

        let churn = latest_before != state.anchor;
        let page = state.page;
        // Offset listing can shift an unseen record backwards into the page we
        // just consumed. Re-reading that page (plus the current one) and
        // deduplicating against the previous page's ids closes that gap.
        let first_page = if churn && page > 0 { page - 1 } else { page };
        let mut total = state.total_hint.unwrap_or(0);
        let mut ids: HashSet<String> = HashSet::new();
        let mut messages: Vec<MessageRecord> = Vec::new();
        let mut last_page_ids: Vec<String> = Vec::new();

        for number in first_page..=page {
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
        state.scanned += messages.len() as u64;
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
            let latest_after = latest_event(&http).await?;
            if latest_after != state.anchor || state.churned {
                // The listing moved under the pass (or may have). Re-run from
                // the first page with the new anchor; durable learning is
                // keyed by native id, so the replay is safe and deduplicated.
                state.page = 0;
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

/// Per-label total/unread counts. Decorative metadata: a failed count refresh
/// never fails a collection listing that already succeeded.
pub(crate) async fn fetch_counts(http: &ProtonHttp) -> HashMap<String, (i64, i64)> {
    match http.finish(api::messages::counts(http).await) {
        Ok(counts) => counts
            .into_iter()
            .map(|count| (count.label_id, (count.total, count.unread)))
            .collect(),
        Err(_) => HashMap::new(),
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
    scanned: u64,
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
