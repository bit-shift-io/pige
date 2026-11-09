//! Bounded, source-scoped contact synchronization.
//!
//! One call drives one contact source through at most a few provider pages. A
//! page's contact changes, its source-scoped cursor, and its snapshot progress
//! commit in one generation-fenced transaction, so a crash or a WorkManager
//! stop loses at most the page that was in flight.
//!
//! Full snapshots are versioned: the first page of a new crawl allocates a new
//! generation, and only the page that reports end-of-enumeration may sweep
//! contacts from older generations. A failed, cancelled, or budget-limited
//! slice leaves the previous local address book untouched.

use crate::providers::{
    AddressBookService, ContactSourceId, ErrorKind, OperationContext, ProviderError,
};
use crate::store::{Store, StoreError};
use crate::sync::history::LearningProgress;
use crate::sync::provider::WorkBudget;
use crate::sync::scheduler::{AccountScheduler, SchedulerError};

/// Learn at most this many contact records in one background slice.
pub const CONTACTS_MAX_PAGES: u32 = 5;
/// Provider page hint for contact enumeration.
pub const CONTACTS_PAGE_SIZE: u32 = 100;

/// Synchronize one bounded slice of one contact source.
///
/// Each complete page commits its contacts and next checkpoint atomically. An
/// expired cursor restarts the source from no cursor once; the replay is
/// idempotent because contacts are keyed by `(source, remote contact, address)`.
pub async fn sync_slice(
    store: &Store,
    service: &mut dyn AddressBookService,
    source: &ContactSourceId,
    ctx: &OperationContext,
    budget: WorkBudget,
) -> Result<LearningProgress, ProviderError> {
    sync(store, service, source, ctx, budget, None).await
}

/// Paged form used by background maintenance: the account gate is taken and
/// released once per page, so an interactive mail/outbox operation can preempt
/// between contact pages instead of waiting for the whole slice.
pub async fn sync_slice_paged(
    store: &Store,
    service: &mut dyn AddressBookService,
    source: &ContactSourceId,
    ctx: &OperationContext,
    budget: WorkBudget,
    scheduler: &AccountScheduler,
) -> Result<LearningProgress, ProviderError> {
    sync(store, service, source, ctx, budget, Some(scheduler)).await
}

/// Durable progress for one contact source without opening a provider.
pub fn contact_progress(
    store: &Store,
    account: &crate::account::AccountId,
    source: &ContactSourceId,
) -> Result<LearningProgress, ProviderError> {
    let state = store
        .contact_source_state(account, source)
        .map_err(store_error)?;
    Ok(LearningProgress {
        scanned: state.scanned,
        total_hint: None,
        complete: state.complete && !state.snapshot_active,
        paused: !state.enabled || (state.snapshot_active && !state.complete),
    })
}

async fn sync(
    store: &Store,
    service: &mut dyn AddressBookService,
    source: &ContactSourceId,
    ctx: &OperationContext,
    budget: WorkBudget,
    scheduler: Option<&AccountScheduler>,
) -> Result<LearningProgress, ProviderError> {
    ctx.ensure_active()?;
    let start = store.contact_sync_start(ctx, source).map_err(store_error)?;
    if !start.enabled {
        return Ok(LearningProgress {
            scanned: start.scanned,
            total_hint: None,
            complete: start.complete,
            paused: true,
        });
    }

    let deadline = budget.deadline.min(ctx.deadline);
    let max_pages = budget.max_pages.max(1);
    let generation = start.generation;
    let mut cursor = start.cursor;
    let mut scanned = start.scanned;
    let mut pages = 0u32;
    let mut reset_used = false;
    let mut pending_reset = false;

    loop {
        if ctx.cancellation.is_cancelled() {
            return Err(ProviderError::cancelled("operation cancelled"));
        }
        if tokio::time::Instant::now() >= deadline || pages >= max_pages {
            break;
        }

        // A background slice takes the account gate for one page only. A busy
        // gate (for example an interactive send) ends this slice; the next
        // scheduled slice resumes from the durable cursor.
        let gate = match scheduler {
            Some(scheduler) => match scheduler.try_background(ctx) {
                Ok(Some(gate)) => Some(gate),
                Ok(None) => break,
                Err(SchedulerError::Cancelled) => {
                    return Err(ProviderError::cancelled("operation cancelled"));
                }
                Err(SchedulerError::Deadline) => break,
            },
            None => None,
        };

        // Re-read under the gate: a disable or a new generation observed while
        // the previous page was in flight fences this slice.
        let state = store
            .contact_source_state(&ctx.account, source)
            .map_err(store_error)?;
        if !state.enabled || state.snapshot_generation != generation {
            drop(gate);
            return contact_progress(store, &ctx.account, source);
        }

        let request_cursor = if pending_reset { None } else { cursor.clone() };
        let fetched = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => {
                Err(ProviderError::cancelled("operation cancelled"))
            }
            _ = tokio::time::sleep_until(deadline) => {
                Err(ProviderError::transient("contact slice deadline exceeded"))
            }
            page = service.next_page(source, request_cursor, ctx) => page,
        };

        let page = match fetched {
            Ok(page) => page,
            Err(error) => match error.kind {
                // A cursor from a different adapter version restarts this
                // source once. The durable checkpoint still fences the commit,
                // and contact upserts are idempotent.
                ErrorKind::CursorInvalid if !reset_used => {
                    reset_used = true;
                    pending_reset = true;
                    drop(gate);
                    tokio::task::yield_now().await;
                    continue;
                }
                // A permission denial disables only this source. Cached
                // contacts and every learned mail-derived suggestion stay.
                ErrorKind::Forbidden => {
                    drop(gate);
                    if let Err(disable) =
                        store.set_contact_source_enabled(&ctx.account, source, false)
                    {
                        log::warn!("disable forbidden contact source {source}: {disable}");
                    }
                    return Err(error);
                }
                ErrorKind::Cancelled => return Err(error),
                _ if tokio::time::Instant::now() >= deadline => {
                    drop(gate);
                    break;
                }
                _ => {
                    drop(gate);
                    // The checkpoint was never advanced for the failed page;
                    // retain the previous complete snapshot.
                    return Err(error);
                }
            },
        };

        match store.apply_contact_page(ctx, source, generation, cursor.as_ref(), &page) {
            Ok(outcome) => {
                pages += 1;
                pending_reset = false;
                scanned = scanned.saturating_add(page.changes.len() as u64);
                if outcome.complete {
                    drop(gate);
                    return Ok(LearningProgress {
                        scanned,
                        total_hint: None,
                        complete: true,
                        paused: false,
                    });
                }
                cursor = Some(page.checkpoint.clone());
            }
            // Another writer moved this source's generation or cursor while the
            // page was in flight. Report durable progress, never a partial write.
            Err(StoreError::StalePage | StoreError::ContactSourceDisabled) => {
                drop(gate);
                return contact_progress(store, &ctx.account, source);
            }
            Err(error) => return Err(store_error(error)),
        }

        drop(gate);
        tokio::task::yield_now().await;
    }

    Ok(LearningProgress {
        scanned,
        total_hint: None,
        complete: false,
        paused: true,
    })
}

fn store_error(error: StoreError) -> ProviderError {
    // A stale generation/checkpoint means another writer moved the source while
    // this slice was in flight. That is a retryable race, not a credential
    // problem: re-authenticating would be misleading and would not fix it.
    match error {
        StoreError::StaleGeneration { .. } | StoreError::StaleCheckpoint { .. } => {
            ProviderError::transient(format!("contact store: {error}"))
        }
        _ => ProviderError::transient(format!("contact store: {error}")),
    }
}
