//! SQLite-backed cache. One database per installation holds every account,
//! folder, and cached message; platform code decides where it lives.

mod accounts;
mod bodies;
mod folders;
mod messages;
mod migrations;
mod schema;
mod search;

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use crate::account::Account;
use crate::mail::{Conversation, Folder, FolderId};

use accounts::accounts_on;
use folders::{folder_total_on, folders_on};
use messages::{conversations_on, message_count_on, unified_conversations_on};

pub use bodies::Body;

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
}

impl StoreError {
    pub(crate) fn corrupt(field: &'static str, value: impl Into<String>) -> Self {
        Self::Corrupt {
            field,
            value: value.into(),
        }
    }
}

/// Handle to the local cache. The connection sits behind a mutex so a `Store`
/// is `Send + Sync`; every method keeps the guard for one short synchronous
/// operation and never across an await.
pub struct Store {
    conn: Mutex<Connection>,
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
        })
    }

    pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}
