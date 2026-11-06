//! The mutation surface: read state, move, archive, trash, and permanent
//! delete, with Gmail label semantics and current-folder-only scope.
//!
//! Every mutation resolves the conversation's messages in the *currently
//! viewed* folder only (never the Sent copy), applies the local change
//! optimistically, executes the transport call, and rolls the local change
//! back if the server call fails. Locks are never held across a network await.

use crate::account::{Account, AccountId};
use crate::mail::{Flags, Folder, FolderId, FolderKind, MessageId, StoredMessage, ThreadId, Uid};
use crate::transport::{Capabilities, FlagOp, MailTransport, RemoteFolder, TransportError};

use super::{AppState, Config, CoreEvent, ModelSnapshot, find_account, lock_state, snapshot};

/// The user-facing error returned when a mutation needs the server but the
/// account is unreachable. Reading, search, and sending still work offline.
pub const RECONNECT_ERROR: &str = "Reconnect to move, archive, or delete";

/// The error returned when a permanent delete is attempted without going
/// through the confirmation dialog.
const CONFIRM_REQUIRED: &str = "Permanent deletion requires confirmation";

type Sender = tokio::sync::broadcast::Sender<CoreEvent>;

/// Everything a mutation needs to act on the conversation's copies in the
/// currently viewed folder, captured under the state lock.
struct Target {
    account: Account,
    folder: Folder,
    /// Messages of the conversation that live in the current folder.
    messages: Vec<StoredMessage>,
    /// Their UIDs, in the same order.
    uids: Vec<Uid>,
}

/// Whether "Archive" should be offered for a conversation in `current` folder
/// on an account with the given capabilities. Gmail can always archive (it just
/// removes the source label) unless viewing Trash or Spam; other servers need
/// an Archive folder to move into.
pub fn archive_available(
    capabilities: &Capabilities,
    current: FolderKind,
    has_archive: bool,
) -> bool {
    // Archiving from Archive (Gmail All Mail) or its generic equivalent is
    // meaningless and, on Gmail, removing All Mail membership would expunge.
    if matches!(
        current,
        FolderKind::Trash | FolderKind::Junk | FolderKind::Archive
    ) {
        return false;
    }
    capabilities.gmail || has_archive
}

/// Mark every copy of the conversation in the current folder read or unread.
/// The Sent copy is deliberately left alone.
pub async fn set_read(thread: &ThreadId, read: bool) -> Result<ModelSnapshot, String> {
    let (target, config, sender) = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized)?;
        let target = resolve_target(state, thread)?;
        (target, state.config.clone(), state.events.clone())
    };

    let mut transport = connect(&config, &target.account).await?;

    // Optimistic local flag change.
    {
        let mut guard = lock_state();
        let state = guard.as_mut().ok_or_else(not_initialized)?;
        for message in &target.messages {
            if message.flags.seen != read {
                let mut flags = message.flags;
                flags.seen = read;
                state
                    .store
                    .set_flags(&message.id, &flags)
                    .map_err(|error| format!("set flags: {error}"))?;
            }
        }
        if let Some(detail) = state.selected_conversation.as_mut() {
            for summary in &mut detail.messages {
                if target
                    .messages
                    .iter()
                    .any(|message| message.id == summary.id && message.flags.seen != read)
                {
                    summary.flags.seen = read;
                }
            }
        }
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);

    let seen = Flags {
        seen: true,
        ..Flags::default()
    };
    let op = if read {
        FlagOp::Add(seen)
    } else {
        FlagOp::Remove(seen)
    };

    let outcome = async {
        select_target(transport.as_mut(), &target).await?;
        transport
            .store_flags(&target.uids, op)
            .await
            .map_err(transport_msg)
    }
    .await;

    if outcome.is_err() {
        rollback_read(&target);
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    outcome.map(|()| snapshot())
}

/// Move the conversation to `dest`. Gmail move copies to the destination label
/// then removes the source label; generic servers use `MOVE` when available and
/// fall back to COPY + delete + expunge otherwise.
pub async fn move_thread(thread: &ThreadId, dest: &FolderId) -> Result<ModelSnapshot, String> {
    let (target, dest_folder, config, sender) = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized)?;
        let target = resolve_target(state, thread)?;
        let dest_folder = state
            .store
            .folders(&target.account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .into_iter()
            .find(|folder| &folder.id == dest)
            .ok_or_else(|| format!("unknown folder {dest}"))?;
        (
            target,
            dest_folder,
            state.config.clone(),
            state.events.clone(),
        )
    };

    let mut transport = connect(&config, &target.account).await?;
    let caps = capabilities(transport.as_mut()).await?;

    remove_optimistically(&target, &sender)?;

    let dest_name = dest_folder.name.clone();
    let source_from_all_mail = caps.gmail && target.folder.kind == FolderKind::Archive;
    let outcome = async {
        select_target(transport.as_mut(), &target).await?;
        if caps.gmail {
            // Gmail "move" removes the source label. From All Mail that label
            // is implicit membership, so deleting the source copy would expunge
            // the message from every label; copy to the destination and keep
            // All Mail. Trash keeps its own copy-then-delete semantics.
            transport
                .copy_messages(&target.uids, &dest_name)
                .await
                .map_err(transport_msg)?;
            if source_from_all_mail {
                Ok(())
            } else {
                transport
                    .delete_messages(&target.uids)
                    .await
                    .map_err(transport_msg)
            }
        } else if caps.move_ext {
            transport
                .move_messages(&target.uids, &dest_name)
                .await
                .map_err(transport_msg)
        } else {
            transport
                .copy_messages(&target.uids, &dest_name)
                .await
                .map_err(transport_msg)?;
            transport
                .delete_messages(&target.uids)
                .await
                .map_err(transport_msg)
        }
    }
    .await;

    conclude(&target, &sender, outcome)?;
    // The server move already succeeded; make the destination visible locally.
    // A failed follow-up sync must not turn a successful move into an error.
    if let Err(error) = super::sync_folder(dest).await {
        log::warn!("sync destination folder after move: {error}");
    }
    Ok(snapshot())
}

/// Archive the conversation. Gmail archive removes the source label; generic
/// archive moves into the Archive folder.
pub async fn archive_thread(thread: &ThreadId) -> Result<ModelSnapshot, String> {
    let (target, archive_folder, config, sender) = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized)?;
        let target = resolve_target(state, thread)?;
        // Never archive from Trash, Spam, or Archive itself. On Gmail, All Mail
        // is implicit membership: removing it would delete the message.
        if matches!(
            target.folder.kind,
            FolderKind::Trash | FolderKind::Junk | FolderKind::Archive
        ) {
            return Err(format!("cannot archive from {}", target.folder.name));
        }
        let archive_folder = state
            .store
            .folders(&target.account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .into_iter()
            .find(|folder| folder.kind == FolderKind::Archive);
        (
            target,
            archive_folder,
            state.config.clone(),
            state.events.clone(),
        )
    };

    let mut transport = connect(&config, &target.account).await?;
    let caps = capabilities(transport.as_mut()).await?;

    // A generic server needs an Archive folder to move into.
    if !caps.gmail && archive_folder.is_none() {
        return Err("no Archive folder to archive into".to_string());
    }

    remove_optimistically(&target, &sender)?;

    let archive_name = archive_folder.as_ref().map(|folder| folder.name.clone());
    let outcome = async {
        select_target(transport.as_mut(), &target).await?;
        if caps.gmail {
            transport
                .delete_messages(&target.uids)
                .await
                .map_err(transport_msg)
        } else {
            // `archive_name` is `Some` for a generic server here.
            transport
                .move_messages(&target.uids, archive_name.as_deref().unwrap_or("Archive"))
                .await
                .map_err(transport_msg)
        }
    }
    .await;

    conclude(&target, &sender, outcome)
}

/// Move the conversation to Trash. Gmail trash copies to the Trash label then
/// removes the source label; generic trash moves into the Trash folder.
pub async fn trash_thread(thread: &ThreadId) -> Result<ModelSnapshot, String> {
    let (target, trash_folder, config, sender) = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized)?;
        let target = resolve_target(state, thread)?;
        if target.folder.kind == FolderKind::Trash {
            return Err("conversation is already in Trash".to_string());
        }
        let trash_folder = state
            .store
            .folders(&target.account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .into_iter()
            .find(|folder| folder.kind == FolderKind::Trash);
        (
            target,
            trash_folder,
            state.config.clone(),
            state.events.clone(),
        )
    };

    // No Trash folder: the UI routes to permanent delete instead.
    let Some(trash_folder) = trash_folder else {
        return Err("no Trash folder; delete permanently instead".to_string());
    };

    let mut transport = connect(&config, &target.account).await?;
    let caps = capabilities(transport.as_mut()).await?;

    remove_optimistically(&target, &sender)?;

    let trash_name = trash_folder.name.clone();
    let outcome = async {
        select_target(transport.as_mut(), &target).await?;
        if caps.gmail {
            transport
                .copy_messages(&target.uids, &trash_name)
                .await
                .map_err(transport_msg)?;
            transport
                .delete_messages(&target.uids)
                .await
                .map_err(transport_msg)
        } else {
            transport
                .move_messages(&target.uids, &trash_name)
                .await
                .map_err(transport_msg)
        }
    }
    .await;

    conclude(&target, &sender, outcome)
}

/// Permanently delete the conversation. Requires `confirm` because it is
/// irreversible; offered only from Trash, or anywhere when no Trash exists.
pub async fn delete_permanently(thread: &ThreadId, confirm: bool) -> Result<ModelSnapshot, String> {
    if !confirm {
        return Err(CONFIRM_REQUIRED.to_string());
    }

    let (target, has_trash, config, sender) = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized)?;
        let target = resolve_target(state, thread)?;
        let has_trash = state
            .store
            .folders(&target.account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .iter()
            .any(|folder| folder.kind == FolderKind::Trash);
        (
            target,
            has_trash,
            state.config.clone(),
            state.events.clone(),
        )
    };

    // With a Trash folder, permanent delete is only for messages already there.
    if has_trash && target.folder.kind != FolderKind::Trash {
        return Err("permanent delete is only available from Trash".to_string());
    }

    let mut transport = connect(&config, &target.account).await?;

    remove_optimistically(&target, &sender)?;

    let outcome = async {
        select_target(transport.as_mut(), &target).await?;
        transport
            .delete_messages(&target.uids)
            .await
            .map_err(transport_msg)
    }
    .await;

    conclude(&target, &sender, outcome)
}

/// Open a transport for `account`, mapping a connection failure to the
/// user-facing offline error.
async fn connect(config: &Config, account: &Account) -> Result<Box<dyn MailTransport>, String> {
    config
        .transport
        .imap(account)
        .await
        .map_err(|_| RECONNECT_ERROR.to_string())
}

async fn capabilities(transport: &mut dyn MailTransport) -> Result<Capabilities, String> {
    transport.capabilities().await.map_err(transport_msg)
}

/// Select the target's folder on the transport.
async fn select_target(transport: &mut dyn MailTransport, target: &Target) -> Result<(), String> {
    transport
        .select(&remote_folder(&target.folder))
        .await
        .map_err(transport_msg)
        .map(|_| ())
}

/// Resolve the conversation's copies in the current folder, plus their account
/// and folder. Fails when the thread is unknown or absent from this view.
fn resolve_target(state: &AppState, thread: &ThreadId) -> Result<Target, String> {
    let messages = state
        .store
        .messages_for_thread(thread)
        .map_err(|error| format!("read conversation: {error}"))?;
    let Some(first) = messages.first() else {
        return Err(format!("unknown conversation {thread}"));
    };
    let account = find_account(&state.store, &first.account_id)?;
    let folder_id = current_folder_id(state, &account.id)?;
    let folder = state
        .store
        .folders(&account.id)
        .map_err(|error| format!("read folders: {error}"))?
        .into_iter()
        .find(|candidate| candidate.id == folder_id)
        .ok_or_else(|| "conversation folder missing".to_string())?;
    let messages: Vec<StoredMessage> = messages
        .into_iter()
        .filter(|message| message.folder_id == folder_id)
        .collect();
    if messages.is_empty() {
        return Err(format!(
            "conversation {thread} has no messages in the current folder"
        ));
    }
    let uids = messages.iter().map(|message| message.uid).collect();
    Ok(Target {
        account,
        folder,
        messages,
        uids,
    })
}

/// The folder a mutation should act on: the selected folder when it belongs to
/// the account, otherwise the account's Inbox (for the merged inbox view).
fn current_folder_id(state: &AppState, account_id: &AccountId) -> Result<FolderId, String> {
    if let Some(folder) = &state.selected_folder {
        let belongs = state
            .store
            .folders(account_id)
            .is_ok_and(|folders| folders.iter().any(|candidate| &candidate.id == folder));
        if belongs {
            return Ok(folder.clone());
        }
    }
    let folders = state
        .store
        .folders(account_id)
        .map_err(|error| format!("read folders: {error}"))?;
    folders
        .into_iter()
        .find(|folder| folder.kind == FolderKind::Inbox)
        .map(|folder| folder.id)
        .ok_or_else(|| format!("no Inbox folder for account {account_id}"))
}

fn remote_folder(folder: &Folder) -> RemoteFolder {
    RemoteFolder {
        name: folder.name.clone(),
        delimiter: folder.delimiter.clone(),
        kind: folder.kind,
    }
}

/// Apply the optimistic local removal and clear the stale reader selection.
fn remove_optimistically(target: &Target, sender: &Sender) -> Result<(), String> {
    {
        let mut guard = lock_state();
        let state = guard.as_mut().ok_or_else(not_initialized)?;
        let ids: Vec<MessageId> = target
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect();
        state
            .store
            .delete_messages(&ids)
            .map_err(|error| format!("delete conversation: {error}"))?;
        if state.selected_conversation.as_ref().is_some_and(|detail| {
            detail.account_id == target.account.id
                && detail
                    .messages
                    .iter()
                    .any(|message| ids.contains(&message.id))
        }) {
            state.invalidate_open();
            state.selected_conversation = None;
        }
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(())
}

/// Finish a removal mutation: roll back on failure, then refresh the view.
fn conclude(
    target: &Target,
    sender: &Sender,
    outcome: Result<(), String>,
) -> Result<ModelSnapshot, String> {
    if outcome.is_err() {
        rollback_removal(target);
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    outcome.map(|()| snapshot())
}

/// Restore the rows removed by an optimistic mutation.
fn rollback_removal(target: &Target) {
    let mut guard = lock_state();
    if let Some(state) = guard.as_mut() {
        for message in &target.messages {
            if let Err(error) = state.store.upsert_message(message) {
                log::error!("rollback message {}: {error}", message.id);
            }
        }
    }
}

/// Restore the flags changed by an optimistic `set_read`.
fn rollback_read(target: &Target) {
    let mut guard = lock_state();
    if let Some(state) = guard.as_mut() {
        for message in &target.messages {
            if let Err(error) = state.store.set_flags(&message.id, &message.flags) {
                log::error!("rollback flags for {}: {error}", message.id);
            }
        }
        if let Some(detail) = state.selected_conversation.as_mut() {
            for summary in &mut detail.messages {
                if let Some(original) = target
                    .messages
                    .iter()
                    .find(|message| message.id == summary.id)
                {
                    summary.flags = original.flags;
                }
            }
        }
    }
}

fn not_initialized() -> String {
    "pige api is not initialized".to_string()
}

fn transport_msg(error: TransportError) -> String {
    error.0
}
