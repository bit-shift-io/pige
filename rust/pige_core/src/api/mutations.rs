//! The mutation surface: read state, move, archive, trash, and permanent
//! delete, with provider-neutral label semantics and current-folder-only scope.
//!
//! Every mutation resolves the conversation's messages in the *currently
//! viewed* folder only (never the Sent copy), maps each to its provider remote
//! identity, applies the local change optimistically, and drives the account's
//! [`MutationService`](crate::providers::MutationService) through the account's
//! scheduler gate. The provider adapter owns protocol specifics (Gmail label
//! removal, `MOVE` vs `COPY`+delete, expunge); the API only ever sees
//! [`ProviderCapabilities`] and a per-item [`MutationOutcome`]. Locks are never
//! held across a network await.
//!
//! A per-item outcome is honored individually: a confirmed `Applied` item keeps
//! its optimistic local change, while a `Rejected` or `Unknown` item is rolled
//! back locally and its error surfaced, so a partial mutation never pretends a
//! failed item succeeded. Because the whole remote read/apply sequence holds the
//! account gate, a stale sync page can never reverse an applied mutation.

use std::sync::Arc;

use crate::account::{Account, AccountId};
use crate::mail::{Folder, FolderId, FolderKind, MessageId, StoredMessage, ThreadId};
use crate::providers::{
    MutationAction, MutationOutcome, MutationRequest, OperationId, ProviderCapabilities,
    RemoteCollectionId, RemoteMessageId,
};
use crate::sync::scheduler::AccountScheduler;

use super::mailbox::{INTERACTIVE_TIMEOUT, open_session, provider_message, scheduler_message};
use super::{
    AppState, Config, CoreEvent, ModelSnapshot, find_account, lock_state, not_initialized_message,
    snapshot,
};

/// The user-facing error returned when a mutation needs the server but the
/// account is unreachable. Reading, search, and sending still work offline.
pub const RECONNECT_ERROR: &str = "Reconnect to move, archive, or delete";

/// The error returned when a permanent delete is attempted without going
/// through the confirmation dialog.
const CONFIRM_REQUIRED: &str = "Permanent deletion requires confirmation";

type Sender = tokio::sync::broadcast::Sender<CoreEvent>;

/// Everything a mutation needs to act on the conversation's copies in the
/// currently viewed folder, captured under the state lock before any await.
struct Prepared {
    account: Account,
    folder: Folder,
    /// Messages of the conversation that live in the current folder, in the
    /// same order as `remote_ids`.
    messages: Vec<StoredMessage>,
    /// Their provider remote identities.
    remote_ids: Vec<RemoteMessageId>,
    config: Config,
    scheduler: Arc<AccountScheduler>,
    ctx: crate::providers::OperationContext,
    sender: Sender,
}

/// Whether "Archive" should be offered for a conversation in `current` folder
/// on an account with the given provider-neutral capabilities. A provider that
/// implements archive as label removal (Gmail) can always archive unless viewing
/// Trash or Spam; a provider that moves into an Archive folder needs one to
/// exist.
pub fn archive_available(
    capabilities: &ProviderCapabilities,
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
    if !capabilities.mutations.archive {
        return false;
    }
    capabilities.mutations.labels || has_archive
}

/// The capability that must be advertised before an action is attempted.
fn action_supported(capabilities: &ProviderCapabilities, action: &MutationAction) -> bool {
    let mutations = &capabilities.mutations;
    match action {
        MutationAction::SetRead(_) => mutations.read,
        MutationAction::SetStar(_) => mutations.star,
        MutationAction::AddLabel(_) | MutationAction::RemoveLabel(_) => mutations.labels,
        MutationAction::Move(_) => mutations.move_messages,
        MutationAction::Archive => mutations.archive,
        MutationAction::Trash => mutations.trash,
        MutationAction::DeletePermanently => mutations.permanent_delete,
    }
}

fn unsupported_message(action: &MutationAction) -> String {
    let what = match action {
        MutationAction::SetRead(_) => "changing the read state",
        MutationAction::SetStar(_) => "starring",
        MutationAction::AddLabel(_) => "adding a label",
        MutationAction::RemoveLabel(_) => "removing a label",
        MutationAction::Move(_) => "moving",
        MutationAction::Archive => "archiving",
        MutationAction::Trash => "moving to Trash",
        MutationAction::DeletePermanently => "permanently deleting",
    };
    format!("this account cannot {what} messages")
}

/// Mark every copy of the conversation in the current folder read or unread.
/// The Sent copy is deliberately left alone.
pub async fn set_read(thread: &ThreadId, read: bool) -> Result<ModelSnapshot, String> {
    let prepared = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized_message)?;
        prepare(state, thread)?
    };

    // Optimistic local flag change.
    {
        let mut guard = lock_state();
        let state = guard.as_mut().ok_or_else(not_initialized_message)?;
        for message in &prepared.messages {
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
                if prepared
                    .messages
                    .iter()
                    .any(|message| message.id == summary.id && message.flags.seen != read)
                {
                    summary.flags.seen = read;
                }
            }
        }
    }
    let _ = prepared.sender.send(CoreEvent::SnapshotChanged);

    let result = run_mutation(&prepared, MutationAction::SetRead(read)).await;

    match result {
        Ok(result) => {
            let outcome = reconcile_read(&prepared, &result);
            let _ = prepared.sender.send(CoreEvent::SnapshotChanged);
            outcome.map(|()| snapshot())
        }
        Err(error) => {
            rollback_read(&prepared);
            let _ = prepared.sender.send(CoreEvent::SnapshotChanged);
            Err(error)
        }
    }
}

/// Move the conversation to `dest`. The provider adapter chooses the correct
/// protocol sequence (Gmail label copy/remove, `MOVE`, or COPY + delete).
pub async fn move_thread(thread: &ThreadId, dest: &FolderId) -> Result<ModelSnapshot, String> {
    let (prepared, dest_folder) = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized_message)?;
        let prepared = prepare(state, thread)?;
        let dest_folder = state
            .store
            .folders(&prepared.account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .into_iter()
            .find(|folder| &folder.id == dest)
            .ok_or_else(|| format!("unknown folder {dest}"))?;
        (prepared, dest_folder)
    };

    let action = MutationAction::Move(RemoteCollectionId::from(dest_folder.name.clone()));
    remove_optimistically(&prepared)?;

    let result = run_mutation(&prepared, action).await;
    let outcome = conclude_removal(&prepared, result);

    // The server move already succeeded; make the destination visible locally.
    // A failed follow-up sync must not turn a successful move into an error.
    if outcome.is_ok()
        && let Err(error) = super::sync_folder(dest).await
    {
        log::warn!("sync destination folder after move: {error}");
    }
    outcome.map(|()| snapshot())
}

/// Archive the conversation. The provider adapter removes the source label on a
/// label-based provider, or moves into the Archive folder otherwise.
pub async fn archive_thread(thread: &ThreadId) -> Result<ModelSnapshot, String> {
    let prepared = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized_message)?;
        let prepared = prepare(state, thread)?;
        // Never archive from Trash, Spam, or Archive itself. On Gmail, All Mail
        // is implicit membership: removing it would delete the message.
        if matches!(
            prepared.folder.kind,
            FolderKind::Trash | FolderKind::Junk | FolderKind::Archive
        ) {
            return Err(format!("cannot archive from {}", prepared.folder.name));
        }
        prepared
    };

    remove_optimistically(&prepared)?;
    let result = run_mutation(&prepared, MutationAction::Archive).await;
    conclude_removal(&prepared, result).map(|()| snapshot())
}

/// Move the conversation to Trash. The provider adapter copies to the Trash
/// label then removes the source label on Gmail, or moves into the Trash folder
/// on a generic server.
pub async fn trash_thread(thread: &ThreadId) -> Result<ModelSnapshot, String> {
    let prepared = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized_message)?;
        let prepared = prepare(state, thread)?;
        if prepared.folder.kind == FolderKind::Trash {
            return Err("conversation is already in Trash".to_string());
        }
        prepared
    };

    // No Trash folder: the UI routes to permanent delete instead.
    let has_trash = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized_message)?;
        state
            .store
            .folders(&prepared.account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .iter()
            .any(|folder| folder.kind == FolderKind::Trash)
    };
    if !has_trash {
        return Err("no Trash folder; delete permanently instead".to_string());
    }

    remove_optimistically(&prepared)?;
    let result = run_mutation(&prepared, MutationAction::Trash).await;
    conclude_removal(&prepared, result).map(|()| snapshot())
}

/// Permanently delete the conversation. Requires `confirm` because it is
/// irreversible; offered only from Trash, or anywhere when no Trash exists.
pub async fn delete_permanently(thread: &ThreadId, confirm: bool) -> Result<ModelSnapshot, String> {
    if !confirm {
        return Err(CONFIRM_REQUIRED.to_string());
    }

    let prepared = {
        let guard = lock_state();
        let state = guard.as_ref().ok_or_else(not_initialized_message)?;
        let prepared = prepare(state, thread)?;
        let has_trash = state
            .store
            .folders(&prepared.account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .iter()
            .any(|folder| folder.kind == FolderKind::Trash);
        // With a Trash folder, permanent delete is only for messages there.
        if has_trash && prepared.folder.kind != FolderKind::Trash {
            return Err("permanent delete is only available from Trash".to_string());
        }
        prepared
    };

    remove_optimistically(&prepared)?;
    let result = run_mutation(&prepared, MutationAction::DeletePermanently).await;
    conclude_removal(&prepared, result).map(|()| snapshot())
}

/// Run one provider mutation for the prepared targets under the account gate.
/// The gate is held for the open (network) and the apply, so the whole
/// read/apply sequence is serialized with syncing. The session's own
/// capabilities gate the attempt: an unsupported action is refused before any
/// mutation request is issued.
async fn run_mutation(
    prepared: &Prepared,
    action: MutationAction,
) -> Result<crate::providers::MutationResult, String> {
    let providers = prepared.config.providers.clone();
    let account = prepared.account.clone();
    let ctx = prepared.ctx.clone();
    let operation_ctx = ctx.clone();
    let request = MutationRequest {
        operation: OperationId::new(uuid::Uuid::new_v4().to_string()),
        targets: prepared.remote_ids.clone(),
        source: Some(RemoteCollectionId::from(prepared.folder.name.clone())),
        action,
    };

    prepared
        .scheduler
        .run_interactive(&ctx, async move {
            let mut session = open_session(&providers, &account, &operation_ctx).await?;
            if !action_supported(&session.capabilities, &request.action) {
                return Err(unsupported_message(&request.action));
            }
            let service = session.mutation_service().map_err(provider_message)?;
            service
                .apply(request, &operation_ctx)
                .await
                .map_err(provider_message)
        })
        .await
        .map_err(scheduler_message)?
}

/// Reconcile a removal mutation's per-item outcomes: keep confirmed items, roll
/// back the ones the server rejected or left uncertain, and report the first
/// such error. A rejected/unknown item is never retried here — the account's
/// next sync reconciles remote truth.
fn reconcile_removal(
    prepared: &Prepared,
    result: &crate::providers::MutationResult,
) -> Result<(), String> {
    let mut failure: Option<String> = None;
    for item in &result.items {
        match &item.outcome {
            MutationOutcome::Applied => {}
            MutationOutcome::Rejected(error) => {
                rollback_message(prepared, &item.message);
                failure.get_or_insert_with(|| error.safe_message.clone());
            }
            MutationOutcome::Unknown => {
                rollback_message(prepared, &item.message);
                failure.get_or_insert_with(|| {
                    "the mail server did not confirm the change".to_string()
                });
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Reconcile a `set_read` mutation's per-item outcomes.
fn reconcile_read(
    prepared: &Prepared,
    result: &crate::providers::MutationResult,
) -> Result<(), String> {
    let mut failure: Option<String> = None;
    for item in &result.items {
        match &item.outcome {
            MutationOutcome::Applied => {}
            MutationOutcome::Rejected(error) => {
                rollback_read_one(prepared, &item.message);
                failure.get_or_insert_with(|| error.safe_message.clone());
            }
            MutationOutcome::Unknown => {
                rollback_read_one(prepared, &item.message);
                failure.get_or_insert_with(|| {
                    "the mail server did not confirm the change".to_string()
                });
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Finish a removal mutation: roll back rejected/uncertain items, then refresh
/// the view. No lock is held across the await.
fn conclude_removal(
    prepared: &Prepared,
    result: Result<crate::providers::MutationResult, String>,
) -> Result<(), String> {
    let outcome = match result {
        Ok(result) => reconcile_removal(prepared, &result),
        Err(error) => {
            rollback_removal(prepared);
            Err(error)
        }
    };
    let _ = prepared.sender.send(CoreEvent::SnapshotChanged);
    outcome
}

/// Resolve the conversation's copies in the current folder, their account and
/// folder, and their provider remote identities. Fails when the thread is
/// unknown, absent from this view, or has a message the provider cannot name.
fn prepare(state: &AppState, thread: &ThreadId) -> Result<Prepared, String> {
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
        .filter(|message| {
            state
                .store
                .message_memberships(&message.id)
                .map(|memberships| memberships.contains(&folder_id))
                .unwrap_or(false)
        })
        .collect();
    if messages.is_empty() {
        return Err(format!(
            "conversation {thread} has no messages in the current folder"
        ));
    }
    let remote_ids: Vec<RemoteMessageId> = messages
        .iter()
        .map(|message| {
            state
                .store
                .remote_message(&account.id, &message.id)
                .map_err(|_| "a message in this conversation has no provider identity".to_string())
        })
        .collect::<Result<_, _>>()?;
    let ctx = state
        .sessions
        .context(&account.id, INTERACTIVE_TIMEOUT)
        .map_err(|_| not_initialized_message())?;
    Ok(Prepared {
        account,
        folder,
        messages,
        remote_ids,
        config: state.config.clone(),
        scheduler: state.scheduler.clone(),
        ctx,
        sender: state.events.clone(),
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

/// The local message a remote identity resolves to, when it is one of the
/// prepared targets.
fn local_id<'a>(prepared: &'a Prepared, remote: &RemoteMessageId) -> Option<&'a MessageId> {
    prepared
        .remote_ids
        .iter()
        .position(|candidate| candidate == remote)
        .map(|index| &prepared.messages[index].id)
}

/// Apply the optimistic local removal and clear the stale reader selection.
///
/// Only the current folder's membership is removed: the content, its body, and
/// any other collection membership survive, so a rollback restores the view
/// without re-downloading anything.
fn remove_optimistically(prepared: &Prepared) -> Result<(), String> {
    {
        let mut guard = lock_state();
        let state = guard.as_mut().ok_or_else(not_initialized_message)?;
        let ids: Vec<MessageId> = prepared
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect();
        for message in &prepared.messages {
            state
                .store
                .remove_membership(&message.id, &prepared.folder.id)
                .map_err(|error| format!("remove conversation: {error}"))?;
        }
        if state.selected_conversation.as_ref().is_some_and(|detail| {
            detail.account_id == prepared.account.id
                && detail
                    .messages
                    .iter()
                    .any(|message| ids.contains(&message.id))
        }) {
            state.invalidate_open();
            state.selected_conversation = None;
        }
    }
    let _ = prepared.sender.send(CoreEvent::SnapshotChanged);
    Ok(())
}

/// Restore the memberships removed by an optimistic mutation.
fn rollback_removal(prepared: &Prepared) {
    let mut guard = lock_state();
    if let Some(state) = guard.as_mut() {
        for message in &prepared.messages {
            if let Err(error) = state.store.add_membership(&message.id, &prepared.folder.id) {
                log::error!("rollback message {}: {error}", message.id);
            }
        }
    }
}

/// Restore one message's membership when its remote mutation did not apply.
fn rollback_message(prepared: &Prepared, remote: &RemoteMessageId) {
    let Some(id) = local_id(prepared, remote) else {
        return;
    };
    let mut guard = lock_state();
    if let Some(state) = guard.as_mut()
        && let Err(error) = state.store.add_membership(id, &prepared.folder.id)
    {
        log::error!("rollback message {id}: {error}");
    }
}

/// Restore the flags changed by an optimistic `set_read`.
fn rollback_read(prepared: &Prepared) {
    let mut guard = lock_state();
    if let Some(state) = guard.as_mut() {
        for message in &prepared.messages {
            if let Err(error) = state.store.set_flags(&message.id, &message.flags) {
                log::error!("rollback flags for {}: {error}", message.id);
            }
        }
        if let Some(detail) = state.selected_conversation.as_mut() {
            for summary in &mut detail.messages {
                if let Some(original) = prepared
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

/// Restore one message's flags when its remote read change did not apply.
fn rollback_read_one(prepared: &Prepared, remote: &RemoteMessageId) {
    let Some(id) = local_id(prepared, remote) else {
        return;
    };
    let Some(original) = prepared.messages.iter().find(|message| &message.id == id) else {
        return;
    };
    let mut guard = lock_state();
    if let Some(state) = guard.as_mut() {
        if let Err(error) = state.store.set_flags(id, &original.flags) {
            log::error!("rollback flags for {id}: {error}");
        }
        if let Some(detail) = state.selected_conversation.as_mut()
            && let Some(summary) = detail.messages.iter_mut().find(|summary| &summary.id == id)
        {
            summary.flags = original.flags;
        }
    }
}
