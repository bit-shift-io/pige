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
    // Opening may connect and authenticate, so race it against the deadline
    // and cancellation; a stalled server must not hang the caller.
    ctx.bound(providers.open(account, ctx))
        .await
        .map_err(|_| RECONNECT_ERROR.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::account::{Account, AccountId, MailBackend};
    use crate::providers::fake::FakeProviderFactory;

    fn account() -> Account {
        Account {
            id: AccountId("acc-open-deadline".to_string()),
            label: "Proton".to_string(),
            email: "person@proton.example".to_string(),
            display_name: "Person".to_string(),
            backend: MailBackend::proton(None),
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        }
    }

    /// Opening a provider session is network work too: a stalled connect/login
    /// must not outlive the operation deadline.
    #[tokio::test]
    async fn hanging_session_open_is_bounded() {
        let factory: Arc<dyn ProviderFactory> =
            Arc::new(FakeProviderFactory::new().open_delay(Duration::from_secs(5)));
        let account = account();
        let ctx = OperationContext::with_timeout(account.id.clone(), 1, Duration::from_millis(50));

        let started = Instant::now();
        let result = open_session(&factory, &account, &ctx).await;
        assert_eq!(result.unwrap_err(), RECONNECT_ERROR);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a hanging session open must be bounded by the operation deadline"
        );
    }
}
