//! Messages the shell and its screens exchange.
//!
//! `update` maps these to plain state changes or to API tasks; screens only
//! describe what happened, never how to reach the network.

use iced::Size;
use pige_core::account::presets::ProviderPreset;
use pige_core::account::{Account, AccountId, Security};
use pige_core::api::ModelSnapshot;
use pige_core::mail::{FolderId, MessageId, ThreadId};
use pige_core::search::{SearchHit, SearchScope};
use pige_core::send::OutboxId;
use pige_core::settings::ThemeMode;

use crate::screens::accounts::AccountField;
use crate::screens::compose::{ComposeField, ComposeSeed};

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
    /// The sync-interval field changed. Kept as text until save so partial
    /// input is not clamped mid-keystroke.
    SettingsSyncChanged(String),
    /// The notifications toggle changed.
    SettingsNotificationsToggled(bool),
    /// The theme choice changed.
    SettingsThemeChanged(ThemeMode),
    /// Persist the edited settings and reschedule background sync.
    SettingsSave,
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
    DismissError,
    Noop,
}
