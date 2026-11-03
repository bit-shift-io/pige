//! The process-wide API state. UI shells and the Android bridge call these
//! functions; each mutation returns a fresh [`ModelSnapshot`] and emits a
//! [`CoreEvent`] for other subscribers.
//!
//! Locks are held only around synchronous SQLite work. Network waits happen
//! after the lock is released, and sync uses its own connection so a snapshot
//! never blocks behind a slow server.

pub mod events;
pub mod snapshot;

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;

use crate::account::{Account, AccountId, AuthKind, ServerConfig};
use crate::mail::{Folder, FolderId, FolderKind, MessageId, StoredMessage, Uid};
use crate::platform::{CredentialStore, Notifier, Opener, TransportFactory};
use crate::store::Store;
use crate::sync::engine::temporary_thread_id;
use crate::sync::sync_account as run_sync_account;
use crate::transport::{RemoteFolder, UidRange};

pub use events::{CoreEvent, subscribe};
pub use snapshot::{
    AccountSection, AccountSummary, ConversationDetail, ConversationSummary, FolderSummary,
    MessageSummary, ModelSnapshot, SyncStatus,
};

/// Database filename inside [`Config::data_dir`].
pub const DB_FILE_NAME: &str = "pige.db";
/// Conversations shown before the user asks for older mail.
const INITIAL_PAGE: u32 = 300;
/// Conversations added by one `load_older` call.
const PAGE_SIZE: u32 = 100;
/// Headers fetched per folder on a first sync.
const SYNC_WINDOW: u32 = 300;

/// Everything the core needs from the host process. Built once per process by
/// the desktop or Android shell and handed to [`init`].
#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub credentials: Arc<dyn CredentialStore>,
    pub notifier: Arc<dyn Notifier>,
    pub opener: Arc<dyn Opener>,
    pub transport: Arc<dyn TransportFactory>,
}

/// User-supplied fields when adding an account. The id and sort order are
/// assigned by the core; the password goes to the credential store, never to
/// SQLite.
pub struct AccountInput {
    pub label: String,
    pub email: String,
    pub display_name: String,
    pub username: String,
    pub password: String,
    pub imap: ServerConfig,
    pub smtp: ServerConfig,
}

struct AppState {
    config: Config,
    store: Store,
    selected_folder: Option<FolderId>,
    conversation_limit: u32,
    selected_conversation: Option<ConversationDetail>,
    /// `Some(true/false)` once a server backfill has probed for older mail;
    /// `None` means fall back to comparing the server total with the cache.
    server_more: Option<bool>,
    status: SyncStatus,
    events: broadcast::Sender<CoreEvent>,
}

impl AppState {
    fn ensure_selection(&mut self) {
        if self.selected_folder.is_some() {
            return;
        }
        let Ok(accounts) = self.store.accounts() else {
            return;
        };
        for account in accounts {
            let Ok(folders) = self.store.folders(&account.id) else {
                continue;
            };
            if let Some(inbox) = folders
                .into_iter()
                .find(|folder| folder.kind == FolderKind::Inbox)
            {
                self.selected_folder = Some(inbox.id);
                return;
            }
        }
    }

    /// Read the current view from the store. The store returns everything in one
    /// read transaction so a concurrent sync cannot interleave sections,
    /// folders, and conversations. Store errors degrade to an empty view rather
    /// than failing a UI refresh.
    fn build_snapshot(&self) -> ModelSnapshot {
        let limit = self.conversation_limit.max(1);
        let view = match self.store.view(self.selected_folder.as_ref(), limit) {
            Ok(view) => view,
            Err(error) => {
                log::warn!("read snapshot view: {error}");
                return ModelSnapshot {
                    status: self.status.clone(),
                    ..ModelSnapshot::default()
                };
            }
        };

        let mut accounts = Vec::with_capacity(view.sections.len());
        let mut sections = Vec::with_capacity(view.sections.len());
        let mut unified_inbox_unread = 0;

        for section in view.sections {
            let mut section_unread = 0;
            let mut folder_summaries = Vec::with_capacity(section.folders.len());
            for folder in section.folders {
                section_unread += folder.unread;
                if folder.kind == FolderKind::Inbox {
                    unified_inbox_unread += folder.unread;
                }
                folder_summaries.push(FolderSummary {
                    id: folder.id,
                    name: folder.name,
                    kind: folder.kind,
                    unread: folder.unread,
                });
            }

            accounts.push(AccountSummary {
                id: section.account.id.clone(),
                label: section.account.label.clone(),
                email: section.account.email.clone(),
                unread: section_unread,
            });
            sections.push(AccountSection {
                account_id: section.account.id,
                label: section.account.label,
                email: section.account.email,
                unread: section_unread,
                folders: folder_summaries,
            });
        }

        let selected_folder = self.selected_folder.clone();
        let cached_has_older = view.selected_total > view.selected_cached;
        let has_server_more = self.server_more.unwrap_or(cached_has_older);
        let has_more = view.has_more_local || has_server_more;

        let conversations = view
            .conversations
            .into_iter()
            .map(|conversation| ConversationSummary {
                thread_id: conversation.thread_id,
                account_id: conversation.account_id,
                subject: conversation.subject,
                sender: conversation.sender,
                snippet: conversation.snippet,
                date: conversation.date,
                unread: conversation.unread > 0,
                message_count: conversation.message_count,
                has_attachments: conversation.has_attachments,
            })
            .collect();

        ModelSnapshot {
            accounts,
            sections,
            unified_inbox_unread,
            selected_folder,
            conversations,
            selected_conversation: self.selected_conversation.clone(),
            status: self.status.clone(),
            has_more,
        }
    }
}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Serializes sync runs so overlapping calls cannot double-fetch the same UIDs
/// or race SQLite writes. Held across network awaits, so it is a tokio mutex.
static SYNC_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    // A panic in one operation must not wedge every later call; recover the
    // guarded state instead of propagating poisoning.
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clear the global state. Intended for test isolation only; production shells
/// call [`init`] exactly once and should not use this.
#[doc(hidden)]
pub fn reset_for_tests() {
    *lock_state() = None;
}

fn database_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILE_NAME)
}

/// Open and migrate the cache, then install the global state. Returns an error
/// if the process is already initialized; tests call [`reset_for_tests`] first.
pub fn init(config: Config) -> Result<(), String> {
    if lock_state().is_some() {
        return Err("pige api is already initialized".to_string());
    }

    std::fs::create_dir_all(&config.data_dir)
        .map_err(|error| format!("create data dir: {error}"))?;
    std::fs::create_dir_all(&config.cache_dir)
        .map_err(|error| format!("create cache dir: {error}"))?;

    let store = Store::open(database_path(&config.data_dir))
        .map_err(|error| format!("open store: {error}"))?;
    store
        .migrate()
        .map_err(|error| format!("migrate store: {error}"))?;

    let sender = events::sender().clone();
    let mut state = AppState {
        config,
        store,
        selected_folder: None,
        conversation_limit: INITIAL_PAGE,
        selected_conversation: None,
        server_more: None,
        status: SyncStatus::Idle,
        events: sender.clone(),
    };
    state.ensure_selection();

    *lock_state() = Some(state);
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(())
}

/// The current view, built from the cache and the current selection.
pub fn snapshot() -> ModelSnapshot {
    let guard = lock_state();
    match guard.as_ref() {
        Some(state) => state.build_snapshot(),
        None => ModelSnapshot::default(),
    }
}

/// Persist an account and its password. The password is written through the
/// platform credential store; the SQLite row contains no secret.
pub async fn add_account(input: AccountInput) -> Result<ModelSnapshot, String> {
    let sender;
    let snapshot;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        let sort_order = state
            .store
            .accounts()
            .map_err(|error| format!("read accounts: {error}"))?
            .len() as i64;
        let id = AccountId::new();
        let account = Account {
            id: id.clone(),
            label: input.label,
            email: input.email.clone(),
            display_name: input.display_name,
            username: if input.username.is_empty() {
                input.email
            } else {
                input.username
            },
            imap: input.imap,
            smtp: input.smtp,
            auth: AuthKind::Password,
            sort_order,
        };

        state
            .config
            .credentials
            .set(&id, &input.password)
            .map_err(|error| format!("store password: {error}"))?;
        if let Err(error) = state.store.upsert_account(&account) {
            if let Err(rollback) = state.config.credentials.delete(&id) {
                log::error!(
                    "rollback credential for {id} after account insert failure: {rollback}"
                );
                return Err(format!(
                    "store account: {error}; credential rollback failed: {rollback}"
                ));
            }
            return Err(format!("store account: {error}"));
        }

        state.ensure_selection();
        sender = state.events.clone();
        snapshot = state.build_snapshot();
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

/// Sync every account. One failing account emits an error but never stops the
/// others; unread rollups in the returned snapshot cover all of them.
pub async fn sync_all() -> Result<ModelSnapshot, String> {
    let _sync = SYNC_LOCK.lock().await;
    let (config, accounts, sender) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let accounts = state
            .store
            .accounts()
            .map_err(|error| format!("read accounts: {error}"))?;
        state.status = SyncStatus::Syncing;
        let config = state.config.clone();
        let sender = state.events.clone();
        (config, accounts, sender)
    };

    let _ = sender.send(CoreEvent::SnapshotChanged);
    let outcome = sync_accounts(&config, &accounts, &sender).await;

    let status = finish_sync(accounts.len(), outcome);
    let snapshot = snapshot();
    let _ = sender.send(CoreEvent::SnapshotChanged);
    if let SyncStatus::Error(message) = status {
        return Err(message);
    }
    Ok(snapshot)
}

/// Sync one account by id. Unknown ids are an error; a failed connection is
/// reported through events and the returned snapshot's status.
pub async fn sync_account(id: &AccountId) -> Result<ModelSnapshot, String> {
    let _sync = SYNC_LOCK.lock().await;
    let (config, account, sender) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let account = state
            .store
            .accounts()
            .map_err(|error| format!("read accounts: {error}"))?
            .into_iter()
            .find(|account| &account.id == id)
            .ok_or_else(|| format!("unknown account {id}"))?;
        state.status = SyncStatus::Syncing;
        (state.config.clone(), account, state.events.clone())
    };

    let _ = sender.send(CoreEvent::SnapshotChanged);
    let accounts = std::slice::from_ref(&account);
    let outcome = sync_accounts(&config, accounts, &sender).await;

    let status = finish_sync(accounts.len(), outcome);
    let snapshot = snapshot();
    let _ = sender.send(CoreEvent::SnapshotChanged);
    if let SyncStatus::Error(message) = status {
        return Err(message);
    }
    Ok(snapshot)
}

/// Select a folder and reset the conversation page.
pub async fn select_folder(id: &FolderId) -> Result<ModelSnapshot, String> {
    let sender;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        state.selected_folder = Some(id.clone());
        state.conversation_limit = INITIAL_PAGE;
        state.selected_conversation = None;
        state.server_more = None;
        sender = state.events.clone();
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot())
}

/// Extend the visible conversation list by one page. Cached rows are used
/// first; when the cache is exhausted the next older headers are backfilled
/// from the server through the injected transport.
pub async fn load_older(folder: &FolderId) -> Result<ModelSnapshot, String> {
    let (request, sender) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        if state.selected_folder.as_ref() != Some(folder) {
            state.selected_folder = Some(folder.clone());
            state.conversation_limit = INITIAL_PAGE;
            state.selected_conversation = None;
            state.server_more = None;
        }

        let limit = state.conversation_limit.max(1);
        let candidate_limit = limit.saturating_add(PAGE_SIZE);
        let candidate = state
            .store
            .conversations(folder, candidate_limit, None)
            .map_err(|error| format!("read conversations: {error}"))?;

        if candidate.len() > limit as usize {
            state.conversation_limit = candidate.len() as u32;
            (None, state.events.clone())
        } else {
            let (account, folder_row) = locate_folder(&state.store, folder)?;
            let oldest = state
                .store
                .messages(folder, u32::MAX, None)
                .map_err(|error| format!("read cached messages: {error}"))?
                .iter()
                .map(|message| message.uid)
                .min();
            let sender = state.events.clone();
            (
                Some(BackfillRequest {
                    config: state.config.clone(),
                    account,
                    folder: folder_row,
                    oldest,
                    limit,
                }),
                sender,
            )
        }
    };

    let Some(request) = request else {
        let _ = sender.send(CoreEvent::SnapshotChanged);
        return Ok(snapshot());
    };

    let store = Store::open(database_path(&request.config.data_dir))
        .map_err(|error| format!("open backfill store: {error}"))?;

    match backfill_older(
        &request.config,
        &store,
        &request.account,
        &request.folder,
        request.oldest,
    )
    .await
    {
        Ok((stored, server_more)) => {
            let mut guard = lock_state();
            if let Some(state) = guard.as_mut()
                && state.selected_folder.as_ref() == Some(&request.folder.id)
                && state.conversation_limit == request.limit
            {
                state.conversation_limit = state.conversation_limit.saturating_add(stored as u32);
                state.server_more = Some(server_more);
            }
            drop(guard);
            let _ = sender.send(CoreEvent::SnapshotChanged);
            Ok(snapshot())
        }
        Err(error) => {
            let _ = sender.send(CoreEvent::Error(error.clone()));
            let _ = sender.send(CoreEvent::SnapshotChanged);
            Err(error)
        }
    }
}

struct BackfillRequest {
    config: Config,
    account: Account,
    folder: Folder,
    oldest: Option<Uid>,
    /// Visible page size when the backfill started; used to detect a selection
    /// or paging change while the network wait was in flight.
    limit: u32,
}

fn locate_folder(store: &Store, id: &FolderId) -> Result<(Account, Folder), String> {
    for account in store
        .accounts()
        .map_err(|error| format!("read accounts: {error}"))?
    {
        let folders = store
            .folders(&account.id)
            .map_err(|error| format!("read folders: {error}"))?;
        if let Some(folder) = folders.into_iter().find(|folder| &folder.id == id) {
            return Ok((account, folder));
        }
    }
    Err(format!("unknown folder {id}"))
}

/// Fetch up to one page of headers below `oldest`, storing them with the sync
/// engine's temporary per-message thread ids. Returns `(stored, server_more)`.
async fn backfill_older(
    config: &Config,
    store: &Store,
    account: &Account,
    folder: &Folder,
    oldest: Option<Uid>,
) -> Result<(usize, bool), String> {
    let oldest = oldest.ok_or_else(|| "no cached mail to page from".to_string())?;
    let mut transport = config
        .transport
        .imap(account)
        .await
        .map_err(|error| format!("{}: {error}", account.label))?;

    let remote = RemoteFolder {
        name: folder.name.clone(),
        delimiter: folder.delimiter.clone(),
        kind: folder.kind,
    };
    let state = transport
        .select(&remote)
        .await
        .map_err(|error| format!("select {}: {error}", folder.name))?;

    if let Some(prior) = folder.uidvalidity
        && prior != state.uidvalidity
    {
        return Err(
            "folder changed on the server; run a sync before loading older mail".to_string(),
        );
    }
    let uidvalidity = state.uidvalidity;

    // Fetch one extra message so `server_more` is known without another round
    // trip. The range walk handles sparse UIDs, not just dense UID sequences.
    let want = PAGE_SIZE as usize;
    let mut found = Vec::new();
    let mut cursor = oldest.0.saturating_sub(1);
    while found.len() <= want && cursor >= 1 {
        let remaining = (want + 1 - found.len()) as u32;
        let from = cursor.saturating_sub(remaining.saturating_sub(1)).max(1);
        let batch = transport
            .fetch_headers(UidRange {
                from: Uid(from),
                to: Uid(cursor),
            })
            .await
            .map_err(|error| format!("fetch older in {}: {error}", folder.name))?;
        found.extend(batch.into_iter().filter(|message| message.uid < oldest));
        if from == 1 {
            break;
        }
        cursor = from - 1;
    }

    found.sort_unstable_by_key(|message| Reverse(message.uid));
    let server_more = found.len() > want;
    found.truncate(want);

    for message in &found {
        let crate::transport::RemoteMessage {
            uid,
            header,
            flags,
            size,
        } = message.clone();
        let mut header = header;
        header.size = size;
        let stored = StoredMessage {
            id: MessageId::new(),
            account_id: account.id.clone(),
            folder_id: folder.id.clone(),
            uid,
            uidvalidity,
            header,
            flags,
            thread_id: temporary_thread_id(&account.id, &folder.id, uidvalidity, uid),
            snippet: String::new(),
            has_attachments: false,
        };
        store
            .upsert_message(&stored)
            .map_err(|error| format!("store older {}: {error}", folder.name))?;
    }

    // Older headers can carry unread flags, so refresh the folder rollups too.
    let all = store
        .messages(&folder.id, u32::MAX, None)
        .map_err(|error| format!("read {}: {error}", folder.name))?;
    let unread = all.iter().filter(|message| !message.flags.seen).count() as u32;
    let mut updated = folder.clone();
    updated.unread = unread;
    updated.total = state.exists;
    updated.uidvalidity = Some(uidvalidity);
    store
        .upsert_folder(&updated)
        .map_err(|error| format!("store folder {}: {error}", folder.name))?;

    Ok((found.len(), server_more))
}

struct SyncOutcome {
    succeeded: usize,
    failed: usize,
    last_error: Option<String>,
}

/// Run the per-account loop. The helper opens its own connection so the global
/// state lock is never held across network awaits.
async fn sync_accounts(
    config: &Config,
    accounts: &[Account],
    sender: &broadcast::Sender<CoreEvent>,
) -> SyncOutcome {
    let store = match Store::open(database_path(&config.data_dir)) {
        Ok(store) => store,
        Err(error) => {
            let message = format!("open sync store: {error}");
            for account in accounts {
                let _ = sender.send(CoreEvent::SyncStarted {
                    account: account.id.clone(),
                });
                let _ = sender.send(CoreEvent::Error(format!("{}: {message}", account.label)));
                let _ = sender.send(CoreEvent::SyncFinished {
                    account: account.id.clone(),
                    new_mail: 0,
                });
            }
            let _ = sender.send(CoreEvent::SnapshotChanged);
            return SyncOutcome {
                succeeded: 0,
                failed: accounts.len(),
                last_error: Some(message),
            };
        }
    };

    let mut outcome = SyncOutcome {
        succeeded: 0,
        failed: 0,
        last_error: None,
    };

    for account in accounts {
        let _ = sender.send(CoreEvent::SyncStarted {
            account: account.id.clone(),
        });

        match sync_one(&store, config, account, sender).await {
            Ok(()) => outcome.succeeded += 1,
            Err(message) => {
                outcome.failed += 1;
                outcome.last_error = Some(message.clone());
                let _ = sender.send(CoreEvent::Error(message));
                let _ = sender.send(CoreEvent::SyncFinished {
                    account: account.id.clone(),
                    new_mail: 0,
                });
            }
        }

        let _ = sender.send(CoreEvent::SnapshotChanged);
    }

    outcome
}

async fn sync_one(
    store: &Store,
    config: &Config,
    account: &Account,
    sender: &broadcast::Sender<CoreEvent>,
) -> Result<(), String> {
    let mut transport = config
        .transport
        .imap(account)
        .await
        .map_err(|error| format!("{}: {error}", account.label))?;

    let report = run_sync_account(store, account, transport.as_mut(), SYNC_WINDOW)
        .await
        .map_err(|error| format!("{}: {error}", account.label))?;

    for mail in &report.new_inbox {
        config.notifier.notify(mail);
        let _ = sender.send(CoreEvent::NewMail(mail.clone()));
    }
    for error in &report.errors {
        let _ = sender.send(CoreEvent::Error(format!("{}: {error}", account.label)));
    }

    let _ = sender.send(CoreEvent::SyncFinished {
        account: account.id.clone(),
        new_mail: report.new_inbox.len(),
    });
    Ok(())
}

/// Update status and re-seed the selection after a sync. Called without any
/// lock held; the caller emits the final snapshot notification.
fn finish_sync(attempted: usize, outcome: SyncOutcome) -> SyncStatus {
    let status = if outcome.failed == 0 || outcome.succeeded > 0 || attempted == 0 {
        SyncStatus::Idle
    } else {
        SyncStatus::Error(
            outcome
                .last_error
                .unwrap_or_else(|| "sync failed".to_string()),
        )
    };

    let mut guard = lock_state();
    if let Some(state) = guard.as_mut() {
        state.status = status.clone();
        state.server_more = None;
        state.ensure_selection();
    }
    status
}
