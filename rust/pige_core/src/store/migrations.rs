//! Schema versioning. Each migration step runs in one transaction and records
//! the version in `meta` so later tasks can add steps without guessing the
//! shape of an existing database.

mod v2;
mod v3;
mod v4;

use rusqlite::OptionalExtension;

use crate::SCHEMA_VERSION;

use super::schema::SCHEMA_SQL;
use super::{Store, StoreError};

impl Store {
    /// Create or upgrade the database to `SCHEMA_VERSION`. Idempotent: calling
    /// it twice is a no-op, and a database from a newer binary is rejected
    /// instead of being silently modified.
    ///
    /// Versions are applied stepwise. v1 is the original cache schema; v2 adds
    /// the FTS5 index and backfills every message already cached; v3 records
    /// attachment disposition for the reader's attachment chips; v4 repairs
    /// list previews cached before the transfer-encoding decoder landed.
    pub fn migrate(&self) -> Result<(), StoreError> {
        let mut version = self.existing_schema_version()?.unwrap_or(0);
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;

        if version < 1 {
            tx.execute_batch(SCHEMA_SQL)?;
            version = 1;
        }
        if version < 2 {
            v2::apply(&tx)?;
            version = 2;
        }
        if version < 3 {
            v3::apply(&tx)?;
            version = 3;
        }
        if version < 4 {
            v4::apply(&tx)?;
            version = 4;
        }

        debug_assert_eq!(version, SCHEMA_VERSION);
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        tx.commit()?;

        log::debug!("store migrated to schema version {SCHEMA_VERSION}");
        Ok(())
    }

    /// Read the recorded schema version. Only valid after [`Store::migrate`].
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        let raw: String = self.connection().query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        raw.parse()
            .map_err(|_| StoreError::corrupt("meta.schema_version", raw))
    }

    fn existing_schema_version(&self) -> Result<Option<u32>, StoreError> {
        let has_meta: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |row| row.get(0),
        )?;
        if has_meta == 0 {
            return Ok(None);
        }

        let raw: Option<String> = self
            .connection()
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            Some(raw) => raw
                .parse()
                .map(Some)
                .map_err(|_| StoreError::corrupt("meta.schema_version", raw)),
            None => Ok(None),
        }
    }
}
