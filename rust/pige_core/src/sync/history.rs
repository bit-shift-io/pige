//! Bounded, restart-safe header-history learning.
//!
//! The crawl is driven by [`MailboxService::history`](crate::providers::MailboxService::history)
//! and never touches the visible mailbox cache: each complete page's headers
//! become replay-safe correspondent evidence, and the page's checkpoint commits
//! atomically with it. The next slice resumes from that durable checkpoint, so
//! a crash, a restart, or an interactive operation between pages loses at most
//! the page that was in flight.
//!
//! Two budgets keep background work polite:
//!
//! - one slice fetches at most [`HISTORY_MAX_PAGES`] pages or runs until
//!   [`HISTORY_SLICE_TIMEOUT`], whichever comes first;
//! - the paged entry point releases the account gate between pages and only
//!   admits [`HISTORY_MAX_CONCURRENT_JOBS`] account jobs at once.
//!
//! A rate-limit response stores its retry time durably before the error is
//! returned. The checkpoint is *not* advanced on a failed page, and a restart
//! observes the same delay instead of hammering the server again.

use std::time::Duration;

use crate::account::Account;
use crate::providers::{
    ErrorKind, HistoryRequest, MailboxService, OperationContext, ProviderError, ProviderFactory,
};
use crate::store::{HistoryProgress, Store, StoreError, history_job};

use super::provider::WorkBudget;
use super::scheduler::{AccountScheduler, SchedulerError};

/// Header records requested per history page.
pub const HISTORY_PAGE_SIZE: u32 = 100;
/// Maximum pages one background slice may fetch.
pub const HISTORY_MAX_PAGES: u32 = 5;
/// Wall-clock budget for one background slice.
pub const HISTORY_SLICE_TIMEOUT: Duration = Duration::from_secs(5);
/// Wall-clock budget for one user-initiated foreground slice.
pub const HISTORY_FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
/// Account jobs that may learn history concurrently in this process.
pub const HISTORY_MAX_CONCURRENT_JOBS: usize = 2;
/// A rate-limit response without an explicit delay is retried after this long.
const HISTORY_DEFAULT_RETRY: Duration = Duration::from_secs(30);

/// Test-only/foreground budget that may run more pages when the user explicitly
/// asks for learning, while still yielding regularly.
pub const HISTORY_FOREGROUND_PAGES: u32 = 25;

static HISTORY_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(HISTORY_MAX_CONCURRENT_JOBS);

/// Restart-safe progress of local learning work for one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LearningProgress {
    /// Cumulative messages the adapter has scanned for this crawl.
    pub scanned: u64,
    /// Adapter's estimate of the accessible total, when it reports one.
    pub total_hint: Option<u64>,
    /// The adapter reached end-of-enumeration; history is fully learned.
    pub complete: bool,
    /// More history remains and this slice stopped before reaching it.
    pub paused: bool,
}

/// The default budget for one background history slice.
pub fn background_budget() -> WorkBudget {
    WorkBudget::with_timeout(HISTORY_PAGE_SIZE, HISTORY_MAX_PAGES, HISTORY_SLICE_TIMEOUT)
}

/// A larger budget for a user-initiated foreground slice. It still yields
/// regularly and still honours the operation context's deadline.
pub fn foreground_budget() -> WorkBudget {
    WorkBudget::with_timeout(
        HISTORY_PAGE_SIZE,
        HISTORY_FOREGROUND_PAGES,
        HISTORY_FOREGROUND_TIMEOUT,
    )
}

/// Learn one bounded slice of header history from `mailbox`.
///
/// Each complete page commits its observations and next checkpoint atomically
/// through [`Store::apply_history_page`](crate::store::Store::apply_history_page).
/// A failed or partial page never advances the cursor. An expired cursor
/// restarts the adapter from no cursor exactly once; the replay is idempotent
/// because evidence is keyed by native identity and a normalized header
/// fingerprint. New-mail notifications are never produced here.
pub async fn learn_slice(
    store: &Store,
    mailbox: &mut dyn MailboxService,
    ctx: &OperationContext,
    budget: WorkBudget,
) -> Result<LearningProgress, ProviderError> {
    learn(store, mailbox, ctx, budget, None).await
}

/// Learn one bounded slice while taking and releasing the account gate once per
/// page. An interactive operation that queues on the gate is handed the lock at
/// the page boundary, so a send (or a mutation) preempts the crawl instead of
/// waiting for the whole slice.
pub async fn learn_slice_paged(
    store: &Store,
    mailbox: &mut dyn MailboxService,
    ctx: &OperationContext,
    budget: WorkBudget,
    scheduler: &AccountScheduler,
) -> Result<LearningProgress, ProviderError> {
    learn(store, mailbox, ctx, budget, Some(scheduler)).await
}

/// Learn one background slice with the process-wide concurrent-job limit.
///
/// This is the entry point the sync worker calls after it has an open mailbox.
/// A slice that cannot get a slot returns its durable progress without opening
/// or driving any provider work.
pub async fn learn_background_slice(
    store: &Store,
    mailbox: &mut dyn MailboxService,
    ctx: &OperationContext,
    budget: WorkBudget,
    scheduler: &AccountScheduler,
) -> Result<LearningProgress, ProviderError> {
    let Some(_slot) = try_background_slot() else {
        return learning_progress(store, &ctx.account);
    };
    learn_slice_paged(store, mailbox, ctx, budget, scheduler).await
}

fn try_background_slot() -> Option<tokio::sync::SemaphorePermit<'static>> {
    HISTORY_SLOTS.try_acquire().ok()
}

/// Open a provider session and learn one background slice for `account`.
///
/// At most [`HISTORY_MAX_CONCURRENT_JOBS`] account jobs run at once; a job that
/// cannot get a slot reports its durable progress and is retried later.
pub async fn learn_account_slice(
    store: &Store,
    providers: &std::sync::Arc<dyn ProviderFactory>,
    account: &Account,
    ctx: &OperationContext,
    budget: WorkBudget,
    scheduler: &AccountScheduler,
) -> Result<LearningProgress, ProviderError> {
    let Some(_slot) = try_background_slot() else {
        return learning_progress(store, &ctx.account);
    };
    let mut session = providers.open(account, ctx).await?;
    let mailbox = session.mailbox_service()?;
    learn_slice_paged(store, mailbox.as_mut(), ctx, budget, scheduler).await
}

/// One account's durable learning progress without network access.
pub fn learning_progress(
    store: &Store,
    account: &crate::account::AccountId,
) -> Result<LearningProgress, ProviderError> {
    let state = store.history_state(account).map_err(store_error)?;
    Ok(LearningProgress {
        scanned: state.progress.scanned,
        total_hint: state.progress.total_hint,
        complete: state.progress.complete,
        // `paused` is the durable "stopped with more remaining" flag, not just
        // the complement of `complete`: an in-flight/not-yet-started crawl is
        // not paused, and a completed crawl is neither paused nor incomplete.
        paused: state.progress.paused && !state.progress.complete,
    })
}

async fn learn(
    store: &Store,
    mailbox: &mut dyn MailboxService,
    ctx: &OperationContext,
    budget: WorkBudget,
    scheduler: Option<&AccountScheduler>,
) -> Result<LearningProgress, ProviderError> {
    let key = history_job(&ctx.account);
    let state = store.history_state(&ctx.account).map_err(store_error)?;
    if state.progress.complete {
        return Ok(LearningProgress {
            scanned: state.progress.scanned,
            total_hint: state.progress.total_hint,
            complete: true,
            paused: false,
        });
    }
    // A durable rate-limit delay survives a restart. Never issue a request
    // before it expires; the next scheduled slice will pick the crawl up.
    if state.retry_after_ms > now_ms() {
        return Ok(LearningProgress {
            scanned: state.progress.scanned,
            total_hint: state.progress.total_hint,
            complete: false,
            paused: true,
        });
    }
    if state.retry_after_ms != 0 {
        store.clear_history_retry_after(ctx).map_err(store_error)?;
    }

    // Mark the crawl active so a concurrent `learning_progress` read can tell
    // an in-flight slice from one that stopped with work remaining. The store
    // never lets this write undo a concurrent completion.
    store
        .record_history_progress(
            ctx,
            HistoryProgress {
                scanned: state.progress.scanned,
                total_hint: state.progress.total_hint,
                complete: false,
                paused: false,
            },
        )
        .map_err(store_error)?;

    let deadline = budget.deadline.min(ctx.deadline);
    let page_size = budget.page_size.max(1);
    let max_pages = budget.max_pages.max(1);
    let mut pages = 0u32;
    let mut reset_used = false;
    let mut pending_reset = false;
    let mut scanned = state.progress.scanned;
    let mut total_hint = state.progress.total_hint;

    loop {
        if ctx.cancellation.is_cancelled() {
            return Err(ProviderError::cancelled("operation cancelled"));
        }
        if tokio::time::Instant::now() >= deadline || pages >= max_pages {
            break;
        }

        // A background slice takes the account gate for one page only. The
        // fair mutex means a queued interactive operation wins the hand-off.
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

        // Re-read the cursor under the gate: it is both the request cursor and
        // the expected-previous value of this page's compare-and-set.
        let stored = store.job_checkpoint(&key).map_err(store_error)?;
        let request = HistoryRequest {
            cursor: if pending_reset { None } else { stored.clone() },
            page_size,
        };

        let fetched = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => {
                Err(ProviderError::cancelled("operation cancelled"))
            }
            _ = tokio::time::sleep_until(deadline) => {
                Err(ProviderError::transient("history slice deadline exceeded"))
            }
            page = mailbox.history(request, ctx) => page,
        };

        let page = match fetched {
            Ok(page) => page,
            Err(error) => match error.kind {
                // The adapter no longer understands the stored cursor. Restart
                // from no cursor once; the durable checkpoint still fences the
                // replay, and evidence dedup makes the repeated page harmless.
                ErrorKind::CursorInvalid if !reset_used => {
                    reset_used = true;
                    pending_reset = true;
                    drop(gate);
                    tokio::task::yield_now().await;
                    continue;
                }
                ErrorKind::RateLimited => {
                    let delay = error.retry_after.unwrap_or(HISTORY_DEFAULT_RETRY);
                    let until = now_ms().saturating_add(duration_ms(delay));
                    store
                        .record_history_retry_after(ctx, until)
                        .map_err(store_error)?;
                    let _ = store.record_history_progress(
                        ctx,
                        HistoryProgress {
                            scanned,
                            total_hint,
                            complete: false,
                            paused: true,
                        },
                    );
                    // The checkpoint was never advanced for the failed page.
                    return Err(error);
                }
                // A local deadline cancels the pending request and leaves the
                // durable cursor exactly where the last complete page put it.
                _ if tokio::time::Instant::now() >= deadline => {
                    drop(gate);
                    break;
                }
                _ => {
                    // Any other failure also leaves the cursor at the last
                    // complete page. Record that more work remains so the UI
                    // does not imply a finished crawl.
                    let _ = store.record_history_progress(
                        ctx,
                        HistoryProgress {
                            scanned,
                            total_hint,
                            complete: false,
                            paused: true,
                        },
                    );
                    return Err(error);
                }
            },
        };

        match store.apply_history_page(ctx, stored.as_ref(), &page) {
            Ok(()) => {}
            // Another writer advanced the history checkpoint while this page
            // was in flight. Discard the page; the next slice reconciles from
            // whatever is durable now.
            Err(StoreError::StalePage) => {
                drop(gate);
                break;
            }
            Err(error) => return Err(store_error(error)),
        }

        pages += 1;
        pending_reset = false;
        // A cursor reset (or adapter rebaseline) may restart the provider's
        // own counter at zero. Progress is cumulative for the UI and must never
        // move backwards, so keep the larger observation.
        let observed = if page.coverage.scanned > 0 {
            page.coverage.scanned
        } else {
            page.messages.len() as u64
        };
        scanned = scanned.max(observed);
        total_hint = page.coverage.total_hint.or(total_hint);

        // End-of-enumeration is the adapter's statement, never a page-size or
        // running-total heuristic.
        let complete = !page.has_more;
        let progress = LearningProgress {
            scanned,
            total_hint,
            complete,
            paused: !complete,
        };
        store
            .record_history_progress(
                ctx,
                HistoryProgress {
                    scanned,
                    total_hint,
                    complete,
                    paused: !complete,
                },
            )
            .map_err(store_error)?;

        // Release the page's gate before doing anything else so an interactive
        // operation can run between pages.
        drop(gate);
        if complete {
            return Ok(progress);
        }
        tokio::task::yield_now().await;
    }

    // Budget exhausted, the account gate was busy, or the deadline cancelled a
    // pending page. Persist the "more remains" state so the UI can report it
    // without opening a provider.
    let progress = LearningProgress {
        scanned,
        total_hint,
        complete: false,
        paused: true,
    };
    store
        .record_history_progress(
            ctx,
            HistoryProgress {
                scanned,
                total_hint,
                complete: false,
                paused: true,
            },
        )
        .map_err(store_error)?;
    Ok(progress)
}

fn store_error(error: StoreError) -> ProviderError {
    match error {
        StoreError::StaleGeneration { .. } | StoreError::StaleCheckpoint { .. } => {
            ProviderError::reauthenticate(format!("history store: {error}"))
        }
        _ => ProviderError::transient(format!("history store: {error}")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn duration_ms(duration: Duration) -> i64 {
    duration.as_millis().min(i64::MAX as u128) as i64
}
