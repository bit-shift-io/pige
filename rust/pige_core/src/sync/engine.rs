//! Incremental read-path sync for one account.

use crate::account::{Account, AccountId};
use crate::mail::{
    Folder, FolderId, FolderKind, MessageId, StoredMessage, ThreadAssigner, ThreadId, Uid,
    sender_display, thread::participants_of,
};
use crate::store::{Store, StoreError};
use crate::transport::{MailTransport, RemoteFolder, RemoteMessage, TransportError, UidRange};

/// How many bytes of text to fetch per message to fill snippets.
const SNIPPET_BYTES: usize = 2048;
/// How many of the newest messages get snippets filled each sync.
const SNIPPET_COUNT: usize = 100;

/// Errors that abort an account sync. Per-folder failures are captured in
/// [`SyncReport::errors`] instead so one bad folder never blocks the account.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
}

/// Summary of one newly-arrived inbox message, for notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMailSummary {
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub thread_id: ThreadId,
    pub from: String,
    pub subject: String,
}

/// Result of syncing one account.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub new_inbox: Vec<NewMailSummary>,
    pub synced_folders: usize,
    pub errors: Vec<String>,
}

/// Sync one account's read path: list folders, then pull Inbox first and the
/// special folders, storing headers and filling snippets. Bodies are never
/// fetched here.
pub async fn sync_account(
    store: &Store,
    account: &Account,
    transport: &mut dyn MailTransport,
    window: u32,
) -> Result<SyncReport, SyncError> {
    let mut report = SyncReport::default();

    let remote_folders = transport.list_folders().await?;

    // Upsert every listed folder, reusing the stored id for a name we already
    // know. This keeps non-synced (Other) folders visible and preserves the
    // sync state of folders that fail to select below.
    let existing = store.folders(&account.id)?;
    for remote in &remote_folders {
        let prior = existing.iter().find(|f| f.name == remote.name);
        store.upsert_folder(&Folder {
            id: prior.map(|f| f.id.clone()).unwrap_or_else(FolderId::new),
            account_id: account.id.clone(),
            name: remote.name.clone(),
            delimiter: remote.delimiter.clone(),
            kind: remote.kind,
            unread: prior.map(|f| f.unread).unwrap_or(0),
            total: prior.map(|f| f.total).unwrap_or(0),
            uidvalidity: prior.and_then(|f| f.uidvalidity),
            uidnext: prior.and_then(|f| f.uidnext),
        })?;
    }

    let folders = store.folders(&account.id)?;

    // Upgrade rows cached before threading existed exactly once. Messages that
    // already carry real thread ids are left alone.
    store.rethread_legacy_threads(&account.id)?;

    // Seed threading from cached headers once per account. This is the only
    // place the index is built, so every folder sync shares conversations and
    // a Sent reply can join its Inbox parent. No bodies are read.
    let existing_messages = store.messages_for_account(&account.id)?;
    let mut threads = ThreadAssigner::from_messages(&existing_messages);

    // Inbox first, then the special folders; Other folders are not auto-synced.
    let mut to_sync: Vec<&RemoteFolder> = remote_folders
        .iter()
        .filter(|f| sync_order(f.kind).is_some())
        .collect();
    to_sync.sort_by_key(|f| sync_order(f.kind).expect("filtered to syncable kinds"));

    for remote in to_sync {
        let local = folders
            .iter()
            .find(|f| f.name == remote.name)
            .expect("folder upserted above");
        match sync_folder(
            store,
            account,
            transport,
            remote,
            local,
            window,
            &mut threads,
        )
        .await
        {
            Ok(summaries) => {
                report.synced_folders += 1;
                report.new_inbox.extend(summaries);
            }
            Err(message) => report.errors.push(message),
        }
    }

    Ok(report)
}

/// Order special folders are synced after Inbox. `None` means "not auto-synced".
fn sync_order(kind: FolderKind) -> Option<usize> {
    match kind {
        FolderKind::Inbox => Some(0),
        FolderKind::Sent => Some(1),
        FolderKind::Drafts => Some(2),
        FolderKind::Trash => Some(3),
        FolderKind::Archive => Some(4),
        FolderKind::Junk => Some(5),
        FolderKind::Other => None,
    }
}

/// Sync one folder and return any newly-arrived inbox summaries it produced.
async fn sync_folder(
    store: &Store,
    account: &Account,
    transport: &mut dyn MailTransport,
    remote: &RemoteFolder,
    local: &Folder,
    window: u32,
    threads: &mut ThreadAssigner,
) -> Result<Vec<NewMailSummary>, String> {
    let folder_id = local.id.clone();
    let account_id = account.id.clone();

    let state = transport
        .select(remote)
        .await
        .map_err(|e| format!("select {}: {e}", remote.name))?;

    // A changed UIDVALIDITY invalidates everything cached under the old epoch.
    let mut reset = false;
    if let Some(prior) = local.uidvalidity
        && prior != state.uidvalidity
    {
        store
            .reset_folder(&account_id, &remote.name, state.uidvalidity)
            .map_err(|e| format!("reset {}: {e}", remote.name))?;
        reset = true;
        // The reset deleted this folder's cached messages; rebuild the index
        // so new headers cannot join conversations that no longer exist.
        *threads = ThreadAssigner::from_messages(
            &store
                .messages_for_account(&account_id)
                .map_err(|e| format!("read {}: {e}", remote.name))?,
        );
    }

    // High-water mark is the UIDNEXT recorded on the previous sync. A first
    // sync, or one that just reset, has no prior state and refetches the newest
    // `window` messages instead.
    let prior_uidnext = if reset || local.uidvalidity.is_none() {
        None
    } else {
        local.uidnext
    };
    let (from, to) = match prior_uidnext {
        None => {
            let from = state.uidnext.saturating_sub(window).max(1);
            (Uid(from), Uid(state.uidnext.saturating_sub(1)))
        }
        Some(high_water) => (Uid(high_water), Uid(state.uidnext.saturating_sub(1))),
    };

    // Cached messages that predate this sync, for flag reconciliation.
    let cached = store
        .messages(&folder_id, u32::MAX, None)
        .map_err(|e| format!("read {}: {e}", remote.name))?;
    let mut cached_uids: Vec<Uid> = cached.iter().map(|m| m.uid).collect();
    cached_uids.sort_unstable();

    // Fetch and store headers for the new UIDs.
    let fetched: Vec<RemoteMessage> = if from <= to {
        transport
            .fetch_headers(UidRange { from, to })
            .await
            .map_err(|e| format!("fetch {}: {e}", remote.name))?
    } else {
        Vec::new()
    };

    let mut inserted: Vec<StoredMessage> = Vec::new();
    for message in &fetched {
        let id = MessageId::new();
        let mut header = message.header.clone();
        // The transport-reported RFC822.SIZE is authoritative over the size the
        // header parser leaves at 0.
        header.size = message.size;
        let participants = participants_of(&header);
        let thread_id = threads.assign(&account_id, &header, header.date, &participants);
        // A message that bridges two existing conversations forces a store
        // merge before it is inserted, so every row ends up on the survivor.
        for (from, into) in threads.take_merges() {
            store
                .merge_threads(&account_id, &from, &into)
                .map_err(|e| format!("merge {}: {e}", remote.name))?;
            // Messages inserted earlier in this batch still carry the losing id
            // in memory; keep their snippet upserts on the survivor.
            for stored in &mut inserted {
                if stored.thread_id == from {
                    stored.thread_id = into.clone();
                }
            }
        }
        let stored = StoredMessage {
            id,
            account_id: account_id.clone(),
            folder_id: folder_id.clone(),
            uid: message.uid,
            uidvalidity: state.uidvalidity,
            header,
            flags: message.flags,
            thread_id,
            snippet: String::new(),
            has_attachments: false,
        };
        store
            .upsert_message(&stored)
            .map_err(|e| format!("store {}: {e}", remote.name))?;
        inserted.push(stored);
    }

    // Fill snippets for the newest messages without fetching full bodies.
    let mut newest: Vec<Uid> = fetched.iter().map(|m| m.uid).collect();
    newest.sort_unstable_by(|a, b| b.cmp(a));
    newest.truncate(SNIPPET_COUNT);
    if !newest.is_empty() {
        let texts = transport
            .fetch_texts(&newest, SNIPPET_BYTES)
            .await
            .map_err(|e| format!("snippets {}: {e}", remote.name))?;
        for (uid, text) in texts {
            if let Some(stored) = inserted.iter().find(|m| m.uid == uid) {
                let mut updated = stored.clone();
                updated.snippet = text;
                store
                    .upsert_message(&updated)
                    .map_err(|e| format!("store {}: {e}", remote.name))?;
            }
        }
    }

    // Reconcile flags for messages we already had cached (not the new ones).
    if !cached_uids.is_empty() {
        let flags = transport
            .fetch_flags(&cached_uids)
            .await
            .map_err(|e| format!("flags {}: {e}", remote.name))?;
        for (uid, flag) in flags {
            if let Some(stored) = cached.iter().find(|m| m.uid == uid) {
                store
                    .set_flags(&stored.id, &flag)
                    .map_err(|e| format!("flags {}: {e}", remote.name))?;
            }
        }
    }

    // Update folder counts and sync state.
    let all = store
        .messages(&folder_id, u32::MAX, None)
        .map_err(|e| format!("read {}: {e}", remote.name))?;
    // Unread is derived from cached messages only, so it undercounts when
    // `window` is smaller than the folder's total; `FolderState` carries no
    // server unread count.
    let unread = all.iter().filter(|m| !m.flags.seen).count() as u32;
    store
        .upsert_folder(&Folder {
            id: folder_id.clone(),
            account_id: account_id.clone(),
            name: remote.name.clone(),
            delimiter: remote.delimiter.clone(),
            kind: remote.kind,
            unread,
            total: state.exists,
            uidvalidity: Some(state.uidvalidity),
            uidnext: Some(state.uidnext),
        })
        .map_err(|e| format!("store {}: {e}", remote.name))?;

    // Newly-arrived inbox messages become notification summaries, newest first.
    // A reset (UIDVALIDITY rotation) re-inserts messages the user already saw,
    // so suppress notifications in that case.
    if remote.kind != FolderKind::Inbox || reset {
        return Ok(Vec::new());
    }
    let mut sorted = inserted;
    sorted.sort_unstable_by_key(|m| std::cmp::Reverse(m.uid));
    Ok(sorted
        .into_iter()
        .map(|m| NewMailSummary {
            account_id: account_id.clone(),
            folder_id: folder_id.clone(),
            thread_id: m.thread_id.clone(),
            from: sender_display(&m.header.from),
            subject: m.header.subject.clone(),
        })
        .collect())
}
