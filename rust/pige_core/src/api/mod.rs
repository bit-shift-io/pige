//! The process-wide API state. UI shells and the Android bridge call these
//! functions; each mutation returns a fresh [`ModelSnapshot`] and emits a
//! [`CoreEvent`] for other subscribers.
//!
//! Locks are held only around synchronous SQLite work. Network waits happen
//! after the lock is released, and sync uses its own connection so a snapshot
//! never blocks behind a slow server.

pub mod accounts;
pub mod body;
pub mod contacts;
pub mod events;
pub mod mailbox;
pub mod mutations;
pub mod snapshot;

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::StreamExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::account::{
    Account, AccountId, AuthKind, MailBackend, ServerConfig, default_account_color,
    is_supported_account_color,
};
use crate::config::{self, ConfigDocument};
use crate::mail::{
    AttachmentPart, Folder, FolderId, FolderKind, MessageId, StoredMessage, ThreadAssigner,
    ThreadId, Uid, attachment_bytes, cache_attachment, check_attachment_size, sender_display,
    thread::participants_of,
};
use crate::platform::{BackupStore, CredentialStore, Notifier, Opener, TransportFactory};
use crate::providers::{
    ContactSourceId, OperationContext, ProviderCapabilities, ProviderFactory, RemoteAttachmentId,
    RemoteContactId, imap::ImapSubmission,
};
use crate::search::{SearchHit, SearchScope};
use crate::send::{
    self, OutboxEntry, OutboxId, OutgoingMessage, compose::forward_message, compose::reply_message,
};
use crate::store::{ImapCoords, PrefetchCandidate, Store};
use crate::sync::provider::{WorkBudget, sync_account as provider_sync_account};
use crate::sync::scheduler::AccountScheduler;
use crate::transport::{RemoteFolder, UidRange};

use body::document_from_body;
use mailbox::{INTERACTIVE_TIMEOUT, SYNC_TIMEOUT, open_session, scheduler_message};

/// How many change pages one account sync pass may apply before it returns the
/// account gate. A bounded budget keeps background sync from occupying an
/// account indefinitely.
const SYNC_PAGES: u32 = 8;

pub use crate::contacts::{RecipientQuery, RecipientSuggestion, SuggestionSource};
pub use crate::sync::history::LearningProgress;
pub use crate::settings::{Settings, ThemeMode};
pub use accounts::{
    answer_auth, begin_auth, can_switch_backend, cancel_auth, finish_auth, switch_backend,
};
#[doc(hidden)]
pub use accounts::{
    clear_switch_crash, inject_switch_crash_after_config, inject_switch_crash_after_secret,
};
pub use contacts::{
    clear_learned_recipients, pause_learning, rebuild_learned_recipients,
    record_recipient_selection, set_contact_sync, suggest_recipients,
};
pub use events::{CoreEvent, back_pressed, subscribe};
pub use mutations::{
    RECONNECT_ERROR, archive_available, archive_thread, delete_permanently, move_thread, set_read,
    trash_thread,
};
pub use snapshot::{
    AccountSection, AccountSummary, ContactSyncSummary, ConversationDetail, ConversationSummary,
    FolderSummary, MessageBody, MessageSummary, ModelSnapshot, SyncStatus,
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
pub(crate) const MAX_BODY_BYTES: u32 = 10 * 1024 * 1024;
const PREFETCH_LIMIT: u32 = 50;
const PREFETCH_CONCURRENCY: usize = 2;
static PREFETCH_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
// Never reset on init/restore: an older task may still deliver its snapshot.
static SNAPSHOT_REVISION: AtomicU64 = AtomicU64::new(1);

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
    /// Provider sessions for mailbox read, sync, and mutation work. The API
    /// dispatches every such operation through this factory and never touches a
    /// protocol transport for them.
    pub providers: Arc<dyn ProviderFactory>,
    /// Protocol factory retained only for SMTP submission and account
    /// connection testing until provider submission lands; mailbox reads and
    /// mutations do not use it.
    pub transport: Arc<dyn TransportFactory>,
    /// Reads and writes configuration backups in the platform's Downloads.
    pub backups: Arc<dyn BackupStore>,
    /// Interactive authentication for providers whose adapters are registered.
    /// `None` until a native adapter is wired; the IMAP/SMTP onboarding form
    /// still uses its explicit password field.
    pub auth: Option<Arc<AsyncMutex<Box<dyn crate::auth::AuthenticationService>>>>,
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

    /// Build an IMAP/SMTP account. The legacy input is an IMAP-only onboarding
    /// wrapper; native backends are never produced from it.
    fn into_account(self, id: AccountId, sort_order: i64) -> Account {
        Account {
            id,
            label: self.label.trim().to_string(),
            email: self.email.trim().to_string(),
            display_name: self.display_name.trim().to_string(),
            backend: MailBackend::ImapSmtp {
                username: if self.username.trim().is_empty() {
                    self.email.trim().to_string()
                } else {
                    self.username.trim().to_string()
                },
                imap: self.imap,
                smtp: self.smtp,
                auth: AuthKind::Password,
            },
            contact_sources: Vec::new(),
            sort_order,
            color: Some(default_account_color(sort_order).to_string()),
        }
    }
}

struct AppState {
    config: Config,
    store: Arc<Store>,
    /// Cancels live account work and durably increments generations. Shared
    /// through the same `Store` handle as `store`.
    sessions: Arc<crate::auth::AccountSessionManager>,
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
    open_generation: u64,
    prefetch_generation: u64,
    /// Last search query, scope, results, and the generation used to reject
    /// slower completions from an older query.
    search: SearchState,
    /// `Some(true/false)` once a server backfill has probed for older mail;
    /// `None` means fall back to comparing the server total with the cache.
    server_more: Option<bool>,
    /// Provider-neutral capabilities observed during the last sync, per account.
    /// Used by the UI to decide which mutation actions to offer.
    capabilities: HashMap<AccountId, ProviderCapabilities>,
    /// Serializes an account's remote-read/apply sequences so a sync and a
    /// mutation cannot interleave.
    scheduler: Arc<AccountScheduler>,
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
    fn invalidate_open(&mut self) {
        self.open_generation = self.open_generation.wrapping_add(1);
    }

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
        self.prefetch_generation = self.prefetch_generation.wrapping_add(1);
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
        let revision = SNAPSHOT_REVISION.fetch_add(1, Ordering::Relaxed);
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
                        revision,
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

        // The SQLite account mirror does not carry presentation-only fields, so
        // resolve colours from the authoritative configuration document.
        let account_colors: HashMap<&str, &str> = self
            .config_doc
            .accounts
            .iter()
            .map(|account| (account.id.0.as_str(), account.resolved_color()))
            .collect();

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

            // `scanned` resets with each snapshot generation and counts page
            // changes, so show the actual distinct cached contacts instead.
            let mut cached_contacts: HashMap<ContactSourceId, HashSet<RemoteContactId>> =
                HashMap::new();
            for cached in self
                .store
                .contact_emails(&section.account.id)
                .unwrap_or_default()
            {
                cached_contacts
                    .entry(cached.source)
                    .or_default()
                    .insert(cached.remote_contact);
            }

            let mut contact_sources = self
                .store
                .contact_source_states(&section.account.id)
                .unwrap_or_default()
                .into_iter()
                .map(|(source, state)| ContactSyncSummary {
                    cached: cached_contacts
                        .get(&source)
                        .map_or(0, |contacts| contacts.len() as u64),
                    source,
                    enabled: state.enabled,
                    scanned: state.scanned,
                    complete: state.complete,
                })
                .collect::<Vec<_>>();
            for configured in &section.account.contact_sources {
                if !contact_sources
                    .iter()
                    .any(|summary| summary.source == configured.id)
                {
                    contact_sources.push(ContactSyncSummary {
                        cached: cached_contacts
                            .get(&configured.id)
                            .map_or(0, |contacts| contacts.len() as u64),
                        source: configured.id.clone(),
                        enabled: configured.enabled,
                        scanned: 0,
                        complete: false,
                    });
                }
            }
            contact_sources.sort_by(|left, right| left.source.cmp(&right.source));

            accounts.push(AccountSummary {
                id: section.account.id.clone(),
                label: section.account.label.clone(),
                email: section.account.email.clone(),
                unread: section_unread,
                color: account_colors
                    .get(section.account.id.0.as_str())
                    .copied()
                    .unwrap_or_else(|| section.account.resolved_color())
                    .to_string(),
                learning_paused: self
                    .store
                    .learning_state(&section.account.id)
                    .map(|state| state.paused)
                    .unwrap_or(false),
                contact_sources,
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
            revision,
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
    // A test must not inherit a stopped worker from an earlier test.
    let mut gate = MAINTENANCE_CANCEL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gate.token = None;
    gate.stop_pending = false;
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

    let store = Arc::new(
        Store::open(database_path(&config.data_dir))
            .map_err(|error| format!("open store: {error}"))?,
    );
    store
        .migrate()
        .map_err(|error| format!("migrate store: {error}"))?;
    let sessions = Arc::new(crate::auth::AccountSessionManager::new(store.clone()));

    // Drop pending-auth rows and staged references left by a previous run so an
    // abandoned attempt cannot leak a protected secret.
    {
        let secrets = crate::auth::CredentialSecretRepository::with_gate(
            config.credentials.clone(),
            store.clone(),
        );
        if let Err(error) = crate::auth::sweep_expired_pending_auth(&store, &secrets) {
            log::warn!("sweep expired pending authentication: {error}");
        }
    }

    let config_path = config::document_path(&config.config_dir);
    let (mut config_doc, startup_warning, writes_blocked) =
        load_or_migrate_document(&store, &config_path);

    // Finish or roll back any backend switch interrupted by a crash. This runs
    // after the document is loaded and before the state is installed, so the
    // configuration and the protected credential are always a consistent pair
    // when the first snapshot is built.
    let mut startup_warning = startup_warning;
    if !writes_blocked {
        match accounts::recover_switch_intents(
            &store,
            &config.credentials,
            &config_path,
            &mut config_doc,
        ) {
            Ok(warnings) => {
                if !warnings.is_empty() {
                    let text = warnings.join(" ");
                    startup_warning = Some(match startup_warning {
                        Some(existing) => format!("{existing} {text}"),
                        None => text,
                    });
                }
            }
            Err(error) => {
                log::warn!("recover interrupted backend switch: {error}");
                let text = format!("An interrupted account switch could not be completed: {error}");
                startup_warning = Some(match startup_warning {
                    Some(existing) => format!("{existing} {text}"),
                    None => text,
                });
            }
        }
    } else if store
        .switch_intents()
        .map(|intents| !intents.is_empty())
        .unwrap_or(false)
    {
        log::warn!("an interrupted backend switch is pending but configuration writes are blocked");
    }

    // A rollback backup without a surviving intent can only be an orphan (for
    // example a crash between the intent delete and the reference discard, or a
    // historical bug). Remove it for every account we can name so no protected
    // secret outlives its switch.
    accounts::sweep_orphaned_switch_references(&store, &config.credentials, &config_doc.accounts);

    let sender = events::sender().clone();
    let mut state = AppState {
        config,
        store,
        sessions,
        config_doc,
        config_path,
        startup_warning,
        config_writes_blocked: writes_blocked,
        selected_folder: None,
        unified_selected: false,
        conversation_limit: INITIAL_PAGE,
        selected_conversation: None,
        open_generation: 0,
        prefetch_generation: 0,
        search: SearchState::default(),
        server_more: None,
        capabilities: HashMap::new(),
        scheduler: Arc::new(AccountScheduler::new()),
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
        Err(config::ConfigError::TooNew { found, supported }) => {
            // The file was written by a newer build. Leave it exactly as it is
            // (never quarantine or overwrite it) and refuse every later write.
            log::warn!("configuration format {found} is newer than supported {supported}");
            let doc = document_from_store(store);
            (
                doc,
                Some(format!(
                    "The configuration file was written by a newer version of pige (format {found}; this build supports {supported}); changes will not be saved."
                )),
                true,
            )
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
        blocked_senders: Vec::new(),
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

/// Durable local-learning progress for one account. This is read-only and never
/// opens a provider, so the UI can show how much history is still to learn.
pub fn learning_progress(account: &AccountId) -> Result<LearningProgress, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    if !state
        .config_doc
        .accounts
        .iter()
        .any(|configured| &configured.id == account)
    {
        return Err(format!("unknown account {account}"));
    }
    crate::sync::history::learning_progress(&state.store, account)
        .map_err(|error| format!("read learning progress: {error}"))
}

/// Opportunistically learn more header history for one account while the app is
/// open.
///
/// This is the foreground counterpart to the small slice that runs after each
/// sync. It uses [`foreground_budget`](crate::sync::history::foreground_budget),
/// so it stops after a bounded number of pages (or the foreground deadline) and
/// the next call resumes from the durable checkpoint. It never inserts history
/// into the visible cache and never emits a new-mail notification. A caller
/// that cannot get one of the process-wide account slots returns the durable
/// progress without doing provider work.
pub async fn learn_history_now(account: &AccountId) -> Result<LearningProgress, String> {
    let (config, store, sessions, scheduler, account) = {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let account = state
            .config_doc
            .accounts
            .iter()
            .find(|configured| &configured.id == account)
            .cloned()
            .ok_or_else(|| format!("unknown account {account}"))?;
        (
            state.config.clone(),
            state.store.clone(),
            state.sessions.clone(),
            state.scheduler.clone(),
            account,
        )
    };

    let ctx = sessions
        .context(&account.id, crate::sync::history::HISTORY_FOREGROUND_TIMEOUT)
        .map_err(|error| format!("{}: {}", account.label, error.safe_message))?;
    crate::sync::history::learn_account_slice(
        &store,
        &config.providers,
        &account,
        &ctx,
        crate::sync::history::foreground_budget(),
        &scheduler,
    )
    .await
    .map_err(|error| format!("{}: {}", account.label, error.safe_message))
}

/// The cancellation token of the currently running native maintenance slice.
/// WorkManager's `onStopped` cancels this through [`cancel_maintenance`].
///
/// `stop_pending` closes the race where a worker is stopped before its pass has
/// installed a token: the next pass consumes the flag and exits immediately
/// instead of running a full budget that the OS already cancelled.
#[derive(Default)]
struct MaintenanceGate {
    token: Option<Arc<CancellationToken>>,
    stop_pending: bool,
}

static MAINTENANCE_CANCEL: Mutex<MaintenanceGate> = Mutex::new(MaintenanceGate {
    token: None,
    stop_pending: false,
});

/// Stop the current [`run_maintenance`] slice.
///
/// Cancellation is observed during the mail/outbox phase as well as between
/// contact/history pages, so the last committed page and its checkpoint
/// survive. This is the bridge used by Android when WorkManager stops the
/// worker; it is safe to call with no maintenance running.
pub fn cancel_maintenance() {
    let mut guard = MAINTENANCE_CANCEL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.token.as_ref() {
        Some(token) => token.cancel(),
        // No pass is running yet. Remember the stop so the pass that WorkManager
        // is about to start does not do a full budget of work first.
        None => guard.stop_pending = true,
    }
}

/// Cancellation and wall-clock scope for one maintenance pass.
///
/// The default (no deadline, no token) is an ordinary interactive sync: every
/// account keeps its own [`SYNC_TIMEOUT`]. A maintenance pass supplies both so
/// one slow account cannot outlive the worker's budget and a stop during the
/// mail phase is observed by the provider call itself, not after it returns.
#[derive(Clone, Default)]
struct SyncBounds {
    deadline: Option<tokio::time::Instant>,
    cancel: Option<Arc<CancellationToken>>,
}

impl SyncBounds {
    fn new(deadline: tokio::time::Instant, cancel: Arc<CancellationToken>) -> Self {
        Self {
            deadline: Some(deadline),
            cancel: Some(cancel),
        }
    }

    /// Whether the pass has been stopped or has reached its overall deadline.
    fn stopped(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
            || self
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    }

    /// Bounded passes run mail/outbox only; history gets its own reserved slice
    /// after the mail phase instead of being learned twice.
    fn is_bounded(&self) -> bool {
        self.deadline.is_some() || self.cancel.is_some()
    }

    /// The timeout for one account operation, capped by the pass deadline.
    fn remaining(&self, cap: std::time::Duration) -> std::time::Duration {
        match self.deadline {
            Some(deadline) => deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(cap),
            None => cap,
        }
    }

    /// Combine the maintenance stop with the account's generation cancellation.
    /// The returned guard cancels the listener task on drop, including early
    /// returns, so a background context never outlives its pass.
    fn context_with_cancel(
        &self,
        ctx: OperationContext,
    ) -> (OperationContext, Option<CancelListener>) {
        match &self.cancel {
            Some(cancel) => {
                let (ctx, listener) = with_maintenance_cancel(ctx, cancel);
                (ctx, Some(CancelListener(listener)))
            }
            None => (ctx, None),
        }
    }
}

/// Cancels a combined account/maintenance listener token when its account
/// operation finishes, so the spawned listener neither lingers nor leaks.
struct CancelListener(CancellationToken);

impl Drop for CancelListener {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Run one bounded native maintenance pass without an activity.
///
/// Mail and outbox delivery run first, so interactive work and already-queued
/// mail keep priority. Any remaining budget is shared between one header-history
/// slice and one contact-source snapshot slice per account. Nothing here
/// promises continuous background sync: WorkManager's periodic minimum is 15
/// minutes and a stopped worker leaves durable checkpoints for the next pass.
pub async fn run_maintenance(budget: std::time::Duration) -> Result<(), String> {
    if !is_initialized() {
        return Err("pige api is not initialized".to_string());
    }
    let token = Arc::new(CancellationToken::new());
    {
        let mut gate = MAINTENANCE_CANCEL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = gate.token.replace(token.clone()) {
            // A newer maintenance pass supersedes an older one; the older
            // slice exits with its last committed checkpoint.
            previous.cancel();
        }
        if std::mem::take(&mut gate.stop_pending) {
            // WorkManager stopped the worker before this pass installed its
            // token. Honour that stop instead of running a full budget.
            token.cancel();
        }
    }
    let deadline = tokio::time::Instant::now() + budget;
    let bounds = SyncBounds::new(deadline, token.clone());
    let result = run_maintenance_inner(&bounds, token.clone()).await;
    // Release the listener tasks and make a late cancel a no-op.
    token.cancel();
    {
        let mut gate = MAINTENANCE_CANCEL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if gate
            .token
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &token))
        {
            gate.token = None;
        }
    }
    result
}

async fn run_maintenance_inner(
    bounds: &SyncBounds,
    cancel: Arc<CancellationToken>,
) -> Result<(), String> {
    let deadline = bounds.deadline.unwrap_or_else(tokio::time::Instant::now);

    // Interactive mail and outbox delivery keep priority over local learning.
    // The pass deadline and stop token reach every account context, and the
    // outer timeout is a hard backstop if an adapter ignores its context.
    if !bounds.stopped() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, sync_all_with_bounds(bounds)).await {
            Ok(Err(error)) => log::warn!("maintenance mail sync: {error}"),
            Ok(Ok(_)) => {}
            Err(_) => log::debug!("maintenance mail phase reached the pass budget"),
        }
    }

    if bounds.stopped() {
        return Ok(());
    }

    let (config, store, sessions, scheduler, accounts, sender) = {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        (
            state.config.clone(),
            state.store.clone(),
            state.sessions.clone(),
            state.scheduler.clone(),
            state
                .store
                .accounts()
                .map_err(|error| format!("read accounts: {error}"))?,
            state.events.clone(),
        )
    };

    // Resolve the enabled contact sources first. If any exist, reserve half of
    // the remaining pass for them: history resumes from a durable cursor, so a
    // starving history crawl must not consume the only budget contacts get.
    let contact_targets: Vec<(&Account, &crate::providers::ContactSourceId)> = accounts
        .iter()
        .flat_map(|account| {
            account
                .contact_sources
                .iter()
                .filter(|configured| configured.enabled)
                .filter(|configured| {
                    store
                        .contact_source_state(&account.id, &configured.id)
                        .is_ok_and(|state| state.enabled)
                })
                .map(move |configured| (account, &configured.id))
        })
        .collect();

    let now = tokio::time::Instant::now();
    let mut history_deadline = deadline;
    if !contact_targets.is_empty() && now < deadline {
        history_deadline = now + (deadline - now) / 2;
    }

    // One bounded history slice per account, then one contact slice per enabled
    // source. Both resume from their durable cursors on the next pass. History
    // may use its reserved share only; unused time falls through to contacts.
    for account in &accounts {
        if cancel.is_cancelled() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= history_deadline {
            break;
        }
        let remaining = history_deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(ctx) = sessions.context(&account.id, remaining) else {
            continue;
        };
        let (ctx, _listener) = bounds.context_with_cancel(ctx);
        let budget = WorkBudget::new(
            crate::sync::history::HISTORY_PAGE_SIZE,
            crate::sync::history::HISTORY_MAX_PAGES,
            history_deadline,
        );
        match crate::sync::history::learn_account_slice(
            &store,
            &config.providers,
            account,
            &ctx,
            budget,
            &scheduler,
        )
        .await
        {
            Ok(progress) => log::debug!(
                "maintenance history for {}: scanned {} complete {}",
                account.label,
                progress.scanned,
                progress.complete
            ),
            Err(error) => log::debug!(
                "maintenance history for {}: {}",
                account.label,
                error.safe_message
            ),
        }
    }

    for (account, source) in contact_targets {
        if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            return Ok(());
        }
        // Re-read under the pass: a disable or a permission failure may have
        // landed while history was running.
        let Ok(state) = store.contact_source_state(&account.id, source) else {
            continue;
        };
        if !state.enabled {
            continue;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(ctx) = sessions.context(&account.id, remaining) else {
            continue;
        };
        let (ctx, _listener) = bounds.context_with_cancel(ctx);
        let mut session = match config.providers.open(account, &ctx).await {
            Ok(session) => session,
            Err(error) => {
                log::debug!(
                    "maintenance contacts for {}: {}",
                    account.label,
                    error.safe_message
                );
                continue;
            }
        };
        let Ok(service) = session.address_book_service() else {
            continue;
        };
        let budget = WorkBudget::new(
            crate::contacts::sync::CONTACTS_PAGE_SIZE,
            crate::contacts::sync::CONTACTS_MAX_PAGES,
            deadline,
        );
        match crate::contacts::sync::sync_slice_paged(
            &store,
            service.as_mut(),
            source,
            &ctx,
            budget,
            &scheduler,
        )
        .await
        {
            Ok(progress) => {
                let _ = sender.send(CoreEvent::ContactSyncProgress {
                    account: account.id.clone(),
                    source: source.clone(),
                    scanned: progress.scanned,
                    complete: progress.complete,
                });
            }
            Err(error) => log::debug!(
                "maintenance contacts for {} / {source}: {}",
                account.label,
                error.safe_message
            ),
        }
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(())
}

/// Combine the account's cancellation (generation invalidation) with the
/// maintenance stop token. The returned token cancels the listener task.
fn with_maintenance_cancel(
    mut ctx: OperationContext,
    maintenance: &Arc<CancellationToken>,
) -> (OperationContext, CancellationToken) {
    let combined = CancellationToken::new();
    let account = ctx.cancellation.clone();
    let maintenance = maintenance.clone();
    let out = combined.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = account.cancelled() => out.cancel(),
            _ = maintenance.cancelled() => out.cancel(),
            // The slice finished and its combined token was dropped/cancelled;
            // let this listener exit instead of lingering until process end.
            _ = out.cancelled() => {}
        }
    });
    ctx.cancellation = combined.clone();
    (ctx, combined)
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
        config::parse_document(&bytes).map_err(|error| format!("read backup: {error}"))?;

    // Everything below is a committed change; a failure before the document is
    // durable leaves the running configuration exactly as it was.
    let previous = state.config_doc.clone();
    if let Err(error) = config::pre_restore_copy(&state.config_path) {
        log::warn!("keep pre-restore copy: {error}");
    }

    // A restore supersedes any interrupted switch, and every current account
    // generation is stale once the document is replaced. Cancel live work
    // *before* rebuilding the mirror so a late worker cannot write into the
    // restored configuration. Bumping generations is deliberately fail-safe:
    // even if the save below fails, in-flight work simply has to restart.
    for account in &previous.accounts {
        if let Err(error) = state.sessions.invalidate(&account.id) {
            log::warn!(
                "invalidate session before restore for {}: {error}",
                account.id
            );
        }
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

    accounts::revoke_sessions_best_effort(state.config.auth.clone(), dropped.clone());

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

    // The restored document is durable. Reconcile any switch that was
    // interrupted before this restore against the restored shapes, so the
    // restored backend is never paired with the other backend's credential.
    // A failure keeps the intent and removes the unmatched active credential;
    // the next start retries the recovery.
    let mut warnings = accounts::reconcile_restored_switches(
        &state.store,
        &state.config.credentials,
        &state.config_doc.accounts,
    );

    // The document is durable; now the dropped accounts' secrets must not
    // linger, or a later restore of the same id would silently re-attach one.
    // A failure is reported rather than hidden: the app cannot both claim a
    // clean restore and keep the old password.
    for id in &dropped {
        // The generation was already advanced before the mirror rebuild. Clean
        // the dropped account's durable jobs and cached provider state now.
        if let Err(error) = state.store.clear_account_jobs(id) {
            log::warn!("clear jobs for restored-away account {id}: {error}");
        }
        if let Err(error) = state.config.credentials.delete(id) {
            log::warn!("delete password for restored-away account {id}: {error}");
            warnings.push(format!(
                "A stored password for a removed account could not be cleared: {error}"
            ));
        }
        if let Err(error) = state.config.credentials.delete_session(id) {
            log::warn!("clear session password for restored-away account {id}: {error}");
        }
        if let Err(error) =
            accounts::discard_switch_backup_reporting(&state.config.credentials, id)
        {
            log::warn!("discard rollback backup for restored-away account {id}: {error}");
            warnings.push(format!(
                "A rollback credential for a removed account could not be cleared: {error}"
            ));
        }
    }

    state.selected_folder = None;
    state.invalidate_open();
    state.prefetch_generation = state.prefetch_generation.wrapping_add(1);
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

    // Cancel every live generation and discard switch remnants before the
    // destructive write. A worker that started earlier must not repopulate a
    // credential or cache row after the reset. Provider-session revocation is
    // best effort and never delays the local cleanup.
    accounts::revoke_sessions_best_effort(state.config.auth.clone(), ids.clone());
    let mut warnings = accounts::discard_pending_switches(&state.store, &state.config.credentials)?;
    for id in &ids {
        if let Err(error) = state.sessions.invalidate(id) {
            log::warn!("invalidate session during factory reset for {id}: {error}");
        }
    }

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

    for id in &ids {
        if let Err(error) = state.config.credentials.delete(id) {
            log::warn!("delete password during factory reset for {id}: {error}");
            warnings.push(format!("A stored password could not be cleared: {error}"));
        }
        if let Err(error) = state.config.credentials.delete_session(id) {
            log::warn!("clear session password during factory reset for {id}: {error}");
        }
        // Remove a rollback backup even if its intent row was already lost.
        if let Err(error) =
            accounts::discard_switch_backup_reporting(&state.config.credentials, id)
        {
            log::warn!("discard rollback backup during factory reset for {id}: {error}");
            warnings.push(format!(
                "A rollback credential could not be cleared during the reset: {error}"
            ));
        }
    }

    // No job, lease, pending-auth reference, or switch intent survives a reset.
    if let Err(error) = state.store.clear_all_jobs() {
        log::warn!("clear durable jobs during factory reset: {error}");
        warnings.push(format!(
            "Some background job state could not be cleared: {error}"
        ));
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
    state.invalidate_open();
    state.prefetch_generation = state.prefetch_generation.wrapping_add(1);
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

        // The legacy form is IMAP-only. Never rewrite a native backend into an
        // IMAP/SMTP account with invented endpoints.
        if existing.imap_smtp().is_none() {
            return Err(
                "this account uses a native backend and cannot be edited as an IMAP/SMTP account"
                    .to_string(),
            );
        }

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

        // An account edit changes the configuration that in-flight work was
        // based on. Cancel every old context and durably advance the generation
        // before writing the replacement credential, so a refresh or sync that
        // started earlier can neither continue nor persist its result.
        state
            .sessions
            .invalidate(id)
            .map_err(|error| format!("invalidate account session: {error}"))?;

        if wrote_secret {
            set_secret(state, id, &input.password, target)
                .map_err(|error| format!("store password: {error}"))?;
        }

        let mut account = input.into_account(id.clone(), existing.sort_order);
        // The legacy editor has no contact-source controls yet; an ordinary
        // endpoint/password edit must not silently disable the account's
        // configured sources.
        account.contact_sources = existing.contact_sources.clone();
        // Editing connection details must not silently reset a colour the user
        // picked. Only fall back to the default when the old account had none.
        account.color = existing
            .color
            .clone()
            .or_else(|| Some(default_account_color(existing.sort_order).to_string()));
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

/// Change only an account's accent colour. The value must come from
/// [`crate::account::ACCOUNT_COLORS`], so snapshots always contain a colour the
/// UI knows how to parse.
pub async fn set_account_color(id: &AccountId, color: &str) -> Result<ModelSnapshot, String> {
    if !is_supported_account_color(color) {
        return Err(format!("unsupported account colour: {color}"));
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
        account.color = Some(color.to_string());
        state.replace_accounts(next)?;
        state.ensure_selection();
        sender = state.events.clone();
        snapshot = state.build_snapshot();
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok(snapshot)
}

/// Remember a sender address as blocked. This only records the choice in the
/// configuration document; local recipient suggestions exclude it, but no inbox
/// filtering reads it yet. The address is trimmed and lowercased so duplicate
/// blocked senders with different casing collapse.
pub fn block_sender(address: &str) -> Result<(), String> {
    let address = address.trim().to_lowercase();
    if address.is_empty() || !address.contains('@') {
        return Err("this message has no sender address to block".to_string());
    }

    let mut guard = lock_state();
    let state = guard
        .as_mut()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state.ensure_writable()?;
    if state.config_doc.blocked_senders.contains(&address) {
        return Ok(());
    }
    state.config_doc.blocked_senders.push(address);
    if let Err(error) = config::save(&state.config_path, &state.config_doc) {
        state.config_doc.blocked_senders.pop();
        return Err(format!("save blocked senders: {error}"));
    }
    Ok(())
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

        // Revoke the provider session in the background; local cleanup never
        // waits for a remote provider and still proceeds if it fails.
        accounts::revoke_sessions_best_effort(state.config.auth.clone(), vec![id.clone()]);

        // An explicit removal supersedes any unfinished switch. Discard its
        // temporary credential references before deleting the account secret.
        if let Err(error) =
            accounts::discard_switch_intent(&state.store, &state.config.credentials, id)
        {
            log::warn!("discard interrupted switch for removed account {id}: {error}");
        }

        // Cancel live work and advance the generation *before* touching the
        // secret, so a refresh already past its old fence cannot re-persist a
        // rotated credential for an account the user can no longer see.
        if let Err(error) = state.sessions.invalidate(id) {
            log::warn!("invalidate session for removed account {id}: {error}");
        }

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

        // No durable job or pending-auth reference may outlive the account.
        if let Err(error) = state.store.clear_account_jobs(id) {
            log::warn!("clear jobs for removed account {id}: {error}");
        }
        // A refresh already past its fence when the generation was bumped could
        // have re-written a rotated credential; delete once more so nothing
        // stale survives now that future writes are fenced.
        if let Err(error) = state.config.credentials.delete(id) {
            log::warn!("clear credential after invalidating removed account {id}: {error}");
        }
        if let Err(error) = state.config.credentials.delete_session(id) {
            log::warn!("clear session credential after invalidating removed account {id}: {error}");
        }
        // A rollback backup must not outlive the account even if its intent row
        // was already gone.
        accounts::discard_switch_backup(&state.config.credentials, id);

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
        state.invalidate_open();
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
    sync_all_with_bounds(&SyncBounds::default()).await
}

/// Sync every account within an optional overall deadline and stop token.
///
/// The public [`sync_all`] passes the default (interactive) bounds; background
/// maintenance supplies both so a per-account `SYNC_TIMEOUT` cannot outlive the
/// pass and a WorkManager stop reaches the in-flight provider call.
async fn sync_all_with_bounds(bounds: &SyncBounds) -> Result<ModelSnapshot, String> {
    let _sync = SYNC_LOCK.lock().await;
    let (config, sessions, scheduler, accounts, sender) = {
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
        let sessions = state.sessions.clone();
        let scheduler = state.scheduler.clone();
        let sender = state.events.clone();
        (config, sessions, scheduler, accounts, sender)
    };

    let _ = sender.send(CoreEvent::SnapshotChanged);
    let (outcome, capabilities) =
        sync_accounts(&config, &sessions, &scheduler, &accounts, &sender, bounds).await;

    let status = finish_sync(accounts.len(), outcome, capabilities);
    // The account sync never visits non-special folders; if one is open,
    // refresh it too so manual refresh and pull-to-refresh update the view. A
    // bounded maintenance pass skips it: its budget belongs to learning, and
    // the visible folder is refreshed by the next interactive sync.
    if !bounds.is_bounded()
        && let Some(folder) = selected_other_folder()
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
    let (config, sessions, scheduler, account, sender) = {
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
        (
            state.config.clone(),
            state.sessions.clone(),
            state.scheduler.clone(),
            account,
            state.events.clone(),
        )
    };

    let _ = sender.send(CoreEvent::SnapshotChanged);
    let accounts = std::slice::from_ref(&account);
    let (outcome, capabilities) = sync_accounts(
        &config,
        &sessions,
        &scheduler,
        accounts,
        &sender,
        &SyncBounds::default(),
    )
    .await;

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
    let (config, account, folder, ctx, scheduler, sender) = {
        let guard = lock_state();
        let state = guard
            .as_ref()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let (account, folder) = locate_folder(&state.store, id)?;
        let ctx = state.sessions.context(&account.id, SYNC_TIMEOUT).ok();
        (
            state.config.clone(),
            account,
            folder,
            ctx,
            state.scheduler.clone(),
            state.events.clone(),
        )
    };
    let Some(ctx) = ctx else {
        return Err(not_initialized_message());
    };

    let store = Store::open(database_path(&config.data_dir))
        .map_err(|error| format!("open folder store: {error}"))?;
    // Hold the account's gate for the whole remote-read/apply sequence so a
    // concurrent mutation for the same account cannot interleave it.
    let operation = {
        let account = account.clone();
        let folder = folder.clone();
        let config = config.clone();
        let store = store;
        async move {
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
            .await
            .map(|_| ())
        }
    };
    scheduler
        .run_interactive(&ctx, operation)
        .await
        .map_err(scheduler_message)??;

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
        state.invalidate_open();
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
        state.invalidate_open();
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
            state.invalidate_open();
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
                .oldest_uid_in_folder(folder)
                .map_err(|error| format!("read oldest uid: {error}"))?;
            // The context and gate are captured here (under the state lock) so
            // the backfill can run through `AccountScheduler` exactly like a
            // sync page, after every lock is released.
            let ctx = state
                .sessions
                .context(&account.id, SYNC_TIMEOUT)
                .map_err(|error| error.safe_message)?;
            let sender = state.events.clone();
            (
                Some(BackfillRequest {
                    config: state.config.clone(),
                    account,
                    folder: folder_row,
                    oldest,
                    limit,
                    ctx,
                    scheduler: state.scheduler.clone(),
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

    // The backfill reads remote headers and durably upserts them, so it holds
    // the account's gate like a sync pass: a concurrent mutation cannot
    // interleave its read/apply, and a stale page cannot reverse a mutation
    // that already completed. (The provider contract has no neutral
    // "older-than-UID" read; see the note on [`backfill_older`].)
    let BackfillRequest {
        config,
        account,
        folder: backfill_folder,
        oldest,
        limit,
        ctx,
        scheduler,
    } = request;
    let operation = backfill_older(&config, &store, &account, &backfill_folder, oldest);
    let outcome = scheduler
        .run_interactive(&ctx, operation)
        .await
        .map_err(scheduler_message)
        .and_then(|stored| stored);

    match outcome {
        Ok((stored, server_more)) => {
            let mut guard = lock_state();
            if let Some(state) = guard.as_mut()
                && state.selected_folder.as_ref() == Some(&backfill_folder.id)
                && state.conversation_limit == limit
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

/// Quietly cache likely-to-be-read Inbox bodies, with bounded connections.
/// This runs only in foreground shells, not in Android's background worker.
pub async fn prefetch_bodies() {
    let Ok(_prefetch) = PREFETCH_LOCK.try_lock() else {
        return;
    };
    let (config, sessions, scheduler, candidates, generation) = {
        let guard = lock_state();
        let Some(state) = guard.as_ref() else { return };
        if !state.config_doc.settings.prefetch_bodies {
            return;
        }
        let candidates = match state
            .store
            .prefetch_candidates(MAX_BODY_BYTES, PREFETCH_LIMIT)
        {
            Ok(candidates) => candidates,
            Err(error) => {
                log::warn!("prefetch candidates: {error}");
                return;
            }
        };
        (
            state.config.clone(),
            state.sessions.clone(),
            state.scheduler.clone(),
            candidates,
            state.prefetch_generation,
        )
    };
    if candidates.is_empty() {
        return;
    }
    let store = match Store::open(database_path(&config.data_dir)) {
        Ok(store) => store,
        Err(error) => {
            log::warn!("open prefetch cache: {error}");
            return;
        }
    };
    // One authenticated session for the whole per-account batch: opening a
    // session per candidate amplified a prefetch pass into hundreds of logins,
    // which Gmail's per-account connection limit rejects outright. The batch
    // reuses the session; only the (unauthenticated) durable caching stays
    // per candidate.
    let mut groups = HashMap::<AccountId, Vec<_>>::new();
    for candidate in candidates {
        groups
            .entry(candidate.account_id.clone())
            .or_default()
            .push(candidate);
    }
    futures::stream::iter(groups)
        .map(|(account_id, candidates)| {
            let store = &store;
            let config = &config;
            let sessions = &sessions;
            let scheduler = &scheduler;
            async move {
                let result: Result<(), String> = async {
                    let account = find_account(store, &account_id)?;
                    let ctx = sessions
                        .context(&account_id, INTERACTIVE_TIMEOUT)
                        .map_err(|error| error.safe_message)?;
                    let providers = config.providers.clone();
                    let mut session = open_session(&providers, &account, &ctx).await?;
                    for candidate in candidates {
                        // A removed/replaced account or reset invalidates this batch.
                        {
                            let guard = lock_state();
                            let Some(state) = guard.as_ref() else { break };
                            if generation != state.prefetch_generation
                                || !state.config_doc.settings.prefetch_bodies
                            {
                                break;
                            }
                            if !prefetch_candidate_is_current(state, &candidate) {
                                continue;
                            }
                        }
                        match store.body(&candidate.message_id) {
                            Ok(Some(_)) => continue,
                            Ok(None) => {}
                            Err(error) => {
                                log::warn!("prefetch cache check: {error}");
                                continue;
                            }
                        }
                        // The remote mapping names the body fetch; a message with
                        // no provider identity cannot be prefetched.
                        let Ok(remote) = store.remote_message(&account_id, &candidate.message_id)
                        else {
                            continue;
                        };
                        let operation =
                            async { body::fetch_body_with(&mut session, &ctx, &remote).await };
                        let content = match scheduler.run_background(&ctx, operation).await {
                            Ok(Some(Ok(content))) => content,
                            // A busy gate skips this candidate; the next quiet
                            // pass retries it. `None` is not an error.
                            Ok(Some(Err(error))) => {
                                log::warn!("prefetch {}: {error}", candidate.message_id);
                                continue;
                            }
                            Ok(None) => continue,
                            Err(error) => {
                                log::debug!(
                                    "prefetch {} deferred: {}",
                                    candidate.message_id,
                                    scheduler_message(error)
                                );
                                continue;
                            }
                        };
                        // Revalidate and cache atomically with respect to reset/removal.
                        let guard = lock_state();
                        if let Some(state) = guard.as_ref()
                            && generation == state.prefetch_generation
                            && prefetch_candidate_is_current(state, &candidate)
                        {
                            body::cache_body_content(&state.store, &candidate.message_id, &content);
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    log::warn!("prefetch {account_id}: {error}");
                }
            }
        })
        .buffer_unordered(PREFETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
}

fn prefetch_candidate_is_current(state: &AppState, candidate: &PrefetchCandidate) -> bool {
    state
        .config_doc
        .accounts
        .iter()
        .any(|a| a.id == candidate.account_id)
        && state
            .store
            .message(&candidate.message_id)
            .ok()
            .flatten()
            .is_some_and(|m| {
                let in_folder = state
                    .store
                    .message_memberships(&m.id)
                    .map(|memberships| memberships.contains(&candidate.folder_id))
                    .unwrap_or(false);
                let same_remote =
                    state
                        .store
                        .imap_coords(&m.id)
                        .ok()
                        .flatten()
                        .is_some_and(|coords| {
                            coords.uid == candidate.uid
                                && coords.uidvalidity == candidate.uidvalidity
                        });
                m.account_id == candidate.account_id
                    && in_folder
                    && same_remote
                    && m.header.size <= MAX_BODY_BYTES
            })
}

/// Open a conversation for the reader: load its messages, resolve the newest
/// message's body (from cache or through the account's provider session), and
/// install the resulting [`ConversationDetail`] as the selected conversation.
///
/// A body over [`MAX_BODY_BYTES`] is opened without a fetch and marked
/// [`MessageBody::TooLarge`]; a missing or unreachable body degrades to
/// `body: None` so the reader always renders its chrome even offline.
pub async fn open_conversation(thread_id: &ThreadId) -> Result<ModelSnapshot, String> {
    // Phase 1: read the conversation, its cached body, and the fetch decision.
    let (messages, config, account, remote, cached, too_large, ctx, events, initial, generation) = {
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
        // The remote mapping names the body fetch. A native account with no
        // mapping opens its cached body (or an empty reader) without a fetch.
        let remote = state
            .store
            .remote_message(&account.id, &messages[0].id)
            .ok();
        let cached = state
            .store
            .body(&messages[0].id)
            .map_err(|error| format!("read cached body: {error}"))?;
        let too_large = messages[0].header.size > MAX_BODY_BYTES;
        state.invalidate_open();
        let generation = state.open_generation;
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
        let ctx = state
            .sessions
            .context(&account.id, INTERACTIVE_TIMEOUT)
            .ok();
        (
            messages,
            state.config.clone(),
            account,
            remote,
            cached,
            too_large,
            ctx,
            state.events.clone(),
            initial,
            generation,
        )
    };
    let _ = events.send(CoreEvent::SnapshotChanged);
    if cached.is_some() || too_large {
        return Ok(initial);
    }

    // Phase 2: fetch an uncached, under-cap body through the provider session.
    // A body read applies nothing durable, so it does not take the account's
    // sync/mutation gate: it opens its own connection and never blocks an
    // interactive read behind a background sync (or vice versa).
    let fetched = match (remote, ctx) {
        (Some(remote), Some(ctx)) => {
            let providers = config.providers.clone();
            let account = account.clone();
            match body::fetch_body(&providers, &account, &ctx, &remote).await {
                Ok(content) => Some(content),
                Err(error) => {
                    log::warn!("open conversation {thread_id}: {error}");
                    None
                }
            }
        }
        _ => None,
    };

    // Phase 3: cache and build the detail under the lock, using the shared
    // store handle rather than opening a second connection.
    let snapshot = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let body = if too_large {
            Some(MessageBody::TooLarge)
        } else if let Some(cached) = cached {
            document_from_body(&cached).map(MessageBody::Document)
        } else if let Some(content) = &fetched {
            body::cache_body_content(&state.store, &messages[0].id, content)
                .map(MessageBody::Document)
        } else {
            None
        };
        if generation != state.open_generation {
            return Ok(state.build_snapshot());
        }
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
    let (config, sessions, scheduler, account, id, sender) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| "pige api is not initialized".to_string())?;
        let account = find_account(&state.store, &message.account_id)?;
        // The compose UI has no attachment picker yet, so it passes no parts.
        // The submission layer still persists the empty vector and the stable
        // operation id before any network work.
        let id = send::queue_submission(&state.store, message, Vec::new())
            .map_err(|error| format!("queue message: {error}"))?;
        (
            state.config.clone(),
            state.sessions.clone(),
            state.scheduler.clone(),
            account,
            id,
            state.events.clone(),
        )
    };
    let _ = sender.send(CoreEvent::SnapshotChanged);

    let store = Store::open(database_path(&config.data_dir))
        .map_err(|error| format!("open outbox store: {error}"))?;

    // Submission is a provider capability from F9 onwards. The flush runs
    // under the same per-account gate as sync and mutations, so a sync pass can
    // never read and reconcile the `submitting` row while SMTP delivery is in
    // flight.
    if let Ok(ctx) = sessions.context(&account.id, INTERACTIVE_TIMEOUT) {
        let providers = config.providers.clone();
        let transports = config.transport.clone();
        let target = account.clone();
        let delivery = async {
            match providers.open(&target, &ctx).await {
                Ok(mut session) => match session.submission_service() {
                    Ok(service) => {
                        if let Err(error) =
                            send::flush_provider_outbox(&store, &target, service.as_mut(), &ctx)
                                .await
                        {
                            log::warn!("flush outbox for {}: {error}", target.label);
                        }
                    }
                    Err(error) => {
                        log::warn!(
                            "no submission service for {}: {}",
                            target.label,
                            error.safe_message
                        );
                    }
                },
                Err(error) => {
                    log::debug!(
                        "outbox provider for {} is unreachable: {}",
                        target.label,
                        error.safe_message
                    );
                    // IMAP is only needed for the Sent copy and mailbox reads;
                    // SMTP can still deliver. Keep the durable state machine
                    // but build the submission service directly from the
                    // protocol transports, so the message is not stranded just
                    // because the mailbox connection failed.
                    if ctx.ensure_active().is_ok() && target.imap_smtp().is_some() {
                        let mut fallback = ImapSubmission::new(transports, target.clone());
                        if let Err(error) =
                            send::flush_provider_outbox(&store, &target, &mut fallback, &ctx).await
                        {
                            log::warn!("SMTP-only flush outbox for {}: {error}", target.label);
                        }
                    }
                }
            }
        };
        if let Err(error) = scheduler.run_interactive(&ctx, delivery).await {
            log::debug!(
                "outbox for {} is waiting for the account gate: {error}",
                account.label
            );
        }
    }

    let entry = store
        .outbox_entry(&id)
        .map_err(|error| format!("read outbox: {error}"))?;
    let queued = entry.as_ref().is_some_and(|entry| {
        matches!(
            entry.state,
            send::SubmissionState::Queued
                | send::SubmissionState::Preparing
                | send::SubmissionState::Submitting
        )
    });
    let failure = entry.as_ref().and_then(|entry| match entry.state {
        send::SubmissionState::Rejected | send::SubmissionState::Uncertain => {
            entry.error.clone().or_else(|| {
                Some("the message may already have been sent; check Sent before retrying".to_string())
            })
        }
        _ => None,
    });

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
    let (config, account, remote, cached_path, ctx) = {
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
        // The remote identity names the attachment fetch; the provider resolves
        // the part within the message it owns.
        let remote = state.store.remote_message(&account.id, message).ok();
        let cached_path = state
            .store
            .attachment(message, part)
            .map_err(|error| format!("read attachment: {error}"))?
            .and_then(|attachment| attachment.cached_path)
            .and_then(|path| verified_cached_attachment(&state.config.cache_dir, &path));
        let ctx = state
            .sessions
            .context(&account.id, INTERACTIVE_TIMEOUT)
            .ok();
        (state.config.clone(), account, remote, cached_path, ctx)
    };

    let path = match cached_path {
        Some(path) => path,
        None => {
            let Some(remote) = remote else {
                return Err("this message has no remote identity to fetch".to_string());
            };
            let Some(ctx) = ctx else {
                return Err(not_initialized_message());
            };

            // The provider body fetch records each part's metadata and remote
            // id; without it there is nothing to name the fetch or the file.
            let metadata = {
                let guard = lock_state();
                let state = guard.as_ref().ok_or_else(not_initialized_message)?;
                state
                    .store
                    .attachment(message, part)
                    .map_err(|error| format!("read attachment: {error}"))?
            };
            // When the part was not cached yet, fetch the body once to learn its
            // descriptors. Its original bytes (when the provider supplies them)
            // are reused so an IMAP message is never downloaded twice for the
            // same open; a provider with no raw form falls back to an explicit
            // `attachment` fetch named by the persisted remote descriptor.
            let (metadata, prefetched) = match metadata {
                Some(metadata) => (metadata, None),
                None => {
                    let providers = config.providers.clone();
                    let account = account.clone();
                    let remote_body = remote.clone();
                    let content = body::fetch_body(&providers, &account, &ctx, &remote_body)
                        .await
                        .map_err(|error| error.to_string())?;
                    let guard = lock_state();
                    let state = guard.as_ref().ok_or_else(not_initialized_message)?;
                    body::cache_body_content(&state.store, message, &content);
                    let metadata = state
                        .store
                        .attachment(message, part)
                        .map_err(|error| format!("read attachment: {error}"))?
                        .ok_or_else(|| {
                            format!("attachment part {part} is not present in the message")
                        })?;
                    (metadata, Some(content))
                }
            };

            // Reject a known oversized part before downloading it again.
            check_attachment_size(metadata.part.size).map_err(|error| error.to_string())?;

            let bytes = match prefetched.as_ref().and_then(|content| content.raw.clone()) {
                Some(raw) => attachment_bytes(&raw, &metadata.part.part_id).ok_or_else(|| {
                    format!(
                        "attachment part {} could not be decoded",
                        metadata.part.part_id
                    )
                })?,
                None => {
                    // Prefer the provider's remote id; fall back to the local
                    // MIME part id for a part cached before the remote id column
                    // existed.
                    let remote_attachment = {
                        let guard = lock_state();
                        let state = guard.as_ref().ok_or_else(not_initialized_message)?;
                        state
                            .store
                            .attachment_remote_id(message, part)
                            .map_err(|error| format!("read attachment remote id: {error}"))?
                            .unwrap_or_else(|| RemoteAttachmentId::from(part.to_string()))
                    };

                    let providers = config.providers.clone();
                    let account = account.clone();
                    let remote_message = remote.clone();
                    let remote_attachment_for_fetch = remote_attachment.clone();
                    body::fetch_attachment(
                        &providers,
                        &account,
                        &ctx,
                        &remote_message,
                        &remote_attachment_for_fetch,
                    )
                    .await?
                }
            };
            check_attachment_size(bytes.len() as u64).map_err(|error| error.to_string())?;

            let path = cache_attachment(&config.cache_dir, message, &metadata.part, &bytes)
                .map_err(|error| error.to_string())?;

            {
                let guard = lock_state();
                let state = guard.as_ref().ok_or_else(not_initialized_message)?;
                state
                    .store
                    .set_attachment_cached_path(message, &metadata.part.part_id, &path)
                    .map_err(|error| format!("store attachment path: {error}"))?;
            }

            path
        }
    };

    let opener = {
        let guard = lock_state();
        guard
            .as_ref()
            .ok_or_else(not_initialized_message)?
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
            sender_email: message
                .header
                .from
                .first()
                .map(|address| address.email.clone())
                .unwrap_or_default(),
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

/// The user-facing error when a captured context is gone (removed account or
/// uninitialized state), shared by the API's orchestration modules.
pub(crate) fn not_initialized_message() -> String {
    "pige api is not initialized".to_string()
}

/// Unix seconds, UTC, for the body cache's `fetched_at`.
pub(crate) fn now_unix() -> i64 {
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
    /// The account-bound context whose gate the backfill runs under.
    ctx: OperationContext,
    scheduler: Arc<AccountScheduler>,
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
///
/// **Contract gap (F8 follow-up).** This is the one remaining mail read that
/// still speaks the protocol transport directly. The provider contract has no
/// neutral "older page" primitive: [`SyncService`](crate::providers::SyncService)
/// pages *newer* mail from a durable cursor, and
/// [`MailboxService::history`](crate::providers::MailboxService::history) is a
/// newest-first crawl across every mailbox that cannot target one collection
/// below a UID or UIDVALIDITY. The caller gates this call under
/// `AccountScheduler`, so it cannot interleave a mutation, but it should
/// eventually route through the boundary.
///
/// The minimal neutral addition would be a mailbox-scoped history request, for
/// example:
///
/// ```ignore
/// // In HistoryRequest (F1-owned), add an optional collection scope and an
/// // exclusive "older than" bound in provider-neutral terms:
/// pub struct HistoryRequest {
///     pub cursor: Option<ProviderCursor>,
///     pub page_size: u32,
///     pub collection: Option<RemoteCollectionId>,
///     pub before: Option<RemoteMessageId>, // "strictly older than this message"
/// }
/// ```
///
/// With `collection` set and `before` naming the oldest cached message, the
/// adapter can translate the bound into its own UID space internally (and
/// reject a UIDVALIDITY change as `CursorInvalid`) without the application ever
/// inventing a UID. That request shape is an F1 contract change and out of this
/// task's file scope.
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
        let remote_id = ImapCoords::new(folder.name.clone(), uidvalidity, uid).encode();
        // A re-page of an already-cached UID reuses its local content id.
        let id = store
            .remote_message_for(
                &account.id,
                crate::providers::IMAP_SMTP_PROVIDER,
                &remote_id,
            )
            .map_err(|error| format!("read remote older {}: {error}", folder.name))?
            .unwrap_or_else(MessageId::new);
        let participants = participants_of(&header);
        let thread_id = threads.assign(&account.id, &header, header.date, &participants);
        for (from, into) in threads.take_merges() {
            store
                .merge_threads(&account.id, &from, &into)
                .map_err(|error| format!("merge older {}: {error}", folder.name))?;
        }
        let stored = StoredMessage {
            id: id.clone(),
            account_id: account.id.clone(),
            folder_id: Some(folder.id.clone()),
            uid: Some(uid),
            uidvalidity: Some(uidvalidity),
            header,
            flags,
            thread_id,
            snippet: String::new(),
            has_attachments: false,
        };
        store
            .upsert_message(&stored)
            .map_err(|error| format!("store older {}: {error}", folder.name))?;
        store
            .set_remote_message(
                &account.id,
                crate::providers::IMAP_SMTP_PROVIDER,
                &remote_id,
                &id,
            )
            .map_err(|error| format!("map older {}: {error}", folder.name))?;
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
    sessions: &crate::auth::AccountSessionManager,
    scheduler: &AccountScheduler,
    accounts: &[Account],
    sender: &broadcast::Sender<CoreEvent>,
    bounds: &SyncBounds,
) -> (SyncOutcome, Vec<(AccountId, ProviderCapabilities)>) {
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
        if bounds.stopped() {
            break;
        }
        let _ = sender.send(CoreEvent::SyncStarted {
            account: account.id.clone(),
        });

        match sync_one(&store, config, sessions, scheduler, account, sender, bounds).await {
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
    sessions: &crate::auth::AccountSessionManager,
    scheduler: &AccountScheduler,
    account: &Account,
    sender: &broadcast::Sender<CoreEvent>,
    bounds: &SyncBounds,
) -> Result<ProviderCapabilities, String> {
    // A bounded pass caps this account's context at the pass deadline instead
    // of the interactive 60s. A missing or cancelled session means the account
    // was removed or invalidated; report it as unreachable rather than syncing.
    let timeout = bounds.remaining(SYNC_TIMEOUT);
    if timeout.is_zero() {
        return Err(format!("{}: {RECONNECT_ERROR}", account.label));
    }
    let Ok(ctx) = sessions.context(&account.id, timeout) else {
        return Err(format!("{}: {RECONNECT_ERROR}", account.label));
    };
    let (ctx, _listener) = bounds.context_with_cancel(ctx);

    // The whole remote-read/apply sequence holds the account's gate so a
    // concurrent mutation for the same account cannot interleave it, and a late
    // sync page can never reverse a completed mutation.
    let providers = config.providers.clone();
    let target = account.clone();
    let sync_ctx = ctx.clone();
    let inner = scheduler
        .run_interactive(&ctx, async move {
            let mut session = providers
                .open(&target, &sync_ctx)
                .await
                .map_err(|error| error.safe_message)?;
            let capabilities = session.capabilities;
            let budget = WorkBudget::new(SYNC_WINDOW, SYNC_PAGES, sync_ctx.deadline);
            let report = provider_sync_account(store, &mut session, &sync_ctx, budget)
                .await
                .map_err(|error| error.to_string())?;

            // Deliver queued mail through the same provider session. A
            // cancelled/expired context or an unreachable provider leaves the
            // rows durable for the next pass; accepted rows only retry the Sent
            // copy, never delivery.
            let mut outbox_errors = Vec::new();
            let has_pending = store
                .outbox_pending(&target.id)
                .is_ok_and(|pending| !pending.is_empty());
            if has_pending
                && let Some(service) = session.submission.as_mut()
            {
                match send::flush_provider_outbox(store, &target, service.as_mut(), &sync_ctx).await
                {
                    Ok(outbox) => outbox_errors.extend(outbox.errors),
                    Err(error) => outbox_errors.push(error.to_string()),
                }
            }

            Ok::<_, String>((report, capabilities, outbox_errors, session))
        })
        .await
        .map_err(scheduler_message)?;
    let (report, capabilities, outbox_errors, mut session) =
        inner.map_err(|error| format!("{}: {error}", account.label))?;

    // After the visible-window sync, learn one bounded slice of header history
    // using the session that is already open, so background learning never
    // amplifies provider connections. A bounded maintenance pass skips this:
    // it runs its own reserved history slice after the mail phase, so doing it
    // here would spend the contact budget twice. The interactive gate is
    // released now; the paged history path re-takes it per page and yields
    // between pages. History never inserts headers into the visible mailbox
    // cache or notifies, and a history failure must not fail the user-visible
    // sync.
    if !bounds.is_bounded()
        && capabilities.mailbox
        && let Some(mailbox) = session.mailbox.as_mut()
        && let Ok(history_ctx) =
            sessions.context(&account.id, crate::sync::history::HISTORY_SLICE_TIMEOUT)
    {
        match crate::sync::history::learn_background_slice(
            store,
            mailbox.as_mut(),
            &history_ctx,
            crate::sync::history::background_budget(),
            scheduler,
        )
        .await
        {
            Ok(progress) => log::debug!(
                "history learning for {}: scanned {} complete {}",
                account.label,
                progress.scanned,
                progress.complete
            ),
            Err(error) => log::debug!(
                "history learning for {}: {}",
                account.label,
                error.safe_message
            ),
        }
    }

    // The provider page reports each collection's server total but not its
    // unread rollup, so recompute `unread` locally from the cached flags. This
    // keeps the navigation rail's counts correct after an ordinary sync without
    // touching the server-reported total that paging compares against.
    if let Ok(folders) = store.folders(&account.id) {
        for folder in folders {
            if let Err(error) = store.recompute_folder_unread(&folder.id) {
                log::warn!("recompute unread for {}: {error}", folder.id);
            }
        }
    }

    // The in-app event always fires so the list refreshes; only the OS
    // notification is suppressed when the user has turned notifications off.
    let notify_new_mail = {
        let guard = lock_state();
        guard
            .as_ref()
            .is_none_or(|state| state.config_doc.settings.notifications)
    };
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
    capabilities: Vec<(AccountId, ProviderCapabilities)>,
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
