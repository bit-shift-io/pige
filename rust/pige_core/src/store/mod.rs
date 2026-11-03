//! SQLite-backed cache. One database per installation holds every account,
//! folder, and cached message; platform code decides where it lives.

mod accounts;
mod folders;
mod messages;
mod migrations;
mod schema;

use std::fmt;
use std::path::Path;

use rusqlite::Connection;

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

/// Handle to the local cache. Methods take `&self`; transactions use SQLite's
/// unchecked transaction so the handle can be shared behind a mutex by the API
/// layer without exposing `&mut Connection`.
pub struct Store {
    conn: Connection,
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
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(Self { conn })
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}
