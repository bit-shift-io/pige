//! In-process serialization of per-account work.
//!
//! A sync pass and a mutation must not interleave their remote read/apply
//! sequences: a mutation that lands between a page's read and its commit would
//! otherwise be observed by the page, and an old page applied after a mutation
//! could roll its server changes back. [`AccountScheduler`] gives each account
//! one async gate held for the *whole* operation (network read through durable
//! apply), while the durable half of the fence — the account generation, lease,
//! and checkpoint compare-and-swap — is enforced in the store.
//!
//! The gate is a [`tokio::sync::Mutex`], never the store's synchronous SQLite
//! mutex, so no operation holds a connection lock across an await. Interactive
//! waits are bounded by the operation context's cancellation and deadline;
//! background work never waits for a busy gate, so it cannot queue up behind
//! itself and delay the UI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::account::AccountId;
use crate::providers::OperationContext;

/// Why an operation could not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SchedulerError {
    /// The operation was cancelled while waiting for the account gate.
    #[error("cancelled while waiting for the account gate")]
    Cancelled,
    /// The operation's deadline passed before it could acquire the gate.
    #[error("the account gate was not acquired before the deadline")]
    Deadline,
}

/// Per-account gates that serialize sync and mutation work.
#[derive(Default)]
pub struct AccountScheduler {
    gates: Mutex<HashMap<AccountId, Arc<AsyncMutex<()>>>>,
}

impl AccountScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The gate for `account`, created on first use. The map lock is held only
    /// for the lookup; it is never held across an await.
    fn gate(&self, account: &AccountId) -> Arc<AsyncMutex<()>> {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gates
            .entry(account.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Run `op` while holding the account's gate, waiting for it in a way
    /// bounded by `ctx.cancellation` and `ctx.deadline`. The gate is held for
    /// the whole future, including any network awaits inside it.
    pub async fn run_interactive<T, F>(
        &self,
        ctx: &OperationContext,
        op: F,
    ) -> Result<T, SchedulerError>
    where
        F: std::future::Future<Output = T>,
    {
        let gate = self.gate(&ctx.account);
        let _guard = tokio::select! {
            guard = gate.lock() => guard,
            _ = ctx.cancellation.cancelled() => return Err(SchedulerError::Cancelled),
            _ = tokio::time::sleep_until(ctx.deadline) => return Err(SchedulerError::Deadline),
        };
        Ok(op.await)
    }

    /// Run `op` while holding the account's gate, but never wait for a busy
    /// gate: a background pass returns `Ok(None)` and is retried on the next
    /// schedule instead of queueing behind whatever holds the account. `op` is
    /// responsible for honouring its own deadline so it does not hold the gate
    /// indefinitely.
    pub async fn run_background<T, F>(
        &self,
        ctx: &OperationContext,
        op: F,
    ) -> Result<Option<T>, SchedulerError>
    where
        F: std::future::Future<Output = T>,
    {
        let gate = self.gate(&ctx.account);
        let _guard = match gate.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(None),
        };
        if ctx.cancellation.is_cancelled() {
            return Err(SchedulerError::Cancelled);
        }
        Ok(Some(op.await))
    }
}
