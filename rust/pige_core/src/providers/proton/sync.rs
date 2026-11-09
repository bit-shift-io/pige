//! Proton incremental sync and semantic mutations.
//!
//! Proton has no dense UID space. The durable checkpoint is an adapter-private
//! [`ProtonCursor`] with five stages:
//!
//! * `Bootstrap`: capture the account's latest event id (the anchor) before a
//!   bounded recent-mail snapshot.
//! * `Snapshot`: enumerate a bounded recent window per folder/label. Pages are
//!   offset-based, so the adapter re-reads the previous page and deduplicates by
//!   opaque message id when the event anchor moved underneath a page.
//! * `Events`: replay event pages from that anchor. `More = 1` keeps the stage
//!   open and never commits the returned id as a completed cursor.
//! * `Complete`: the durable event id. The next pass fetches the delta since it.
//! * `Reconcile`: the server asked for a reset (`Refresh`), the delta cursor
//!   expired, or an event payload was too partial to apply. The adapter
//!   re-enumerates the whole scoped window and retires only records absent from
//!   that *completed* generation, so a short event-retention gap cannot keep a
//!   deleted message cached forever.
//!
//! `is_new_arrival` is true only for a `Create` event observed in a genuine
//! delta page, never for snapshot/history/reconciliation or for mail replayed
//! from the bootstrap anchor.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::providers::{
    ChangePage, ErrorKind, MailChange, MutationAction, MutationCapabilities, MutationItemResult,
    MutationOutcome, MutationRequest, MutationResult, MutationService, OperationContext,
    ProviderCursor, ProviderError, RemoteMessageId, SyncRequest, SyncService,
};

use proton_core::api;
use proton_core::api::events::{MessageEvent, action};

use super::client::{ProtonClient, ProtonHttp};
use super::mailbox::{
    PROTON_MAX_PAGE, collection_changes, fetch_counts, fetch_labels, latest_event,
};
use super::normalize;

const SYNC_CURSOR_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum ProtonCursor {
    /// Capture the latest event id before the snapshot starts.
    Bootstrap { anchor: String },
    /// Bounded recent-window enumeration for one collection.
    Snapshot {
        anchor: String,
        observed: String,
        collection: usize,
        page: u32,
        /// Opaque ids from the page just emitted, used to suppress the
        /// deliberate overlap re-read after churn.
        #[serde(default)]
        tail: Vec<String>,
        /// A page saw the anchor move. The collection is enumerated once more
        /// before it is considered covered.
        #[serde(default)]
        churned: bool,
        /// Pinned listing page size; offsets are page-size-dependent and a
        /// changed size must not silently shift every boundary.
        #[serde(default)]
        page_size: u32,
        /// Ids from the last completed sweep. Once a new sweep completes, ids
        /// in this set that were not observed are retired.
        #[serde(default)]
        known: Vec<String>,
        /// Ids observed so far in this sweep (union across every collection).
        #[serde(default)]
        seen: Vec<String>,
    },
    /// Event replay (`catch_up` means the bootstrap anchor is still being
    /// replayed, so additions are historical, not live notifications).
    Events {
        event_id: String,
        catch_up: bool,
        /// Ids from the last completed sweep, retained so a reset-driven
        /// reconciliation can retire records that disappeared during an event
        /// retention gap.
        #[serde(default)]
        known: Vec<String>,
    },
    /// A fully consumed event stream.
    Complete {
        event_id: String,
        #[serde(default)]
        known: Vec<String>,
    },
    /// Post-reset reconciliation sweep.
    Reconcile {
        anchor: String,
        observed: String,
        collection: usize,
        page: u32,
        #[serde(default)]
        tail: Vec<String>,
        #[serde(default)]
        churned: bool,
        #[serde(default)]
        page_size: u32,
        #[serde(default)]
        known: Vec<String>,
        #[serde(default)]
        seen: Vec<String>,
    },
}

/// Mutable state of one snapshot/reconciliation sweep. Kept out of the cursor
/// enum so the paging logic does not have to unpack and rebuild a dozen fields.
#[derive(Debug, Clone)]
struct SweepState {
    anchor: String,
    observed: String,
    collection: usize,
    page: u32,
    tail: Vec<String>,
    churned: bool,
    page_size: u32,
    known: Vec<String>,
    seen: Vec<String>,
}

/// Proton sync service.
pub struct ProtonSync {
    client: ProtonClient,
}

impl ProtonSync {
    pub fn new(client: ProtonClient) -> Self {
        Self { client }
    }

    async fn bootstrap_page(
        &mut self,
        anchor: String,
        initial_limit: u32,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        let http = self.client.request_http(ctx).await?;
        let anchor = if anchor.is_empty() {
            latest_event(&http).await?
        } else {
            anchor
        };
        // The first snapshot page is produced in the same call so a bounded
        // mailbox does not spend a round trip on an empty bootstrap step.
        self.snapshot_page(
            http,
            SweepState {
                anchor: anchor.clone(),
                observed: anchor,
                collection: 0,
                page: 0,
                tail: Vec::new(),
                churned: false,
                page_size: initial_limit.clamp(1, PROTON_MAX_PAGE),
                known: Vec::new(),
                seen: Vec::new(),
            },
            false,
            ctx,
        )
        .await
    }

    async fn snapshot_page(
        &mut self,
        http: ProtonHttp,
        sweep: SweepState,
        reconcile: bool,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        let labels = fetch_labels(&http).await?;
        let counts = fetch_counts(&http).await?;
        let mut changes = collection_changes(&labels, &counts);
        let syncable = normalize::syncable_collections(&labels);

        let Some(collection_id) = syncable.get(sweep.collection).cloned() else {
            // The planned collection set shrank under us (a label disappeared
            // between pages). Finish without retiring anything: this was not a
            // completed sweep over the scope it started with.
            return Ok(ChangePage {
                changes,
                checkpoint: encode_cursor(&ProtonCursor::Events {
                    event_id: sweep.anchor,
                    catch_up: true,
                    known: sweep.seen,
                })?,
                has_more: true,
            });
        };

        let latest_before = latest_event(&http).await?;
        let churn = latest_before != sweep.observed;
        let churned = sweep.churned || churn;
        // Offset pages are unstable: a deletion above the cursor shifts an
        // unseen message backwards into the page already consumed. Re-read the
        // previous page and deduplicate by the account-wide opaque id. The
        // overlap also runs while a moved anchor has not yet gone clean.
        let first_page = if (churn || sweep.churned) && sweep.page > 0 {
            sweep.page - 1
        } else {
            sweep.page
        };
        let mut total = 0u32;
        let mut ids = std::collections::HashSet::new();
        let mut last_page_ids: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = sweep.seen.iter().cloned().collect();
        for number in first_page..=sweep.page {
            // A fenced/cancelled context must not fetch or emit another page.
            ctx.ensure_active()?;
            let (reported_total, page_messages) =
                list_messages_in(&http, &collection_id, number, sweep.page_size).await?;
            total = total.max(reported_total);
            if number == sweep.page {
                last_page_ids = page_messages
                    .iter()
                    .map(|message| message.id.clone())
                    .collect();
            }
            for message in page_messages {
                // Track every record the listing returned, including records
                // suppressed from this page by the overlap `tail`, so a
                // completed sweep never retires something it actually saw.
                seen.insert(message.id.clone());
                if !ids.insert(message.id.clone()) || sweep.tail.iter().any(|id| id == &message.id)
                {
                    continue;
                }
                changes.push(MailChange::UpsertMessage {
                    message: normalize::message_record(&message),
                    // Snapshot/reconciliation observations never notify.
                    is_new_arrival: false,
                });
            }
        }

        // Re-read the anchor *after* the listing. A deletion between the
        // pre-listing read and the final page can shift the boundary record
        // into a page already consumed, so completion must never rely only on
        // the anchor sampled before the page.
        let latest_after = latest_event(&http).await?;
        let moved = latest_after != latest_before;
        let churned = churned || moved;

        let next_page = sweep.page.saturating_add(1);
        let at_end = u64::from(next_page) * u64::from(sweep.page_size) >= u64::from(total);
        // A collection whose listing moved under a page gets one clean pass
        // before the bounded window is considered covered.
        let restart = at_end && churned;
        let (next_collection, next_number) = if restart {
            (sweep.collection, 0)
        } else if at_end {
            (sweep.collection + 1, 0)
        } else {
            (sweep.collection, next_page)
        };
        let mut seen: Vec<String> = seen.into_iter().collect();
        seen.sort_unstable();
        let next = if next_collection >= syncable.len() {
            // The scope is fully enumerated and the anchor held for this pass.
            // Retire records that were known after the previous completed
            // sweep but did not appear anywhere in this one. This is the only
            // place a snapshot emits removals; one bounded page never does.
            for id in &sweep.known {
                if !seen.iter().any(|seen| seen == id) {
                    changes.push(MailChange::RemoveMessage(RemoteMessageId::new(id.clone())));
                }
            }
            ProtonCursor::Events {
                event_id: sweep.anchor,
                // Mail created while the bounded snapshot ran is historical
                // import, not a live notification.
                catch_up: true,
                known: seen,
            }
        } else {
            let next_tail = if restart || next_collection != sweep.collection {
                Vec::new()
            } else {
                last_page_ids
            };
            if reconcile {
                ProtonCursor::Reconcile {
                    anchor: sweep.anchor,
                    observed: latest_after,
                    collection: next_collection,
                    page: next_number,
                    tail: next_tail,
                    churned: churned && !restart,
                    page_size: sweep.page_size,
                    known: sweep.known,
                    seen,
                }
            } else {
                ProtonCursor::Snapshot {
                    anchor: sweep.anchor,
                    observed: latest_after,
                    collection: next_collection,
                    page: next_number,
                    tail: next_tail,
                    churned: churned && !restart,
                    page_size: sweep.page_size,
                    known: sweep.known,
                    seen,
                }
            }
        };
        ctx.ensure_active()?;
        Ok(ChangePage {
            changes,
            checkpoint: encode_cursor(&next)?,
            has_more: true,
        })
    }

    async fn event_page(
        &mut self,
        event_id: String,
        catch_up: bool,
        known: Vec<String>,
        page_size: u32,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        let http = self.client.request_http(ctx).await?;
        let batch = match http.finish(api::events::get_events(&http, &event_id).await) {
            Ok(batch) => batch,
            // An event id older than the server's retention window is a reset,
            // not a fatal sync failure.
            Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::CursorInvalid) => {
                return self
                    .reset_page(&http, Vec::new(), known, page_size, ctx)
                    .await;
            }
            Err(error) => return Err(error),
        };
        if batch.refresh {
            return self
                .reset_page(&http, Vec::new(), known, page_size, ctx)
                .await;
        }

        let (changes, partial) = event_changes(&batch.messages, !catch_up);
        if partial {
            // A partial event payload cannot be upserted as a whole record
            // without erasing unmentioned headers or membership. Repair it now
            // with a bounded reconciliation instead of waiting for an unrelated
            // server reset.
            return self.reset_page(&http, changes, known, page_size, ctx).await;
        }
        let next = if batch.more {
            ProtonCursor::Events {
                event_id: batch.event_id,
                catch_up,
                known,
            }
        } else {
            ProtonCursor::Complete {
                event_id: batch.event_id,
                known,
            }
        };
        ctx.ensure_active()?;
        Ok(ChangePage {
            changes,
            checkpoint: encode_cursor(&next)?,
            has_more: batch.more,
        })
    }

    /// Start a reconciliation after a server-side event reset or a payload the
    /// adapter cannot apply safely. Capture a fresh anchor and re-enumerate the
    /// recent window; absent records are retired only when that scoped sweep
    /// completes, so cached messages survive a short event retention gap.
    async fn reset_page(
        &self,
        http: &ProtonHttp,
        mut changes: Vec<MailChange>,
        known: Vec<String>,
        page_size: u32,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        ctx.ensure_active()?;
        let anchor = latest_event(http).await?;
        let labels = fetch_labels(http).await?;
        let counts = fetch_counts(http).await?;
        changes.extend(collection_changes(&labels, &counts));
        ctx.ensure_active()?;
        Ok(ChangePage {
            changes,
            checkpoint: encode_cursor(&ProtonCursor::Reconcile {
                observed: anchor.clone(),
                anchor,
                collection: 0,
                page: 0,
                tail: Vec::new(),
                churned: false,
                page_size: page_size.clamp(1, PROTON_MAX_PAGE),
                known,
                seen: Vec::new(),
            })?,
            has_more: true,
        })
    }

    async fn reconcile_page(
        &mut self,
        sweep: SweepState,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        let http = self.client.request_http(ctx).await?;
        self.snapshot_page(http, sweep, true, ctx).await
    }
}

#[async_trait]
impl SyncService for ProtonSync {
    async fn next_page(
        &mut self,
        request: SyncRequest,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        ctx.ensure_active()?;
        let cursor = match &request.cursor {
            Some(cursor) => decode_cursor(cursor)?,
            None => ProtonCursor::Bootstrap {
                anchor: String::new(),
            },
        };
        let requested_page_size = request.initial_limit.clamp(1, PROTON_MAX_PAGE);
        match cursor {
            ProtonCursor::Bootstrap { anchor } => {
                self.bootstrap_page(anchor, requested_page_size, ctx).await
            }
            ProtonCursor::Snapshot {
                anchor,
                observed,
                collection,
                page,
                tail,
                churned,
                page_size,
                known,
                seen,
            } => {
                let http = self.client.request_http(ctx).await?;
                self.snapshot_page(
                    http,
                    SweepState {
                        anchor,
                        observed,
                        collection,
                        page,
                        tail,
                        churned,
                        page_size: if page_size == 0 {
                            requested_page_size
                        } else {
                            page_size.clamp(1, PROTON_MAX_PAGE)
                        },
                        known,
                        seen,
                    },
                    false,
                    ctx,
                )
                .await
            }
            ProtonCursor::Events {
                event_id,
                catch_up,
                known,
            } => {
                self.event_page(event_id, catch_up, known, requested_page_size, ctx)
                    .await
            }
            ProtonCursor::Complete { event_id, known } => {
                // A delta after the bootstrap is live mail.
                self.event_page(event_id, false, known, requested_page_size, ctx)
                    .await
            }
            ProtonCursor::Reconcile {
                anchor,
                observed,
                collection,
                page,
                tail,
                churned,
                page_size,
                known,
                seen,
            } => {
                self.reconcile_page(
                    SweepState {
                        anchor,
                        observed,
                        collection,
                        page,
                        tail,
                        churned,
                        page_size: if page_size == 0 {
                            requested_page_size
                        } else {
                            page_size.clamp(1, PROTON_MAX_PAGE)
                        },
                        known,
                        seen,
                    },
                    ctx,
                )
                .await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// Proton semantic mutations.
pub struct ProtonMutations {
    client: ProtonClient,
}

impl ProtonMutations {
    pub fn new(client: ProtonClient) -> Self {
        Self { client }
    }

    pub fn capabilities(&self) -> MutationCapabilities {
        MutationCapabilities {
            read: true,
            star: true,
            labels: true,
            move_messages: true,
            archive: true,
            trash: true,
            permanent_delete: true,
        }
    }

    async fn attempt(
        &self,
        http: &ProtonHttp,
        target: &RemoteMessageId,
        request: &MutationRequest,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        ctx.ensure_active()?;
        let ids = [target.as_str().to_string()];
        match &request.action {
            MutationAction::SetRead(read) => {
                let result = if *read {
                    api::messages::mark_read(http, &ids).await
                } else {
                    api::messages::mark_unread(http, &ids).await
                };
                http.finish(result)
            }
            MutationAction::SetStar(star) => {
                let result = if *star {
                    api::messages::label(http, normalize::STARRED, &ids).await
                } else {
                    api::messages::unlabel(http, normalize::STARRED, &ids).await
                };
                http.finish(result)
            }
            MutationAction::AddLabel(label) => {
                http.finish(api::messages::label(http, label.as_str(), &ids).await)
            }
            MutationAction::RemoveLabel(label) => {
                http.finish(api::messages::unlabel(http, label.as_str(), &ids).await)
            }
            MutationAction::Move(destination) => {
                let mut applied = false;
                let removal = match request.source.as_ref() {
                    Some(source) if source != destination => Some(source.as_str()),
                    // A move with no source leaves the Inbox location.
                    None => Some(normalize::INBOX),
                    Some(_) => None,
                };
                if let Some(source) = removal {
                    match http.finish(api::messages::unlabel(http, source, &ids).await) {
                        Ok(()) => applied = true,
                        Err(error) => return Err(partial_step(applied, error)),
                    }
                }
                match http.finish(api::messages::label(http, destination.as_str(), &ids).await) {
                    Ok(()) => Ok(()),
                    Err(error) => Err(partial_step(applied, error)),
                }
            }
            MutationAction::Archive => {
                // Archive is exactly "leave the Inbox"; unrelated labels stay.
                http.finish(api::messages::unlabel(http, normalize::INBOX, &ids).await)
            }
            MutationAction::Trash => {
                // Proton Trash is a location label, not a hard delete.
                let mut applied = false;
                for label in [normalize::INBOX, normalize::ARCHIVE, normalize::SPAM] {
                    match http.finish(api::messages::unlabel(http, label, &ids).await) {
                        Ok(()) => applied = true,
                        Err(error) => return Err(partial_step(applied, error)),
                    }
                }
                match http.finish(api::messages::label(http, normalize::TRASH, &ids).await) {
                    Ok(()) => Ok(()),
                    Err(error) => Err(partial_step(applied, error)),
                }
            }
            MutationAction::DeletePermanently => {
                http.finish(api::messages::delete(http, &ids).await)
            }
        }
    }
}

#[async_trait]
impl MutationService for ProtonMutations {
    async fn apply(
        &mut self,
        request: MutationRequest,
        ctx: &OperationContext,
    ) -> Result<MutationResult, ProviderError> {
        ctx.ensure_active()?;
        let http = self.client.request_http(ctx).await?;
        let mut items = Vec::with_capacity(request.targets.len());
        for target in &request.targets {
            ctx.ensure_active()?;
            let outcome = match self.attempt(&http, target, &request, ctx).await {
                Ok(()) => MutationOutcome::Applied,
                Err(error) if is_ambiguous_write(&error) => MutationOutcome::Unknown,
                Err(error) => MutationOutcome::Rejected(error),
            };
            items.push(MutationItemResult {
                message: target.clone(),
                outcome,
            });
        }
        Ok(MutationResult { items })
    }
}

/// A multi-step mutation that failed after an earlier step succeeded may be
/// partially applied, so it is ambiguous rather than a clean rejection.
fn partial_step(applied: bool, error: ProviderError) -> ProviderError {
    if applied {
        ProviderError::unknown_write_outcome("the Proton mutation may have only partially applied")
            .with_source_detail(format!("{:?}", error.kind))
    } else {
        error
    }
}

/// A transport failure after a write was sent may already have reached Proton.
/// A typed server rejection (including a rate limit) is a definite rejection.
fn is_ambiguous_write(error: &ProviderError) -> bool {
    matches!(
        error.kind,
        ErrorKind::Offline | ErrorKind::Transient | ErrorKind::UnknownWriteOutcome
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn list_messages_in(
    http: &ProtonHttp,
    collection: &crate::providers::RemoteCollectionId,
    page: u32,
    page_size: u32,
) -> Result<(u32, Vec<proton_core::model::MessageMetadata>), ProviderError> {
    let query = api::messages::ListQuery {
        label_id: Some(collection.as_str().to_string()),
        page: Some(page),
        page_size: Some(page_size),
        ..api::messages::ListQuery::default()
    };
    http.finish(api::messages::list_messages(http, &query).await)
}

/// Normalize one event batch. The boolean is true when the batch contained a
/// payload that is not safe to upsert as a whole record; the caller then starts
/// a bounded reconciliation instead of silently dropping the change until some
/// unrelated server reset.
fn event_changes(messages: &[MessageEvent], live: bool) -> (Vec<MailChange>, bool) {
    let mut changes = Vec::new();
    let mut partial = false;
    for event in messages {
        match event.action {
            action::DELETE => changes.push(MailChange::RemoveMessage(RemoteMessageId::new(
                event.id.clone(),
            ))),
            action::CREATE | action::UPDATE | action::UPDATE_FLAGS => {
                let Some(message) = event.message.as_ref() else {
                    // Listing metadata is the only safe header source; a
                    // missing payload is never fetched as a body here.
                    partial = true;
                    continue;
                };
                if !is_complete_metadata(message) {
                    // Never turn a partial flag/update payload into a full
                    // record that erases unmentioned headers or membership.
                    partial = true;
                    continue;
                }
                changes.push(MailChange::UpsertMessage {
                    message: normalize::message_record(message),
                    is_new_arrival: live && event.action == action::CREATE,
                });
            }
            _ => {}
        }
    }
    (changes, partial)
}

/// Proton's event payload is a full metadata object. A payload that omits the
/// authoritative arrival time or every label is not safe to upsert as a whole
/// record: doing so would erase membership the event did not mention.
fn is_complete_metadata(message: &proton_core::model::MessageMetadata) -> bool {
    !message.id.is_empty() && message.time > 0 && !message.label_ids.is_empty()
}

fn encode_cursor(cursor: &ProtonCursor) -> Result<ProviderCursor, ProviderError> {
    let payload = serde_json::to_string(cursor).map_err(|error| {
        ProviderError::transient("could not encode the sync cursor")
            .with_source_detail(error.to_string())
    })?;
    Ok(ProviderCursor::new(SYNC_CURSOR_VERSION, payload))
}

fn decode_cursor(cursor: &ProviderCursor) -> Result<ProtonCursor, ProviderError> {
    if cursor.version != SYNC_CURSOR_VERSION {
        return Err(ProviderError::cursor_invalid(
            "the sync cursor was written by a different version",
        ));
    }
    serde_json::from_str(&cursor.payload)
        .map_err(|_| ProviderError::cursor_invalid("the sync cursor could not be read"))
}

/// Test-only inspection of the opaque stage name. The application never parses
/// a checkpoint.
#[doc(hidden)]
pub fn cursor_stage(cursor: &ProviderCursor) -> Result<&'static str, ProviderError> {
    Ok(match decode_cursor(cursor)? {
        ProtonCursor::Bootstrap { .. } => "bootstrap",
        ProtonCursor::Snapshot { .. } => "snapshot",
        ProtonCursor::Events { .. } => "events",
        ProtonCursor::Complete { .. } => "complete",
        ProtonCursor::Reconcile { .. } => "reconcile",
    })
}
