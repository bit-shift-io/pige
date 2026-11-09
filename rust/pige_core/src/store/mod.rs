//! SQLite-backed cache. One database per installation holds every account,
//! folder, and cached message; platform code decides where it lives.

mod accounts;
mod attachments;
mod bodies;
mod contacts;
mod correspondents;
mod folders;
mod jobs;
mod lifecycle;
mod memberships;
mod messages;
mod migrations;
mod remote_ids;
mod schema;
mod search;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};

use crate::account::{Account, AccountId};
use crate::mail::{Conversation, Folder, FolderId, MessageId};
use crate::providers::{RemoteCollectionId, RemoteMessageId};

use accounts::accounts_on;
use folders::{folder_total_on, folders_on};
use messages::{conversations_on, message_count_on, unified_conversations_on};

pub use attachments::StoredAttachment;
pub use bodies::Body;
pub use contacts::{
    CachedContactEmail, ContactPageOutcome, ContactSourceState, ContactSyncStart, contacts_job,
};
pub use correspondents::{
    ExistingCachePage, HISTORY_SOURCE, HistoryProgress, HistoryState, LearnedAddress,
    LearningState, history_job,
};
pub(crate) use correspondents::record_observations_in;
pub(crate) use jobs::check_generation_in;
pub use jobs::{AccountLease, INITIAL_GENERATION, PendingAuthRecord};
pub use lifecycle::SwitchIntent;
pub use messages::PrefetchCandidate;
pub use remote_ids::{IMAP_SMTP_BACKEND_KEY, ImapCoords, MessageLocation};

/// Test-only hooks for migration fault injection. Never called in production;
/// exported so integration tests can exercise rollback paths.
#[doc(hidden)]
pub use migrations::{clear_migration_fault, inject_migration_fault};

/// Test-only hook that runs inside a change-page transaction to force a
/// cross-process snapshot conflict. Never set in production.
#[doc(hidden)]
pub use memberships::{clear_page_contention_hook, inject_page_contention_hook};

/// A consistent read of the cache used to build one UI snapshot.
#[derive(Debug, Default)]
pub struct StoreView {
    pub sections: Vec<AccountFolders>,
    pub conversations: Vec<Conversation>,
    /// Whether the folder has cached conversations beyond the requested page.
    pub has_more_local: bool,
    /// Folder total reported by the server (0 when unknown).
    pub selected_total: u32,
    /// Number of messages cached locally for the selected folder.
    pub selected_cached: u32,
}

/// One account plus its folders, read under the same transaction as the rest of
/// a [`StoreView`].
#[derive(Debug)]
pub struct AccountFolders {
    pub account: Account,
    pub folders: Vec<Folder>,
}

/// Errors surfaced by the store. Callers map these into user-visible strings at
/// the API boundary; the core never panics on bad persisted data.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid {field} value {value:?}")]
    Corrupt { field: &'static str, value: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew { found: u32, supported: u32 },
    /// A durable write carried an account generation older than the one the
    /// store now holds: the account was invalidated (removed or re-authenticated)
    /// while the write was in flight. The write must not be applied.
    #[error("account generation {expected} is stale (current generation is {actual})")]
    StaleGeneration { expected: u64, actual: u64 },
    /// A checkpoint was produced by a stale generation and must not advance the
    /// job it belongs to.
    #[error("job checkpoint from generation {generation} is stale")]
    StaleCheckpoint { generation: u64 },
    /// A change page lost the commit race and was discarded: either it was
    /// based on a job cursor the store has since moved past (a concurrent
    /// mutation or a second process advanced it) or the commit hit a SQLite
    /// lock/snapshot conflict from another writer. Applying it could regress
    /// newer state, so it is discarded; the caller reconciles from the durable
    /// checkpoint. A genuine failure (disk full, corruption, constraint
    /// violation) is never reported as `StalePage`.
    #[error("a change page based on an outdated checkpoint was discarded")]
    StalePage,
    /// A learning page carried an epoch older than the account's durable
    /// learning epoch: clear, rebuild, pause, or resume happened while it was in
    /// flight. The page must be discarded rather than resurrect forgotten
    /// suggestions.
    #[error("learning epoch {expected} is stale (current epoch is {actual})")]
    StaleLearningEpoch { expected: u64, actual: u64 },
    /// Learning is explicitly paused for the account, so no ingestion path may
    /// write evidence or advance a crawl checkpoint.
    #[error("recipient learning is paused")]
    LearningPaused,
    /// Clear disabled automatic `ExistingCache`/`History` ingestion until an
    /// explicit rebuild re-enables it.
    #[error("automatic recipient learning is disabled")]
    LearningDisabled,
    /// The user or a provider permission failure disabled this contact source.
    /// Its cached contacts are preserved, but no page may commit for it.
    #[error("contact source is disabled")]
    ContactSourceDisabled,
    /// Another process holds the account's cross-process lease.
    #[error("account is busy with another operation")]
    LeaseHeld,
    /// The caller's lease was taken over by another process before it wrote.
    #[error("account lease was lost before the write")]
    LeaseLost,
    /// A local message has no remote mapping, so its remote identity is unknown.
    #[error("message {0} has no remote mapping")]
    NoRemoteMessage(MessageId),
    /// A change referenced a provider collection that has not been cached yet.
    #[error("unknown remote collection {0}")]
    UnknownRemoteCollection(RemoteCollectionId),
    /// A change referenced a provider message that has not been cached yet.
    #[error("unknown remote message {0}")]
    UnknownRemoteMessage(RemoteMessageId),
    /// One outgoing attachment exceeds the per-part cap.
    #[error("attachment is too large: {size} bytes exceeds the {limit} byte limit")]
    AttachmentTooLarge { size: u64, limit: u64 },
    /// The sum of outgoing attachment bytes exceeds the per-submission cap.
    #[error(
        "attachment total is too large: {size} bytes exceeds the {limit} byte submission limit"
    )]
    AttachmentsTooLarge { size: u64, limit: u64 },
    /// A structured outgoing part has invalid identity or metadata.
    #[error("invalid submission attachment: {reason}")]
    InvalidAttachment { reason: String },
}

impl StoreError {
    pub(crate) fn corrupt(field: &'static str, value: impl Into<String>) -> Self {
        Self::Corrupt {
            field,
            value: value.into(),
        }
    }

    /// Whether this is a SQLite lock/snapshot conflict: another connection held
    /// the write lock (`SQLITE_BUSY`/`SQLITE_LOCKED`), or committed a write
    /// since this transaction read its checkpoint snapshot
    /// (`SQLITE_BUSY_SNAPSHOT`).
    ///
    /// This is the cross-process form of losing the checkpoint race, so a caller
    /// may discard the work and reconcile from the durable checkpoint rather
    /// than abort. Genuine failures (disk full, corruption, constraint
    /// violations, I/O errors) are deliberately *not* contention.
    pub fn is_contention(&self) -> bool {
        let StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = self else {
            return false;
        };
        matches!(
            code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    }
}

/// Handle to the local cache. The connection sits behind a mutex so a `Store`
/// is `Send + Sync`; every method keeps the guard for one short synchronous
/// operation and never across an await.
pub struct Store {
    conn: Mutex<Connection>,
    /// The database path. Kept so the migration can open a dedicated
    /// connection and, for the message-table rebuild only, run with foreign-key
    /// enforcement disabled before its transaction; ordinary store operations
    /// never touch the path or disable foreign keys.
    path: PathBuf,
    /// Process-local per-account gate. A synchronous mutex that serializes
    /// credential writes against invalidation/removal *within this process*;
    /// the durable half of the gate (generation + lease) lives in `jobs`.
    /// Entries are never removed, so a guard stays valid for the store's life.
    gates: Mutex<HashMap<AccountId, Arc<Mutex<()>>>>,
}

impl Store {
    /// Open (but do not migrate) the database at `path`, creating parent
    /// directories as needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            gates: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `f` while holding the account's process-local gate.
    ///
    /// This is the in-process half of the account gate: the credential-write
    /// path and `invalidate`/removal both take it, so a generation check and the
    /// credential write it fences cannot be split by a concurrent invalidation.
    /// `f` must be synchronous — never hold this across an await.
    pub fn with_account_gate<T>(&self, account: &AccountId, f: impl FnOnce() -> T) -> T {
        let gate = {
            let mut gates = self
                .gates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            gates
                .entry(account.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f()
    }

    /// Read everything a snapshot needs inside one deferred SQLite transaction,
    /// so a writer on another connection cannot interleave between sections,
    /// folders, and conversations.
    pub fn view(
        &self,
        folder: Option<&FolderId>,
        unified: bool,
        limit: u32,
    ) -> Result<StoreView, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;

        let accounts = accounts_on(&tx)?;
        let mut sections = Vec::with_capacity(accounts.len());
        for account in accounts {
            let folders = folders_on(&tx, &account.id)?;
            sections.push(AccountFolders { account, folders });
        }

        let mut conversations = Vec::new();
        let mut has_more_local = false;
        let mut selected_total = 0;
        let mut selected_cached = 0;
        if unified {
            let mut rows = unified_conversations_on(&tx, limit.saturating_add(1))?;
            has_more_local = rows.len() > limit as usize;
            rows.truncate(limit as usize);
            conversations = rows;
        } else if let Some(folder) = folder {
            let mut rows = conversations_on(&tx, folder, limit.saturating_add(1), None)?;
            has_more_local = rows.len() > limit as usize;
            rows.truncate(limit as usize);
            conversations = rows;
            selected_total = folder_total_on(&tx, folder)?.unwrap_or(0);
            selected_cached = message_count_on(&tx, folder)?;
        }

        tx.commit()?;
        Ok(StoreView {
            sections,
            conversations,
            has_more_local,
            selected_total,
            selected_cached,
        })
    }

    /// Read one `meta` value by key. Settings and schema bookkeeping live here;
    /// the SQLite connection itself is never exposed.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        let value = self
            .connection()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Insert or replace one `meta` value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Remove one `meta` value. Used when a setting moves to the configuration
    /// document, so a later downgrade cannot resurrect the stale value.
    pub fn delete_meta(&self, key: &str) -> Result<(), StoreError> {
        self.connection()
            .execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ffi;

    fn sqlite_error(result_code: i32) -> StoreError {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            ffi::Error::new(result_code),
            None,
        ))
    }

    #[test]
    fn contention_is_sqlite_busy_or_locked_only() {
        // Busy (including the WAL snapshot-conflict extended code) and locked are
        // the cross-process lost-race outcomes.
        assert!(sqlite_error(ffi::SQLITE_BUSY).is_contention());
        assert!(sqlite_error(ffi::SQLITE_BUSY_SNAPSHOT).is_contention());
        assert!(sqlite_error(ffi::SQLITE_BUSY_RECOVERY).is_contention());
        assert!(sqlite_error(ffi::SQLITE_LOCKED).is_contention());
        assert!(sqlite_error(ffi::SQLITE_LOCKED_SHAREDCACHE).is_contention());

        // Genuine failures must never be swallowed as a lost race.
        assert!(!sqlite_error(ffi::SQLITE_FULL).is_contention());
        assert!(!sqlite_error(ffi::SQLITE_CORRUPT).is_contention());
        assert!(!sqlite_error(ffi::SQLITE_CONSTRAINT).is_contention());
        assert!(!sqlite_error(ffi::SQLITE_IOERR).is_contention());
        assert!(!sqlite_error(ffi::SQLITE_NOTADB).is_contention());

        // Non-SQLite store errors are not contention either.
        assert!(!StoreError::StalePage.is_contention());
        assert!(
            !StoreError::StaleGeneration {
                expected: 1,
                actual: 2
            }
            .is_contention()
        );
    }
}
