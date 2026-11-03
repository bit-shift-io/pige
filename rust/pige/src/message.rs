//! Messages the shell and its screens exchange.
//!
//! `update` maps these to plain state changes or to API tasks; screens only
//! describe what happened, never how to reach the network.

use iced::Size;
use pige_core::account::AccountId;
use pige_core::api::ModelSnapshot;
use pige_core::mail::{FolderId, ThreadId};

#[derive(Debug, Clone)]
pub enum Message {
    /// A fresh view of the cache.
    Snapshot(ModelSnapshot),
    Resized(Size),
    /// The periodic UI tick that keeps relative dates fresh.
    Tick,
    ToggleDrawer,
    CloseDrawer,
    /// The pinned unified-inbox shortcut.
    SelectAllInboxes,
    SelectFolder(FolderId),
    /// Expand or collapse one account's folder group.
    ToggleAccount(AccountId),
    LoadOlder,
    SyncNow,
    /// Pull-to-refresh from the list.
    Refresh,
    Search(String),
    /// A conversation was chosen; the reader lands in a later task.
    OpenConversation(ThreadId),
    /// The reader's "Load images" bar was pressed: re-parse the body with
    /// remote images allowed.
    LoadImages,
    /// The reader's earlier-message section was toggled.
    ToggleEarlier,
    /// Open a link from a message body.
    OpenUrl(String),
    Compose,
    AddAccount,
    Settings,
    Error(String),
    DismissError,
    Noop,
}
