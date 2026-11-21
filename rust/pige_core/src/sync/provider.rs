//! Transactional provider sync: pull change pages and commit each one with its
//! cursor atomically, under an account-generation and previous-checkpoint
//! compare-and-swap.
//!
//! The orchestrator is protocol-neutral: it drives a [`ProviderSession`]'s
//! [`SyncService`](crate::providers::SyncService) and never sees an IMAP UID, a
//! Gmail resource name, or a cursor payload. Each page's changes and its
//! checkpoint commit in one store transaction
//! ([`Store::apply_change_page`](crate::store::Store::apply_change_page)), and
//! only while the durable state is what the page was based on. That single
//! guarantee makes a crash before commit replay safely (the checkpoint did not
//! advance) and makes a page that arrives after a concurrent mutation
//! discardable instead of destructive (the checkpoint moved).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::account::AccountId;
use crate::mail::sender_display;
use crate::providers::{
    ChangePage, ErrorKind, JobKey, JobKind, MailChange, OperationContext, ProviderSession,
    SyncRequest,
};
use crate::store::{AccountLease, Store, StoreError};

use super::{NewMailSummary, SyncError, SyncReport};

/// Identifies this process's sync passes in the durable account lease. A
/// counter distinguishes overlapping passes in one process; the process id
/// distinguishes processes.
static LEASE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn lease_holder() -> String {
    format!(
        "sync-{}-{}",
        std::process::id(),
        LEASE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Grace period beyond the pass deadline so a slow final page cannot lose its
/// lease mid-commit. The lease is renewed before each page anyway.
const LEASE_GRACE: Duration = Duration::from_secs(30);

fn lease_ttl(budget: &WorkBudget) -> Duration {
    budget
        .deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .saturating_add(LEASE_GRACE)
        .max(LEASE_GRACE)
}

/// The durable job source for an account's whole-mailbox sync. Provider paging
/// is account-wide (one cursor covers every collection), so a single job key
/// names it. An on-demand `Other` folder is folded into the *cursor* by the
/// adapter (see `providers::imap::sync::request_mailbox`), never into a second
/// job.
pub const ACCOUNT_SYNC_SOURCE: &str = "account";

/// How much work one [`sync_account`] pass may do before yielding the account
/// gate. A bounded budget is what keeps background sync from occupying an
/// account indefinitely and blocking interactive work behind it.
#[derive(Debug, Clone, Copy)]
pub struct WorkBudget {
    /// Provider messages a bootstrap preface may import (the adapter's initial
    /// limit), also used as the per-request page hint.
    pub page_size: u32,
    /// Maximum change pages applied in this pass.
    pub max_pages: u32,
    /// Wall-clock instant the pass must stop by.
    pub deadline: tokio::time::Instant,
}

impl WorkBudget {
    pub fn new(page_size: u32, max_pages: u32, deadline: tokio::time::Instant) -> Self {
        Self {
            page_size,
            max_pages,
            deadline,
        }
    }

    /// A budget that stops `timeout` from now.
    pub fn with_timeout(page_size: u32, max_pages: u32, timeout: std::time::Duration) -> Self {
        Self::new(page_size, max_pages, tokio::time::Instant::now() + timeout)
    }
}

/// The durable job key for an account's whole-mailbox sync. Callers use it to
/// read the checkpoint (for example to fold an on-demand folder request into
/// the cursor).
pub fn account_job(account: &AccountId) -> JobKey {
    JobKey {
        account: account.clone(),
        kind: JobKind::MailSync,
        source: ACCOUNT_SYNC_SOURCE.to_string(),
    }
}

/// Synchronize one account's read path from a provider session.
///
/// The loop reads the durable cursor, asks the provider for the next page,
/// commits the page and its checkpoint in one fenced transaction, and yields
/// between pages. Notifications are produced only for adapter-confirmed new
/// arrivals, so a historic crawl/backfill or an initial import never notifies.
pub async fn sync_account(
    store: &Store,
    session: &mut ProviderSession,
    ctx: &OperationContext,
    budget: WorkBudget,
) -> Result<SyncReport, SyncError> {
    ctx.ensure_active().map_err(SyncError::Provider)?;
    // Never do network work for an account that was invalidated while this
    // pass was queued; the in-transaction check fences the actual writes.
    store.check_generation(&ctx.account, ctx.generation)?;

    // Android's WorkManager sync can run in a separate process from the UI, so
    // the in-process scheduler gate is not enough. Take the durable account
    // lease for the whole pass and hand it to every page commit; another
    // process holding it means this pass simply has no work to do.
    let lease = match store.acquire_lease(&ctx.account, &lease_holder(), lease_ttl(&budget)) {
        Ok(lease) => lease,
        Err(StoreError::LeaseHeld) => return Ok(SyncReport::default()),
        Err(error) => return Err(SyncError::Store(error)),
    };
    let result = sync_account_leased(store, session, ctx, budget, &lease).await;
    // The lease row stays behind with a past expiry, so the fencing token keeps
    // increasing across hand-offs even when a process dies mid-pass.
    let _ = store.release_lease(&lease);
    result
}

async fn sync_account_leased(
    store: &Store,
    session: &mut ProviderSession,
    ctx: &OperationContext,
    budget: WorkBudget,
    lease: &AccountLease,
) -> Result<SyncReport, SyncError> {
    let key = account_job(&ctx.account);
    let transport = session.provider.as_str().to_string();
    let mut report = SyncReport::default();
    let mut pages = 0u32;
    let mut reset_used = false;
    let mut pending_reset = false;

    loop {
        ctx.ensure_active().map_err(SyncError::Provider)?;
        if pages >= budget.max_pages.max(1) {
            break;
        }
        if tokio::time::Instant::now() >= budget.deadline {
            break;
        }

        // Re-read the durable cursor each iteration: it is both the cursor the
        // provider pages from and the expected-previous value of the commit, so
        // a concurrent writer is always observed.
        let stored = store.job_checkpoint(&key)?;
        let request_cursor = if pending_reset { None } else { stored.clone() };
        let request = SyncRequest {
            cursor: request_cursor,
            initial_limit: budget.page_size.max(1),
        };

        // The page may hit the network, so race it against cancellation and the
        // deadline: a stalled provider call must not outlive this pass.
        let page = match ctx
            .bound(session.sync_service()?.next_page(request, ctx))
            .await
        {
            Ok(page) => page,
            // A cursor the adapter cannot read restarts a scoped reconciliation
            // from no cursor; the same transaction will then overwrite the bad
            // checkpoint. Only recurse once, so a provider that always rejects
            // cannot loop forever.
            Err(error) if error.kind == ErrorKind::CursorInvalid && !reset_used => {
                reset_used = true;
                pending_reset = true;
                continue;
            }
            Err(error) if error.kind == ErrorKind::Cancelled => {
                return Err(SyncError::Provider(error));
            }
            Err(error) => {
                report.errors.push(error.safe_message);
                break;
            }
        };

        // A page may have taken a long time on the network. Renew before the
        // write so the lease is live throughout the commit. If the lease was
        // lost, still attempt the commit: its in-transaction generation check
        // reports the more specific reason (for example, the account was
        // removed) before its lease check rejects a genuine takeover.
        match store.renew_lease(lease, lease_ttl(&budget)) {
            Ok(_) | Err(StoreError::LeaseLost) => {}
            Err(error) => return Err(SyncError::Store(error)),
        }

        match store.apply_change_page(ctx, &key, &transport, stored.as_ref(), &page, Some(lease)) {
            Ok(()) => {
                pages += 1;
                pending_reset = false;
                collect_notifications(store, &transport, &key, &page, &mut report)?;
            }
            // A newer writer advanced the job while this page was in flight.
            // Discard it; the next pass resumes from the durable checkpoint.
            Err(StoreError::StalePage) => break,
            Err(error) => return Err(SyncError::Store(error)),
        }

        if !page.has_more {
            break;
        }
        // Let other account work run between pages. No lock is held here.
        tokio::task::yield_now().await;
    }

    Ok(report)
}

/// Turn an applied page's confirmed arrivals into notification summaries,
/// resolving each remote message to its cached content and thread. Historic
/// crawl/backfill and initial imports carry `is_new_arrival == false` and are
/// skipped.
fn collect_notifications(
    store: &Store,
    transport: &str,
    key: &JobKey,
    page: &ChangePage,
    report: &mut SyncReport,
) -> Result<(), StoreError> {
    for change in &page.changes {
        let MailChange::UpsertMessage {
            message,
            is_new_arrival: true,
        } = change
        else {
            continue;
        };
        let Some(id) = store.remote_message_for(&key.account, transport, &message.remote_id)?
        else {
            continue;
        };
        let Some(stored) = store.message(&id)? else {
            continue;
        };
        let folder_id = store
            .message_memberships(&id)?
            .into_iter()
            .next()
            .unwrap_or_default();
        report.new_inbox.push(NewMailSummary {
            account_id: key.account.clone(),
            message_id: id.clone(),
            folder_id,
            thread_id: stored.thread_id,
            from: sender_display(&stored.header.from),
            subject: stored.header.subject,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_job_is_stable_per_account() {
        let account = AccountId("acc".to_string());
        let key = account_job(&account);
        assert_eq!(key.account, account);
        assert_eq!(key.kind, JobKind::MailSync);
        assert_eq!(key.source, ACCOUNT_SYNC_SOURCE);
    }
}
