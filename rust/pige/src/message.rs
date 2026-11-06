//! Messages the shell and its screens exchange.
//!
//! `update` maps these to plain state changes or to API tasks; screens only
//! describe what happened, never how to reach the network.

use iced::Size;
use pige_core::account::presets::ProviderPreset;
use pige_core::account::{Account, AccountId, Security};
use pige_core::api::{ConfigOutcome, ModelSnapshot};
use pige_core::mail::{FolderId, MessageId, ThreadId};
use pige_core::search::{SearchHit, SearchScope};
use pige_core::send::OutboxId;
use pige_core::settings::{Settings, ThemeMode};

use crate::screens::accounts::AccountField;
use crate::screens::advanced::Row as AdvancedRow;
use crate::screens::compose::{ComposeField, ComposeSeed};

/// The successful result of saving an account. `warning` carries the
/// session-only credential notice; it is shown in the
/// shell toast after the account screen closes.
#[derive(Debug, Clone)]
pub struct AccountSaveResult {
    pub snapshot: ModelSnapshot,
    pub warning: Option<String>,
    pub account_id: AccountId,
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
    Spin,
    /// A fresh view of the cache.
    Snapshot(ModelSnapshot),
    Resized(Size),
    /// Android status/navigation bar and soft-keyboard insets. Desktop never
    /// emits this; the app pads its root by them so content stays inside the
    /// system bars.
    Insets(iced_android::Insets),
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
    /// A user-facing sync finished; successful syncs schedule quiet prefetch.
    Synced(Result<ModelSnapshot, String>),
    AccountSynced(AccountId, Result<ModelSnapshot, String>),
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
    /// The reader's earlier-message section was toggled.
    ToggleEarlier,
    /// The reader toolbar: return to the list (deselect the conversation).
    ConversationBack,
    /// The Android Back button/gesture. Closes the innermost surface.
    Back,
    /// The reader toolbar: archive the open conversation.
    ArchiveConversation,
    /// The reader toolbar: move the open conversation to Trash.
    TrashConversation,
    /// The reader toolbar: mark the open conversation unread.
    MarkUnread,
    /// The reader toolbar: open the "Move to folder" picker.
    OpenMovePicker,
    /// The reader toolbar: dismiss the "Move to folder" picker.
    CloseMovePicker,
    /// The reader toolbar: move the conversation to a chosen folder.
    MoveConversation(FolderId),
    /// The reader toolbar: open the permanent-delete confirmation dialog.
    OpenDeleteConfirm,
    /// The reader toolbar: cancel the permanent-delete confirmation.
    CancelDeleteConfirm,
    /// The reader toolbar: confirm the permanent delete.
    ConfirmDelete,
    /// Open a link from a message body.
    OpenUrl(String),
    /// Download (if needed) and open one attachment from the reader.
    OpenAttachment(MessageId, String),
    Compose,
    /// Start a reply or forward from the open conversation.
    Reply,
    Forward,
    /// A reply/forward prefill or a saved draft arrived from the API.
    ComposeLoaded(Result<ComposeSeed, String>),
    /// A draft row was chosen from the Drafts folder.
    OpenDraft(OutboxId),
    ComposeField(ComposeField, String),
    ComposeBody(iced::widget::text_editor::Action),
    ComposeSend,
    ComposeSent(Result<pige_core::api::SendOutcome, String>),
    ComposeSaveDraft,
    ComposeSaved(Result<OutboxId, String>),
    ComposeDiscard,
    ComposeDiscarded(Result<ModelSnapshot, String>),
    ComposeClose,
    AddAccount,
    /// The full-window settings screen was requested.
    Settings,
    /// Close the settings screen and return to mail.
    CloseSettings,
    /// Open the Advanced screen from Settings.
    OpenAdvanced,
    /// Close the Advanced screen and return to Settings.
    CloseAdvanced,
    /// An Advanced-screen action row was pressed.
    Advanced(AdvancedRow),
    /// A backup finished; the name is the file written.
    BackupDone(Result<String, String>),
    /// The backup list arrived.
    BackupsListed(Result<Vec<String>, String>),
    /// Whether the app may read backups from a previous install (Android).
    RestoreAccessChecked(bool),
    /// The system "All files access" screen was opened.
    RestoreAccessRequested(Result<(), String>),
    /// A backup was chosen from the restore picker; ask to confirm.
    RestoreChosen(String),
    /// The restore was confirmed and should run.
    RestoreConfirmed,
    /// The restore picker or its confirmation was dismissed.
    RestoreCancelled,
    /// A restore finished, with the file name it used.
    RestoreDone(String, Result<ConfigOutcome, String>),
    /// The factory-reset confirmation was accepted.
    ResetConfirmed,
    /// The factory-reset confirmation was dismissed.
    ResetCancelled,
    /// A factory reset finished.
    ResetDone(Result<ConfigOutcome, String>),
    /// Persisted settings arrived from the store (startup and screen open).
    SettingsLoaded(Settings),
    /// The sync-interval field changed. Kept as text until save so partial
    /// input is not clamped mid-keystroke.
    SettingsSyncChanged(String),
    /// The notifications toggle changed.
    SettingsNotificationsToggled(bool),
    /// The background-sync toggle changed.
    SettingsBackgroundSyncToggled(bool),
    SettingsPrefetchToggled(bool),
    /// The theme choice changed.
    SettingsThemeChanged(ThemeMode),
    /// Persist the edited settings and reschedule background sync.
    SettingsSave,
    /// The save finished, off the UI thread.
    SettingsSaved(Result<(), String>),
    /// The operating system reported its light/dark preference.
    SystemTheme(iced::theme::Mode),
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
    /// A brief result message for the shared toast.
    Toast(String),
    /// The 4-second expiry for the toast with this generation; a stale value
    /// must not dismiss a newer message.
    ToastExpired(u64),
    Noop,
}
