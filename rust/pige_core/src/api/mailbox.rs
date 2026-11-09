//! Provider-session orchestration shared by the API's mailbox read and sync
//! paths.
//!
//! The API never talks to a protocol transport directly for mailbox work: it
//! opens a [`ProviderSession`] through the injected [`ProviderFactory`] and
//! drives a capability service. Opening a session is where a connection refusal
//! becomes the user-facing [`RECONNECT_ERROR`], so every caller reports an
//! unreachable account the same way.

use std::sync::Arc;
use std::time::Duration;

use crate::account::Account;
use crate::providers::{OperationContext, ProviderError, ProviderFactory, ProviderSession};
use crate::sync::scheduler::SchedulerError;

use super::RECONNECT_ERROR;

/// Timeout for one interactive account operation (a body/attachment read or a
/// mutation). Bounds how long the account gate may be held.
pub(crate) const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for one account sync pass.
pub(crate) const SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// Map a scheduler wait failure to the user-facing offline error: a busy gate
/// that could not be entered before the deadline is indistinguishable to the
/// user from an unreachable server.
pub(crate) fn scheduler_message(error: SchedulerError) -> String {
    match error {
        SchedulerError::Cancelled | SchedulerError::Deadline => RECONNECT_ERROR.to_string(),
    }
}

/// A provider error's safe, user-facing message. Never the wire detail.
pub(crate) fn provider_message(error: ProviderError) -> String {
    error.safe_message
}

/// Open a provider session for `account`, mapping any open failure (connection
/// refused, no registered backend) to the reconnect error so a mutation or read
/// can report it uniformly. The caller owns the context's deadline.
pub(crate) async fn open_session(
    providers: &Arc<dyn ProviderFactory>,
    account: &Account,
    ctx: &OperationContext,
) -> Result<ProviderSession, String> {
    providers
        .open(account, ctx)
        .await
        .map_err(|_| RECONNECT_ERROR.to_string())
}
