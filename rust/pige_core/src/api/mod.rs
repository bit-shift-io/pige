//! The process-wide API state. UI shells and the Android bridge call these
//! functions; each mutation returns a fresh [`ModelSnapshot`] and emits a
//! [`CoreEvent`] for other subscribers.
//!
//! Locks are held only around synchronous SQLite work. Network waits happen
//! after the lock is released, and sync uses its own connection so a snapshot
//! never blocks behind a slow server.

pub mod events;
pub mod snapshot;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::account::{Account, AccountId, AuthKind, ServerConfig};
use crate::mail::{FolderId, FolderKind};
use crate::platform::{CredentialStore, Notifier, Opener, TransportFactory};
use crate::store::Store;
use crate::sync::sync_account as run_sync_account;

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

    /// Read the current view from the store. Store errors degrade to an empty
    /// section rather than failing a UI refresh.
    fn build_snapshot(&self) -> ModelSnapshot {
        let mut accounts = Vec::new();
        let mut sections = Vec::new();
        let mut unified_inbox_unread = 0;

        match self.store.accounts() {
            Ok(stored_accounts) => {
                for account in stored_accounts {
                    let folders = match self.store.folders(&account.id) {
                        Ok(folders) => folders,
                        Err(error) => {
                            log::warn!("read folders for {}: {error}", account.id);
                            Vec::new()
                        }
                    };

                    let mut section_unread = 0;
                    let mut folder_summaries = Vec::with_capacity(folders.len());
                    for folder in folders {
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
                        id: account.id.clone(),
                        label: account.label.clone(),
                        email: account.email.clone(),
                        unread: section_unread,
                    });
                    sections.push(AccountSection {
                        account_id: account.id,
                        label: account.label,
                        email: account.email,
                        unread: section_unread,
                        folders: folder_summaries,
                    });
                }
            }
            Err(error) => log::warn!("read accounts for snapshot: {error}"),
        }

        let limit = self.conversation_limit.max(1);
        let mut conversations = Vec::new();
        let mut has_more = false;
        if let Some(folder_id) = &self.selected_folder {
            match self.store.conversations(folder_id, limit + 1, None) {
                Ok(mut rows) => {
                    has_more = rows.len() > limit as usize;
                    rows.truncate(limit as usize);
                    conversations = rows
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
                }
                Err(error) => log::warn!("read conversations for {folder_id}: {error}"),
            }
        }

        ModelSnapshot {
            accounts,
            sections,
            unified_inbox_unread,
            selected_folder: self.selected_folder.clone(),
            conversations,
            selected_conversation: self.selected_conversation.clone(),
            status: self.status.clone(),
            has_more,
        }
    }
}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    // A panic in one operation must not wedge every later call; recover the
    // guarded state instead of propagating poisoning.
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn database_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILE_NAME)
}

/// Open and migrate the cache, then install the global state. Production calls
/// this once; tests call it again with a fresh directory to isolate cases.
pub fn init(config: Config) -> Result<(), String> {
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
            let _ = state.config.credentials.delete(&id);
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
    let (config, accounts, sender) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        state.status = SyncStatus::Syncing;
        let config = state.config.clone();
        let accounts = state
            .store
            .accounts()
            .map_err(|error| format!("read accounts: {error}"))?;
        let sender = state.events.clone();
        (config, accounts, sender)
    };

    let _ = sender.send(CoreEvent::SnapshotChanged);
    let outcome = sync_accounts(&config, &accounts, &sender).await;

    finish_sync(accounts.len(), outcome);
    let snapshot = snapshot();
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

/// Sync one account by id. Unknown ids are an error; a failed connection is
/// reported through events and the returned snapshot's status.
pub async fn sync_account(id: &AccountId) -> Result<ModelSnapshot, String> {
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

    finish_sync(accounts.len(), outcome);
    let snapshot = snapshot();
    let _ = sender.send(CoreEvent::SnapshotChanged);
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
        sender = state.events.clone();
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot())
}

/// Extend the current conversation list by one page using the store's date
/// cursor. Selecting a different folder resets to the first page.
pub async fn load_older(folder: &FolderId) -> Result<ModelSnapshot, String> {
    let sender;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        if state.selected_folder.as_ref() != Some(folder) {
            state.selected_folder = Some(folder.clone());
            state.conversation_limit = INITIAL_PAGE;
            state.selected_conversation = None;
        }

        let limit = state.conversation_limit.max(1);
        let current = state
            .store
            .conversations(folder, limit, None)
            .map_err(|error| format!("read conversations: {error}"))?;

        if current.len() >= limit as usize
            && let Some(cursor) = current.last().map(|conversation| conversation.date)
        {
            let older = state
                .store
                .conversations(folder, PAGE_SIZE, Some(cursor))
                .map_err(|error| format!("read older conversations: {error}"))?;
            if !older.is_empty() {
                state.conversation_limit = limit.saturating_add(older.len() as u32);
            }
        }

        sender = state.events.clone();
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot())
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
            let _ = sender.send(CoreEvent::Error(message.clone()));
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
fn finish_sync(attempted: usize, outcome: SyncOutcome) {
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
        state.status = status;
        state.ensure_selection();
    }
}
