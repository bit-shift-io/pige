//! Core events pushed to the UI and platform adapters. The channel is a
//! process-wide broadcast: subscribers that fall behind skip old events rather
//! than blocking sync.

use std::sync::OnceLock;

use tokio::sync::broadcast;

use crate::account::AccountId;
use crate::sync::NewMailSummary;

/// Capacity covers a first-sync burst (the initial window is 300 messages per
/// folder) plus progress events. A slow UI drops stale events and re-reads the
/// latest snapshot, which is always authoritative.
const EVENT_CAPACITY: usize = 4096;

/// Something the UI or platform layer should react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    /// The model changed; call `api::snapshot()` for the new state.
    SnapshotChanged,
    /// [`crate::api::search_mail`] replaced the snapshot's search results.
    SearchChanged,
    SyncStarted {
        account: AccountId,
    },
    SyncFinished {
        account: AccountId,
        new_mail: usize,
    },
    NewMail(NewMailSummary),
    /// The platform's Back gesture/button asked the UI to move back.
    BackPressed,
    /// A recoverable failure, already formatted for display.
    Error(String),
}

static SENDER: OnceLock<broadcast::Sender<CoreEvent>> = OnceLock::new();

/// The process-wide event sender, created on first use so [`subscribe`] works
/// even if called before [`crate::api::init`].
pub(crate) fn sender() -> &'static broadcast::Sender<CoreEvent> {
    SENDER.get_or_init(|| broadcast::channel(EVENT_CAPACITY).0)
}

/// Subscribe to core events. Multiple UI/platform subscribers are supported.
pub fn subscribe() -> broadcast::Receiver<CoreEvent> {
    sender().subscribe()
}

/// Ask the running UI to handle Back. Best-effort: with no subscriber (for
/// example a WorkManager process with no window) the event is dropped.
pub fn back_pressed() {
    let _ = sender().send(CoreEvent::BackPressed);
}
