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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::providers::{
    ChangePage, ErrorKind, MailChange, MutationAction, MutationCapabilities, MutationItemResult,
    MutationOutcome, MutationRequest, MutationResult, MutationService, OperationContext,
    ProviderCursor, ProviderError, RemoteCollectionId, RemoteMessageId, SyncRequest, SyncService,
};

use proton_core::api;
use proton_core::api::events::{MessageEvent, action};

use super::client::{ProtonClient, ProtonHttp};
use super::mailbox::{
    PROTON_MAX_PAGE, collection_changes, fetch_counts, fetch_labels, latest_event,
};
use super::normalize;

const SYNC_CURSOR_VERSION: u32 = 3;

/// Adapter-local baseline of the last completed sweep, used to retire records
/// after an event retention gap. Never serialized into the cursor.
const EVENTS_BASELINE: &str = "events";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum ProtonCursor {
    /// Capture the latest event id before the snapshot starts.
    Bootstrap { anchor: String },
    /// Bounded recent-window enumeration, advanced by collection id.
    Snapshot(SweepState),
    /// Event replay (`catch_up` means the bootstrap anchor is still being
    /// replayed, so additions are historical, not live notifications). The
    /// last completed sweep's ids are held adapter-locally under
    /// [`EVENTS_BASELINE`], not in this cursor.
    Events { event_id: String, catch_up: bool },
    /// A fully consumed event stream.
    Complete { event_id: String },
    /// Post-reset reconciliation sweep.
    Reconcile(SweepState),
}

/// The accumulator for one snapshot/reconciliation sweep, held adapter-locally
/// (not in the durable cursor).
///
/// `known` is the previous completed sweep's ids; `seen` is the union of ids
/// observed so far. Keeping these out of [`SweepState`] is what bounds the
/// cursor: serializing the whole set on every page would make a large mailbox
/// re-encode megabytes per page (O(n^2) over the sweep). The table lives on the
/// account's shared [`ProtonClient`], so it survives the per-pass provider
/// session; only a process restart loses it, and the sweep then fails safe by
/// not retiring anything.
#[derive(Debug, Clone, Default)]
pub(crate) struct SweepBaseline {
    known: Vec<String>,
    seen: Vec<String>,
}

/// Adapter-private sweep/al-completed-sweep baselines for one account.
///
/// Held on the [`ProtonClient`] (shared across provider sessions/passes for an
/// account, cloned cheaply), never serialized into the durable cursor.
#[derive(Debug, Default)]
pub(crate) struct SweepTable {
    next_id: u64,
    baselines: HashMap<String, SweepBaseline>,
}

impl SweepTable {
    fn begin(&mut self, known: Vec<String>) -> String {
        // Only one sweep is ever in flight per account; drop any abandoned one.
        self.baselines.retain(|key, _| key == EVENTS_BASELINE);
        self.next_id = self.next_id.saturating_add(1);
        let handle = format!("sweep-{}", self.next_id);
        self.baselines.insert(
            handle.clone(),
            SweepBaseline {
                known,
                seen: Vec::new(),
            },
        );
        handle
    }

    fn set_events(&mut self, known: Vec<String>) {
        self.baselines.insert(
            EVENTS_BASELINE.to_string(),
            SweepBaseline {
                known,
                seen: Vec::new(),
            },
        );
    }

    fn put(&mut self, handle: String, baseline: SweepBaseline) {
        self.baselines.insert(handle, baseline);
    }
}

/// Holds an in-flight sweep's baseline for one page and reinserts it on every
/// exit: a normal page, an early finish (disarmed first via [`Self::take`]), or
/// an error propagating through `?`. Without this, a single transient page
/// error would discard the accumulated `seen` set, and the retry would fail
/// safe by retiring nothing for the whole sweep.
struct SweepGuard {
    table: Arc<Mutex<SweepTable>>,
    handle: String,
    baseline: Option<SweepBaseline>,
}

impl SweepGuard {
    fn new(table: Arc<Mutex<SweepTable>>, handle: String, baseline: SweepBaseline) -> Self {
        Self {
            table,
            handle,
            baseline: Some(baseline),
        }
    }

    fn baseline(&self) -> &SweepBaseline {
        self.baseline.as_ref().expect("a live sweep guard is armed")
    }

    fn baseline_mut(&mut self) -> &mut SweepBaseline {
        self.baseline.as_mut().expect("a live sweep guard is armed")
    }

    /// Disarm the guard and hand back the baseline (used when the sweep ends).
    fn take(&mut self) -> SweepBaseline {
        self.baseline.take().expect("a live sweep guard is armed")
    }
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        if let Some(baseline) = self.baseline.take() {
            self.table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .put(self.handle.clone(), baseline);
        }
    }
}

/// Mutable state of one snapshot/reconciliation sweep.
///
/// Collections are tracked by opaque id, never by position: Proton labels can
/// be deleted between pages, which shifts every later offset and would make a
/// positional cursor skip the collection that slid into the consumed slot.
///
/// The cursor carries only a handle (`sweep`) into the adapter-local
/// [`SweepBaseline`]; the baseline itself is never serialized into the
/// checkpoint, so the payload stays bounded regardless of mailbox size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SweepState {
    anchor: String,
    observed: String,
    /// Handle into the adapter-local sweep baseline. Empty only for a cursor
    /// that escaped without a baseline (a restart mid-sweep); such a sweep
    /// fails safe by never retiring.
    #[serde(default)]
    sweep: String,
    /// Collections this sweep planned to enumerate when it started. Compared
    /// with the live set on every page.
    #[serde(default)]
    scope: Option<Vec<String>>,
    /// Collections already fully enumerated in this sweep.
    #[serde(default)]
    done: Vec<String>,
    /// Collection currently inside its offset page sequence.
    #[serde(default)]
    current: Option<String>,
    page: u32,
    /// Opaque ids from the page just emitted, used to suppress the deliberate
    /// overlap re-read after churn.
    #[serde(default)]
    tail: Vec<String>,
    /// A page saw the anchor move. The collection is enumerated once more
    /// before it is considered covered.
    #[serde(default)]
    churned: bool,
    /// Pinned listing page size; offsets are page-size-dependent and a changed
    /// size must not silently shift every boundary.
    #[serde(default)]
    page_size: u32,
    /// The live collection set no longer matches `scope`. The sweep still
    /// enumerates every live collection, but it never retires records: it did
    /// not cover a stable scope, and a message can disappear from the scoped
    /// labels while still existing under an aggregate view.
    #[serde(default)]
    scope_changed: bool,
}

/// Proton sync service.
pub struct ProtonSync {
    client: ProtonClient,
    /// The account's shared sweep baselines (on the [`ProtonClient`]), so they
    /// survive a per-pass provider session.
    baselines: Arc<Mutex<SweepTable>>,
}

impl ProtonSync {
    pub fn new(client: ProtonClient) -> Self {
        let baselines = client.sweep_table();
        Self { client, baselines }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SweepTable> {
        self.baselines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register a new sweep baseline and return its handle.
    fn begin_sweep(&self, known: Vec<String>) -> String {
        self.lock().begin(known)
    }

    /// Take an in-flight sweep's baseline out of the table for this page.
    fn take_sweep(&self, handle: &str) -> Option<SweepBaseline> {
        self.lock().baselines.remove(handle)
    }

    /// The last completed sweep's ids, for seeding a reconciliation. Missing
    /// after a process restart means an empty baseline: the reconcile then
    /// retires nothing (fail safe) until the next sweep rebuilds it.
    fn events_known(&self) -> Vec<String> {
        self.lock()
            .baselines
            .get(EVENTS_BASELINE)
            .map(|baseline| baseline.known.clone())
            .unwrap_or_default()
    }

    /// Replace the last completed sweep's ids.
    fn set_events_baseline(&self, known: Vec<String>) {
        self.lock().set_events(known);
    }

    /// Close a fully enumerated sweep and record its baseline adapter-locally.
    ///
    /// Retirement only happens when the live collection set matched the scope
    /// this sweep started with. When a label appeared or disappeared mid-sweep,
    /// the baseline is merged instead: `known` and `seen` are both retained, so
    /// a later stable sweep can still retire a record deleted during an event
    /// gap. Set membership is checked with a `HashSet`, not a repeated `Vec`
    /// scan. The baseline never enters the durable cursor.
    fn finish_sweep(
        &self,
        anchor: String,
        known: &[String],
        seen: Vec<String>,
        scope_changed: bool,
        mut changes: Vec<MailChange>,
    ) -> Result<(Vec<MailChange>, ProtonCursor), ProviderError> {
        if scope_changed {
            let mut baseline = known.to_vec();
            baseline.extend(seen);
            baseline.sort_unstable();
            baseline.dedup();
            self.set_events_baseline(baseline);
            return Ok((
                changes,
                ProtonCursor::Events {
                    event_id: anchor,
                    // Mail created while the bounded snapshot ran is historical
                    // import, not a live notification.
                    catch_up: true,
                },
            ));
        }

        let observed: HashSet<&str> = seen.iter().map(String::as_str).collect();
        for id in known {
            if !observed.contains(id.as_str()) {
                changes.push(MailChange::RemoveMessage(RemoteMessageId::new(id.clone())));
            }
        }
        self.set_events_baseline(seen);
        Ok((
            changes,
            ProtonCursor::Events {
                event_id: anchor,
                catch_up: true,
            },
        ))
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
        let sweep = self.begin_sweep(Vec::new());
        // The first snapshot page is produced in the same call so a bounded
        // mailbox does not spend a round trip on an empty bootstrap step.
        self.snapshot_page(
            http,
            SweepState {
                anchor: anchor.clone(),
                observed: anchor,
                sweep,
                scope: None,
                done: Vec::new(),
                current: None,
                page: 0,
                tail: Vec::new(),
                churned: false,
                page_size: initial_limit.clamp(1, PROTON_MAX_PAGE),
                scope_changed: false,
            },
            false,
            ctx,
        )
        .await
    }

    async fn snapshot_page(
        &mut self,
        http: ProtonHttp,
        mut sweep: SweepState,
        reconcile: bool,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        let labels = fetch_labels(&http).await?;
        let counts = fetch_counts(&http).await?;
        // `fetch_counts` treats transient failures as decorative, and a passed
        // deadline is Transient, so re-check the context before building a
        // checkpoint from an operation that may already be over.
        ctx.ensure_active()?;
        let mut changes = collection_changes(&labels, &counts);
        let syncable = normalize::syncable_collections(&labels);
        let live_scope: Vec<String> = syncable.iter().map(|id| id.as_str().to_string()).collect();

        // Hold the sweep baseline in a guard for this page; it is reinserted on
        // any exit (including an error), so a transient failure never discards
        // the accumulated set. It is never serialized into the cursor. A missing
        // entry means the process restarted mid-sweep: earlier seen ids are
        // gone, so this sweep must not retire anything.
        let baseline = self.take_sweep(&sweep.sweep);
        if baseline.is_none() {
            sweep.scope_changed = true;
        }
        let mut sweep_guard = SweepGuard::new(
            self.baselines.clone(),
            sweep.sweep.clone(),
            baseline.unwrap_or_default(),
        );

        // Freeze the scope on the first page. Any later difference means a
        // label appeared or disappeared mid-sweep: keep enumerating the live
        // collections, but never retire on this generation.
        match &sweep.scope {
            Some(planned) => {
                if planned != &live_scope {
                    sweep.scope_changed = true;
                }
            }
            None => sweep.scope = Some(live_scope.clone()),
        }

        // Resume the in-progress collection by id. If it was deleted, abandon
        // its partial page sequence and move to the next live collection.
        let mut collection = match sweep.current.as_ref() {
            Some(current) if live_scope.iter().any(|id| id == current) => {
                Some(RemoteCollectionId::new(current.clone()))
            }
            Some(_) => {
                sweep.current = None;
                sweep.page = 0;
                sweep.tail.clear();
                sweep.churned = false;
                None
            }
            None => None,
        };
        if collection.is_none() {
            collection = live_scope
                .iter()
                .find(|id| !sweep.done.contains(*id))
                .map(|id| RemoteCollectionId::new(id.clone()));
        }
        let Some(collection_id) = collection else {
            // Nothing left to enumerate. Either the sweep genuinely finished or
            // the scope shrank to empty; `finish_sweep` only retires when the
            // scope held still.
            let mut baseline = sweep_guard.take();
            let (changes, next) = self.finish_sweep(
                sweep.anchor.clone(),
                &baseline.known,
                std::mem::take(&mut baseline.seen),
                sweep.scope_changed,
                changes,
            )?;
            ctx.ensure_active()?;
            return Ok(ChangePage {
                changes,
                checkpoint: encode_cursor(&next)?,
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
        let mut ids = HashSet::new();
        let mut last_page_ids: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = sweep_guard.baseline().seen.iter().cloned().collect();
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
        if restart {
            sweep.page = 0;
            sweep.tail.clear();
            sweep.churned = false;
            sweep.current = Some(collection_id.as_str().to_string());
        } else if at_end {
            sweep.done.push(collection_id.as_str().to_string());
            sweep.current = None;
            sweep.page = 0;
            sweep.tail.clear();
            sweep.churned = false;
        } else {
            sweep.page = next_page;
            sweep.tail = last_page_ids;
            sweep.churned = churned;
            sweep.current = Some(collection_id.as_str().to_string());
        }
        sweep_guard.baseline_mut().seen = {
            let mut seen: Vec<String> = seen.into_iter().collect();
            seen.sort_unstable();
            seen
        };
        sweep.observed = latest_after;

        let sweep_complete = sweep.current.is_none()
            && live_scope
                .iter()
                .all(|collection| sweep.done.contains(collection));
        if sweep_complete {
            // The scope is fully enumerated and the anchor held for this pass.
            // Retire records that were known after the previous completed
            // sweep but did not appear anywhere in this one. This is the only
            // place a snapshot emits removals; one bounded page never does.
            let mut baseline = sweep_guard.take();
            let (changes, next) = self.finish_sweep(
                sweep.anchor.clone(),
                &baseline.known,
                std::mem::take(&mut baseline.seen),
                sweep.scope_changed,
                changes,
            )?;
            ctx.ensure_active()?;
            return Ok(ChangePage {
                changes,
                checkpoint: encode_cursor(&next)?,
                has_more: true,
            });
        }

        // On the normal path `sweep_guard` drops at the end of this function
        // and reinserts the accumulated baseline under the same handle.
        let next = if reconcile {
            ProtonCursor::Reconcile(sweep)
        } else {
            ProtonCursor::Snapshot(sweep)
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
        page_size: u32,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        let http = self.client.request_http(ctx).await?;
        let known = self.events_known();
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
            }
        } else {
            ProtonCursor::Complete {
                event_id: batch.event_id,
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
        &mut self,
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
        ctx.ensure_active()?;
        changes.extend(collection_changes(&labels, &counts));
        let scope = normalize::syncable_collections(&labels)
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        let sweep = self.begin_sweep(known);
        Ok(ChangePage {
            changes,
            checkpoint: encode_cursor(&ProtonCursor::Reconcile(SweepState {
                observed: anchor.clone(),
                anchor,
                sweep,
                scope: Some(scope),
                done: Vec::new(),
                current: None,
                page: 0,
                tail: Vec::new(),
                churned: false,
                page_size: page_size.clamp(1, PROTON_MAX_PAGE),
                scope_changed: false,
            }))?,
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
            ProtonCursor::Snapshot(sweep) => {
                let http = self.client.request_http(ctx).await?;
                let mut sweep = sweep;
                // A cursor written by an older build (0) adopts this request's
                // size once.
                if sweep.page_size == 0 {
                    sweep.page_size = requested_page_size;
                }
                self.snapshot_page(http, sweep, false, ctx).await
            }
            ProtonCursor::Events { event_id, catch_up } => {
                self.event_page(event_id, catch_up, requested_page_size, ctx)
                    .await
            }
            ProtonCursor::Complete { event_id } => {
                // A delta after the bootstrap is live mail.
                self.event_page(event_id, false, requested_page_size, ctx)
                    .await
            }
            ProtonCursor::Reconcile(sweep) => {
                let mut sweep = sweep;
                if sweep.page_size == 0 {
                    sweep.page_size = requested_page_size;
                }
                self.reconcile_page(sweep, ctx).await
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
        ProtonCursor::Snapshot(_) => "snapshot",
        ProtonCursor::Events { .. } => "events",
        ProtonCursor::Complete { .. } => "complete",
        ProtonCursor::Reconcile(_) => "reconcile",
    })
}
