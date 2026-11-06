//! The process-wide API state. UI shells and the Android bridge call these
//! functions; each mutation returns a fresh [`ModelSnapshot`] and emits a
//! [`CoreEvent`] for other subscribers.
//!
//! Locks are held only around synchronous SQLite work. Network waits happen
//! after the lock is released, and sync uses its own connection so a snapshot
//! never blocks behind a slow server.

pub mod events;
pub mod mutations;
pub mod snapshot;

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;

use crate::account::{Account, AccountId, AuthKind, ServerConfig};
use crate::config::{self, ConfigDocument};
use crate::html::{MailDocument, parse_html, plain_text_document};
use crate::mail::{
    AttachmentPart, Folder, FolderId, FolderKind, MessageId, StoredMessage, ThreadAssigner,
    ThreadId, Uid, attachment_bytes, cache_attachment, check_attachment_size, extract_attachments,
    sender_display, thread::participants_of,
};
use crate::platform::{BackupStore, CredentialStore, Notifier, Opener, TransportFactory};
use crate::search::{SearchHit, SearchScope};
use crate::send::{
    self, OutboxEntry, OutboxId, OutgoingMessage, compose::forward_message, compose::reply_message,
};
use crate::store::{Body, Store};
use crate::sync::sync_account as run_sync_account;
use crate::transport::{Capabilities, RemoteFolder, UidRange};

pub use crate::settings::{Settings, ThemeMode};
pub use events::{CoreEvent, back_pressed, subscribe};
pub use mutations::{
    RECONNECT_ERROR, archive_available, archive_thread, delete_permanently, move_thread, set_read,
    trash_thread,
};
pub use snapshot::{
    AccountSection, AccountSummary, ConversationDetail, ConversationSummary, FolderSummary,
    MessageBody, MessageSummary, ModelSnapshot, SyncStatus,
};

/// Database filename inside [`Config::data_dir`].
pub const DB_FILE_NAME: &str = "pige.db";
/// Conversations shown before the user asks for older mail.
const INITIAL_PAGE: u32 = 300;
/// Conversations added by one `load_older` call.
const PAGE_SIZE: u32 = 100;
/// Search hits returned to one query. One hit per conversation, so this is a
/// generous cap for the list screen.
const SEARCH_LIMIT: u32 = 100;
/// Headers fetched per folder on a first sync.
const SYNC_WINDOW: u32 = 300;
/// The largest raw message body the reader will download, in bytes. Messages
/// whose cached `RFC822.SIZE` exceeds this cap are opened without a body and
/// shown a "message too large" state instead of fetching hundreds of MiB.
/// This is separate from the sanitizer's 2 MiB HTML input cap.
const MAX_BODY_BYTES: u32 = 10 * 1024 * 1024;

/// The result of submitting a message: the refreshed cache view plus whether it
/// is still waiting in the outbox for a network connection.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub snapshot: ModelSnapshot,
    pub queued: bool,
}

/// The result of a configuration change that also replaces settings (restore
/// and factory reset). Settings are not part of [`ModelSnapshot`], so the shell
/// needs them here to refresh the theme and reschedule background sync.
#[derive(Debug, Clone)]
pub struct ConfigOutcome {
    pub snapshot: ModelSnapshot,
    pub settings: Settings,
    /// Non-fatal problems, such as a password that could not be cleared after
    /// a restore or reset. The shell appends these to its result message.
    pub warnings: Vec<String>,
}

/// Everything the core needs from the host process. Built once per process by
/// the desktop or Android shell and handed to [`init`].
#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Where `pige.json` lives (`$XDG_CONFIG_HOME/bitshift/pige` on Linux, an
    /// app-private directory on Android).
    pub config_dir: PathBuf,
    pub credentials: Arc<dyn CredentialStore>,
    pub notifier: Arc<dyn Notifier>,
    pub opener: Arc<dyn Opener>,
    pub transport: Arc<dyn TransportFactory>,
    /// Reads and writes configuration backups in the platform's Downloads.
    pub backups: Arc<dyn BackupStore>,
}

/// User-supplied fields when adding an account. The id and sort order are
/// assigned by the core; the password goes to the credential store, never to
/// SQLite.
#[derive(Clone)]
pub struct AccountInput {
    pub label: String,
    pub email: String,
    pub display_name: String,
    pub username: String,
    pub password: String,
    pub imap: ServerConfig,
    pub smtp: ServerConfig,
}

impl std::fmt::Debug for AccountInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountInput")
            .field("label", &self.label)
            .field("email", &self.email)
            .field("display_name", &self.display_name)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("imap", &self.imap)
            .field("smtp", &self.smtp)
            .finish()
    }
}

impl AccountInput {
    /// Validate the fields needed to create an account. The UI calls this via
    /// [`test_account`], and `add_account` re-checks it so no shell can bypass
    /// the rules.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_with_password(true)
    }

    /// Edits may leave the password blank to keep the stored secret.
    fn validate_for_edit(&self) -> Result<(), String> {
        self.validate_with_password(false)
    }

    fn validate_with_password(&self, require_password: bool) -> Result<(), String> {
        if self.label.trim().is_empty() {
            return Err("Account label is required".to_string());
        }
        let email = self.email.trim();
        if email.is_empty() {
            return Err("Email address is required".to_string());
        }
        let Some((local, domain)) = email.split_once('@') else {
            return Err("Email address is not valid".to_string());
        };
        if local.is_empty() || domain.is_empty() || domain.contains('@') {
            return Err("Email address is not valid".to_string());
        }
        if self.username.trim().is_empty() {
            return Err("Username is required".to_string());
        }
        if require_password && self.password.is_empty() {
            return Err("Password is required".to_string());
        }
        if self.imap.host.trim().is_empty() {
            return Err("IMAP host is required".to_string());
        }
        if self.imap.port == 0 {
            return Err("IMAP port is required".to_string());
        }
        if self.smtp.host.trim().is_empty() {
            return Err("SMTP host is required".to_string());
        }
        if self.smtp.port == 0 {
            return Err("SMTP port is required".to_string());
        }
        Ok(())
    }

    fn into_account(self, id: AccountId, sort_order: i64) -> Account {
        Account {
            id,
            label: self.label.trim().to_string(),
            email: self.email.trim().to_string(),
            display_name: self.display_name.trim().to_string(),
            username: if self.username.trim().is_empty() {
                self.email.trim().to_string()
            } else {
                self.username.trim().to_string()
            },
            imap: self.imap,
            smtp: self.smtp,
            auth: AuthKind::Password,
            sort_order,
        }
    }
}

struct AppState {
    config: Config,
    store: Store,
    /// The authoritative accounts and settings document. The SQLite accounts
    /// table is a derived mirror kept only so cascade deletes and joins work.
    config_doc: ConfigDocument,
    /// `<config_dir>/pige.json`.
    config_path: PathBuf,
    /// A one-time message for the shell (for example a quarantined document),
    /// taken with [`take_startup_warning`].
    startup_warning: Option<String>,
    /// Set when the document exists but could not be read. Saves are refused so
    /// an unreadable configuration is never overwritten.
    config_writes_blocked: bool,
    selected_folder: Option<FolderId>,
    unified_selected: bool,
    conversation_limit: u32,
    selected_conversation: Option<ConversationDetail>,
    /// Last search query, scope, results, and the generation used to reject
    /// slower completions from an older query.
    search: SearchState,
    /// `Some(true/false)` once a server backfill has probed for older mail;
    /// `None` means fall back to comparing the server total with the cache.
    server_more: Option<bool>,
    /// Server capabilities observed during the last sync, per account. Used by
    /// the UI to decide which mutation actions to offer.
    capabilities: HashMap<AccountId, Capabilities>,
    status: SyncStatus,
    events: broadcast::Sender<CoreEvent>,
}

/// Search state shared between the API and snapshots. Every new request takes a
/// generation; only the completion matching the newest generation is committed.
#[derive(Debug, Default)]
struct SearchState {
    generation: u64,
    query: String,
    scope: SearchScope,
    results: Vec<SearchHit>,
}

impl SearchState {
    fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Commit a completed search unless a newer one has started.
    fn complete(
        &mut self,
        generation: u64,
        query: String,
        scope: SearchScope,
        results: Vec<SearchHit>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.query = query;
        self.scope = scope;
        self.results = results;
        true
    }
}

impl AppState {
    /// Replace the account list, persisting the document first and then
    /// rebuilding the SQLite mirror. A failure leaves the in-memory document
    /// exactly as it was, so the UI cannot show an account the disk does not
    /// have. Callers own any credential rollback.
    fn replace_accounts(&mut self, accounts: Vec<Account>) -> Result<(), String> {
        self.ensure_writable()?;
        let previous = self.config_doc.accounts.clone();
        self.config_doc.accounts = accounts;
        if let Err(error) = config::save(&self.config_path, &self.config_doc) {
            self.config_doc.accounts = previous;
            return Err(format!("save configuration: {error}"));
        }
        if let Err(error) = self.store.sync_accounts(&self.config_doc.accounts) {
            // The document is durable but the mirror failed; put the document
            // back so the two cannot disagree after a restart.
            self.config_doc.accounts = previous;
            let _ = config::save(&self.config_path, &self.config_doc);
            return Err(format!("update account cache: {error}"));
        }
        Ok(())
    }

    /// Refuse configuration writes when `pige.json` exists but could not be
    /// read, so the unreadable file is never overwritten.
    fn ensure_writable(&self) -> Result<(), String> {
        if self.config_writes_blocked {
            return Err(
                "the configuration file could not be read and will not be overwritten".to_string(),
            );
        }
        Ok(())
    }

    fn ensure_selection(&mut self) {
        if self.selected_folder.is_some() || self.unified_selected {
            return;
        }
        let Ok(accounts) = self.store.accounts() else {
            return;
        };
        if accounts.is_empty() {
            return;
        }

        // The unified inbox is the default once at least one account exists.
        // Its conversation list is resolved from every account's Inbox.
        self.unified_selected = true;
    }

    /// Read the current view from the store. The store returns everything in one
    /// read transaction so a concurrent sync cannot interleave sections,
    /// folders, and conversations. Store errors degrade to an empty view rather
    /// than failing a UI refresh.
    fn build_snapshot(&self) -> ModelSnapshot {
        let limit = self.conversation_limit.max(1);
        let view =
            match self
                .store
                .view(self.selected_folder.as_ref(), self.unified_selected, limit)
            {
                Ok(view) => view,
                Err(error) => {
                    log::warn!("read snapshot view: {error}");
                    return ModelSnapshot {
                        status: self.status.clone(),
                        unified_selected: self.unified_selected,
                        search_query: self.search.query.clone(),
                        search_scope: self.search.scope.clone(),
                        search_results: self.search.results.clone(),
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
                account_id: section.account.id.clone(),
                label: section.account.label,
                email: section.account.email,
                unread: section_unread,
                capabilities: self
                    .capabilities
                    .get(&section.account.id)
                    .copied()
                    .unwrap_or_default(),
                folders: folder_summaries,
            });
        }

        let selected_folder = self.selected_folder.clone();
        let cached_has_older = view.selected_total > view.selected_cached;
        let has_server_more = self.server_more.unwrap_or(cached_has_older);
        // Server backfill for a merged inbox is deferred; the cached page still
        // knows whether another page exists locally.
        let has_more = view.has_more_local || (!self.unified_selected && has_server_more);

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

        let drafts = self.store.outbox_drafts(None).unwrap_or_default();

        ModelSnapshot {
            accounts,
            sections,
            unified_inbox_unread,
            unified_selected: self.unified_selected,
            selected_folder,
            conversations,
            selected_conversation: self.selected_conversation.clone(),
            search_query: self.search.query.clone(),
            search_scope: self.search.scope.clone(),
            search_results: self.search.results.clone(),
            status: self.status.clone(),
            has_more,
            drafts,
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

    // Android's `AndroidPaths::load` returns empty paths when app storage is
    // unavailable; refuse them rather than writing `pige.db` to the process
    // working directory.
    if config.data_dir.as_os_str().is_empty()
        || config.cache_dir.as_os_str().is_empty()
        || config.config_dir.as_os_str().is_empty()
    {
        return Err("data, cache, and configuration directories must not be empty".to_string());
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

    let config_path = config::document_path(&config.config_dir);
    let (config_doc, startup_warning, writes_blocked) =
        load_or_migrate_document(&store, &config_path);

    let sender = events::sender().clone();
    let mut state = AppState {
        config,
        store,
        config_doc,
        config_path,
        startup_warning,
        config_writes_blocked: writes_blocked,
        selected_folder: None,
        unified_selected: false,
        conversation_limit: INITIAL_PAGE,
        selected_conversation: None,
        search: SearchState::default(),
        server_more: None,
        capabilities: HashMap::new(),
        status: SyncStatus::Idle,
        events: sender.clone(),
    };
    state.ensure_selection();

    // Install the state first, then clear any subscriber backlog. The warning
    // waits in `startup_warning` for the shell to take it: sending it on the
    // event channel here would drop it, because the shell subscribes after
    // `init` returns.
    *lock_state() = Some(state);
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(())
}

/// Read the configuration document, or create it: from the SQLite cache on a
/// fresh install, and after quarantining an unreadable file. Returns the
/// document, a warning for the shell, and whether writes must be blocked
/// because a file exists that could not be read (so it is never overwritten).
fn load_or_migrate_document(store: &Store, path: &Path) -> (ConfigDocument, Option<String>, bool) {
    match config::load(path) {
        Ok(Some(doc)) => {
            if let Err(error) = store.sync_accounts(&doc.accounts) {
                log::warn!("sync account mirror from configuration: {error}");
            }
            (doc, None, false)
        }
        Ok(None) => {
            let doc = document_from_store(store);
            match config::save(path, &doc) {
                Ok(()) => {
                    // The document now owns settings; drop the legacy row.
                    let _ = crate::settings::clear_settings(store);
                    (doc, None, false)
                }
                Err(error) => {
                    log::warn!("write configuration document: {error}");
                    (
                        doc,
                        Some(format!(
                            "Could not write the configuration file ({error}); changes will not be saved."
                        )),
                        true,
                    )
                }
            }
        }
        Err(config::ConfigError::Json(error)) => {
            log::warn!("unreadable configuration: {error}");
            let doc = document_from_store(store);
            match config::quarantine(path) {
                Ok(_) => {
                    let _ = config::save(path, &doc);
                    let _ = crate::settings::clear_settings(store);
                    (
                        doc,
                        Some(
                            "The saved configuration could not be read and was set aside; pige started from its cache."
                                .to_string(),
                        ),
                        false,
                    )
                }
                Err(quarantine_error) => {
                    // Could not move the bad file aside; leave it and refuse
                    // to overwrite it.
                    log::warn!("quarantine configuration: {quarantine_error}");
                    (
                        doc,
                        Some(format!(
                            "The saved configuration could not be read ({error}) and could not be set aside ({quarantine_error}); changes will not be saved."
                        )),
                        true,
                    )
                }
            }
        }
        Err(config::ConfigError::Io(error)) => {
            // The file exists but could not be read. Leave it untouched and
            // refuse to overwrite it on any later write.
            log::warn!("could not read configuration: {error}");
            let doc = document_from_store(store);
            (
                doc,
                Some(format!(
                    "Could not read the configuration file ({error}); changes will not be saved."
                )),
                true,
            )
        }
        Err(config::ConfigError::NoRoot) => {
            let doc = document_from_store(store);
            (
                doc,
                Some("No configuration directory is available.".to_string()),
                true,
            )
        }
    }
}

/// Build a document from the existing SQLite cache, for the one-time upgrade
/// from the pre-config-document layout.
fn document_from_store(store: &Store) -> ConfigDocument {
    ConfigDocument {
        version: config::CURRENT_VERSION,
        accounts: store.accounts().unwrap_or_default(),
        settings: crate::settings::load_settings(store),
    }
}

/// The current view, built from the cache and the current selection.
pub fn snapshot() -> ModelSnapshot {
    let guard = lock_state();
    match guard.as_ref() {
        Some(state) => state.build_snapshot(),
        None => ModelSnapshot::default(),
    }
}

/// Whether [`init`] has installed the process-wide state.
pub fn is_initialized() -> bool {
    lock_state().is_some()
}

/// Take the one-time start-up warning (for example a quarantined
/// configuration), so the shell can show it. Returns `None` once taken and on
/// every later call.
pub fn take_startup_warning() -> Option<String> {
    let mut guard = lock_state();
    guard
        .as_mut()
        .and_then(|state| state.startup_warning.take())
}

/// The persisted user settings, from the configuration document.
pub fn settings() -> Settings {
    let guard = lock_state();
    match guard.as_ref() {
        Some(state) => state.config_doc.settings.clone(),
        None => Settings::default(),
    }
}

/// Persist user settings into the configuration document, normalising the sync
/// interval first. A failed write leaves the in-memory settings unchanged.
pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let mut guard = lock_state();
    let state = guard
        .as_mut()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state.ensure_writable()?;
    let mut settings = settings.clone();
    settings.sync_minutes = Settings::clamp_minutes(settings.sync_minutes);
    let previous = state.config_doc.settings.clone();
    state.config_doc.settings = settings;
    if let Err(error) = config::save(&state.config_path, &state.config_doc) {
        state.config_doc.settings = previous;
        return Err(format!("save settings: {error}"));
    }
    Ok(())
}

/// Write the accounts and settings to a dated backup file in the platform's
/// Downloads folder and return its name. Passwords are never included.
pub async fn backup_config() -> Result<String, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    if !state.config.backups.available() {
        return Err("there is no Downloads folder to save a backup in".to_string());
    }
    let bytes = serde_json::to_vec_pretty(&state.config_doc)
        .map_err(|error| format!("serialise configuration: {error}"))?;
    let name = crate::platform::backup::backup_name(now_ms());
    state
        .config
        .backups
        .write(&name, &bytes)
        .map_err(|error| format!("save backup: {error}"))?;
    Ok(name)
}

/// The backup files in Downloads, newest first.
pub async fn list_backups() -> Result<Vec<String>, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state
        .config
        .backups
        .list()
        .map_err(|error| format!("list backups: {error}"))
}

/// Replace accounts and settings with the named backup from Downloads. A bad
/// name or unreadable file leaves the current configuration untouched; on
/// success the previous document is kept as `pige.json.pre-restore`, cache for
/// accounts the backup dropped is cascaded away, and those accounts' stored
/// passwords are deleted.
pub async fn restore_config(file_name: String) -> Result<ConfigOutcome, String> {
    if !crate::platform::backup::is_backup_name(&file_name) {
        return Err(format!("{file_name:?} is not a backup file name"));
    }

    let mut guard = lock_state();
    let state = guard
        .as_mut()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state.ensure_writable()?;

    let bytes = state
        .config
        .backups
        .read(&file_name)
        .map_err(|error| format!("read backup: {error}"))?;
    let restored: ConfigDocument =
        serde_json::from_slice(&bytes).map_err(|error| format!("read backup: {error}"))?;

    // Everything below is a committed change; a failure before the document is
    // durable leaves the running configuration exactly as it was.
    let previous = state.config_doc.clone();
    if let Err(error) = config::pre_restore_copy(&state.config_path) {
        log::warn!("keep pre-restore copy: {error}");
    }

    let dropped: Vec<AccountId> = previous
        .accounts
        .iter()
        .filter(|account| {
            !restored
                .accounts
                .iter()
                .any(|candidate| candidate.id == account.id)
        })
        .map(|account| account.id.clone())
        .collect();

    state.config_doc = restored;
    if let Err(error) = config::save(&state.config_path, &state.config_doc) {
        state.config_doc = previous;
        return Err(format!("save configuration: {error}"));
    }
    if let Err(error) = state.store.sync_accounts(&state.config_doc.accounts) {
        state.config_doc = previous;
        let _ = config::save(&state.config_path, &state.config_doc);
        return Err(format!("update account cache: {error}"));
    }

    // The document is durable; now the dropped accounts' secrets must not
    // linger, or a later restore of the same id would silently re-attach one.
    // A failure is reported rather than hidden: the app cannot both claim a
    // clean restore and keep the old password.
    let mut warnings = Vec::new();
    for id in &dropped {
        if let Err(error) = state.config.credentials.delete(id) {
            log::warn!("delete password for restored-away account {id}: {error}");
            warnings.push(format!(
                "A stored password for a removed account could not be cleared: {error}"
            ));
        }
        if let Err(error) = state.config.credentials.delete_session(id) {
            log::warn!("clear session password for restored-away account {id}: {error}");
        }
    }

    state.selected_folder = None;
    state.selected_conversation = None;
    state.server_more = None;
    state.conversation_limit = INITIAL_PAGE;
    state.ensure_selection();

    let sender = state.events.clone();
    let outcome = ConfigOutcome {
        snapshot: state.build_snapshot(),
        settings: state.config_doc.settings.clone(),
        warnings,
    };
    drop(guard);
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(outcome)
}

/// Wipe every account, stored password, cached row, and setting, leaving a
/// working first-run app. Destructive and irreversible.
pub async fn factory_reset() -> Result<ConfigOutcome, String> {
    let mut guard = lock_state();
    let state = guard
        .as_mut()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state.ensure_writable()?;

    let previous = state.config_doc.clone();
    let ids: Vec<AccountId> = previous
        .accounts
        .iter()
        .map(|account| account.id.clone())
        .collect();

    state.config_doc = ConfigDocument::default();
    if let Err(error) = config::save(&state.config_path, &state.config_doc) {
        state.config_doc = previous;
        return Err(format!("save configuration: {error}"));
    }
    if let Err(error) = state.store.sync_accounts(&[]) {
        state.config_doc = previous;
        let _ = config::save(&state.config_path, &state.config_doc);
        return Err(format!("update account cache: {error}"));
    }

    let mut warnings = Vec::new();
    for id in &ids {
        if let Err(error) = state.config.credentials.delete(id) {
            log::warn!("delete password during factory reset for {id}: {error}");
            warnings.push(format!("A stored password could not be cleared: {error}"));
        }
        if let Err(error) = state.config.credentials.delete_session(id) {
            log::warn!("clear session password during factory reset for {id}: {error}");
        }
    }

    // Cached bodies and attachments live as files outside SQLite; clear them so
    // nothing from the previous configuration is left on disk.
    if let Ok(entries) = std::fs::read_dir(&state.config.cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }

    state.selected_folder = None;
    state.selected_conversation = None;
    state.server_more = None;
    state.conversation_limit = INITIAL_PAGE;

    let sender = state.events.clone();
    let outcome = ConfigOutcome {
        snapshot: state.build_snapshot(),
        settings: state.config_doc.settings.clone(),
        warnings,
    };
    drop(guard);
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(outcome)
}

/// Milliseconds since the Unix epoch, or `0` if the clock is before it.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Search cached mail for the search screen. The SQLite scan runs on a
/// blocking worker with its own connection, so a long query never stalls the
/// UI thread or the global state lock. Results are also stored in the snapshot
/// and announced with [`CoreEvent::SearchChanged`].
pub async fn search_mail(query: &str, scope: SearchScope) -> Result<Vec<SearchHit>, String> {
    let query = query.trim().to_string();
    let (data_dir, sender, generation) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let generation = state.search.begin();
        (
            state.config.data_dir.clone(),
            state.events.clone(),
            generation,
        )
    };

    let hits = if query.is_empty() {
        Vec::new()
    } else {
        let search_query = query.clone();
        let search_scope = scope.clone();
        tokio::task::spawn_blocking(move || {
            let store = Store::open(database_path(&data_dir))
                .map_err(|error| format!("open search store: {error}"))?;
            crate::search::search(&store, &search_query, &search_scope, SEARCH_LIMIT)
                .map_err(|error| format!("search mail: {error}"))
        })
        .await
        .map_err(|error| format!("search task: {error}"))??
    };

    let applied = {
        let mut guard = lock_state();
        guard.as_mut().is_some_and(|state| {
            state
                .search
                .complete(generation, query, scope, hits.clone())
        })
    };
    if applied {
        let _ = sender.send(CoreEvent::SearchChanged);
    }
    Ok(hits)
}

/// Persist an account and its password. The password is written through the
/// platform credential store; the SQLite row contains no secret.
pub async fn add_account(input: AccountInput) -> Result<ModelSnapshot, String> {
    add_account_and_id(input)
        .await
        .map(|(_, snapshot)| snapshot)
}

/// Add an account and return its freshly assigned id alongside the snapshot.
/// Callers that immediately sync the new account use the id instead of
/// guessing from list order, which can tie after a removal.
pub async fn add_account_and_id(input: AccountInput) -> Result<(AccountId, ModelSnapshot), String> {
    add_account_inner(input, false).await
}

/// Add an account whose password can only live for this process. The UI calls
/// this after [`add_account`] fails with
/// [`crate::platform::credentials_unavailable`], and must show a warning that
/// the password will not survive a restart. Nothing is written to SQLite.
pub async fn add_account_with_session(input: AccountInput) -> Result<ModelSnapshot, String> {
    add_account_with_session_and_id(input)
        .await
        .map(|(_, snapshot)| snapshot)
}

/// Session-only counterpart to [`add_account_and_id`].
pub async fn add_account_with_session_and_id(
    input: AccountInput,
) -> Result<(AccountId, ModelSnapshot), String> {
    add_account_inner(input, true).await
}

async fn add_account_inner(
    input: AccountInput,
    session_only: bool,
) -> Result<(AccountId, ModelSnapshot), String> {
    input.validate()?;

    let sender;
    let result;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        let accounts = state.config_doc.accounts.clone();
        // len() can tie after a removal because sort_order is user-editable
        // data in later tasks. A strict max + 1 keeps new accounts at the end.
        let sort_order = accounts
            .iter()
            .map(|account| account.sort_order)
            .max()
            .map_or(0, |maximum| maximum.saturating_add(1));
        let id = AccountId::new();
        let account = input.clone().into_account(id.clone(), sort_order);
        let target = if session_only {
            SecretTarget::SessionOnly
        } else {
            SecretTarget::Persistent
        };

        set_secret(state, &id, &input.password, target)
            .map_err(|error| format!("store password: {error}"))?;
        let mut next = state.config_doc.accounts.clone();
        next.push(account);
        if let Err(error) = state.replace_accounts(next) {
            if let Err(rollback) = delete_secret(state, &id, target) {
                log::error!(
                    "rollback credential for {id} after account insert failure: {rollback}"
                );
                return Err(format!("{error}; credential rollback failed: {rollback}"));
            }
            return Err(error);
        }

        state.ensure_selection();
        sender = state.events.clone();
        result = (id, state.build_snapshot());
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(result)
}

/// Update an existing account's label, identity, servers, and optionally its
/// password. An empty password keeps the stored secret.
pub async fn update_account(id: &AccountId, input: AccountInput) -> Result<ModelSnapshot, String> {
    update_account_inner(id, input, false).await
}

/// Session-only counterpart to [`update_account`] for machines without Secret
/// Service.
pub async fn update_account_with_session(
    id: &AccountId,
    input: AccountInput,
) -> Result<ModelSnapshot, String> {
    update_account_inner(id, input, true).await
}

async fn update_account_inner(
    id: &AccountId,
    input: AccountInput,
    session_only: bool,
) -> Result<ModelSnapshot, String> {
    input.validate_for_edit()?;

    let sender;
    let snapshot;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        let existing = state
            .config_doc
            .accounts
            .iter()
            .find(|account| &account.id == id)
            .cloned()
            .ok_or_else(|| format!("unknown account {id}"))?;

        let wrote_secret = !input.password.is_empty();
        let target = if session_only {
            SecretTarget::Transient
        } else {
            SecretTarget::Persistent
        };
        let previous_secret = if wrote_secret {
            state.config.credentials.get(id).ok().flatten()
        } else {
            None
        };
        if wrote_secret {
            set_secret(state, id, &input.password, target)
                .map_err(|error| format!("store password: {error}"))?;
        }

        let account = input.into_account(id.clone(), existing.sort_order);
        let mut next = state.config_doc.accounts.clone();
        if let Some(slot) = next.iter_mut().find(|candidate| &candidate.id == id) {
            *slot = account;
        }
        if let Err(error) = state.replace_accounts(next) {
            if wrote_secret {
                // Restore the secret we replaced. With no previous secret, the
                // newly written one must be removed instead of left orphaned.
                let rollback = match previous_secret {
                    Some(secret) => set_secret(state, id, &secret, target),
                    None => delete_secret(state, id, target),
                };
                if let Err(rollback) = rollback {
                    log::error!(
                        "rollback credential for {id} after account update failure: {rollback}"
                    );
                    return Err(format!("{error}; credential rollback failed: {rollback}"));
                }
            }
            return Err(error);
        }

        state.ensure_selection();
        sender = state.events.clone();
        snapshot = state.build_snapshot();
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

/// Change only an account's display label. Labels are presentation data, so
/// this never touches credentials or cached mail.
pub async fn rename_account(id: &AccountId, label: &str) -> Result<ModelSnapshot, String> {
    let label = label.trim();
    if label.is_empty() {
        return Err("Account label is required".to_string());
    }

    let sender;
    let snapshot;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let mut next = state.config_doc.accounts.clone();
        let account = next
            .iter_mut()
            .find(|account| &account.id == id)
            .ok_or_else(|| format!("unknown account {id}"))?;
        account.label = label.to_string();
        state.replace_accounts(next)?;
        state.ensure_selection();
        sender = state.events.clone();
        snapshot = state.build_snapshot();
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

/// Remove an account, its stored password, and every cached row that belongs
/// to it. The credential is deleted before the SQLite row so a failed removal
/// never leaves a secret behind for an account the user can no longer see; if
/// the database delete then fails, the secret is restored.
pub async fn remove_account(id: &AccountId) -> Result<ModelSnapshot, String> {
    let sender;
    let snapshot;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        state
            .config_doc
            .accounts
            .iter()
            .find(|account| &account.id == id)
            .ok_or_else(|| format!("unknown account {id}"))?;

        let previous_secret = state.config.credentials.get(id).ok().flatten();
        state
            .config
            .credentials
            .delete(id)
            .map_err(|error| format!("delete password: {error}"))?;
        if let Err(error) = state.config.credentials.delete_session(id) {
            log::warn!("clear session password for {id}: {error}");
        }

        let next: Vec<Account> = state
            .config_doc
            .accounts
            .iter()
            .filter(|account| &account.id != id)
            .cloned()
            .collect();
        if let Err(error) = state.replace_accounts(next) {
            if let Some(secret) = previous_secret
                && let Err(restore) = state.config.credentials.set(id, &secret)
                && let Err(session_restore) = state.config.credentials.set_session(id, &secret)
            {
                log::error!(
                    "restore credential for {id} after account removal failure: {restore}; {session_restore}"
                );
                return Err(format!("{error}; credential restore failed: {restore}"));
            }
            return Err(error);
        }

        // Clear any selection that pointed at the removed account, then pick
        // the unified inbox again when other accounts remain.
        let selected_still_exists = match state.selected_folder.as_ref() {
            Some(folder) => state
                .store
                .accounts()
                .map(|accounts| {
                    accounts.into_iter().any(|account| {
                        state.store.folders(&account.id).is_ok_and(|folders| {
                            folders.iter().any(|candidate| &candidate.id == folder)
                        })
                    })
                })
                .unwrap_or(false),
            None => false,
        };
        if !selected_still_exists {
            state.selected_folder = None;
        }
        state.selected_conversation = None;
        state.server_more = None;
        state.conversation_limit = INITIAL_PAGE;
        state.ensure_selection();

        sender = state.events.clone();
        snapshot = state.build_snapshot();
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

/// Full configuration for one account, for the account editor. The password is
/// intentionally absent because it lives only in the credential store.
pub fn account(id: &AccountId) -> Result<Account, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state
        .store
        .accounts()
        .map_err(|error| format!("read accounts: {error}"))?
        .into_iter()
        .find(|account| &account.id == id)
        .ok_or_else(|| format!("unknown account {id}"))
}

/// Open a real IMAP connection with the supplied settings and password without
/// persisting an account. Validation errors and connection failures are both
/// returned as user-facing strings.
pub async fn test_account(input: &AccountInput) -> Result<(), String> {
    input.validate()?;
    let account = input.clone().into_account(AccountId::new(), 0);
    let config = {
        let guard = lock_state();
        guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?
            .config
            .clone()
    };

    config
        .transport
        .imap_with_password(&account, &input.password)
        .await
        .map_err(|error| format!("Connection test failed: {error}"))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum SecretTarget {
    Persistent,
    /// A brand-new account that never had a persistent secret.
    SessionOnly,
    /// A replacement for an existing account while the OS store is down; an
    /// older persistent secret may still exist and must not be forgotten.
    Transient,
}

fn set_secret(
    state: &AppState,
    id: &AccountId,
    secret: &str,
    target: SecretTarget,
) -> Result<(), String> {
    match target {
        SecretTarget::Persistent => state.config.credentials.set(id, secret),
        SecretTarget::SessionOnly => state.config.credentials.set_session(id, secret),
        SecretTarget::Transient => state.config.credentials.set_transient(id, secret),
    }
}

fn delete_secret(state: &AppState, id: &AccountId, target: SecretTarget) -> Result<(), String> {
    match target {
        SecretTarget::Persistent => state.config.credentials.delete(id),
        SecretTarget::SessionOnly | SecretTarget::Transient => {
            state.config.credentials.delete_session(id)
        }
    }
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
    let (outcome, capabilities) = sync_accounts(&config, &accounts, &sender).await;

    let status = finish_sync(accounts.len(), outcome, capabilities);
    // The account sync never visits non-special folders; if one is open,
    // refresh it too so manual refresh and pull-to-refresh update the view.
    if let Some(folder) = selected_other_folder()
        && let Err(error) = sync_folder_inner(&folder).await
    {
        log::warn!("refresh open folder {}: {error}", folder);
    }
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
    let (outcome, capabilities) = sync_accounts(&config, accounts, &sender).await;

    let status = finish_sync(accounts.len(), outcome, capabilities);
    let snapshot = snapshot();
    let _ = sender.send(CoreEvent::SnapshotChanged);
    if let SyncStatus::Error(message) = status {
        return Err(message);
    }
    Ok(snapshot)
}

/// Sync one folder on demand. Used to open a non-special folder (a Gmail user
/// label or any "Other" IMAP folder, which the account sync deliberately
/// skips), and to refresh a destination folder after a move. Serialized with
/// account syncs through [`SYNC_LOCK`] so it cannot race SQLite writes.
pub async fn sync_folder(id: &FolderId) -> Result<ModelSnapshot, String> {
    let _sync = SYNC_LOCK.lock().await;
    sync_folder_inner(id).await
}

/// One-folder sync that assumes `SYNC_LOCK` is already held. Locates the
/// account and folder, connects, and runs the engine's one-folder sync with a
/// thread index seeded from the account's cached messages.
async fn sync_folder_inner(id: &FolderId) -> Result<ModelSnapshot, String> {
    let (config, account, folder, sender) = {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let (account, folder) = locate_folder(&state.store, id)?;
        (state.config.clone(), account, folder, state.events.clone())
    };

    let store = Store::open(database_path(&config.data_dir))
        .map_err(|error| format!("open folder store: {error}"))?;
    let mut transport = config
        .transport
        .imap(&account)
        .await
        .map_err(|error| format!("{}: {error}", account.label))?;
    let remote = RemoteFolder {
        name: folder.name.clone(),
        delimiter: folder.delimiter.clone(),
        kind: folder.kind,
    };
    let mut threads = ThreadAssigner::from_messages(
        &store
            .messages_for_account(&account.id)
            .map_err(|error| format!("read thread index: {error}"))?,
    );
    crate::sync::engine::sync_folder(
        &store,
        &account,
        transport.as_mut(),
        &remote,
        &folder,
        SYNC_WINDOW,
        &mut threads,
    )
    .await?;

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot())
}

/// The currently selected folder when it is one the account sync never visits,
/// so a manual refresh can sync it too.
fn selected_other_folder() -> Option<FolderId> {
    let guard = lock_state();
    let state = guard.as_ref()?;
    let folder = state.selected_folder.as_ref()?;
    let (_, row) = locate_folder(&state.store, folder).ok()?;
    (row.kind == FolderKind::Other).then(|| folder.clone())
}

/// Select a folder and reset the conversation page. Non-special folders are
/// synced on first open ("other folders sync when opened"); a network failure
/// is logged and the cached snapshot is returned rather than failing the
/// selection.
pub async fn select_folder(id: &FolderId) -> Result<ModelSnapshot, String> {
    let sender;
    let needs_sync = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        state.selected_folder = Some(id.clone());
        state.unified_selected = false;
        state.conversation_limit = INITIAL_PAGE;
        state.selected_conversation = None;
        state.server_more = None;
        let needs_sync = locate_folder(&state.store, id).is_ok_and(|(_, folder)| {
            folder.kind == FolderKind::Other
                && (folder.uidnext.is_none()
                    || state
                        .store
                        .messages(id, 1, None)
                        .map(|messages| messages.is_empty())
                        .unwrap_or(true))
        });
        sender = state.events.clone();
        needs_sync
    };
    let _ = sender.send(CoreEvent::SnapshotChanged);

    if needs_sync && let Err(error) = sync_folder(id).await {
        log::warn!("sync newly opened folder {}: {error}", id);
    }
    Ok(snapshot())
}

/// Select the merged inbox across every account and reset the page.
pub async fn select_unified() -> Result<ModelSnapshot, String> {
    let sender;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        state.selected_folder = None;
        state.unified_selected = true;
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

        state.unified_selected = false;
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

/// Extend the merged inbox by one cached page. Cross-account server backfill
/// is deferred for now, so this only grows the local merged list.
pub async fn load_older_unified() -> Result<ModelSnapshot, String> {
    let sender = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        if !state.unified_selected {
            None
        } else {
            let limit = state.conversation_limit.max(1);
            let candidate_limit = limit.saturating_add(PAGE_SIZE);
            let candidate = state
                .store
                .unified_conversations(candidate_limit)
                .map_err(|error| format!("read unified conversations: {error}"))?;
            if candidate.len() > limit as usize {
                state.conversation_limit = candidate.len() as u32;
            }
            Some(state.events.clone())
        }
    };

    if let Some(sender) = sender {
        let _ = sender.send(CoreEvent::SnapshotChanged);
    }
    Ok(snapshot())
}

/// Open a conversation for the reader: load its messages, resolve the newest
/// message's body (from cache or through the account's transport), and install
/// the resulting [`ConversationDetail`] as the selected conversation.
///
/// A body over [`MAX_BODY_BYTES`] is opened without a fetch and marked
/// [`MessageBody::TooLarge`]; a missing or unreachable body degrades to
/// `body: None` so the reader always renders its chrome even offline.
pub async fn open_conversation(thread_id: &ThreadId) -> Result<ModelSnapshot, String> {
    // Phase 1: read the conversation, its cached body, and the fetch decision.
    let (messages, config, account, folder, cached, too_large, events, initial) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let messages = state
            .store
            .messages_for_thread(thread_id)
            .map_err(|error| format!("read conversation: {error}"))?;
        if messages.is_empty() {
            return Err(format!("unknown conversation {thread_id}"));
        }
        let account = state
            .store
            .accounts()
            .map_err(|error| format!("read accounts: {error}"))?
            .into_iter()
            .find(|account| account.id == messages[0].account_id)
            .ok_or_else(|| "conversation account missing".to_string())?;
        let folder = state
            .store
            .folders(&account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .into_iter()
            .find(|folder| folder.id == messages[0].folder_id)
            .ok_or_else(|| "conversation folder missing".to_string())?;
        let cached = state
            .store
            .body(&messages[0].id)
            .map_err(|error| format!("read cached body: {error}"))?;
        let too_large = messages[0].header.size > MAX_BODY_BYTES;
        let body = if too_large {
            Some(MessageBody::TooLarge)
        } else if let Some(cached) = &cached {
            document_from_body(cached).map(MessageBody::Document)
        } else {
            Some(MessageBody::Loading)
        };
        state.selected_conversation = Some(conversation_detail_from_store(
            &state.store,
            thread_id,
            &account.id,
            &messages,
            body,
        ));
        let initial = state.build_snapshot();
        (
            messages,
            state.config.clone(),
            account,
            folder,
            cached,
            too_large,
            state.events.clone(),
            initial,
        )
    };
    let _ = events.send(CoreEvent::SnapshotChanged);
    if cached.is_some() || too_large {
        return Ok(initial);
    }

    // Phase 2: fetch raw bytes only when uncached and under the size cap.
    let fetched = if cached.is_none() && !too_large {
        match fetch_body_bytes(&config, &account, &folder, &messages[0]).await {
            Ok(raw) => Some(raw),
            Err(error) => {
                log::warn!("open conversation {thread_id}: {error}");
                None
            }
        }
    } else {
        None
    };

    // Phase 3: cache and build the detail under the lock, using the shared
    // store handle rather than opening a second connection.
    let snapshot = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let body = resolve_body(
            &state.store,
            &messages[0].id,
            cached,
            fetched.as_deref(),
            too_large,
        );
        state.selected_conversation = Some(conversation_detail_from_store(
            &state.store,
            thread_id,
            &account.id,
            &messages,
            body,
        ));
        state.build_snapshot()
    };
    let _ = events.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

fn conversation_detail_from_store(
    store: &Store,
    thread_id: &ThreadId,
    account_id: &AccountId,
    messages: &[StoredMessage],
    body: Option<MessageBody>,
) -> ConversationDetail {
    let attachments = match store.attachments(&messages[0].id) {
        Ok(attachments) => attachments,
        Err(error) => {
            log::warn!("read attachments {}: {error}", messages[0].id);
            Vec::new()
        }
    };
    // Earlier messages carry their cached bodies so "Earlier messages" can
    // show real content instead of names only. Reading cached SQLite rows is
    // cheap; a missing body degrades to `None` and the reader shows the
    // snippet plus a "Not downloaded" label.
    let earlier_bodies: HashMap<MessageId, MessageBody> = messages
        .iter()
        .skip(1)
        .filter_map(|message| {
            if message.header.size > MAX_BODY_BYTES {
                return Some((message.id.clone(), MessageBody::TooLarge));
            }
            store.body(&message.id).ok().flatten().and_then(|cached| {
                document_from_body(&cached)
                    .map(|document| (message.id.clone(), MessageBody::Document(document)))
            })
        })
        .collect();
    build_conversation_detail(
        thread_id,
        account_id,
        messages,
        body,
        &earlier_bodies,
        attachments,
    )
}

/// Build a reply draft for the newest message in a conversation. Recipients,
/// subject, In-Reply-To/References, and the quoted body are prefilled; the UI
/// only has to append the user's new text.
pub fn compose_reply(thread_id: &ThreadId) -> Result<OutgoingMessage, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    let messages = state
        .store
        .messages_for_thread(thread_id)
        .map_err(|error| format!("read conversation: {error}"))?;
    let newest = messages
        .first()
        .ok_or_else(|| format!("unknown conversation {thread_id}"))?;
    let account = find_account(&state.store, &newest.account_id)?;
    let body = message_text(&state.store, newest);
    Ok(reply_message(&account, &newest.header, &body))
}

/// Build a forward draft for the newest message in a conversation. No recipient
/// is guessed; the subject and quoted body are prefilled.
pub fn compose_forward(thread_id: &ThreadId) -> Result<OutgoingMessage, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    let messages = state
        .store
        .messages_for_thread(thread_id)
        .map_err(|error| format!("read conversation: {error}"))?;
    let newest = messages
        .first()
        .ok_or_else(|| format!("unknown conversation {thread_id}"))?;
    let account = find_account(&state.store, &newest.account_id)?;
    let body = message_text(&state.store, newest);
    Ok(forward_message(&account, &newest.header, &body))
}

fn find_account(store: &Store, id: &AccountId) -> Result<Account, String> {
    store
        .accounts()
        .map_err(|error| format!("read accounts: {error}"))?
        .into_iter()
        .find(|account| &account.id == id)
        .ok_or_else(|| format!("unknown account {id}"))
}

fn message_text(store: &Store, message: &crate::mail::StoredMessage) -> String {
    store
        .body(&message.id)
        .ok()
        .flatten()
        .and_then(|body| body.plain_text)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| message.snippet.clone())
}

/// Queue a message durably, then try to flush this account's outbox. A network
/// failure leaves the row queued and reports `queued: true`; a hard send
/// failure (for example a rejected recipient) keeps the compose form open by
/// returning the outbox error.
pub async fn send_mail(message: OutgoingMessage) -> Result<SendOutcome, String> {
    let (config, account, id, sender) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let account = find_account(&state.store, &message.account_id)?;
        let id = state
            .store
            .insert_outbox(&message, send::OutboxState::Queued)
            .map_err(|error| format!("queue message: {error}"))?;
        (state.config.clone(), account, id, state.events.clone())
    };
    let _ = sender.send(CoreEvent::SnapshotChanged);

    let store = Store::open(database_path(&config.data_dir))
        .map_err(|error| format!("open outbox store: {error}"))?;
    // SMTP delivery is the important half. IMAP is only needed for the
    // best-effort Sent APPEND, so an unreachable IMAP connection must not keep
    // a message queued when SMTP is available.
    if let Ok(mut smtp) = config.transport.smtp(&account).await {
        let result = match config.transport.imap(&account).await {
            Ok(mut imap) => {
                send::flush_outbox(&store, &account, smtp.as_mut(), imap.as_mut()).await
            }
            Err(error) => {
                log::warn!(
                    "IMAP unavailable for {} while sending; appending to Sent later: {error}",
                    account.label
                );
                send::flush_outbox_smtp_only(&store, &account, smtp.as_mut()).await
            }
        };
        if let Err(error) = result {
            log::warn!("flush outbox for {}: {error}", account.label);
        }
    }

    let entry = store
        .outbox_entry(&id)
        .map_err(|error| format!("read outbox: {error}"))?;
    let queued = entry.as_ref().is_some_and(|entry| {
        matches!(
            entry.state,
            send::OutboxState::Queued | send::OutboxState::Sending
        )
    });
    let failure = entry
        .as_ref()
        .filter(|entry| entry.state == send::OutboxState::Failed)
        .and_then(|entry| entry.error.clone());

    let snapshot = snapshot();
    let _ = sender.send(CoreEvent::SnapshotChanged);
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(SendOutcome { snapshot, queued })
}

/// Save a new draft. Drafts are local rows and never touch the network.
pub fn save_draft(message: OutgoingMessage) -> Result<OutboxId, String> {
    let sender;
    let id;
    {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        id = send::save_draft(&state.store, message)
            .map_err(|error| format!("save draft: {error}"))?;
        sender = state.events.clone();
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(id)
}

/// Replace an existing draft's fields while keeping its id.
pub fn update_draft(id: &OutboxId, message: OutgoingMessage) -> Result<(), String> {
    let sender;
    {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        send::update_draft(&state.store, id, &message)
            .map_err(|error| format!("update draft: {error}"))?;
        sender = state.events.clone();
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(())
}

/// Delete a draft and refresh the snapshot.
pub fn discard_draft(id: &OutboxId) -> Result<ModelSnapshot, String> {
    let sender;
    let snapshot;
    {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        send::discard_draft(&state.store, id).map_err(|error| format!("discard draft: {error}"))?;
        sender = state.events.clone();
        snapshot = state.build_snapshot();
    }
    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

/// Drafts for the Drafts folder, newest first across every account.
pub fn list_drafts() -> Result<Vec<OutboxEntry>, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state
        .store
        .outbox_drafts(None)
        .map_err(|error| format!("list drafts: {error}"))
}

/// Drafts for one account, used by an account-scoped Drafts folder.
pub fn list_drafts_for(account: &AccountId) -> Result<Vec<OutboxEntry>, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state
        .store
        .outbox_drafts(Some(account))
        .map_err(|error| format!("list drafts: {error}"))
}

/// Load one full draft, including its body, for editing.
pub fn draft(id: &OutboxId) -> Result<Option<OutboxEntry>, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state
        .store
        .outbox_entry(id)
        .map_err(|error| format!("read draft: {error}"))
}

/// Open a URL with the platform's default handler. Synchronous because the
/// opener resolves immediately.
pub fn open_url(url: &str) -> Result<(), String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state.config.opener.open_url(url)
}

/// Fetch, cache, and open one attachment through the platform opener.
///
/// Cached bytes are reused across opens. The first open fetches the raw
/// message, extracts the requested part, writes it below
/// `cache_dir/attachments`, records the metadata and cache path, and only then
/// hands the file to [`Opener::open_file`]. Parts over
/// [`crate::mail::MAX_ATTACHMENT_BYTES`] are rejected before they are written.
pub async fn open_attachment(message: &MessageId, part: &str) -> Result<PathBuf, String> {
    let (config, account, folder, stored_message, stored_part, cached_path) = {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;

        let stored_message = state
            .store
            .message(message)
            .map_err(|error| format!("read message: {error}"))?
            .ok_or_else(|| format!("unknown message {message}"))?;
        let account = find_account(&state.store, &stored_message.account_id)?;
        let folder = state
            .store
            .folders(&account.id)
            .map_err(|error| format!("read folders: {error}"))?
            .into_iter()
            .find(|folder| folder.id == stored_message.folder_id)
            .ok_or_else(|| "message folder is not cached".to_string())?;

        let stored = state
            .store
            .attachment(message, part)
            .map_err(|error| format!("read attachment: {error}"))?;
        let cached_path = stored
            .as_ref()
            .and_then(|attachment| attachment.cached_path.as_deref())
            .and_then(|path| verified_cached_attachment(&state.config.cache_dir, path));
        let stored_part = stored.map(|attachment| attachment.part);

        (
            state.config.clone(),
            account,
            folder,
            stored_message,
            stored_part,
            cached_path,
        )
    };

    let path = match cached_path {
        Some(path) => path,
        None => {
            // Reject a previously known oversized part before downloading the
            // whole message again.
            if let Some(part) = &stored_part {
                check_attachment_size(part.size).map_err(|error| error.to_string())?;
            }

            let raw = fetch_body_bytes(&config, &account, &folder, &stored_message).await?;
            let parts = extract_attachments(&raw);
            let part_meta = parts
                .iter()
                .find(|candidate| candidate.part_id == part)
                .cloned()
                .ok_or_else(|| format!("attachment part {part} is not present in the message"))?;
            // Refuse before allocating and decoding an oversized MIME part.
            check_attachment_size(part_meta.size).map_err(|error| error.to_string())?;
            let bytes = attachment_bytes(&raw, &part_meta.part_id).ok_or_else(|| {
                format!("attachment part {} could not be decoded", part_meta.part_id)
            })?;
            check_attachment_size(bytes.len() as u64).map_err(|error| error.to_string())?;

            let path = cache_attachment(&config.cache_dir, message, &part_meta, &bytes)
                .map_err(|error| error.to_string())?;

            {
                let guard = lock_state();
                let state = guard
                    .as_ref()
                    .ok_or_else(|| "pige api is not initialized".to_string())?;
                state
                    .store
                    .store_attachments(message, &parts)
                    .map_err(|error| format!("store attachment metadata: {error}"))?;
                state
                    .store
                    .set_attachment_cached_path(message, &part_meta.part_id, &path)
                    .map_err(|error| format!("store attachment path: {error}"))?;
            }

            path
        }
    };

    let opener = {
        let guard = lock_state();
        guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?
            .config
            .opener
            .clone()
    };
    opener.open_file(&path)?;
    Ok(path)
}

/// Accept a recorded cache path only when it still resolves to a regular file
/// below the attachments directory. This keeps a stale or tampered row from
/// handing an arbitrary file to the platform opener.
fn verified_cached_attachment(cache_dir: &Path, path: &Path) -> Option<PathBuf> {
    let root = cache_dir.join("attachments");
    if !path.starts_with(&root) || !path.is_file() {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if check_attachment_size(metadata.len()).is_err() {
        return None;
    }
    let canonical_root = std::fs::canonicalize(&root).ok()?;
    let canonical_path = std::fs::canonicalize(path).ok()?;
    canonical_path
        .starts_with(&canonical_root)
        .then(|| path.to_path_buf())
}

/// Fetch the raw bytes of a message through the account's transport.
async fn fetch_body_bytes(
    config: &Config,
    account: &Account,
    folder: &Folder,
    message: &StoredMessage,
) -> Result<Vec<u8>, String> {
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
    transport
        .select(&remote)
        .await
        .map_err(|error| format!("select {}: {error}", folder.name))?;
    transport
        .fetch_body(message.uid)
        .await
        .map_err(|error| format!("fetch body {}: {error}", message.uid))
}

/// Resolve the newest message's body: too-large wins, then the cached body,
/// then a freshly fetched raw message. Fetched bodies with no text are left
/// uncached so a later retry can succeed.
fn resolve_body(
    store: &Store,
    message_id: &MessageId,
    cached: Option<Body>,
    fetched: Option<&[u8]>,
    too_large: bool,
) -> Option<MessageBody> {
    if too_large {
        return Some(MessageBody::TooLarge);
    }
    if let Some(body) = cached {
        return document_from_body(&body).map(MessageBody::Document);
    }
    cache_fetched_body(store, message_id, fetched?).map(MessageBody::Document)
}

/// Cache a fetched message through the same path for opens and prefetch.
fn cache_fetched_body(store: &Store, message_id: &MessageId, raw: &[u8]) -> Option<MailDocument> {
    let parts = extract_attachments(raw);
    if let Err(error) = store.store_attachments(message_id, &parts) {
        log::warn!("cache attachments {message_id}: {error}");
    }
    let (html, plain_text) = extract_body_parts(raw);
    let body = Body {
        plain_text: plain_text.clone(),
        html: html.clone(),
        fetched_at: now_unix(),
    };
    let document = document_from_body(&body);
    if document.is_some()
        && let Err(error) = store.upsert_body(message_id, &body)
    {
        log::warn!("cache body {message_id}: {error}");
    }
    document
}

/// Extract the HTML and plain-text bodies from a raw RFC 5322 message.
fn extract_body_parts(raw: &[u8]) -> (Option<String>, Option<String>) {
    let Some(parsed) = mail_parser::MessageParser::new().parse(raw) else {
        return (None, None);
    };
    let html = parsed.html_bodies().next().map(|body| body.into_owned());
    let plain_text = parsed.text_bodies().next().map(|body| body.into_owned());
    (html, plain_text)
}

/// Build a [`MailDocument`] from a cached body, preferring HTML.
fn document_from_body(body: &Body) -> Option<MailDocument> {
    if let Some(html) = &body.html {
        Some(parse_html(html, false))
    } else {
        body.plain_text.as_deref().map(plain_text_document)
    }
}

/// Assemble the reader view for a conversation, attaching the newest message's
/// resolved body and any cached bodies for earlier messages.
fn build_conversation_detail(
    thread_id: &ThreadId,
    account_id: &AccountId,
    messages: &[StoredMessage],
    body: Option<MessageBody>,
    earlier_bodies: &HashMap<MessageId, MessageBody>,
    attachments: Vec<AttachmentPart>,
) -> ConversationDetail {
    let subject = messages
        .first()
        .map(|message| message.header.subject.clone())
        .unwrap_or_default();
    let summaries = messages
        .iter()
        .enumerate()
        .map(|(index, message)| MessageSummary {
            id: message.id.clone(),
            sender: sender_display(&message.header.from),
            to: sender_display(&message.header.to),
            date: message.header.date,
            snippet: message.snippet.clone(),
            flags: message.flags,
            has_attachments: message.has_attachments || (index == 0 && !attachments.is_empty()),
            attachments: if index == 0 {
                attachments.clone()
            } else {
                Vec::new()
            },
            body: if index == 0 {
                body.clone()
            } else {
                earlier_bodies.get(&message.id).cloned()
            },
        })
        .collect();

    ConversationDetail {
        thread_id: thread_id.clone(),
        account_id: account_id.clone(),
        subject,
        messages: summaries,
    }
}

/// Unix seconds, UTC, for the body cache's `fetched_at`.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
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
    //
    // With no cached mail (a folder opened before its first sync), start at the
    // server's UIDNEXT high-water mark so paging can bootstrap an empty folder
    // instead of failing forever; the top UID is included in that case.
    let (ceiling, mut cursor, include_ceiling) = match oldest {
        Some(uid) => (uid.0, uid.0.saturating_sub(1), false),
        None => {
            let top = state.uidnext.saturating_sub(1);
            (top, top, true)
        }
    };
    let want = PAGE_SIZE as usize;
    let mut found = Vec::new();
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
        found.extend(batch.into_iter().filter(|message| {
            if include_ceiling {
                message.uid.0 <= ceiling
            } else {
                message.uid.0 < ceiling
            }
        }));
        if from == 1 {
            break;
        }
        cursor = from - 1;
    }

    found.sort_unstable_by_key(|message| Reverse(message.uid));
    let server_more = found.len() > want;
    found.truncate(want);
    // References point backwards, so assignment must see the oldest message
    // first. Upsert order does not matter because rows are keyed naturally.
    found.sort_unstable_by_key(|message| message.uid);

    let mut threads = ThreadAssigner::from_messages(
        &store
            .messages_for_account(&account.id)
            .map_err(|error| format!("read thread index: {error}"))?,
    );

    for message in &found {
        let crate::transport::RemoteMessage {
            uid,
            header,
            flags,
            size,
        } = message.clone();
        let mut header = header;
        header.size = size;
        let participants = participants_of(&header);
        let thread_id = threads.assign(&account.id, &header, header.date, &participants);
        for (from, into) in threads.take_merges() {
            store
                .merge_threads(&account.id, &from, &into)
                .map_err(|error| format!("merge older {}: {error}", folder.name))?;
        }
        let stored = StoredMessage {
            id: MessageId::new(),
            account_id: account.id.clone(),
            folder_id: folder.id.clone(),
            uid,
            uidvalidity,
            header,
            flags,
            thread_id,
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
    // Record the high-water mark so a later paging call (and the open-sync
    // guard) treats this folder as synced.
    updated.uidnext = Some(state.uidnext);
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
/// state lock is never held across network awaits. Returns the outcome plus the
/// capabilities observed for each successfully synced account.
async fn sync_accounts(
    config: &Config,
    accounts: &[Account],
    sender: &broadcast::Sender<CoreEvent>,
) -> (SyncOutcome, Vec<(AccountId, Capabilities)>) {
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
            return (
                SyncOutcome {
                    succeeded: 0,
                    failed: accounts.len(),
                    last_error: Some(message),
                },
                Vec::new(),
            );
        }
    };

    let mut outcome = SyncOutcome {
        succeeded: 0,
        failed: 0,
        last_error: None,
    };
    let mut capabilities = Vec::new();

    for account in accounts {
        let _ = sender.send(CoreEvent::SyncStarted {
            account: account.id.clone(),
        });

        match sync_one(&store, config, account, sender).await {
            Ok(caps) => {
                outcome.succeeded += 1;
                capabilities.push((account.id.clone(), caps));
            }
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

    (outcome, capabilities)
}

async fn sync_one(
    store: &Store,
    config: &Config,
    account: &Account,
    sender: &broadcast::Sender<CoreEvent>,
) -> Result<Capabilities, String> {
    let mut transport = config
        .transport
        .imap(account)
        .await
        .map_err(|error| format!("{}: {error}", account.label))?;

    // Capabilities drive the UI's archive/move choices. A failure here must not
    // abort the whole sync, so fall back to the (generic) default.
    let capabilities = match transport.capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            log::warn!("capabilities for {}: {error}", account.label);
            Capabilities::default()
        }
    };

    let report = run_sync_account(store, account, transport.as_mut(), SYNC_WINDOW)
        .await
        .map_err(|error| format!("{}: {error}", account.label))?;

    // Outbox delivery shares this account's connection. A missing SMTP
    // connection is expected offline: the rows stay queued for the next sync.
    let mut outbox_errors = Vec::new();
    let has_pending = store
        .outbox_pending(&account.id)
        .is_ok_and(|pending| !pending.is_empty());
    if has_pending {
        match config.transport.smtp(account).await {
            Ok(mut smtp) => {
                match send::flush_outbox(store, account, smtp.as_mut(), transport.as_mut()).await {
                    Ok(outbox) => outbox_errors.extend(outbox.errors),
                    Err(error) => outbox_errors.push(error.to_string()),
                }
            }
            Err(error) => log::debug!("outbox for {} is waiting: {error}", account.label),
        }
    }

    // The in-app event always fires so the list refreshes; only the OS
    // notification is suppressed when the user has turned notifications off.
    let notify_new_mail = crate::settings::load_settings(store).notifications;
    for mail in &report.new_inbox {
        if notify_new_mail {
            config.notifier.notify(mail);
        }
        let _ = sender.send(CoreEvent::NewMail(mail.clone()));
    }
    for error in report.errors.iter().chain(outbox_errors.iter()) {
        let _ = sender.send(CoreEvent::Error(format!("{}: {error}", account.label)));
    }

    let _ = sender.send(CoreEvent::SyncFinished {
        account: account.id.clone(),
        new_mail: report.new_inbox.len(),
    });
    Ok(capabilities)
}

/// Update status and re-seed the selection after a sync. Called without any
/// lock held; the caller emits the final snapshot notification.
fn finish_sync(
    attempted: usize,
    outcome: SyncOutcome,
    capabilities: Vec<(AccountId, Capabilities)>,
) -> SyncStatus {
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
        for (account, caps) in capabilities {
            state.capabilities.insert(account, caps);
        }
        state.ensure_selection();
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(thread: &str) -> SearchHit {
        SearchHit {
            thread_id: ThreadId(thread.into()),
            account_id: AccountId("account".into()),
            subject: thread.into(),
            sender: "sender".into(),
            date: 1,
        }
    }

    #[test]
    fn an_older_search_completion_cannot_replace_newer_state() {
        let mut search = SearchState::default();
        let older = search.begin();
        let newer = search.begin();

        assert!(!search.complete(
            older,
            "older".into(),
            SearchScope::AllAccounts,
            vec![hit("older")],
        ));
        assert_eq!(search.query, "");
        assert!(search.results.is_empty());

        assert!(search.complete(
            newer,
            "newer".into(),
            SearchScope::Folder(FolderId("folder".into())),
            vec![hit("newer")],
        ));
        assert_eq!(search.query, "newer");
        assert_eq!(search.scope, SearchScope::Folder(FolderId("folder".into())));
        assert_eq!(search.results[0].thread_id, ThreadId("newer".into()));
    }

    #[test]
    fn the_latest_search_completion_commits_its_results() {
        let mut search = SearchState::default();
        let generation = search.begin();

        assert!(search.complete(
            generation,
            "query".into(),
            SearchScope::AllAccounts,
            vec![hit("thread")],
        ));
        assert_eq!(search.query, "query");
        assert_eq!(search.results.len(), 1);
    }

    #[test]
    fn cached_path_outside_the_attachments_root_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"secret").unwrap();

        assert!(verified_cached_attachment(cache, &outside).is_none());
    }

    #[test]
    fn cached_path_with_parent_traversal_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path();
        let root = cache.join("attachments");
        std::fs::create_dir_all(&root).unwrap();
        let outside = cache.join("outside.txt");
        std::fs::write(&outside, b"secret").unwrap();

        let traversal = root.join("..").join("outside.txt");
        assert!(traversal.starts_with(&root));
        assert!(verified_cached_attachment(cache, &traversal).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cached_symlink_escaping_the_attachments_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path();
        let root = cache.join("attachments");
        std::fs::create_dir_all(&root).unwrap();
        let outside = cache.join("outside.txt");
        std::fs::write(&outside, b"secret").unwrap();
        let link = root.join("link.txt");
        symlink(&outside, &link).unwrap();

        assert!(link.is_file(), "the symlink resolves to a regular file");
        assert!(verified_cached_attachment(cache, &link).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_attachment_replaces_a_symlink_instead_of_following_it() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path();
        let message = MessageId("message-1".to_string());
        let part = AttachmentPart {
            part_id: "2".to_string(),
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 3,
            content_id: None,
            disposition: crate::mail::AttachmentDisposition::Attachment,
        };

        let directory = cache.join("attachments/message-1/2");
        std::fs::create_dir_all(&directory).unwrap();
        let target = dir.path().join("outside.txt");
        std::fs::write(&target, b"keep").unwrap();
        let planted = directory.join("report.pdf");
        symlink(&target, &planted).unwrap();

        let path = cache_attachment(cache, &message, &part, b"new").expect("cache attachment");

        assert_eq!(path, planted);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }
}
