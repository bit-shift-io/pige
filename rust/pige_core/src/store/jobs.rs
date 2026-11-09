//! Durable generation, lease, pending-auth, and checkpoint records.
//!
//! These are the *fencing* half of the account gate: the in-process
//! cancellation token lives in `crate::auth::AccountSessionManager`, while the
//! durable generation and cross-process lease live here so two processes (or a
//! restart) cannot resurrect an account that was removed while a refresh was in
//! flight.
//!
//! Nothing in this module ever stores a secret. A pending authentication
//! attempt records only the *reference* to its protected credential entry.

use std::time::Duration;

use rusqlite::{OptionalExtension, params};

use crate::account::AccountId;
use crate::providers::{AuthAttemptId, JobKey, ProviderCursor, ProviderId};

use super::{Store, StoreError};

/// A brand-new account starts at generation 1. Invalidating it moves to 2, and
/// so on; the value is never reused.
pub const INITIAL_GENERATION: u64 = 1;

/// A held cross-process account lease. `fencing` increases monotonically every
/// time the lease changes hands, so a writer that lost the lease is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLease {
    pub account: AccountId,
    pub holder: String,
    pub fencing: u64,
    pub expires_at_ms: i64,
}

impl AccountLease {
    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// A pending interactive authentication attempt. It references the protected
/// pending credential entry by key (`credential_ref`); it never holds the
/// secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAuthRecord {
    pub attempt: AuthAttemptId,
    /// `None` only if a caller stages an attempt with no account at all.
    pub account: Option<AccountId>,
    pub provider: ProviderId,
    /// Secure-store key of the staged secret, e.g. `pending_<uuid>`.
    pub credential_ref: String,
    pub generation: u64,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

/// A resumable job cursor stamped with the generation that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalCheckpoint {
    pub generation: u64,
    pub cursor: ProviderCursor,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn ttl_ms(ttl: Duration) -> i64 {
    ttl.as_millis().min(i64::MAX as u128) as i64
}

/// The compare-and-set generation check executed *inside* a caller's SQLite
/// transaction, so it cannot be split from the write it fences.
pub(crate) fn check_generation_in(
    tx: &rusqlite::Connection,
    account: &AccountId,
    expected: u64,
) -> Result<(), StoreError> {
    let actual: Option<i64> = tx
        .query_row(
            "SELECT generation FROM account_generation WHERE account_id = ?1",
            [&account.0],
            |row| row.get(0),
        )
        .optional()?;
    let actual = actual.map_or(INITIAL_GENERATION, |value| value as u64);
    if actual != expected {
        return Err(StoreError::StaleGeneration { expected, actual });
    }
    Ok(())
}

/// Store a job cursor inside a caller-owned transaction, re-checking the
/// generation and refusing to regress an existing checkpoint. Factored out so
/// [`Store::apply_change_page`](crate::store::Store::apply_change_page) can
/// commit the page's changes and its checkpoint atomically.
pub(crate) fn set_checkpoint_in(
    conn: &rusqlite::Connection,
    key: &JobKey,
    generation: u64,
    cursor: &ProviderCursor,
) -> Result<(), StoreError> {
    let job_key = serde_json::to_string(key)?;
    let cursor_json = serde_json::to_string(cursor)?;
    check_generation_in(conn, &key.account, generation)?;
    let existing: Option<i64> = conn
        .query_row(
            "SELECT generation FROM job_checkpoints WHERE job_key = ?1",
            [&job_key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing
        && existing as u64 > generation
    {
        return Err(StoreError::StaleCheckpoint { generation });
    }
    conn.execute(
        "INSERT INTO job_checkpoints (job_key, generation, cursor) VALUES (?1, ?2, ?3)
         ON CONFLICT(job_key) DO UPDATE SET
             generation = excluded.generation,
             cursor = excluded.cursor",
        params![job_key, generation as i64, cursor_json],
    )?;
    Ok(())
}

/// Read one job cursor inside a caller-owned transaction.
pub(crate) fn checkpoint_cursor_in(
    conn: &rusqlite::Connection,
    key: &JobKey,
) -> Result<Option<ProviderCursor>, StoreError> {
    let job_key = serde_json::to_string(key)?;
    let stored: Option<String> = conn
        .query_row(
            "SELECT cursor FROM job_checkpoints WHERE job_key = ?1",
            [&job_key],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(StoreError::from)
}

/// Delete one job's checkpoint inside a caller-owned transaction. Used when a
/// full contact snapshot completes: the next slice must start a fresh
/// generation instead of paging past the end of the old one.
pub(crate) fn delete_checkpoint_in(
    conn: &rusqlite::Connection,
    key: &JobKey,
) -> Result<(), StoreError> {
    let job_key = serde_json::to_string(key)?;
    conn.execute("DELETE FROM job_checkpoints WHERE job_key = ?1", [job_key])?;
    Ok(())
}

/// Compare-and-swap guard executed *inside* a page's SQLite transaction: the
/// store's cursor for `key` must still equal `expected` (including both being
/// absent) or the page was produced against a checkpoint another writer has
/// since moved past. Such a page must be discarded, never applied, so a late or
/// out-of-order page cannot regress state.
pub(crate) fn check_checkpoint_in(
    conn: &rusqlite::Connection,
    key: &JobKey,
    expected: Option<&ProviderCursor>,
) -> Result<(), StoreError> {
    let job_key = serde_json::to_string(key)?;
    let stored: Option<String> = conn
        .query_row(
            "SELECT cursor FROM job_checkpoints WHERE job_key = ?1",
            [&job_key],
            |row| row.get(0),
        )
        .optional()?;
    let stored: Option<ProviderCursor> =
        stored.map(|json| serde_json::from_str(&json)).transpose()?;
    if stored.as_ref() == expected {
        Ok(())
    } else {
        Err(StoreError::StalePage)
    }
}

impl Store {
    /// The current durable generation for `account`. A never-seen account is
    /// [`INITIAL_GENERATION`].
    pub fn generation(&self, account: &AccountId) -> Result<u64, StoreError> {
        self.connection()
            .query_row(
                "SELECT generation FROM account_generation WHERE account_id = ?1",
                [&account.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.map(|value| value as u64).unwrap_or(INITIAL_GENERATION))
            .map_err(StoreError::from)
    }

    /// Allocate the next free generation for `account` and return it. A removed
    /// account keeps its row, so the number is never reused.
    pub fn bump_generation(&self, account: &AccountId) -> Result<u64, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT generation FROM account_generation WHERE account_id = ?1",
                [&account.0],
                |row| row.get(0),
            )
            .optional()?;
        // A never-seen account is already at INITIAL_GENERATION, so invalidating
        // it must move to the next number, not re-assert the initial one.
        let next = current.map_or(INITIAL_GENERATION + 1, |value| value as u64 + 1);
        tx.execute(
            "INSERT INTO account_generation (account_id, generation) VALUES (?1, ?2)
             ON CONFLICT(account_id) DO UPDATE SET generation = excluded.generation",
            params![account.0, next as i64],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Fail with [`StoreError::StaleGeneration`] unless `expected` is still the
    /// durable generation. This is the compare-and-set every durable write runs
    /// inside its transaction before touching account-scoped state.
    pub fn check_generation(&self, account: &AccountId, expected: u64) -> Result<(), StoreError> {
        let actual = self.generation(account)?;
        if actual != expected {
            return Err(StoreError::StaleGeneration { expected, actual });
        }
        Ok(())
    }

    /// Take the account's lease for `holder`, or fail with
    /// [`StoreError::LeaseHeld`] if another live holder owns it. Re-taking your
    /// own lease advances the fencing token.
    pub fn acquire_lease(
        &self,
        account: &AccountId,
        holder: &str,
        ttl: Duration,
    ) -> Result<AccountLease, StoreError> {
        let now = now_ms();
        let expires_at = now.saturating_add(ttl_ms(ttl));
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let existing: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT holder, fencing, expires_at FROM account_lease WHERE account_id = ?1",
                [&account.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let fencing = match existing {
            Some((owner, _fencing, held_until)) if held_until > now && owner != holder => {
                return Err(StoreError::LeaseHeld);
            }
            // Re-acquiring your *own* live lease keeps the fencing token, so a
            // caller's existing handle is not silently invalidated.
            Some((owner, fencing, held_until)) if held_until > now && owner == holder => {
                fencing as u64
            }
            Some((_, fencing, _)) => fencing as u64 + 1,
            None => 1,
        };
        tx.execute(
            "INSERT INTO account_lease (account_id, holder, fencing, expires_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id) DO UPDATE SET
                 holder = excluded.holder,
                 fencing = excluded.fencing,
                 expires_at = excluded.expires_at",
            params![account.0, holder, fencing as i64, expires_at],
        )?;
        tx.commit()?;
        Ok(AccountLease {
            account: account.clone(),
            holder: holder.to_string(),
            fencing,
            expires_at_ms: expires_at,
        })
    }

    /// Extend a held, unexpired lease. Fails with [`StoreError::LeaseLost`] if
    /// another holder took the lease, the fencing token moved, or the lease has
    /// already expired (an expired lease can only be re-acquired, which advances
    /// the fencing token).
    pub fn renew_lease(
        &self,
        lease: &AccountLease,
        ttl: Duration,
    ) -> Result<AccountLease, StoreError> {
        let now = now_ms();
        let expires_at = now.saturating_add(ttl_ms(ttl));
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE account_lease SET expires_at = ?1
             WHERE account_id = ?2 AND holder = ?3 AND fencing = ?4 AND expires_at > ?5",
            params![
                expires_at,
                lease.account.0,
                lease.holder,
                lease.fencing as i64,
                now
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::LeaseLost);
        }
        tx.commit()?;
        let mut renewed = lease.clone();
        renewed.expires_at_ms = expires_at;
        Ok(renewed)
    }

    /// Release the lease if this holder still owns it. The row is kept (with an
    /// empty holder and a past expiry) so the fencing token stays monotonic
    /// across hand-offs. Releasing a lease that was already taken over is
    /// harmless.
    pub fn release_lease(&self, lease: &AccountLease) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE account_lease SET holder = '', expires_at = 0
             WHERE account_id = ?1 AND holder = ?2 AND fencing = ?3",
            params![lease.account.0, lease.holder, lease.fencing as i64],
        )?;
        Ok(())
    }

    /// Verify `lease` is still the live holder's lease before a durable write.
    /// Fails with [`StoreError::LeaseLost`] otherwise.
    pub fn ensure_lease(&self, lease: &AccountLease) -> Result<(), StoreError> {
        let now = now_ms();
        let row: Option<(String, i64, i64)> = self
            .connection()
            .query_row(
                "SELECT holder, fencing, expires_at FROM account_lease WHERE account_id = ?1",
                [&lease.account.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match row {
            Some((holder, fencing, expires_at))
                if holder == lease.holder
                    && fencing as u64 == lease.fencing
                    && expires_at > now =>
            {
                Ok(())
            }
            _ => Err(StoreError::LeaseLost),
        }
    }

    /// Acquire the account lease and verify `expected_generation` *in one
    /// transaction*. This is the durable fence on the credential-write path: a
    /// credential may only be written while holding this lease at the right
    /// generation, so a removal/revocation that ran first is observed.
    ///
    /// Fails with [`StoreError::LeaseHeld`] if another live holder owns the
    /// account or [`StoreError::StaleGeneration`] if it was invalidated.
    pub fn fence_credential_write(
        &self,
        account: &AccountId,
        holder: &str,
        expected_generation: u64,
        ttl: Duration,
    ) -> Result<AccountLease, StoreError> {
        let now = now_ms();
        let expires_at = now.saturating_add(ttl_ms(ttl));
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, account, expected_generation)?;
        let existing: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT holder, fencing, expires_at FROM account_lease WHERE account_id = ?1",
                [&account.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let fencing = match existing {
            Some((owner, _, held_until)) if held_until > now && owner != holder => {
                return Err(StoreError::LeaseHeld);
            }
            Some((owner, fencing, held_until)) if held_until > now && owner == holder => {
                fencing as u64
            }
            Some((_, fencing, _)) => fencing as u64 + 1,
            None => 1,
        };
        tx.execute(
            "INSERT INTO account_lease (account_id, holder, fencing, expires_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id) DO UPDATE SET
                 holder = excluded.holder,
                 fencing = excluded.fencing,
                 expires_at = excluded.expires_at",
            params![account.0, holder, fencing as i64, expires_at],
        )?;
        tx.commit()?;
        Ok(AccountLease {
            account: account.clone(),
            holder: holder.to_string(),
            fencing,
            expires_at_ms: expires_at,
        })
    }

    /// Atomically advance the account generation and fence out every
    /// outstanding lease in one transaction. Removal/revocation uses this so a
    /// credential write started earlier can never complete: its lease is fenced
    /// (any later `ensure_lease` fails) and its generation is stale.
    pub fn invalidate_account(&self, account: &AccountId) -> Result<u64, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT generation FROM account_generation WHERE account_id = ?1",
                [&account.0],
                |row| row.get(0),
            )
            .optional()?;
        let next = current.map_or(INITIAL_GENERATION + 1, |value| value as u64 + 1);
        tx.execute(
            "INSERT INTO account_generation (account_id, generation) VALUES (?1, ?2)
             ON CONFLICT(account_id) DO UPDATE SET generation = excluded.generation",
            params![account.0, next as i64],
        )?;
        // Keep the fencing token monotonic and drop any live holder so an older
        // writer's `ensure_lease` fails before it writes.
        tx.execute(
            "INSERT INTO account_lease (account_id, holder, fencing, expires_at)
             VALUES (?1, '', 1, 0)
             ON CONFLICT(account_id) DO UPDATE SET
                 holder = '', fencing = account_lease.fencing + 1, expires_at = 0",
            params![account.0],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Record a pending authentication attempt. Only non-secret references are
    /// stored.
    pub fn insert_pending_auth(&self, record: &PendingAuthRecord) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO pending_auth
                 (attempt_id, account_id, provider, credential_ref, generation, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(attempt_id) DO UPDATE SET
                 account_id = excluded.account_id,
                 provider = excluded.provider,
                 credential_ref = excluded.credential_ref,
                 generation = excluded.generation,
                 expires_at = excluded.expires_at",
            params![
                record.attempt.as_str(),
                record.account.as_ref().map(|account| account.0.clone()),
                record.provider.as_str(),
                record.credential_ref,
                record.generation as i64,
                record.created_at_ms,
                record.expires_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Read one pending attempt by id.
    pub fn pending_auth(
        &self,
        attempt: &AuthAttemptId,
    ) -> Result<Option<PendingAuthRecord>, StoreError> {
        self.connection()
            .query_row(
                "SELECT account_id, provider, credential_ref, generation, created_at, expires_at
                 FROM pending_auth WHERE attempt_id = ?1",
                [attempt.as_str()],
                |row| {
                    let account: Option<String> = row.get(0)?;
                    let provider: String = row.get(1)?;
                    Ok(PendingAuthRecord {
                        attempt: attempt.clone(),
                        account: account.map(AccountId),
                        provider: ProviderId::new(provider),
                        credential_ref: row.get(2)?,
                        generation: row.get::<_, i64>(3)? as u64,
                        created_at_ms: row.get(4)?,
                        expires_at_ms: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Remove a pending attempt. Idempotent.
    pub fn remove_pending_auth(&self, attempt: &AuthAttemptId) -> Result<(), StoreError> {
        self.connection()
            .execute("DELETE FROM pending_auth WHERE attempt_id = ?1", [attempt.as_str()])?;
        Ok(())
    }

    /// Every pending attempt whose expiry is at or before `at_ms`, oldest first.
    pub fn expired_pending_auth(&self, at_ms: i64) -> Result<Vec<PendingAuthRecord>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT attempt_id, account_id, provider, credential_ref, generation, created_at, expires_at
             FROM pending_auth WHERE expires_at <= ?1 ORDER BY expires_at",
        )?;
        let rows = statement.query_map([at_ms], |row| {
            let account: Option<String> = row.get(1)?;
            let provider: String = row.get(2)?;
            Ok(PendingAuthRecord {
                attempt: AuthAttemptId::new(row.get::<_, String>(0)?),
                account: account.map(AccountId),
                provider: ProviderId::new(provider),
                credential_ref: row.get(3)?,
                generation: row.get::<_, i64>(4)? as u64,
                created_at_ms: row.get(5)?,
                expires_at_ms: row.get(6)?,
            })
        })?;
        let mut records = Vec::new();
        for record in rows {
            records.push(record?);
        }
        Ok(records)
    }

    /// Store a job's resumable cursor, rejecting a checkpoint produced by a
    /// stale account generation or an older checkpoint.
    pub fn set_checkpoint(
        &self,
        key: &JobKey,
        generation: u64,
        cursor: &ProviderCursor,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        set_checkpoint_in(&tx, key, generation, cursor)?;
        tx.commit()?;
        Ok(())
    }

    /// Read one job's resumable cursor, if any. This is the value a caller
    /// passes back as the expected-previous checkpoint of the next committed
    /// page, and the cursor it sends to the provider.
    pub fn job_checkpoint(&self, key: &JobKey) -> Result<Option<ProviderCursor>, StoreError> {
        Ok(self.checkpoint(key)?.map(|checkpoint| checkpoint.cursor))
    }

    /// Read one job's checkpoint, if any.
    pub fn checkpoint(&self, key: &JobKey) -> Result<Option<JournalCheckpoint>, StoreError> {
        let job_key = serde_json::to_string(key)?;
        self.connection()
            .query_row(
                "SELECT generation, cursor FROM job_checkpoints WHERE job_key = ?1",
                [&job_key],
                |row| {
                    let generation: i64 = row.get(0)?;
                    let cursor: String = row.get(1)?;
                    Ok((generation as u64, cursor))
                },
            )
            .optional()?
            .map(|(generation, cursor_json)| {
                let cursor: ProviderCursor = serde_json::from_str(&cursor_json)?;
                Ok(JournalCheckpoint { generation, cursor })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::JobKind;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("pige.db")).unwrap();
        store.migrate().unwrap();
        (dir, store)
    }

    #[test]
    fn generation_starts_at_one_and_bumps_monotonically() {
        let (_dir, store) = store();
        let account = AccountId::new();
        assert_eq!(store.generation(&account).unwrap(), INITIAL_GENERATION);
        assert_eq!(store.bump_generation(&account).unwrap(), 2);
        assert_eq!(store.bump_generation(&account).unwrap(), 3);
        assert_eq!(store.generation(&account).unwrap(), 3);
        assert!(matches!(
            store.check_generation(&account, 2),
            Err(StoreError::StaleGeneration {
                expected: 2,
                actual: 3
            })
        ));
        store.check_generation(&account, 3).unwrap();
    }

    #[test]
    fn a_live_lease_is_held_until_released_and_fences_stale_holders() {
        let (_dir, store) = store();
        let account = AccountId::new();
        let first = store
            .acquire_lease(&account, "process-a", Duration::from_secs(60))
            .unwrap();
        assert_eq!(first.fencing, 1);
        assert!(matches!(
            store.acquire_lease(&account, "process-b", Duration::from_secs(60)),
            Err(StoreError::LeaseHeld)
        ));

        store.ensure_lease(&first).unwrap();
        store.release_lease(&first).unwrap();
        let second = store
            .acquire_lease(&account, "process-b", Duration::from_secs(60))
            .unwrap();
        assert_eq!(second.fencing, 2);
        assert!(matches!(store.ensure_lease(&first), Err(StoreError::LeaseLost)));
    }

    #[test]
    fn an_expired_lease_can_be_taken_over() {
        let (_dir, store) = store();
        let account = AccountId::new();
        let first = store
            .acquire_lease(&account, "process-a", Duration::from_millis(0))
            .unwrap();
        let second = store
            .acquire_lease(&account, "process-b", Duration::from_secs(60))
            .unwrap();
        assert!(second.fencing > first.fencing);
    }

    #[test]
    fn an_expired_lease_cannot_be_renewed_but_can_be_reacquired() {
        let (_dir, store) = store();
        let account = AccountId::new();
        let expired = store
            .acquire_lease(&account, "process-a", Duration::from_millis(0))
            .unwrap();
        assert!(matches!(
            store.renew_lease(&expired, Duration::from_secs(60)),
            Err(StoreError::LeaseLost)
        ));
        let fresh = store
            .acquire_lease(&account, "process-a", Duration::from_secs(60))
            .unwrap();
        assert!(fresh.fencing > expired.fencing);
    }

    #[test]
    fn reacquiring_your_own_live_lease_keeps_the_fencing_token() {
        let (_dir, store) = store();
        let account = AccountId::new();
        let first = store
            .acquire_lease(&account, "process-a", Duration::from_secs(60))
            .unwrap();
        let again = store
            .acquire_lease(&account, "process-a", Duration::from_secs(60))
            .unwrap();
        assert_eq!(first.fencing, again.fencing);
        // The earlier handle is still valid, and another process is still fenced.
        store.ensure_lease(&first).unwrap();
        assert!(matches!(
            store.acquire_lease(&account, "process-b", Duration::from_secs(60)),
            Err(StoreError::LeaseHeld)
        ));
    }

    #[test]
    fn credential_write_fence_checks_generation_inside_the_lease_transaction() {
        let (_dir, store) = store();
        let account = AccountId::new();
        let lease = store
            .fence_credential_write(&account, "cred-a", INITIAL_GENERATION, Duration::from_secs(30))
            .unwrap();
        assert_eq!(lease.fencing, 1);
        store.ensure_lease(&lease).unwrap();
        store.release_lease(&lease).unwrap();

        store.bump_generation(&account).unwrap();
        assert!(matches!(
            store.fence_credential_write(
                &account,
                "cred-a",
                INITIAL_GENERATION,
                Duration::from_secs(30)
            ),
            Err(StoreError::StaleGeneration { .. })
        ));
    }

    #[test]
    fn invalidate_account_bumps_generation_and_fences_outstanding_leases() {
        let (_dir, store) = store();
        let account = AccountId::new();
        let lease = store
            .fence_credential_write(&account, "cred-a", INITIAL_GENERATION, Duration::from_secs(60))
            .unwrap();
        let next = store.invalidate_account(&account).unwrap();
        assert_eq!(next, 2);
        assert_eq!(store.generation(&account).unwrap(), 2);
        assert!(matches!(store.ensure_lease(&lease), Err(StoreError::LeaseLost)));
        assert!(matches!(
            store.fence_credential_write(
                &account,
                "cred-b",
                INITIAL_GENERATION,
                Duration::from_secs(60)
            ),
            Err(StoreError::StaleGeneration { .. })
        ));
    }

    #[test]
    fn pending_auth_records_round_trip_and_expire() {
        let (_dir, store) = store();
        let attempt = AuthAttemptId::new("11111111-1111-1111-1111-111111111111");
        let record = PendingAuthRecord {
            attempt: attempt.clone(),
            account: Some(AccountId::new()),
            provider: ProviderId::from("imap-smtp"),
            credential_ref: format!("pending_{}", attempt.as_str()),
            generation: 1,
            created_at_ms: 0,
            expires_at_ms: 10,
        };
        store.insert_pending_auth(&record).unwrap();
        assert_eq!(store.pending_auth(&attempt).unwrap(), Some(record.clone()));
        assert_eq!(store.expired_pending_auth(9).unwrap().len(), 0);
        assert_eq!(store.expired_pending_auth(10).unwrap(), vec![record]);
        store.remove_pending_auth(&attempt).unwrap();
        assert_eq!(store.pending_auth(&attempt).unwrap(), None);
    }

    #[test]
    fn checkpoints_are_fenced_by_generation() {
        let (_dir, store) = store();
        let account = AccountId::new();
        let key = JobKey {
            account: account.clone(),
            kind: JobKind::MailSync,
            source: "INBOX".to_string(),
        };
        let cursor = ProviderCursor::new(1, "a");
        store.set_checkpoint(&key, INITIAL_GENERATION, &cursor).unwrap();
        assert_eq!(
            store.checkpoint(&key).unwrap(),
            Some(JournalCheckpoint {
                generation: INITIAL_GENERATION,
                cursor: cursor.clone()
            })
        );

        store.bump_generation(&account).unwrap();
        assert!(matches!(
            store.set_checkpoint(&key, INITIAL_GENERATION, &ProviderCursor::new(1, "b")),
            Err(StoreError::StaleGeneration { .. })
        ));
        store
            .set_checkpoint(&key, 2, &ProviderCursor::new(1, "c"))
            .unwrap();
        assert!(matches!(
            store.set_checkpoint(&key, 2, &ProviderCursor::new(1, "d")),
            Ok(())
        ));
    }
}
