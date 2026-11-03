//! Cloneable view models. Snapshots contain only what the current view needs,
//! with ordering and unread rollups already resolved by the core.

use crate::account::AccountId;
use crate::html::MailDocument;
use crate::mail::{Flags, FolderId, FolderKind, MessageId, ThreadId};

/// One account row in the navigation rail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    pub id: AccountId,
    pub label: String,
    pub email: String,
    /// Every unread message cached for this account (all folders).
    pub unread: u32,
}

/// One account's expandable folder group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSection {
    pub account_id: AccountId,
    pub label: String,
    pub email: String,
    /// Sum of the section's folder unread counts.
    pub unread: u32,
    pub folders: Vec<FolderSummary>,
}

/// A folder row inside an account section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSummary {
    pub id: FolderId,
    pub name: String,
    pub kind: FolderKind,
    pub unread: u32,
}

/// One conversation row in the message list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummary {
    pub thread_id: ThreadId,
    pub account_id: AccountId,
    pub subject: String,
    pub sender: String,
    pub snippet: String,
    pub date: i64,
    pub unread: bool,
    pub message_count: u32,
    pub has_attachments: bool,
}

/// The reader pane for one conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationDetail {
    pub thread_id: ThreadId,
    pub subject: String,
    pub messages: Vec<MessageSummary>,
}

/// The body of a message, or why it is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageBody {
    /// The parsed document, ready to render.
    Document(MailDocument),
    /// The raw message exceeds the fetch cap and was not downloaded.
    TooLarge,
}

/// One message inside a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: MessageId,
    pub sender: String,
    pub to: String,
    pub date: i64,
    pub snippet: String,
    pub flags: Flags,
    pub has_attachments: bool,
    /// The body for the newest message; earlier messages carry `None` so a
    /// conversation's ASTs are not duplicated across every row.
    pub body: Option<MessageBody>,
}

/// Background sync state exposed to the UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SyncStatus {
    #[default]
    Idle,
    Syncing,
    Error(String),
}

/// Everything the UI needs to render the current view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSnapshot {
    pub accounts: Vec<AccountSummary>,
    pub sections: Vec<AccountSection>,
    /// Unread across every account's Inbox folders; rendered above sections.
    pub unified_inbox_unread: u32,
    /// Whether the unified inbox, rather than one folder, is selected.
    pub unified_selected: bool,
    pub selected_folder: Option<FolderId>,
    pub conversations: Vec<ConversationSummary>,
    pub selected_conversation: Option<ConversationDetail>,
    pub status: SyncStatus,
    /// Whether more cached conversations exist below the current page.
    pub has_more: bool,
}
