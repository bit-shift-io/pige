//! Schema versioning. Each migration step runs in one transaction and records
//! the version in `meta` so later tasks can add steps without guessing the
//! shape of an existing database.

mod v10;
mod v11;
mod v12;
mod v13;
mod v2;
mod v3;
mod v4;
mod v5;
mod v6;
mod v7;
mod v8;
mod v9;

use std::cell::Cell;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::SCHEMA_VERSION;

use super::schema::SCHEMA_SQL;
use super::{Store, StoreError};

thread_local! {
    /// Test-only fault injection point for the v7 message-table rebuild. `0`
    /// means no fault; any other value makes the migration fail at the matching
    /// step so a test can prove rollback. Thread-local so one test's injection
    /// can never affect a concurrently running migration in another test.
    static MIGRATION_FAULT: Cell<u8> = const { Cell::new(0) };
}

/// Inject a failure into the next migration on this thread (test-only).
#[doc(hidden)]
pub fn inject_migration_fault(point: u8) {
    MIGRATION_FAULT.with(|fault| fault.set(point));
}

/// Clear any injected migration failure on this thread (test-only).
#[doc(hidden)]
pub fn clear_migration_fault() {
    MIGRATION_FAULT.with(|fault| fault.set(0));
}

pub(crate) fn migration_fault() -> u8 {
    MIGRATION_FAULT.with(Cell::get)
}

impl Store {
    /// Create or upgrade the database to `SCHEMA_VERSION`. Idempotent: calling
    /// it twice is a no-op, and a database from a newer binary is rejected
    /// instead of being silently modified.
    ///
    /// Versions are applied stepwise. v1 is the original cache schema; v2 adds
    /// the FTS5 index and backfills every message already cached; v3 records
    /// attachment disposition for the reader's attachment chips; v4 repairs
    /// list previews cached before the transfer-encoding decoder landed; v5
    /// versions the account mirror by typed backend; v6 adds durable account
    /// generations, cross-process leases, pending-auth references, and
    /// generation-fenced checkpoints; v7 separates remote message identity from
    /// collection membership; v8 records the provider signature status and the
    /// remote attachment id on the cached message and attachment; v9 generalizes
    /// the outbox into a provider-neutral durable submission with structured
    /// parts, operation identity, prepared artifacts, and receipts; v10 adds the
    /// recoverable backend-switch intent; v11 adds source contacts and the
    /// replay-safe correspondent evidence ledger; v12 adds the indexed recipient
    /// search projection and contact-source kind; v13 adds the rank index that
    /// bounds short-prefix lookups to an indexed top-N.
    pub fn migrate(&self) -> Result<(), StoreError> {
        self.migrate_to(SCHEMA_VERSION)
    }

    /// Upgrade the database to `target` (clamped to [`SCHEMA_VERSION`]).
    ///
    /// This is the real migration pipeline; integration tests use it with an
    /// older `target` to build fixtures from the *actual* historical schema
    /// rather than a hand-rolled approximation.
    ///
    /// The whole upgrade is one transaction. When it reaches v7 it runs on a
    /// dedicated connection with foreign-key enforcement disabled *before* the
    /// transaction, because dropping and recreating `messages` must not let the
    /// child tables' `ON DELETE CASCADE` delete bodies and attachments. That
    /// connection is discarded afterwards; ordinary store operations always run
    /// with foreign keys enabled.
    #[doc(hidden)]
    pub fn migrate_to(&self, target: u32) -> Result<(), StoreError> {
        let target = target.min(SCHEMA_VERSION);
        let mut version = self.existing_schema_version()?.unwrap_or(0);
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version >= target {
            return Ok(());
        }

        // A dedicated migration connection so v7 can toggle `foreign_keys`
        // before its transaction; it never touches the live store connection.
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let disable_foreign_keys = version < 7 && target >= 7;
        if disable_foreign_keys {
            // Only the v7 rebuild needs this, and only on this dedicated
            // connection before its transaction. Ordinary store operations —
            // and every other migration — keep foreign keys enforced.
            conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        } else {
            conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        }

        let tx = conn.unchecked_transaction()?;

        if version < 1 && target >= 1 {
            tx.execute_batch(SCHEMA_SQL)?;
            version = 1;
        }
        if version < 2 && target >= 2 {
            v2::apply(&tx)?;
            version = 2;
        }
        if version < 3 && target >= 3 {
            v3::apply(&tx)?;
            version = 3;
        }
        if version < 4 && target >= 4 {
            v4::apply(&tx)?;
            version = 4;
        }
        if version < 5 && target >= 5 {
            v5::apply(&tx)?;
            version = 5;
        }
        if version < 6 && target >= 6 {
            v6::apply(&tx)?;
            version = 6;
        }
        if version < 7 && target >= 7 {
            v7::apply(&tx)?;
            version = 7;
        }
        if version < 8 && target >= 8 {
            v8::apply(&tx)?;
            version = 8;
        }
        if version < 9 && target >= 9 {
            v9::apply(&tx)?;
            version = 9;
        }
        if version < 10 && target >= 10 {
            v10::apply(&tx)?;
            version = 10;
        }
        if version < 11 && target >= 11 {
            v11::apply(&tx)?;
            version = 11;
        }
        if version < 12 && target >= 12 {
            v12::apply(&tx)?;
            version = 12;
        }
        if version < 13 && target >= 13 {
            v13::apply(&tx)?;
            version = 13;
        }

        // The rebuild must not leave a dangling reference behind; check before
        // committing so a violation rolls the whole migration back.
        if disable_foreign_keys {
            let violations: i64 =
                tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if violations > 0 {
                return Err(StoreError::corrupt(
                    "migration.foreign_key_check",
                    violations.to_string(),
                ));
            }
        }

        debug_assert_eq!(version, target);
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [version.to_string()],
        )?;
        tx.commit()?;

        if disable_foreign_keys {
            conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        }

        log::debug!("store migrated to schema version {version}");
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
