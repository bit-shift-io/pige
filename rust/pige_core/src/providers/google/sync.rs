//! Gmail incremental synchronization.
//!
//! Gmail does not expose UIDs. The durable checkpoint is an adapter-private
//! [`GmailCursor`] with four stages:
//!
//! * `Bootstrap`: list a bounded recent window per syncable system collection
//!   after capturing a history anchor.
//! * `CatchUp`: replay every history page from that anchor, so changes made
//!   while the bootstrap listing ran are not lost.
//! * `Delta`: apply the next history page and advance using the response
//!   `historyId` (never lexicographic or numeric id arithmetic).
//! * `Reconcile`: the history cursor expired. Re-list a bounded recent window
//!   without treating skipped older messages as deletions, then resume from a
//!   fresh anchor.
//!
//! A message id is Gmail's account-wide id, so one message is one record with
//! many label memberships. Metadata is header-only. `is_new_arrival` is true
//! only for `messagesAdded` events observed in a genuine `Delta` page.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::providers::{
    ChangePage, ErrorKind, MailChange, MessageRecord, OperationContext, ProviderCursor,
    ProviderError, RemoteCollectionId, SyncRequest, SyncService,
};

use super::client::{GmailApi, HistoryResponse};
use super::normalize;

/// Initial request concurrency per account. All metadata batches stay at or
/// below this bound.
pub(crate) const MAX_CONCURRENT_REQUESTS: usize = 4;

/// The brief's bounded bootstrap window per syncable system collection.
const BOOTSTRAP_PER_COLLECTION: u32 = 300;

/// Gmail's maximum `maxResults` for a message list.
const GMAIL_MAX_RESULTS: u32 = 500;

const SYNC_CURSOR_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum GmailCursor {
    Bootstrap {
        anchor: String,
        collection: String,
        page_token: Option<String>,
        sweep: bool,
    },
    CatchUp {
        anchor: String,
        page_token: Option<String>,
    },
    Delta {
        history_id: String,
        page_token: Option<String>,
    },
    Reconcile {
        anchor: String,
        sweep: bool,
        page_token: Option<String>,
    },
}

impl GmailCursor {
    fn anchor(&self) -> &str {
        match self {
            Self::Bootstrap { anchor, .. }
            | Self::CatchUp { anchor, .. }
            | Self::Reconcile { anchor, .. } => anchor,
            Self::Delta { history_id, .. } => history_id,
        }
    }
}

/// Reconcile paging token. Reconcile has no collection field in its public
/// stage shape, so the adapter carries the remaining collections and the
/// current Gmail page token in this opaque token.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReconcileToken {
    remaining: Vec<String>,
    current: String,
    page: Option<String>,
}

/// Gmail sync service.
pub struct GmailSync {
    api: GmailApi,
    /// Messages already normalized in this bootstrap/reconcile pass. Gmail's
    /// metadata response carries every label membership, so a message seen in
    /// one collection does not need another metadata request when another
    /// collection lists it.
    seen: HashSet<String>,
    seen_key: Option<(String, bool)>,
}

impl GmailSync {
    pub fn new(client: super::GoogleClient, user: impl Into<String>) -> Self {
        Self {
            api: GmailApi::new(client, user),
            seen: HashSet::new(),
            seen_key: None,
        }
    }

    fn reset_seen(&mut self) {
        self.seen.clear();
        self.seen_key = None;
    }

    fn begin_bootstrap_seen(&mut self, anchor: &str, sweep: bool) {
        let key = (anchor.to_string(), sweep);
        if self.seen_key.as_ref() != Some(&key) {
            self.seen.clear();
            self.seen_key = Some(key);
        }
    }

    async fn ensure_anchor(
        &self,
        anchor: String,
        ctx: &OperationContext,
    ) -> Result<String, ProviderError> {
        if !anchor.is_empty() {
            return Ok(anchor);
        }
        Ok(self.api.profile(ctx).await?.history_id)
    }

    async fn collection_changes(
        &self,
        ctx: &OperationContext,
    ) -> Result<Vec<MailChange>, ProviderError> {
        let labels = self.api.labels(ctx).await?.labels;
        Ok(collection_upserts(&labels))
    }

    async fn bootstrap_page(
        &mut self,
        anchor: String,
        collection: String,
        page_token: Option<String>,
        sweep: bool,
        initial_limit: u32,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        let anchor = self.ensure_anchor(anchor, ctx).await?;
        self.begin_bootstrap_seen(&anchor, sweep);

        let labels = self.api.labels(ctx).await?.labels;
        let mut changes = collection_upserts(&labels);
        let syncable = normalize::syncable_collections(&labels);
        let allowed: Vec<RemoteCollectionId> = if sweep {
            syncable.clone()
        } else if collection.is_empty() {
            Vec::new()
        } else {
            vec![RemoteCollectionId::new(collection.clone())]
        };

        let current = if sweep {
            start_collection(&allowed, &collection)
        } else {
            allowed.first().cloned()
        };
        let Some(current) = current else {
            return Ok(ChangePage {
                changes,
                checkpoint: encode_cursor(&GmailCursor::CatchUp {
                    anchor,
                    page_token: None,
                })?,
                has_more: true,
            });
        };

        let next_collection = if sweep {
            next_after(&allowed, current.as_str()).map(|next| next.as_str().to_string())
        } else {
            None
        };

        // At least the brief's 300-message recent window per collection, even
        // when the scheduler asked for a smaller page; never exceed Gmail's
        // per-request maximum.
        let limit = initial_limit.clamp(BOOTSTRAP_PER_COLLECTION, GMAIL_MAX_RESULTS);
        let listing = self
            .api
            .list_messages(
                Some(current.as_str()),
                page_token.as_deref(),
                limit,
                true,
                ctx,
            )
            .await?;
        let ids: Vec<String> = listing
            .messages
            .into_iter()
            .map(|message| message.id)
            .collect();
        let fetched = self.metadata_for(&ids, ctx).await?;
        let mut newly_seen = HashSet::new();
        for id in &ids {
            match fetched.get(id).and_then(Option::as_ref) {
                Some(record) => {
                    let mut record = record.clone();
                    if !record
                        .collections
                        .iter()
                        .any(|collection| collection == &current)
                    {
                        record.collections.push(current.clone());
                        record.collections.sort();
                        record.collections.dedup();
                    }
                    changes.push(MailChange::UpsertMessage {
                        message: record,
                        is_new_arrival: false,
                    });
                    newly_seen.insert(id.clone());
                }
                None if fetched.contains_key(id) => {
                    // Listed, then gone before its metadata could be read.
                    changes.push(MailChange::RemoveMessage(
                        crate::providers::RemoteMessageId::new(id.clone()),
                    ));
                    newly_seen.insert(id.clone());
                }
                None if self.seen.contains(id) => {
                    // Normalized earlier in this pass (another label listed the
                    // same account-wide message). No second metadata request,
                    // but the membership still has to be recorded.
                    changes.push(MailChange::SetMembership {
                        message: crate::providers::RemoteMessageId::new(id.clone()),
                        collection: current.clone(),
                        present: true,
                    });
                }
                None => {}
            }
        }
        self.seen.extend(newly_seen);

        let checkpoint = match next_collection {
            Some(next) => GmailCursor::Bootstrap {
                anchor,
                collection: next,
                page_token: None,
                sweep: true,
            },
            None => GmailCursor::CatchUp {
                anchor,
                page_token: None,
            },
        };
        Ok(ChangePage {
            changes,
            checkpoint: encode_cursor(&checkpoint)?,
            has_more: true,
        })
    }

    async fn catch_up_page(
        &mut self,
        anchor: String,
        page_token: Option<String>,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        self.reset_seen();
        let anchor = self.ensure_anchor(anchor, ctx).await?;
        match self.api.history(&anchor, page_token.as_deref(), ctx).await {
            Ok(response) => {
                let changes = self.history_changes(&response, false, ctx).await?;
                let has_more = response.next_page_token.is_some();
                let checkpoint = match response.next_page_token {
                    Some(token) => GmailCursor::CatchUp {
                        anchor,
                        page_token: Some(token),
                    },
                    None => GmailCursor::Delta {
                        history_id: history_id_or(&response, &anchor),
                        page_token: None,
                    },
                };
                Ok(ChangePage {
                    changes,
                    checkpoint: encode_cursor(&checkpoint)?,
                    has_more,
                })
            }
            Err(error) if is_expired_history(&error) => {
                self.reconcile_checkpoint(&anchor, ctx).await
            }
            Err(error) => Err(error),
        }
    }

    async fn delta_page(
        &mut self,
        history_id: String,
        page_token: Option<String>,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        self.reset_seen();
        match self
            .api
            .history(&history_id, page_token.as_deref(), ctx)
            .await
        {
            Ok(response) => {
                // Gmail history has no label-rename event, so refresh the
                // collection list on each delta. The remote id is the label
                // id, so a rename updates only the display name.
                let mut changes = self.collection_changes(ctx).await?;
                changes.extend(self.history_changes(&response, true, ctx).await?);
                let has_more = response.next_page_token.is_some();
                let checkpoint = match response.next_page_token {
                    Some(token) => GmailCursor::Delta {
                        // Never advance the start id until every page of this
                        // delta has been consumed.
                        history_id,
                        page_token: Some(token),
                    },
                    None => GmailCursor::Delta {
                        history_id: history_id_or(&response, &history_id),
                        page_token: None,
                    },
                };
                Ok(ChangePage {
                    changes,
                    checkpoint: encode_cursor(&checkpoint)?,
                    has_more,
                })
            }
            Err(error) if is_expired_history(&error) => {
                self.reconcile_checkpoint(&history_id, ctx).await
            }
            Err(error) => Err(error),
        }
    }

    async fn reconcile_checkpoint(
        &self,
        expired: &str,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        // The expired id cannot be reused. Anchor the bounded repair at the
        // current head; changes after that point are caught by the next delta.
        // Nothing is removed just because it is older than the recent window.
        let head = self.api.profile(ctx).await?.history_id;
        if head.is_empty() {
            return Err(
                ProviderError::cursor_invalid("Gmail did not provide a history anchor")
                    .with_source_detail(format!("expired history id {expired}")),
            );
        }
        Ok(ChangePage {
            changes: Vec::new(),
            checkpoint: encode_cursor(&GmailCursor::Reconcile {
                anchor: head,
                sweep: true,
                page_token: None,
            })?,
            has_more: true,
        })
    }

    async fn reconcile_page(
        &mut self,
        anchor: String,
        page_token: Option<String>,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        self.reset_seen();
        let anchor = self.ensure_anchor(anchor, ctx).await?;
        let mut changes = Vec::new();

        let mut token = match page_token {
            Some(token) => decode_reconcile_token(&token)?,
            None => {
                let labels = self.api.labels(ctx).await?.labels;
                changes.extend(collection_upserts(&labels));
                let mut remaining: Vec<String> = normalize::syncable_collections(&labels)
                    .into_iter()
                    .map(|collection| collection.as_str().to_string())
                    .collect();
                if remaining.is_empty() {
                    return Ok(ChangePage {
                        changes,
                        checkpoint: encode_cursor(&GmailCursor::Delta {
                            history_id: anchor,
                            page_token: None,
                        })?,
                        has_more: false,
                    });
                }
                let current = remaining.remove(0);
                ReconcileToken {
                    remaining,
                    current,
                    page: None,
                }
            }
        };

        let listing = self
            .api
            .list_messages(
                Some(&token.current),
                token.page.as_deref(),
                BOOTSTRAP_PER_COLLECTION,
                true,
                ctx,
            )
            .await?;
        let ids: Vec<String> = listing
            .messages
            .into_iter()
            .map(|message| message.id)
            .collect();
        let fetched = self.metadata_for(&ids, ctx).await?;
        let current = RemoteCollectionId::new(token.current.clone());
        for id in &ids {
            if let Some(record) = fetched.get(id).and_then(Option::as_ref) {
                let mut record = record.clone();
                if !record.collections.iter().any(|label| label == &current) {
                    record.collections.push(current.clone());
                    record.collections.sort();
                    record.collections.dedup();
                }
                // A reconcile is a repair, not a new arrival and not a full
                // snapshot sweep: no absent-older-message removals.
                changes.push(MailChange::UpsertMessage {
                    message: record,
                    is_new_arrival: false,
                });
            }
        }

        let checkpoint = match listing.next_page_token {
            Some(page) => {
                token.page = Some(page);
                GmailCursor::Reconcile {
                    anchor,
                    sweep: true,
                    page_token: Some(encode_reconcile_token(&token)?),
                }
            }
            None if !token.remaining.is_empty() => {
                let current = token.remaining.remove(0);
                token.current = current;
                token.page = None;
                GmailCursor::Reconcile {
                    anchor,
                    sweep: true,
                    page_token: Some(encode_reconcile_token(&token)?),
                }
            }
            None => GmailCursor::Delta {
                history_id: anchor,
                page_token: None,
            },
        };
        let has_more = matches!(checkpoint, GmailCursor::Reconcile { .. });
        Ok(ChangePage {
            changes,
            checkpoint: encode_cursor(&checkpoint)?,
            has_more,
        })
    }

    /// Fetch metadata for `ids`, deduplicating requests and reusing records
    /// already normalized in this pass. `None` in the map means Gmail returned
    /// 404 for a listed message; a transient error aborts the whole page so the
    /// caller does not commit a partial batch.
    async fn metadata_for(
        &mut self,
        ids: &[String],
        ctx: &OperationContext,
    ) -> Result<HashMap<String, Option<MessageRecord>>, ProviderError> {
        use futures::StreamExt;

        let mut wanted: Vec<String> = Vec::new();
        let mut seen_here = HashSet::new();
        for id in ids {
            if self.seen.contains(id) || !seen_here.insert(id.clone()) {
                continue;
            }
            wanted.push(id.clone());
        }

        let results: Vec<(String, Result<Option<MessageRecord>, ProviderError>)> =
            futures::stream::iter(wanted.into_iter().map(|id| {
                let api = self.api.clone();
                async move {
                    let result = match api.message_metadata(&id, ctx).await {
                        Ok(message) => Ok(Some(normalize::message_record(&message))),
                        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
                        Err(error) => Err(error),
                    };
                    (id, result)
                }
            }))
            .buffer_unordered(MAX_CONCURRENT_REQUESTS)
            .collect()
            .await;

        let mut fetched = HashMap::new();
        for (id, result) in results {
            fetched.insert(id, result?);
        }
        Ok(fetched)
    }

    async fn history_changes(
        &mut self,
        response: &HistoryResponse,
        new_arrival: bool,
        ctx: &OperationContext,
    ) -> Result<Vec<MailChange>, ProviderError> {
        let mut added_ids = Vec::new();
        let mut seen = HashSet::new();
        for record in &response.history {
            for added in &record.messages_added {
                if seen.insert(added.message.id.clone()) {
                    added_ids.push(added.message.id.clone());
                }
            }
        }
        let fetched = self.metadata_for(&added_ids, ctx).await?;

        let mut changes = Vec::new();
        for record in &response.history {
            for added in &record.messages_added {
                match fetched.get(&added.message.id).and_then(Option::as_ref) {
                    Some(record) => changes.push(MailChange::UpsertMessage {
                        message: record.clone(),
                        is_new_arrival: new_arrival,
                    }),
                    None if fetched.contains_key(&added.message.id) => {
                        changes.push(MailChange::RemoveMessage(
                            crate::providers::RemoteMessageId::new(added.message.id.clone()),
                        ));
                    }
                    None => {}
                }
            }
            // Deletions are tombstones even when the same id was also added in
            // this range: both events must be applied, never skipped by a
            // "seen ids" set.
            for deleted in &record.messages_deleted {
                changes.push(MailChange::RemoveMessage(
                    crate::providers::RemoteMessageId::new(deleted.message.id.clone()),
                ));
            }
            for labels in &record.labels_added {
                for label in &labels.label_ids {
                    if normalize::is_membership_label(label) {
                        changes.push(MailChange::SetMembership {
                            message: crate::providers::RemoteMessageId::new(
                                labels.message.id.clone(),
                            ),
                            collection: RemoteCollectionId::new(label.clone()),
                            present: true,
                        });
                    }
                }
            }
            for labels in &record.labels_removed {
                for label in &labels.label_ids {
                    if normalize::is_membership_label(label) {
                        changes.push(MailChange::SetMembership {
                            message: crate::providers::RemoteMessageId::new(
                                labels.message.id.clone(),
                            ),
                            collection: RemoteCollectionId::new(label.clone()),
                            present: false,
                        });
                    }
                }
            }
        }
        Ok(changes)
    }
}

#[async_trait]
impl SyncService for GmailSync {
    async fn next_page(
        &mut self,
        request: SyncRequest,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        ctx.ensure_active()?;
        let cursor = match &request.cursor {
            Some(cursor) => decode_cursor(cursor)?,
            None => GmailCursor::Bootstrap {
                anchor: String::new(),
                collection: String::new(),
                page_token: None,
                sweep: true,
            },
        };
        match cursor {
            GmailCursor::Bootstrap {
                anchor,
                collection,
                page_token,
                sweep,
            } => {
                self.bootstrap_page(
                    anchor,
                    collection,
                    page_token,
                    sweep,
                    request.initial_limit,
                    ctx,
                )
                .await
            }
            GmailCursor::CatchUp { anchor, page_token } => {
                self.catch_up_page(anchor, page_token, ctx).await
            }
            GmailCursor::Delta {
                history_id,
                page_token,
            } => self.delta_page(history_id, page_token, ctx).await,
            GmailCursor::Reconcile {
                anchor, page_token, ..
            } => self.reconcile_page(anchor, page_token, ctx).await,
        }
    }
}

/// Ask the next sync page to bootstrap one collection on demand (for example a
/// newly selected user label), preserving the current history anchor.
pub fn request_collection(
    cursor: Option<&ProviderCursor>,
    collection: &RemoteCollectionId,
) -> Result<ProviderCursor, ProviderError> {
    let anchor = match cursor {
        Some(cursor) => decode_cursor(cursor)?.anchor().to_string(),
        None => String::new(),
    };
    encode_cursor(&GmailCursor::Bootstrap {
        anchor,
        collection: collection.as_str().to_string(),
        page_token: None,
        sweep: false,
    })
}

/// Test-only inspection of the opaque stage name. The application never parses
/// a checkpoint.
#[doc(hidden)]
pub fn cursor_stage(cursor: &ProviderCursor) -> Result<&'static str, ProviderError> {
    Ok(match decode_cursor(cursor)? {
        GmailCursor::Bootstrap { .. } => "bootstrap",
        GmailCursor::CatchUp { .. } => "catch-up",
        GmailCursor::Delta { .. } => "delta",
        GmailCursor::Reconcile { .. } => "reconcile",
    })
}

fn collection_upserts(labels: &[super::client::Label]) -> Vec<MailChange> {
    let mut changes: Vec<MailChange> = labels
        .iter()
        .filter_map(normalize::collection_record)
        .map(MailChange::UpsertCollection)
        .collect();
    changes.push(MailChange::UpsertCollection(
        normalize::all_mail_collection(),
    ));
    changes
}

/// Start a sweep at `current` when it is still present, otherwise at the first
/// collection.
fn start_collection(allowed: &[RemoteCollectionId], current: &str) -> Option<RemoteCollectionId> {
    if current.is_empty() {
        return allowed.first().cloned();
    }
    let current = RemoteCollectionId::new(current);
    if allowed.contains(&current) {
        Some(current)
    } else {
        allowed.first().cloned()
    }
}

/// The collection after `current` in stable order, if any.
fn next_after(allowed: &[RemoteCollectionId], current: &str) -> Option<RemoteCollectionId> {
    let position = allowed
        .iter()
        .position(|collection| collection.as_str() == current)?;
    allowed.get(position + 1).cloned()
}

fn is_expired_history(error: &ProviderError) -> bool {
    matches!(error.kind, ErrorKind::NotFound | ErrorKind::CursorInvalid)
}

fn history_id_or(response: &HistoryResponse, fallback: &str) -> String {
    if response.history_id.is_empty() {
        fallback.to_string()
    } else {
        response.history_id.clone()
    }
}

fn encode_cursor(cursor: &GmailCursor) -> Result<ProviderCursor, ProviderError> {
    let payload = serde_json::to_string(cursor).map_err(|error| {
        ProviderError::transient("could not encode the sync cursor")
            .with_source_detail(error.to_string())
    })?;
    Ok(ProviderCursor::new(SYNC_CURSOR_VERSION, payload))
}

fn decode_cursor(cursor: &ProviderCursor) -> Result<GmailCursor, ProviderError> {
    if cursor.version != SYNC_CURSOR_VERSION {
        return Err(ProviderError::cursor_invalid(
            "the sync cursor was written by a different version",
        ));
    }
    serde_json::from_str(&cursor.payload)
        .map_err(|_| ProviderError::cursor_invalid("the sync cursor could not be read"))
}

fn encode_reconcile_token(token: &ReconcileToken) -> Result<String, ProviderError> {
    serde_json::to_string(token).map_err(|error| {
        ProviderError::transient("could not encode the reconcile cursor")
            .with_source_detail(error.to_string())
    })
}

fn decode_reconcile_token(token: &str) -> Result<ReconcileToken, ProviderError> {
    serde_json::from_str(token)
        .map_err(|_| ProviderError::cursor_invalid("the reconcile cursor could not be read"))
}
