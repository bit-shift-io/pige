//! Gmail read-side mailbox access.
//!
//! Collections are Gmail labels plus the provider-owned All Mail virtual
//! collection. Bodies are decoded from the Gmail MIME payload; the raw RFC 5322
//! bytes are not requested, so `raw` stays `None` (the reader can reconstruct
//! display text from the normalized parts). The history crawl is separate from
//! mail sync: it lists the account's accessible messages (including Spam and
//! Trash) newest first, in header-only metadata pages.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::mail::check_attachment_size;
use crate::providers::{
    BodyContent, CollectionRecord, HistoryCoverage, HistoryPage, HistoryRequest, MailboxService,
    MessageRecord, OperationContext, ProviderCursor, ProviderError, RemoteAttachmentId,
    RemoteMessageId,
};

use super::client::GmailApi;
use super::normalize;

const HISTORY_CURSOR_VERSION: u32 = 1;
const HISTORY_MIN_PAGE: u32 = 1;
const HISTORY_MAX_PAGE: u32 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryCursor {
    v: u32,
    page_token: Option<String>,
    scanned: u64,
    total_hint: Option<u64>,
    include_spam_trash: bool,
}

/// Gmail mailbox reads.
pub struct GmailMailbox {
    api: GmailApi,
}

impl GmailMailbox {
    pub fn new(client: super::GoogleClient, user: impl Into<String>) -> Self {
        Self {
            api: GmailApi::new(client, user),
        }
    }
}

#[async_trait]
impl MailboxService for GmailMailbox {
    async fn collections(
        &mut self,
        ctx: &OperationContext,
    ) -> Result<Vec<CollectionRecord>, ProviderError> {
        ctx.ensure_active()?;
        let response = self.api.labels(ctx).await?;
        let mut collections: Vec<CollectionRecord> = response
            .labels
            .iter()
            .filter_map(normalize::collection_record)
            .collect();
        // The virtual All Mail collection is provider-owned: it is not Gmail's
        // real `ALL` label id and its membership is decided by the adapter.
        collections.push(normalize::all_mail_collection());
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
        let message = self.api.message_full(id.as_str(), ctx).await?;
        Ok(normalize::body_content(&message))
    }

    async fn attachment(
        &mut self,
        message: &RemoteMessageId,
        id: &RemoteAttachmentId,
        ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError> {
        ctx.ensure_active()?;
        let response = match self
            .api
            .attachment(message.as_str(), id.as_str(), ctx)
            .await
        {
            Ok(response) => response,
            Err(error) if error.kind == crate::providers::ErrorKind::NotFound => {
                // A part without a separate attachmentId is addressed by its
                // Gmail part id; decode it from the full MIME payload.
                let full = self.api.message_full(message.as_str(), ctx).await?;
                let bytes = normalize::attachment_data(&full, id.as_str()).ok_or_else(|| {
                    ProviderError::not_found("the attachment is not in this message")
                })?;
                check_attachment_size(bytes.len() as u64).map_err(|_| {
                    ProviderError::unsupported("the attachment is larger than the client accepts")
                })?;
                return Ok(bytes);
            }
            Err(error) => return Err(error),
        };
        let bytes = normalize::decode_attachment(&response)?;
        check_attachment_size(bytes.len() as u64).map_err(|_| {
            ProviderError::unsupported("the attachment is larger than the client accepts")
        })?;
        Ok(bytes)
    }

    async fn history(
        &mut self,
        request: HistoryRequest,
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        ctx.ensure_active()?;
        let page_size = request.page_size.clamp(HISTORY_MIN_PAGE, HISTORY_MAX_PAGE);

        let mut state = match &request.cursor {
            Some(cursor) => decode_history_cursor(cursor)?,
            None => HistoryCursor {
                v: HISTORY_CURSOR_VERSION,
                page_token: None,
                scanned: 0,
                total_hint: None,
                include_spam_trash: true,
            },
        };
        if state.v != HISTORY_CURSOR_VERSION {
            return Err(ProviderError::cursor_invalid(
                "the history cursor was written by a different version",
            ));
        }

        // Account-wide listing: no labelIds filter. The explicit
        // includeSpamTrash policy makes accessible Spam/Trash header-only
        // observations available to the learner, which decides their trust.
        let response = self
            .api
            .list_messages(
                None,
                state.page_token.as_deref(),
                page_size,
                state.include_spam_trash,
                ctx,
            )
            .await?;

        let ids: Vec<String> = response
            .messages
            .into_iter()
            .map(|message| message.id)
            .collect();
        let messages = self.metadata_batch(&ids, ctx).await?;
        state.scanned += messages.len() as u64;
        if let Some(total) = response.result_size_estimate {
            state.total_hint = Some(state.total_hint.map_or(total, |hint| hint.max(total)));
        }
        state.page_token = response.next_page_token.clone();

        Ok(HistoryPage {
            messages,
            checkpoint: encode_history_cursor(&state)?,
            has_more: response.next_page_token.is_some(),
            coverage: HistoryCoverage {
                scanned: state.scanned,
                total_hint: state.total_hint,
            },
        })
    }
}

impl GmailMailbox {
    /// Fetch header metadata with the adapter's bounded concurrency (four
    /// requests). A message deleted between the listing and the metadata read
    /// is simply absent from the learning page.
    async fn metadata_batch(
        &self,
        ids: &[String],
        ctx: &OperationContext,
    ) -> Result<Vec<MessageRecord>, ProviderError> {
        use futures::StreamExt;

        let api = self.api.clone();
        let mut records = Vec::new();
        let mut stream = futures::stream::iter(ids.iter().cloned().map(|id| {
            let api = api.clone();
            async move {
                match api.message_metadata(&id, ctx).await {
                    Ok(message) => Ok(Some(normalize::message_record(&message))),
                    Err(error) if error.kind == crate::providers::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error),
                }
            }
        }))
        .buffer_unordered(super::sync::MAX_CONCURRENT_REQUESTS);

        while let Some(result) = stream.next().await {
            if let Some(record) = result? {
                records.push(record);
            }
        }
        Ok(records)
    }
}

fn collection_order(id: &crate::providers::RemoteCollectionId) -> (u8, &str) {
    let rank = match id.as_str() {
        normalize::INBOX_LABEL => 0,
        normalize::SENT_LABEL => 1,
        normalize::DRAFTS_LABEL => 2,
        normalize::TRASH_LABEL => 3,
        normalize::SPAM_LABEL => 4,
        normalize::ALL_MAIL_COLLECTION => 5,
        _ => 6,
    };
    (rank, id.as_str())
}

fn encode_history_cursor(state: &HistoryCursor) -> Result<ProviderCursor, ProviderError> {
    let payload = serde_json::to_string(state).map_err(|error| {
        ProviderError::transient("could not encode the history cursor")
            .with_source_detail(error.to_string())
    })?;
    Ok(ProviderCursor::new(HISTORY_CURSOR_VERSION, payload))
}

fn decode_history_cursor(cursor: &ProviderCursor) -> Result<HistoryCursor, ProviderError> {
    serde_json::from_str(&cursor.payload)
        .map_err(|_| ProviderError::cursor_invalid("the history cursor could not be read"))
}
