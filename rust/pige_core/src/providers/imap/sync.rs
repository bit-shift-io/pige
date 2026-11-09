//! Read-path IMAP synchronization behind the provider `SyncService` trait.
//!
//! The adapter owns a low-level [`MailTransport`] and translates IMAP's
//! per-mailbox UIDVALIDITY/high-water model into provider-neutral
//! [`MailChange`]s. The durable progress lives entirely in a [`ProviderCursor`]
//! (never in local columns): each mailbox records the UIDVALIDITY epoch it was
//! last read under, the high-water UIDNEXT, the UIDs already reported with their
//! last-known flags (for flag reconciliation and expunge detection), and whether
//! a *complete* enumeration has ever been performed.
//!
//! Identity is the F4 `(mailbox, UIDVALIDITY, UID)` remote id. A folder rename
//! therefore changes the identity of every message under it; the adapter retires
//! the vanished mailbox's mapping and resyncs the new name instead of trying to
//! match UIDs by a name heuristic.
//!
//! A bounded initial window is deliberately *not* a full snapshot: the
//! checkpoint records `snapshot_complete = false`, so a caller must not sweep
//! messages older than the window. Only a bootstrap that reached UID 1 sets the
//! flag.

use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::mail::{Flags, FolderKind, Uid};
use crate::providers::error::ProviderError;
use crate::providers::traits::SyncService;
use crate::providers::types::{
    ChangePage, CollectionRecord, MailChange, MessageRecord, OperationContext, ProviderCursor,
    SyncRequest,
};
use crate::sync::engine::{bootstrap_window, delta_window, sync_order};
use crate::transport::{FolderState, MailTransport, RemoteFolder, RemoteMessage, UidRange};

use super::{identity, transport_error};

/// Cursor payload version for [`ImapSync`]. A cursor from another version is
/// rejected as [`crate::providers::ErrorKind::CursorInvalid`].
const SYNC_CURSOR_VERSION: u32 = 1;

/// Durable sync progress for one account, encoded as the opaque checkpoint.
///
/// Exposed only so adapter tests can assert on the encoded stage; the
/// application must treat a [`ProviderCursor`] as opaque.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImapSyncState {
    pub version: u32,
    /// Per-mailbox progress, in the order mailboxes were first seen.
    pub mailboxes: Vec<MailboxProgress>,
    /// Mailboxes a caller asked to begin tracking on the next page. The shared
    /// [`SyncRequest`] (F1-owned) carries no mailbox, so an on-demand `Other`
    /// folder has no in-band way to name itself; F7 folds the request into the
    /// cursor with [`request_mailbox`]. Cleared once the next page has attempted
    /// them.
    #[serde(default)]
    pub requested: Vec<String>,
}

/// Durable progress for a single mailbox inside [`ImapSyncState`].
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxProgress {
    pub name: String,
    pub uidvalidity: u32,
    /// Server UIDNEXT observed at the last sync; the incremental high-water.
    pub uidnext: u32,
    /// UIDs already reported under `uidvalidity`, with the flags last seen.
    pub known: Vec<KnownUid>,
    /// Whether the whole mailbox had been enumerated when this state was
    /// written. A bounded bootstrap window leaves this false so older cached
    /// messages are never swept.
    pub snapshot_complete: bool,
}

/// One previously-reported message: its UID and the flags last seen for it.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownUid {
    pub uid: u32,
    pub flags: Flags,
}

/// Decode a checkpoint produced by [`ImapSync`]. Test-only inspection; the
/// application never parses a cursor payload.
#[doc(hidden)]
pub fn decode_sync_cursor(cursor: &ProviderCursor) -> Result<ImapSyncState, ProviderError> {
    let state: ImapSyncState = serde_json::from_str(&cursor.payload)
        .map_err(|_| ProviderError::cursor_invalid("the sync cursor could not be read"))?;
    if state.version != SYNC_CURSOR_VERSION {
        return Err(ProviderError::cursor_invalid(
            "the sync cursor was written by a different version",
        ));
    }
    Ok(state)
}

/// Ask the next [`SyncService::next_page`] to begin tracking `mailbox`, even an
/// `Other` folder the cursor does not yet know.
///
/// The shared `SyncRequest` (F1-owned) carries no mailbox, so an on-demand
/// folder cannot name itself through the neutral interface. F7 folds the
/// requested mailbox into the checked cursor with this helper; the next page
/// bootstraps it under its own `(mailbox, UIDVALIDITY, UID)` identity, and
/// thereafter it is tracked normally. Adoption is needed only once — once a
/// page has recorded progress for the mailbox, ordinary deltas keep it current.
pub fn request_mailbox(
    cursor: Option<&ProviderCursor>,
    mailbox: &str,
) -> Result<ProviderCursor, ProviderError> {
    let mut state = match cursor {
        Some(cursor) => decode_sync_cursor(cursor)?,
        None => ImapSyncState::empty(),
    };
    if !state.requested.iter().any(|name| name == mailbox) {
        state.requested.push(mailbox.to_string());
    }
    encode_sync_cursor(&state)
}

impl ImapSyncState {
    fn empty() -> Self {
        Self {
            version: SYNC_CURSOR_VERSION,
            mailboxes: Vec::new(),
            requested: Vec::new(),
        }
    }

    fn progress(&self, name: &str) -> Option<&MailboxProgress> {
        self.mailboxes.iter().find(|mailbox| mailbox.name == name)
    }

    fn set_progress(&mut self, progress: MailboxProgress) {
        match self
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == progress.name)
        {
            Some(slot) => *slot = progress,
            None => self.mailboxes.push(progress),
        }
    }
}

/// A [`SyncService`] over a live IMAP transport.
pub struct ImapSync {
    transport: Box<dyn MailTransport>,
}

impl ImapSync {
    pub fn new(transport: Box<dyn MailTransport>) -> Self {
        Self { transport }
    }
}

#[async_trait]
impl SyncService for ImapSync {
    async fn next_page(
        &mut self,
        request: SyncRequest,
        ctx: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        ctx.ensure_active()?;

        let mut state = match &request.cursor {
            Some(cursor) => decode_sync_cursor(cursor)?,
            None => ImapSyncState::empty(),
        };
        // A cursor that has never tracked a mailbox is this account's first
        // sync: its inbox mail is genuinely new to the user and worth
        // notifying. Any later bootstrap (a UIDVALIDITY reset) is a re-read of
        // pre-existing mail and must not notify.
        let account_first_sync = state.mailboxes.is_empty();

        let folders = self
            .transport
            .list_folders()
            .await
            .map_err(transport_error)?;
        let mut changes = Vec::new();

        // Collection membership: a name that did not exist in the cursor is a
        // new collection; a name the cursor knew that is gone has been removed,
        // and its messages retire with it.
        for folder in &folders {
            if state.progress(&folder.name).is_none() {
                changes.push(MailChange::UpsertCollection(collection_record(folder)));
            }
        }
        let current: HashSet<&str> = folders.iter().map(|folder| folder.name.as_str()).collect();
        let vanished: Vec<MailboxProgress> = state
            .mailboxes
            .iter()
            .filter(|mailbox| !current.contains(mailbox.name.as_str()))
            .cloned()
            .collect();
        for mailbox in &vanished {
            changes.push(MailChange::RemoveCollection(
                identity::collection_remote_id(&mailbox.name),
            ));
            // Retire the old identity rather than reusing it under a new name:
            // every UID named this mailbox, which no longer exists.
            for known in &mailbox.known {
                changes.push(MailChange::RemoveMessage(identity::message_remote_id(
                    &mailbox.name,
                    mailbox.uidvalidity,
                    Uid(known.uid),
                )));
            }
        }
        state
            .mailboxes
            .retain(|mailbox| !vanished.iter().any(|gone| gone.name == mailbox.name));

        // Sync the special folders automatically, in engine order, plus any
        // folder the cursor already tracks, plus any folder a caller asked to
        // adopt on this page with [`request_mailbox`]. A brand-new on-demand
        // "Other" folder reaches this set through the request and then keeps
        // syncing on its own once progress exists.
        let mut to_sync: Vec<RemoteFolder> = folders
            .iter()
            .filter(|folder| {
                sync_order(folder.kind).is_some()
                    || state.progress(&folder.name).is_some()
                    || state.requested.iter().any(|name| name == &folder.name)
            })
            .cloned()
            .collect();
        to_sync.sort_by_key(|folder| sync_order(folder.kind).unwrap_or(usize::MAX));

        for folder in to_sync {
            // A long run of unselectable folders must still honor a cancel.
            ctx.ensure_active()?;
            let name = folder.name.clone();
            let folder_state = match self.transport.select(&folder).await {
                Ok(folder_state) => folder_state,
                // A mailbox we cannot select is skipped so one bad folder never
                // wedges the whole account.
                Err(_) => continue,
            };

            // Report the server's total message count so the application can
            // tell whether older mail remains on the server (paging) even after
            // a bounded window. `unread` is left to the application to recompute
            // from the cached flags. An idempotent re-upsert for an already
            // known collection is harmless.
            changes.push(MailChange::UpsertCollection(CollectionRecord {
                remote_id: identity::collection_remote_id(&name),
                name: name.clone(),
                kind: folder.kind,
                delimiter: folder.delimiter.clone(),
                total: Some(folder_state.exists),
                unread: None,
            }));

            let mut prior = state.progress(&name).cloned();
            // A changed UIDVALIDITY invalidates everything cached under the old
            // epoch for *this* mailbox only; the same UID now names a different
            // message, so its old identity is retired.
            if let Some(progress) = &prior
                && progress.uidvalidity != folder_state.uidvalidity
            {
                for known in &progress.known {
                    changes.push(MailChange::RemoveMessage(identity::message_remote_id(
                        &name,
                        progress.uidvalidity,
                        Uid(known.uid),
                    )));
                }
                prior = None;
            }

            let progress = match prior {
                None => {
                    let new_arrival = account_first_sync && folder.kind == FolderKind::Inbox;
                    self.bootstrap(&name, folder_state, new_arrival, &request, &mut changes)
                        .await?
                }
                Some(prior) => {
                    self.delta(&name, folder.kind, folder_state, prior, &mut changes)
                        .await?
                }
            };
            state.set_progress(progress);
        }

        // A requested mailbox has now either been adopted (progress recorded)
        // or does not exist / could not be selected; either way the request is
        // spent and a caller re-requests if it still wants the folder.
        state.requested.clear();

        Ok(ChangePage {
            changes,
            checkpoint: encode_sync_cursor(&state)?,
            // The whole account is paged in one pass; bounding large mailboxes
            // into multiple pages is deferred to the scheduler in F7.
            has_more: false,
        })
    }
}

impl ImapSync {
    /// First-time (or post-reset) sync of one mailbox: fetch the newest
    /// `initial_limit` messages and record whether that covered the whole
    /// mailbox.
    async fn bootstrap(
        &mut self,
        name: &str,
        folder_state: FolderState,
        new_arrival: bool,
        request: &SyncRequest,
        changes: &mut Vec<MailChange>,
    ) -> Result<MailboxProgress, ProviderError> {
        let window = request.initial_limit.max(1);
        let (from, to) = bootstrap_window(folder_state.uidnext, window);
        let mut fetched = if from <= to {
            self.transport
                .fetch_headers(UidRange { from, to })
                .await
                .map_err(transport_error)?
        } else {
            Vec::new()
        };
        fetched.sort_unstable_by_key(|message| message.uid);
        let known: Vec<KnownUid> = fetched
            .iter()
            .map(|message| KnownUid {
                uid: message.uid.0,
                flags: message.flags,
            })
            .collect();
        for message in fetched {
            // The account's first inbox bootstrap reports mail the user is
            // seeing for the first time; any other bootstrap (a UIDVALIDITY
            // reset or a later adoption) is pre-existing mail and never a new
            // arrival.
            changes.push(upsert(name, folder_state.uidvalidity, message, new_arrival));
        }
        Ok(MailboxProgress {
            name: name.to_string(),
            uidvalidity: folder_state.uidvalidity,
            uidnext: folder_state.uidnext,
            known,
            // Reaching UID 1 means every UID in the epoch was seen.
            snapshot_complete: from == Uid(1),
        })
    }

    /// Incremental sync of one mailbox: fetch UIDs above the high-water mark,
    /// then reconcile flags and expunges for the already-known set.
    async fn delta(
        &mut self,
        name: &str,
        kind: FolderKind,
        folder_state: FolderState,
        prior: MailboxProgress,
        changes: &mut Vec<MailChange>,
    ) -> Result<MailboxProgress, ProviderError> {
        // Fetch and report mail above the high-water mark first. Only inbox mail
        // is a new arrival worth notifying about.
        let (from, to) = delta_window(prior.uidnext, folder_state.uidnext);
        let mut new_mail = if from <= to {
            self.transport
                .fetch_headers(UidRange { from, to })
                .await
                .map_err(transport_error)?
        } else {
            Vec::new()
        };
        new_mail.sort_unstable_by_key(|message| message.uid);
        for message in &new_mail {
            changes.push(upsert(
                name,
                folder_state.uidvalidity,
                message.clone(),
                kind == FolderKind::Inbox,
            ));
        }

        // Reconcile the flags of messages we already reported. A message the
        // FETCH response no longer lists has been expunged from the mailbox and
        // must not linger in the known set.
        let mut known: Vec<KnownUid> = Vec::new();
        if !prior.known.is_empty() {
            let uids: Vec<Uid> = prior.known.iter().map(|known| Uid(known.uid)).collect();
            let headers = self
                .transport
                .fetch_learning_headers(&uids)
                .await
                .map_err(transport_error)?;
            let returned: HashSet<u32> = headers.iter().map(|message| message.uid.0).collect();
            for message in &headers {
                let old = prior.known.iter().find(|known| known.uid == message.uid.0);
                if old.is_some_and(|old| old.flags != message.flags) {
                    // A full record, so an unmentioned flag/membership is not
                    // erased; only a genuine flag change is emitted.
                    changes.push(upsert(
                        name,
                        folder_state.uidvalidity,
                        message.clone(),
                        false,
                    ));
                }
                known.push(KnownUid {
                    uid: message.uid.0,
                    flags: message.flags,
                });
            }
            for old in &prior.known {
                if !returned.contains(&old.uid) {
                    changes.push(MailChange::RemoveMessage(identity::message_remote_id(
                        name,
                        folder_state.uidvalidity,
                        Uid(old.uid),
                    )));
                }
            }
        }
        for message in new_mail {
            known.push(KnownUid {
                uid: message.uid.0,
                flags: message.flags,
            });
        }

        known.sort_unstable_by_key(|entry| entry.uid);
        known.dedup_by_key(|entry| entry.uid);
        Ok(MailboxProgress {
            name: name.to_string(),
            uidvalidity: folder_state.uidvalidity,
            uidnext: folder_state.uidnext,
            known,
            // A delta never widens what is known, so it cannot complete an
            // earlier bounded bootstrap.
            snapshot_complete: prior.snapshot_complete,
        })
    }
}

/// Build a provider [`CollectionRecord`] from a listed folder.
fn collection_record(folder: &RemoteFolder) -> CollectionRecord {
    CollectionRecord {
        remote_id: identity::collection_remote_id(&folder.name),
        name: folder.name.clone(),
        kind: folder.kind,
        delimiter: folder.delimiter.clone(),
        total: None,
        unread: None,
    }
}

/// Build a full [`MailChange::UpsertMessage`] from one fetched header.
fn upsert(
    mailbox: &str,
    uidvalidity: u32,
    message: RemoteMessage,
    is_new_arrival: bool,
) -> MailChange {
    let mut header = message.header;
    // The transport-reported size is authoritative over the header parser.
    header.size = message.size;
    MailChange::UpsertMessage {
        message: MessageRecord {
            remote_id: identity::message_remote_id(mailbox, uidvalidity, message.uid),
            remote_thread: None,
            header,
            flags: message.flags,
            collections: vec![identity::collection_remote_id(mailbox)],
            snippet: String::new(),
            has_attachments: false,
        },
        is_new_arrival,
    }
}

fn encode_sync_cursor(state: &ImapSyncState) -> Result<ProviderCursor, ProviderError> {
    let payload = serde_json::to_string(state).map_err(|error| {
        ProviderError::transient("could not encode the sync cursor")
            .with_source_detail(error.to_string())
    })?;
    Ok(ProviderCursor::new(SYNC_CURSOR_VERSION, payload))
}
