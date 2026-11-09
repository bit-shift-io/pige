//! Source-address-book contacts, kept separate from mail-derived evidence.
//!
//! R1 establishes the durable tables so native contact adapters can land later
//! without reshaping the database. Deleting or denying a contact source never
//! touches `correspondence_evidence`: learned suggestions remain independent.
//!
//! R2 keeps the indexed recipient projection in sync here: contact edits refresh
//! only the affected address keys, and deleting a source removes its contact
//! signals while leaving mail-derived signals intact.
//!
//! R6 adds restart-safe, source-scoped full snapshots. Each page's contact
//! changes and its checkpoint commit in one transaction. Only a completed
//! snapshot generation sweeps contacts the provider no longer reports; a
//! failure, cancellation, or budget exit leaves the previous cache untouched.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};

use crate::account::AccountId;
use crate::contacts::normalize::normalize_address;
use crate::contacts::rank::SAVED_CONTACT_KIND;
use crate::contacts::types::AddressKey;
use crate::mail::Address;
use crate::providers::{
    ContactChange, ContactPage, ContactSourceId, JobKey, JobKind, OperationContext, ProviderCursor,
    RemoteContactId,
};

use super::correspondents::refresh_recipient_index_in;
use super::jobs::{
    check_checkpoint_in, check_generation_in, checkpoint_cursor_in, delete_checkpoint_in,
    set_checkpoint_in,
};
use super::{Store, StoreError};

/// One address belonging to a remote contact source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedContactEmail {
    pub source: ContactSourceId,
    pub remote_contact: RemoteContactId,
    pub name: Option<String>,
    pub address: Address,
}

/// Durable synchronization state for one contact source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactSourceState {
    /// Whether the user (or a permission failure) enabled this source.
    pub enabled: bool,
    /// The full-snapshot generation the cached contacts belong to.
    pub snapshot_generation: u64,
    /// A snapshot is in progress and a resumable cursor may exist.
    pub snapshot_active: bool,
    /// Contact records observed in the current/last snapshot.
    pub scanned: u64,
    /// The last snapshot reached end-of-enumeration.
    pub complete: bool,
    pub updated_at: i64,
}

impl ContactSourceState {
    fn default_enabled() -> Self {
        Self {
            enabled: true,
            snapshot_generation: 0,
            snapshot_active: false,
            scanned: 0,
            complete: false,
            updated_at: 0,
        }
    }
}

/// The cursor and generation a contact slice starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSyncStart {
    pub generation: u64,
    pub cursor: Option<ProviderCursor>,
    pub scanned: u64,
    pub complete: bool,
    pub enabled: bool,
}

/// What one committed contact page did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactPageOutcome {
    pub complete: bool,
    /// Contacts swept because the completed generation did not report them.
    pub swept: usize,
}

/// The durable job key for one contact source's snapshot cursor.
pub fn contacts_job(account: &AccountId, source: &ContactSourceId) -> JobKey {
    JobKey {
        account: account.clone(),
        kind: JobKind::Contacts,
        source: source.as_str().to_string(),
    }
}

impl Store {
    /// Record (or rename) one contact source for an account. Saved address
    /// books are the default; Google-style Other contacts use
    /// [`Self::upsert_contact_source_with_kind`].
    pub fn upsert_contact_source(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        display_name: Option<&str>,
    ) -> Result<(), StoreError> {
        self.upsert_contact_source_with_kind(account, source, display_name, SAVED_CONTACT_KIND)
    }

    /// Record a contact source with an explicit kind (`saved` or `other`).
    /// Changing the kind reclassifies every address already cached for it.
    pub fn upsert_contact_source_with_kind(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        display_name: Option<&str>,
        kind: &str,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO contact_sources (account_id, source_id, display_name, kind)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, source_id) DO UPDATE SET
                 display_name = excluded.display_name,
                 kind = excluded.kind",
            params![account.0, source.as_str(), display_name, kind],
        )?;
        refresh_source_keys_in(&tx, account, source)?;
        tx.commit()?;
        Ok(())
    }

    /// Add one address to a source contact. Invalid addresses are rejected as a
    /// `Corrupt` value rather than silently stored under a bogus key.
    pub fn upsert_contact_email(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        remote_contact: &RemoteContactId,
        address: &Address,
    ) -> Result<(), StoreError> {
        let key = normalize_address(address)
            .map_err(|error| StoreError::corrupt("contacts.email", error.to_string()))?;
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO contact_emails
                 (account_id, source_id, remote_contact_id, address_key, email, display_name,
                  snapshot_generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
             ON CONFLICT(account_id, source_id, remote_contact_id, address_key) DO UPDATE SET
                 email = excluded.email,
                 display_name = excluded.display_name",
            params![
                account.0,
                source.as_str(),
                remote_contact.as_str(),
                key.0,
                address.email.trim(),
                address.name,
            ],
        )?;
        refresh_recipient_index_in(&tx, account, &key)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove a whole contact source (permission denial or deletion). Learned
    /// mail evidence is deliberately untouched; only the source's contact
    /// signals disappear from the projection.
    pub fn remove_contact_source(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        clear_contact_source_data_in(&tx, account, source)?;
        tx.commit()?;
        Ok(())
    }

    /// Every cached address from every source for one account.
    pub fn contact_emails(
        &self,
        account: &AccountId,
    ) -> Result<Vec<CachedContactEmail>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT source_id, remote_contact_id, display_name, email
             FROM contact_emails
             WHERE account_id = ?1
             ORDER BY source_id, remote_contact_id, address_key",
        )?;
        let mut rows = statement.query([&account.0])?;
        let mut emails = Vec::new();
        while let Some(row) = rows.next()? {
            let name: Option<String> = row.get(2)?;
            emails.push(CachedContactEmail {
                source: ContactSourceId::from(row.get::<_, String>(0)?),
                remote_contact: RemoteContactId::from(row.get::<_, String>(1)?),
                address: Address::new(name.clone(), row.get::<_, String>(3)?),
                name,
            });
        }
        Ok(emails)
    }

    /// Durable synchronization state for one source. An unknown source is
    /// enabled with no snapshot, so a first sync can start immediately.
    pub fn contact_source_state(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
    ) -> Result<ContactSourceState, StoreError> {
        contact_source_state_in(&self.connection(), account, source)
    }

    /// Every source that has durable synchronization state, ordered by id.
    pub fn contact_source_states(
        &self,
        account: &AccountId,
    ) -> Result<Vec<(ContactSourceId, ContactSourceState)>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT source_id, enabled, snapshot_generation, snapshot_active, scanned,
                    complete, updated_at
             FROM contact_source_state
             WHERE account_id = ?1
             ORDER BY source_id",
        )?;
        let mut rows = statement.query([&account.0])?;
        let mut states = Vec::new();
        while let Some(row) = rows.next()? {
            states.push((
                ContactSourceId::from(row.get::<_, String>(0)?),
                state_from_row(row)?,
            ));
        }
        Ok(states)
    }

    /// Enable or disable one source. Disabling cancels its resumable job but
    /// deliberately keeps every already-cached contact; only an explicit source
    /// removal ([`Self::remove_contact_source`]) deletes local contact rows.
    pub fn set_contact_source_enabled(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        enabled: bool,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let state = contact_source_state_in(&tx, account, source)?;
        tx.execute(
            "INSERT INTO contact_source_state
                 (account_id, source_id, enabled, snapshot_generation, snapshot_active,
                  scanned, complete, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(account_id, source_id) DO UPDATE SET
                 enabled = excluded.enabled,
                 snapshot_active = CASE
                     WHEN excluded.enabled = 0 THEN 0
                     ELSE contact_source_state.snapshot_active
                 END,
                 updated_at = excluded.updated_at",
            params![
                account.0,
                source.as_str(),
                bool_int(enabled),
                state.snapshot_generation.min(i64::MAX as u64) as i64,
                bool_int(state.snapshot_active),
                state.scanned.min(i64::MAX as u64) as i64,
                bool_int(state.complete),
                now_unix(),
            ],
        )?;
        if !enabled {
            delete_checkpoint_in(&tx, &contacts_job(account, source))?;
        }
        if enabled {
            tx.execute(
                "INSERT OR IGNORE INTO contact_sources
                     (account_id, source_id, display_name, kind)
                 VALUES (?1, ?2, NULL, ?3)",
                params![account.0, source.as_str(), SAVED_CONTACT_KIND],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Start or resume one source's full snapshot.
    ///
    /// No cursor means a new snapshot generation begins and progress restarts.
    /// An existing cursor continues it exactly where the last committed page
    /// stopped. The current generation is durable so every page can fence its
    /// write and only the final page may sweep absent contacts.
    pub fn contact_sync_start(
        &self,
        ctx: &OperationContext,
        source: &ContactSourceId,
    ) -> Result<ContactSyncStart, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        let state = contact_source_state_in(&tx, &ctx.account, source)?;
        if !state.enabled {
            tx.commit()?;
            return Ok(ContactSyncStart {
                generation: state.snapshot_generation,
                cursor: None,
                scanned: state.scanned,
                complete: state.complete,
                enabled: false,
            });
        }

        let key = contacts_job(&ctx.account, source);
        let mut cursor = checkpoint_cursor_in(&tx, &key)?;
        let fresh = cursor.is_none() || state.complete;
        let (generation, scanned, complete, active) = if fresh {
            // A completed (or never-started) job starts a fresh generation.
            // Retire the finished snapshot's cursor so the first page of the
            // new generation is requested from no cursor. The previous
            // generation's rows stay until this one completes.
            if cursor.is_some() {
                delete_checkpoint_in(&tx, &key)?;
                cursor = None;
            }
            (
                state.snapshot_generation.saturating_add(1).max(1),
                0,
                false,
                true,
            )
        } else {
            // A legacy checkpoint written before generations existed resumes
            // as generation 1 rather than sweeping rows as generation 0.
            let generation = state.snapshot_generation.max(1);
            (generation, state.scanned, false, true)
        };
        tx.execute(
            "INSERT INTO contact_sources (account_id, source_id, display_name, kind)
             VALUES (?1, ?2, NULL, ?3)
             ON CONFLICT(account_id, source_id) DO NOTHING",
            params![ctx.account.0, source.as_str(), SAVED_CONTACT_KIND],
        )?;
        write_state_in(
            &tx,
            &ctx.account,
            source,
            true,
            ContactSourceState {
                enabled: true,
                snapshot_generation: generation,
                snapshot_active: active,
                scanned,
                complete,
                updated_at: now_unix(),
            },
        )?;
        tx.commit()?;

        Ok(ContactSyncStart {
            generation,
            cursor,
            scanned,
            complete,
            enabled: true,
        })
    }

    /// Commit one contact page's changes, its cursor, and its progress in one
    /// generation-fenced transaction.
    ///
    /// `expected_previous` is the cursor the page was produced against. When
    /// the page completes the snapshot, contacts not observed in
    /// `expected_generation` are swept and the resumable cursor is removed so
    /// the next slice starts a fresh generation.
    pub fn apply_contact_page(
        &self,
        ctx: &OperationContext,
        source: &ContactSourceId,
        expected_generation: u64,
        expected_previous: Option<&ProviderCursor>,
        page: &ContactPage,
    ) -> Result<ContactPageOutcome, StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        let state = contact_source_state_in(&tx, &ctx.account, source)?;
        if !state.enabled {
            return Err(StoreError::ContactSourceDisabled);
        }
        if !state.snapshot_active || state.snapshot_generation != expected_generation {
            return Err(StoreError::StalePage);
        }
        let key = contacts_job(&ctx.account, source);
        check_checkpoint_in(&tx, &key, expected_previous)?;

        tx.execute(
            "INSERT OR IGNORE INTO contact_sources
                 (account_id, source_id, display_name, kind)
             VALUES (?1, ?2, NULL, ?3)",
            params![ctx.account.0, source.as_str(), SAVED_CONTACT_KIND],
        )?;

        let mut affected: HashSet<AddressKey> = HashSet::new();
        for change in &page.changes {
            match change {
                ContactChange::Upsert(record) => {
                    let old_keys =
                        remote_contact_keys_in(&tx, &ctx.account, source, &record.remote_id)?;
                    affected.extend(old_keys.iter().cloned());
                    let default_name = record
                        .names
                        .iter()
                        .map(|name| name.trim())
                        .find(|name| !name.is_empty())
                        .map(str::to_string);
                    let mut new_keys = HashSet::new();
                    for email in &record.emails {
                        let Ok(key) = normalize_address(email) else {
                            continue;
                        };
                        let display_name = email
                            .name
                            .clone()
                            .filter(|name| !name.trim().is_empty())
                            .or_else(|| default_name.clone());
                        upsert_contact_email_in(
                            &tx,
                            &ctx.account,
                            source,
                            &record.remote_id,
                            &key,
                            email,
                            display_name.as_deref(),
                            expected_generation,
                        )?;
                        new_keys.insert(key.clone());
                        affected.insert(key);
                    }
                    for old_key in old_keys {
                        if !new_keys.contains(&old_key) {
                            tx.execute(
                                "DELETE FROM contact_emails
                                 WHERE account_id = ?1 AND source_id = ?2
                                   AND remote_contact_id = ?3 AND address_key = ?4",
                                params![
                                    ctx.account.0,
                                    source.as_str(),
                                    record.remote_id.as_str(),
                                    old_key.0
                                ],
                            )?;
                        }
                    }
                }
                ContactChange::Delete(remote) => {
                    for key in remote_contact_keys_in(&tx, &ctx.account, source, remote)? {
                        affected.insert(key);
                    }
                    tx.execute(
                        "DELETE FROM contact_emails
                         WHERE account_id = ?1 AND source_id = ?2 AND remote_contact_id = ?3",
                        params![ctx.account.0, source.as_str(), remote.as_str()],
                    )?;
                }
            }
        }

        let scanned = state
            .scanned
            .saturating_add(page.changes.len().min(u64::MAX as usize) as u64);
        let complete = !page.has_more;
        let mut swept = 0usize;

        if complete {
            // Only end-of-enumeration may retire contacts from older
            // generations. This is the single destructive step of a snapshot.
            let stale = stale_keys_in(&tx, &ctx.account, source, expected_generation)?;
            swept = stale.len();
            affected.extend(stale);
            tx.execute(
                "DELETE FROM contact_emails
                 WHERE account_id = ?1 AND source_id = ?2 AND snapshot_generation <> ?3",
                params![
                    ctx.account.0,
                    source.as_str(),
                    expected_generation.min(i64::MAX as u64) as i64
                ],
            )?;
        }
        // Keep the final page's cursor too: it names the completed sweep
        // generation for diagnostics. `contact_sync_start` sees the completed
        // state and retires it before the next generation.
        set_checkpoint_in(&tx, &key, ctx.generation, &page.checkpoint)?;

        for key in &affected {
            refresh_recipient_index_in(&tx, &ctx.account, key)?;
        }

        write_state_in(
            &tx,
            &ctx.account,
            source,
            true,
            ContactSourceState {
                enabled: true,
                snapshot_generation: expected_generation,
                snapshot_active: !complete,
                scanned,
                complete,
                updated_at: now_unix(),
            },
        )?;
        tx.commit()?;
        Ok(ContactPageOutcome { complete, swept })
    }
}

fn state_from_row(row: &rusqlite::Row<'_>) -> Result<ContactSourceState, rusqlite::Error> {
    Ok(ContactSourceState {
        enabled: row.get::<_, i64>(1)? != 0,
        snapshot_generation: row.get::<_, i64>(2)?.max(0) as u64,
        snapshot_active: row.get::<_, i64>(3)? != 0,
        scanned: row.get::<_, i64>(4)?.max(0) as u64,
        complete: row.get::<_, i64>(5)? != 0,
        updated_at: row.get(6)?,
    })
}

fn contact_source_state_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
) -> Result<ContactSourceState, StoreError> {
    Ok(conn
        .query_row(
            "SELECT source_id, enabled, snapshot_generation, snapshot_active, scanned,
                    complete, updated_at
             FROM contact_source_state
             WHERE account_id = ?1 AND source_id = ?2",
            params![account.0, source.as_str()],
            state_from_row,
        )
        .optional()?
        .unwrap_or_else(ContactSourceState::default_enabled))
}

fn write_state_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
    enabled: bool,
    state: ContactSourceState,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO contact_source_state
             (account_id, source_id, enabled, snapshot_generation, snapshot_active,
              scanned, complete, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(account_id, source_id) DO UPDATE SET
             enabled = excluded.enabled,
             snapshot_generation = excluded.snapshot_generation,
             snapshot_active = excluded.snapshot_active,
             scanned = excluded.scanned,
             complete = excluded.complete,
             updated_at = excluded.updated_at",
        params![
            account.0,
            source.as_str(),
            bool_int(enabled),
            state.snapshot_generation.min(i64::MAX as u64) as i64,
            bool_int(state.snapshot_active),
            state.scanned.min(i64::MAX as u64) as i64,
            bool_int(state.complete),
            state.updated_at,
        ],
    )?;
    Ok(())
}

fn remote_contact_keys_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
    remote: &RemoteContactId,
) -> Result<Vec<AddressKey>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT address_key FROM contact_emails
         WHERE account_id = ?1 AND source_id = ?2 AND remote_contact_id = ?3
         ORDER BY address_key",
    )?;
    let mut rows = statement.query(params![account.0, source.as_str(), remote.as_str()])?;
    let mut keys = Vec::new();
    while let Some(row) = rows.next()? {
        keys.push(AddressKey(row.get(0)?));
    }
    Ok(keys)
}

fn stale_keys_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
    generation: u64,
) -> Result<Vec<AddressKey>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT address_key FROM contact_emails
         WHERE account_id = ?1 AND source_id = ?2 AND snapshot_generation <> ?3
         ORDER BY address_key",
    )?;
    let mut rows = statement.query(params![
        account.0,
        source.as_str(),
        generation.min(i64::MAX as u64) as i64
    ])?;
    let mut keys = Vec::new();
    while let Some(row) = rows.next()? {
        keys.push(AddressKey(row.get(0)?));
    }
    Ok(keys)
}

#[allow(clippy::too_many_arguments)]
fn upsert_contact_email_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
    remote: &RemoteContactId,
    key: &AddressKey,
    address: &Address,
    display_name: Option<&str>,
    generation: u64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO contact_emails
             (account_id, source_id, remote_contact_id, address_key, email, display_name,
              snapshot_generation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(account_id, source_id, remote_contact_id, address_key) DO UPDATE SET
             email = excluded.email,
             display_name = excluded.display_name,
             snapshot_generation = excluded.snapshot_generation",
        params![
            account.0,
            source.as_str(),
            remote.as_str(),
            key.0,
            address.email.trim(),
            display_name,
            generation.min(i64::MAX as u64) as i64,
        ],
    )?;
    Ok(())
}

/// Delete one source's cached contacts and sync state inside a caller-owned
/// transaction. Learned evidence is deliberately untouched. Used by explicit
/// source replacement/removal and by [`Store::remove_contact_source`].
pub(crate) fn clear_contact_source_data_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
) -> Result<(), StoreError> {
    let keys = source_keys_in(conn, account, source)?;
    conn.execute(
        "DELETE FROM contact_emails WHERE account_id = ?1 AND source_id = ?2",
        params![account.0, source.as_str()],
    )?;
    conn.execute(
        "DELETE FROM contact_source_state WHERE account_id = ?1 AND source_id = ?2",
        params![account.0, source.as_str()],
    )?;
    conn.execute(
        "DELETE FROM contact_sources WHERE account_id = ?1 AND source_id = ?2",
        params![account.0, source.as_str()],
    )?;
    delete_checkpoint_in(conn, &contacts_job(account, source))?;
    for key in keys {
        refresh_recipient_index_in(conn, account, &key)?;
    }
    Ok(())
}

fn source_keys_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
) -> Result<Vec<AddressKey>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT address_key FROM contact_emails
         WHERE account_id = ?1 AND source_id = ?2
         ORDER BY address_key",
    )?;
    let mut rows = statement.query(params![account.0, source.as_str()])?;
    let mut keys = Vec::new();
    while let Some(row) = rows.next()? {
        keys.push(AddressKey(row.get(0)?));
    }
    Ok(keys)
}

fn refresh_source_keys_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
) -> Result<(), StoreError> {
    for key in source_keys_in(conn, account, source)? {
        refresh_recipient_index_in(conn, account, &key)?;
    }
    Ok(())
}

fn bool_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
