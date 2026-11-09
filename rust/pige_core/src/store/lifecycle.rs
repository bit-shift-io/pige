//! Account lifecycle storage: recoverable backend-switch intents, provider
//! cache clearing, submission blockers, and generation-safe removal cleanup.
//!
//! Nothing in this module stores a secret. A [`SwitchIntent`] holds the old and
//! target account shapes (non-secret configuration) plus *references* into the
//! platform credential store. The credentials themselves never reach SQLite.

use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};

use crate::account::{Account, AccountId};
use crate::providers::JobKey;
use crate::send::SubmissionState;

use super::{Store, StoreError};

/// A durable, non-secret record of an in-progress backend switch.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchIntent {
    pub account: AccountId,
    /// The account shape being activated.
    pub target: Account,
    /// The account shape to restore if the switch rolls back.
    pub previous: Account,
    /// Secure-store reference of the staged authentication candidate.
    pub credential_ref: String,
    /// Secure-store reference holding a copy of the previous active credential,
    /// when one existed. Never the secret itself.
    pub backup_ref: Option<String>,
    /// Whether the new credential is session-only.
    pub session_only: bool,
    /// Whether the previous account had an active credential at all.
    pub previous_present: bool,
    /// Whether the previous active credential was persisted in the OS store.
    pub previous_persistent: bool,
    /// Durable, *confirmed* progress. Each value is written only after the step
    /// it names returned success, so recovery never treats an attempted write as
    /// a completed one. See the phase constants in `api::accounts`:
    /// `0` intent written, `1` rollback backup confirmed, `2` new active
    /// credential write confirmed, `3` target configuration save confirmed,
    /// `4` provider cache cleared/completed.
    pub phase: u8,
    pub created_at_ms: i64,
}

/// Parse one `job_checkpoints.job_key` value. A corrupt key is ignored for the
/// scoped deletions rather than aborting an entire account cleanup.
fn parse_job_key(raw: &str) -> Option<JobKey> {
    serde_json::from_str(raw).ok()
}

impl Store {
    /// Record (or replace) one account's switch intent. The caller must already
    /// have refused to overwrite an unfinished intent through
    /// [`Self::switch_intent`].
    pub fn insert_switch_intent(&self, intent: &SwitchIntent) -> Result<(), StoreError> {
        let target = serde_json::to_string(&intent.target)?;
        let previous = serde_json::to_string(&intent.previous)?;
        self.connection().execute(
            "INSERT INTO switch_intents (
                 account_id, target_account, previous_account, credential_ref, backup_ref,
                 persistence, previous_present, previous_persistent, phase, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(account_id) DO UPDATE SET
                 target_account = excluded.target_account,
                 previous_account = excluded.previous_account,
                 credential_ref = excluded.credential_ref,
                 backup_ref = excluded.backup_ref,
                 persistence = excluded.persistence,
                 previous_present = excluded.previous_present,
                 previous_persistent = excluded.previous_persistent,
                 phase = excluded.phase,
                 created_at = excluded.created_at",
            params![
                intent.account.0,
                target,
                previous,
                intent.credential_ref,
                intent.backup_ref,
                if intent.session_only {
                    "session"
                } else {
                    "persistent"
                },
                i64::from(intent.previous_present),
                i64::from(intent.previous_persistent),
                i64::from(intent.phase),
                intent.created_at_ms,
            ],
        )?;
        Ok(())
    }

    fn switch_intent_from_row(row: &rusqlite::Row<'_>) -> Result<SwitchIntent, StoreError> {
        let account: String = row.get(0)?;
        let target: String = row.get(1)?;
        let previous: String = row.get(2)?;
        let credential_ref: String = row.get(3)?;
        let backup_ref: Option<String> = row.get(4)?;
        let persistence: String = row.get(5)?;
        let previous_present: bool = row.get::<_, i64>(6)? != 0;
        let previous_persistent: bool = row.get::<_, i64>(7)? != 0;
        let phase: u8 = row.get::<_, i64>(8)? as u8;
        let created_at_ms: i64 = row.get(9)?;
        Ok(SwitchIntent {
            account: AccountId(account),
            target: serde_json::from_str(&target)?,
            previous: serde_json::from_str(&previous)?,
            credential_ref,
            backup_ref,
            session_only: persistence == "session",
            previous_present,
            previous_persistent,
            phase,
            created_at_ms,
        })
    }

    /// Read one account's unfinished switch intent, if any.
    pub fn switch_intent(&self, account: &AccountId) -> Result<Option<SwitchIntent>, StoreError> {
        self.connection()
            .query_row(
                "SELECT account_id, target_account, previous_account, credential_ref, backup_ref,
                        persistence, previous_present, previous_persistent, phase, created_at
                 FROM switch_intents WHERE account_id = ?1",
                [&account.0],
                |row| {
                    Self::switch_intent_from_row(row).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Every unfinished switch intent, used once at startup to complete or roll
    /// back interrupted switches.
    pub fn switch_intents(&self) -> Result<Vec<SwitchIntent>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT account_id, target_account, previous_account, credential_ref, backup_ref,
                    persistence, previous_present, previous_persistent, phase, created_at
             FROM switch_intents ORDER BY created_at",
        )?;
        let mut rows = statement.query([])?;
        let mut intents = Vec::new();
        while let Some(row) = rows.next()? {
            intents.push(Self::switch_intent_from_row(row)?);
        }
        Ok(intents)
    }

    /// Remove one account's switch intent. Idempotent.
    pub fn remove_switch_intent(&self, account: &AccountId) -> Result<(), StoreError> {
        self.connection().execute(
            "DELETE FROM switch_intents WHERE account_id = ?1",
            [&account.0],
        )?;
        Ok(())
    }

    /// Durably advance an intent's phase. A missing intent is a no-op.
    pub fn set_switch_intent_phase(
        &self,
        account: &AccountId,
        phase: u8,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE switch_intents SET phase = ?1 WHERE account_id = ?2",
            params![i64::from(phase), account.0],
        )?;
        Ok(())
    }

    /// The outbox states that make a backend switch unsafe: anything whose
    /// delivery or Sent-copy outcome is not final. Raw pre-provider states are
    /// decoded through [`SubmissionState`], so a legacy `failed`/`sending` row is
    /// conservatively treated as ambiguous and blocks the switch.
    pub fn outbox_switch_blockers(&self, account: &AccountId) -> Result<Vec<String>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare("SELECT state FROM outbox WHERE account_id = ?1")?;
        let mut rows = statement.query([&account.0])?;
        let mut blockers = Vec::new();
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let blocked = match SubmissionState::from_db(&raw) {
                Some(state) => state.is_pending() || state == SubmissionState::Uncertain,
                // An unknown state is not proof that delivery is final.
                None => true,
            };
            if blocked {
                blockers.push(raw);
            }
        }
        Ok(blockers)
    }

    /// Clear every provider-derived cache row for `account`: remote identity
    /// mappings, collection folders, messages (with bodies, attachments, FTS,
    /// and memberships), threads, and job checkpoints. Drafts, outbox rows,
    /// learned data in other tables, the account row, its generation/lease
    /// tombstones, and any switch intent are deliberately preserved. Keeping the
    /// intent is what lets a crash between this clear and the reference cleanup
    /// be retried at the next start instead of orphaning a rollback credential.
    ///
    /// Returns cache files that are no longer referenced so the caller can
    /// remove them best-effort.
    pub fn clear_provider_cache(&self, account: &AccountId) -> Result<Vec<PathBuf>, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;

        let mut files = Vec::new();
        {
            let mut statement = tx.prepare(
                "SELECT a.cached_path FROM attachments a
                 JOIN messages m ON m.id = a.message_id
                 WHERE m.account_id = ?1 AND a.cached_path IS NOT NULL",
            )?;
            let mut rows = statement.query([&account.0])?;
            while let Some(row) = rows.next()? {
                let path: String = row.get(0)?;
                files.push(PathBuf::from(path));
            }
        }
        {
            let mut statement = tx.prepare(
                "SELECT raw_path FROM messages
                 WHERE account_id = ?1 AND raw_path IS NOT NULL",
            )?;
            let mut rows = statement.query([&account.0])?;
            while let Some(row) = rows.next()? {
                let path: String = row.get(0)?;
                files.push(PathBuf::from(path));
            }
        }

        // Delete mappings and cache content first. Deleting the folders would
        // cascade memberships, but the explicit order keeps this readable and
        // works even if a mapping outlived its folder.
        tx.execute(
            "DELETE FROM remote_messages WHERE account_id = ?1",
            [&account.0],
        )?;
        tx.execute(
            "DELETE FROM remote_collections WHERE account_id = ?1",
            [&account.0],
        )?;
        tx.execute("DELETE FROM folders WHERE account_id = ?1", [&account.0])?;
        tx.execute("DELETE FROM messages WHERE account_id = ?1", [&account.0])?;
        // A draft's thread link has no foreign key, so preserve any thread row a
        // surviving outbox entry still points at rather than leaving it dangling.
        tx.execute(
            "DELETE FROM threads
             WHERE account_id = ?1
               AND id NOT IN (
                   SELECT thread_id FROM outbox
                   WHERE account_id = ?1 AND thread_id IS NOT NULL
               )",
            [&account.0],
        )?;

        // `job_checkpoints` keys the account inside JSON, so filter in Rust
        // rather than relying on SQLite's optional JSON1 extension.
        let mut checkpoint_keys = Vec::new();
        {
            let mut statement = tx.prepare("SELECT job_key FROM job_checkpoints")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let raw: String = row.get(0)?;
                if parse_job_key(&raw).is_some_and(|key| key.account == *account) {
                    checkpoint_keys.push(raw);
                }
            }
        }
        for key in checkpoint_keys {
            tx.execute("DELETE FROM job_checkpoints WHERE job_key = ?1", [key])?;
        }

        tx.commit()?;
        Ok(files)
    }

    /// Clear the cached contacts, snapshot state, and checkpoint of one
    /// replaced contact source. Mail, learned evidence, and other sources are
    /// untouched. A replacement source must not resume a cursor that belonged
    /// to its predecessor.
    pub fn clear_contact_source_cache(
        &self,
        account: &AccountId,
        source: &str,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        super::contacts::clear_contact_source_data_in(
            &tx,
            account,
            &crate::providers::ContactSourceId::new(source),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Remove the durable job state owned by one removed/dropped account. The
    /// generation tombstone and lease fencing token are intentionally retained
    /// so an in-flight older credential write can never win after removal.
    pub fn clear_account_jobs(&self, account: &AccountId) -> Result<(), StoreError> {
        let conn = self.connection();
        let mut keys = Vec::new();
        {
            let mut statement = conn.prepare("SELECT job_key FROM job_checkpoints")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let raw: String = row.get(0)?;
                if parse_job_key(&raw).is_some_and(|key| key.account == *account) {
                    keys.push(raw);
                }
            }
        }
        for key in keys {
            conn.execute("DELETE FROM job_checkpoints WHERE job_key = ?1", [key])?;
        }
        conn.execute(
            "DELETE FROM pending_auth WHERE account_id = ?1",
            [&account.0],
        )?;
        conn.execute(
            "DELETE FROM switch_intents WHERE account_id = ?1",
            [&account.0],
        )?;
        Ok(())
    }

    /// Remove every durable job/lease record for a factory reset. Generations
    /// are bumps on `account_generation` and are not reused; no `accounts` row
    /// survives a reset.
    pub fn clear_all_jobs(&self) -> Result<(), StoreError> {
        let conn = self.connection();
        conn.execute("DELETE FROM job_checkpoints", [])?;
        conn.execute("DELETE FROM pending_auth", [])?;
        conn.execute("DELETE FROM switch_intents", [])?;
        conn.execute("DELETE FROM account_lease", [])?;
        Ok(())
    }

    /// Whether an account has an unfinished switch intent.
    pub fn has_switch_intent(&self, account: &AccountId) -> Result<bool, StoreError> {
        Ok(self.switch_intent(account)?.is_some())
    }
}
