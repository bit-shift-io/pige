//! Messages the shell and its screens exchange.
//!
//! `update` maps these to plain state changes or to API tasks; screens only
//! describe what happened, never how to reach the network.

use iced::Size;
use pige_core::account::presets::ProviderPreset;
use pige_core::account::{Account, AccountId, Security};
use pige_core::api::ModelSnapshot;
use pige_core::mail::{FolderId, ThreadId};
use pige_core::search::{SearchHit, SearchScope};

use crate::screens::accounts::AccountField;

/// The successful result of saving an account. `warning` carries the
/// session-only credential notice or a failed first sync; it is shown in the
/// shell snackbar after the account screen closes.
#[derive(Debug, Clone)]
pub struct AccountSaveResult {
    pub snapshot: ModelSnapshot,
    pub warning: Option<String>,
}

/// A password in flight through the update loop. Its `Debug` implementation
/// never prints the secret, so logging a message cannot leak it.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Password(<redacted>)")
    }
}

#[derive(Debug, Clone)]
// A snapshot is deliberately passed by value: it is the authoritative view
// model, and boxing it would only move the allocation to every match arm.
#[allow(clippy::large_enum_variant)]
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
    /// The search field changed.
    SearchChanged(String),
    /// The user cycled the search scope chip.
    SearchScopeChanged(SearchScope),
    /// A background search finished. The query is echoed so a slow response
    /// for an older keystroke cannot replace newer results.
    SearchResults {
        query: String,
        scope: SearchScope,
        result: Result<Vec<SearchHit>, String>,
    },
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
    /// The account screen is open.
    OpenAccounts,
    CloseAccounts,
    AccountNew,
    AccountEdit(AccountId),
    AccountLoaded(Result<Account, String>),
    AccountCancel,
    AccountFieldChanged(AccountField, String),
    AccountPasswordChanged(Password),
    AccountPresetChanged(ProviderPreset),
    AccountImapSecurityChanged(Security),
    AccountSmtpSecurityChanged(Security),
    AccountTest,
    AccountTested(Result<(), String>),
    AccountSave,
    AccountSaved(Result<AccountSaveResult, String>),
    AccountRemove(AccountId),
    AccountRemoveConfirmed(AccountId),
    AccountRemoveCancelled,
    AccountRemoved(Result<ModelSnapshot, String>),
    AccountLabelDraft(AccountId, String),
    AccountLabelCommit(AccountId),
    AccountLabelRenamed(Result<ModelSnapshot, String>),
    Error(String),
    DismissError,
    Noop,
}
