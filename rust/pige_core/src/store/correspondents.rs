//! Replay-safe correspondent evidence and the aggregated suggestion projection.
//!
//! Every native message or accepted operation is recorded once as
//! `correspondence_evidence`, with `(address, role)` rows underneath it. The
//! `contact_addresses` projection is always recomputed from those role rows, so
//! suggestions never depend on an increment that a replayed page could repeat.
//!
//! Evidence is intentionally not tied to `messages` by a foreign key: deleting
//! a remote message, evicting cache content, or clearing a folder must leave the
//! learned correspondent intact. Only deleting the account cascades it away.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};

use crate::account::AccountId;
use crate::contacts::learn;
use crate::contacts::normalize::normalize_address;
use crate::contacts::types::{
    AddressKey, EvidenceId, HeaderRole, LearningOrigin, Observation, ObservationTrust,
};
use crate::mail::{Address, MessageId};
use crate::providers::{HistoryPage, JobKey, JobKind, OperationContext, ProviderCursor};

use super::jobs::{check_checkpoint_in, check_generation_in, set_checkpoint_in};
use super::messages::cached_messages_after_on;
use super::{Store, StoreError};

/// The durable history job's source. History paging is account-wide, like sync.
pub const HISTORY_SOURCE: &str = "account";

/// The durable key for an account's header-history crawl.
pub fn history_job(account: &AccountId) -> JobKey {
    JobKey {
        account: account.clone(),
        kind: JobKind::History,
        source: HISTORY_SOURCE.to_string(),
    }
}

/// One aggregated suggestion, derived from evidence rather than a counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedAddress {
    pub key: AddressKey,
    pub address: Address,
    /// Number of distinct pieces of evidence that mentioned this address.
    pub evidence_count: u32,
    pub outgoing: bool,
    pub trust: ObservationTrust,
    pub last_seen: i64,
}

/// Restart-safe progress of local learning work for one account.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LearningState {
    /// Last cached `messages.id` learned by the ExistingCache backfill.
    pub existing_cache_cursor: Option<MessageId>,
    pub updated_at: i64,
}

/// Result of one bounded ExistingCache backfill page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExistingCachePage {
    pub scanned: u32,
    pub has_more: bool,
}

impl Store {
    /// Persist one piece of correspondence evidence and its normalized
    /// observations atomically.
    ///
    /// `evidence` is the caller's stable native identity (provider message id,
    /// outbox operation id, or local cache id). Repeating the same identity is
    /// idempotent. Invalid addresses are skipped without failing the write.
    ///
    /// `fingerprint` is the optional normalized header hint (see
    /// [`crate::contacts::learn::header_fingerprint`]) that lets a different
    /// native id for the same logical message converge on existing evidence.
    /// Passing `None` keeps dedup to the native identity alone.
    pub fn record_observations(
        &self,
        ctx: &OperationContext,
        evidence: &EvidenceId,
        origin: LearningOrigin,
        fingerprint: Option<&str>,
        observations: &[Observation],
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        record_observations_in(
            &tx,
            &ctx.account,
            evidence,
            origin,
            fingerprint,
            observations,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Persist a history page's observations and advance the history checkpoint
    /// in one transaction. History messages are learned, not inserted into the
    /// visible cache.
    ///
    /// `expected_previous` is the checkpoint the page was produced against (the
    /// same cursor the caller last read). The write is a compare-and-set: a
    /// replayed or out-of-order older page whose expected cursor no longer
    /// matches the durable checkpoint is rejected with [`StoreError::StalePage`]
    /// and neither its observations nor its checkpoint are applied. Pass `None`
    /// for the very first page.
    pub fn apply_history_page(
        &self,
        ctx: &OperationContext,
        expected_previous: Option<&ProviderCursor>,
        page: &HistoryPage,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        let key = history_job(&ctx.account);
        check_checkpoint_in(&tx, &key, expected_previous)?;

        for message in &page.messages {
            let observations =
                learn::observe_message(&ctx.account, message, LearningOrigin::History);
            let fingerprint = learn::header_fingerprint(&message.header);
            let evidence = EvidenceId::for_message(&message.remote_id);
            record_observations_in(
                &tx,
                &ctx.account,
                &evidence,
                LearningOrigin::History,
                Some(&fingerprint),
                &observations,
            )?;
        }

        // The history job checkpoint (above, via `job_checkpoints`) is the single
        // durable history cursor. `contact_learning_state` only tracks the local
        // ExistingCache backfill; there is no second, unread history copy.
        set_checkpoint_in(&tx, &key, ctx.generation, &page.checkpoint)?;
        tx.commit()?;
        Ok(())
    }

    /// Learn up to `limit` already-cached messages that have not yet been
    /// backfilled, then durably advance the local cursor.
    ///
    /// The cursor is a `messages.id` stored in `contact_learning_state`, so a
    /// crash before commit replays the same batch and a restart resumes after
    /// the last committed row. `ExistingCache` reads cached headers only, so it
    /// learns From/To/Cc; `Reply-To` and `Bcc` are not persisted by F5 and are
    /// deliberately excluded here (provider history/live sync still see them).
    pub fn learn_existing_cache_page(
        &self,
        ctx: &OperationContext,
        limit: u32,
    ) -> Result<ExistingCachePage, StoreError> {
        let limit = limit.max(1);
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;

        let cursor: Option<String> = tx
            .query_row(
                "SELECT existing_cache_cursor FROM contact_learning_state WHERE account_id = ?1",
                [&ctx.account.0],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let after = cursor.clone().map(MessageId);
        let mut messages =
            cached_messages_after_on(&tx, &ctx.account, after.as_ref(), limit.saturating_add(1))?;
        let has_more = messages.len() > limit as usize;
        messages.truncate(limit as usize);

        for message in &messages {
            let observations =
                learn::observe_cached_message(&ctx.account, &message.header, message.flags.draft);
            let fingerprint = learn::header_fingerprint(&message.header);
            let evidence = EvidenceId::for_local(&message.id);
            record_observations_in(
                &tx,
                &ctx.account,
                &evidence,
                LearningOrigin::ExistingCache,
                Some(&fingerprint),
                &observations,
            )?;
        }

        let next_cursor = messages
            .last()
            .map(|message| message.id.0.clone())
            .or(cursor);
        tx.execute(
            "INSERT INTO contact_learning_state (account_id, existing_cache_cursor, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET
                 existing_cache_cursor = excluded.existing_cache_cursor,
                 updated_at = excluded.updated_at",
            params![ctx.account.0, next_cursor, now_unix()],
        )?;
        tx.commit()?;

        Ok(ExistingCachePage {
            scanned: messages.len() as u32,
            has_more,
        })
    }

    /// Restart-safe local learning progress for one account.
    pub fn learning_state(&self, account: &AccountId) -> Result<LearningState, StoreError> {
        let row: Option<(Option<String>, i64)> = self
            .connection()
            .query_row(
                "SELECT existing_cache_cursor, updated_at
                 FROM contact_learning_state WHERE account_id = ?1",
                [&account.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((cursor, updated_at)) => LearningState {
                existing_cache_cursor: cursor.map(MessageId),
                updated_at,
            },
            None => LearningState::default(),
        })
    }

    /// All aggregated correspondent suggestions for one account, newest first.
    pub fn learned_addresses(
        &self,
        account: &AccountId,
    ) -> Result<Vec<LearnedAddress>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT address_key, email, display_name, trust, outgoing, last_seen, evidence_count
             FROM contact_addresses
             WHERE account_id = ?1
             ORDER BY last_seen DESC, evidence_count DESC, address_key",
        )?;
        let mut rows = statement.query([&account.0])?;
        let mut learned = Vec::new();
        while let Some(row) = rows.next()? {
            learned.push(learned_from_row(row)?);
        }
        Ok(learned)
    }

    /// One aggregated correspondent, keyed by its normalized address.
    pub fn learned_address(
        &self,
        account: &AccountId,
        key: &AddressKey,
    ) -> Result<Option<LearnedAddress>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT address_key, email, display_name, trust, outgoing, last_seen, evidence_count
             FROM contact_addresses
             WHERE account_id = ?1 AND address_key = ?2",
        )?;
        let mut rows = statement.query(params![account.0, key.0])?;
        match rows.next()? {
            Some(row) => Ok(Some(learned_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// The distinct header roles evidence has recorded for one address.
    pub fn learned_roles(
        &self,
        account: &AccountId,
        key: &AddressKey,
    ) -> Result<Vec<HeaderRole>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT DISTINCT role FROM evidence_addresses
             WHERE account_id = ?1 AND address_key = ?2
             ORDER BY CASE role
                 WHEN 'from' THEN 0
                 WHEN 'reply_to' THEN 1
                 WHEN 'to' THEN 2
                 WHEN 'cc' THEN 3
                 WHEN 'bcc' THEN 4
                 ELSE 5 END",
        )?;
        let mut rows = statement.query(params![account.0, key.0])?;
        let mut roles = Vec::new();
        while let Some(row) = rows.next()? {
            let role: String = row.get(0)?;
            if let Some(role) = HeaderRole::from_db(&role) {
                roles.push(role);
            }
        }
        Ok(roles)
    }
}

/// Record observations inside a caller-owned transaction. The caller is
/// responsible for the generation fence. Used by history, live sync, and the
/// F9 acceptance transaction so each lands atomically with its other evidence.
pub(crate) fn record_observations_in(
    tx: &rusqlite::Connection,
    account: &AccountId,
    evidence: &EvidenceId,
    origin: LearningOrigin,
    fingerprint: Option<&str>,
    observations: &[Observation],
) -> Result<(), StoreError> {
    // Re-normalize and deduplicate one more time so a caller cannot bypass the
    // pure extractor and smuggle duplicate or invalid rows into the ledger.
    let mut accepted = Vec::new();
    let mut seen = HashSet::new();
    for observation in observations {
        let Ok(key) = normalize_address(&observation.address) else {
            continue;
        };
        if !seen.insert((key.clone(), observation.role)) {
            continue;
        }
        let mut observation = observation.clone();
        observation.key = key;
        accepted.push(observation);
    }
    if accepted.is_empty() {
        return Ok(());
    }

    let canonical = canonical_evidence_id(tx, account, evidence, fingerprint)?;
    let observed_at = accepted
        .iter()
        .map(|observation| observation.last_seen)
        .min()
        .unwrap_or(0);
    let last_seen = accepted
        .iter()
        .map(|observation| observation.last_seen)
        .max()
        .unwrap_or(0);
    let fingerprint = fingerprint.filter(|value| !value.is_empty());

    tx.execute(
        "INSERT INTO correspondence_evidence
             (account_id, evidence_id, origin, fingerprint, observed_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, evidence_id) DO UPDATE SET
             origin = CASE
                 WHEN (CASE excluded.origin
                     WHEN 'accepted_send' THEN 4
                     WHEN 'selection' THEN 3
                     WHEN 'live_arrival' THEN 2
                     WHEN 'history' THEN 1
                     ELSE 0 END)
                    > (CASE correspondence_evidence.origin
                     WHEN 'accepted_send' THEN 4
                     WHEN 'selection' THEN 3
                     WHEN 'live_arrival' THEN 2
                     WHEN 'history' THEN 1
                     ELSE 0 END)
                 THEN excluded.origin
                 ELSE correspondence_evidence.origin
             END,
             fingerprint = COALESCE(correspondence_evidence.fingerprint, excluded.fingerprint),
             observed_at = MIN(correspondence_evidence.observed_at, excluded.observed_at),
             last_seen = MAX(correspondence_evidence.last_seen, excluded.last_seen)",
        params![
            account.0,
            canonical,
            origin.as_str(),
            fingerprint,
            observed_at,
            last_seen
        ],
    )?;

    let mut affected = Vec::new();
    let mut affected_seen = HashSet::new();
    for observation in &accepted {
        let display_name = observation
            .address
            .name
            .as_deref()
            .filter(|name| !name.is_empty());
        tx.execute(
            "INSERT INTO evidence_addresses
                 (account_id, evidence_id, address_key, role, email, display_name,
                  outgoing, trust, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(account_id, evidence_id, address_key, role) DO UPDATE SET
                 last_seen = MAX(evidence_addresses.last_seen, excluded.last_seen),
                 outgoing = MAX(evidence_addresses.outgoing, excluded.outgoing),
                 display_name = CASE
                     WHEN (CASE excluded.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                        > (CASE evidence_addresses.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                     THEN excluded.display_name
                     WHEN (CASE excluded.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                        = (CASE evidence_addresses.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                       AND excluded.last_seen >= evidence_addresses.last_seen
                       AND excluded.display_name IS NOT NULL
                       AND excluded.display_name <> ''
                     THEN excluded.display_name
                     ELSE evidence_addresses.display_name
                 END,
                 email = CASE
                     WHEN (CASE excluded.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                        > (CASE evidence_addresses.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                     THEN excluded.email
                     WHEN (CASE excluded.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                        = (CASE evidence_addresses.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                       AND excluded.last_seen >= evidence_addresses.last_seen
                     THEN excluded.email
                     ELSE evidence_addresses.email
                 END,
                 trust = CASE
                     WHEN (CASE excluded.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                        > (CASE evidence_addresses.trust
                         WHEN 'deliberate' THEN 2
                         WHEN 'ordinary' THEN 1
                         ELSE 0 END)
                     THEN excluded.trust
                     ELSE evidence_addresses.trust
                 END",
            params![
                account.0,
                canonical,
                observation.key.0,
                observation.role.as_str(),
                observation.address.email,
                display_name,
                bool_int(observation.outgoing),
                observation.trust.as_str(),
                observation.last_seen,
            ],
        )?;
        if affected_seen.insert(observation.key.0.clone()) {
            affected.push(observation.key.clone());
        }
    }

    for key in affected {
        recompute_contact_address_in(tx, account, &key)?;
    }
    Ok(())
}

fn canonical_evidence_id(
    tx: &rusqlite::Connection,
    account: &AccountId,
    evidence: &EvidenceId,
    fingerprint: Option<&str>,
) -> Result<String, StoreError> {
    // Native identity is primary.
    let exact: Option<String> = tx
        .query_row(
            "SELECT evidence_id FROM correspondence_evidence
             WHERE account_id = ?1 AND evidence_id = ?2",
            params![account.0, evidence.0],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(exact) = exact {
        return Ok(exact);
    }
    // Fingerprint is only a secondary hint; choose the oldest deterministic
    // native identity so every replay of a UID reset/copy converges on it.
    if let Some(fingerprint) = fingerprint.filter(|value| !value.is_empty()) {
        let existing: Option<String> = tx
            .query_row(
                "SELECT evidence_id FROM correspondence_evidence
                 WHERE account_id = ?1 AND fingerprint = ?2
                 ORDER BY evidence_id LIMIT 1",
                params![account.0, fingerprint],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
    }
    Ok(evidence.0.clone())
}

fn recompute_contact_address_in(
    tx: &rusqlite::Connection,
    account: &AccountId,
    key: &AddressKey,
) -> Result<(), StoreError> {
    let mut statement = tx.prepare(
        "SELECT evidence_id, email, display_name, trust, outgoing, last_seen
         FROM evidence_addresses
         WHERE account_id = ?1 AND address_key = ?2
         ORDER BY CASE trust
             WHEN 'deliberate' THEN 2
             WHEN 'ordinary' THEN 1
             ELSE 0 END DESC,
             last_seen DESC,
             evidence_id ASC",
    )?;
    let mut rows = statement.query(params![account.0, key.0])?;

    let mut evidence = HashSet::new();
    let mut email: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut strongest = ObservationTrust::SpamOrTrash;
    let mut outgoing = false;
    let mut last_seen = 0i64;
    while let Some(row) = rows.next()? {
        let evidence_id: String = row.get(0)?;
        evidence.insert(evidence_id);
        let row_email: String = row.get(1)?;
        let row_name: Option<String> = row.get(2)?;
        let trust: String = row.get(3)?;
        let row_outgoing: i64 = row.get(4)?;
        let row_seen: i64 = row.get(5)?;
        if email.is_none() {
            email = Some(row_email);
        }
        if display_name.is_none()
            && let Some(name) = row_name.filter(|name| !name.is_empty())
        {
            display_name = Some(name);
        }
        if let Some(parsed) = ObservationTrust::from_db(&trust)
            && parsed.rank() > strongest.rank()
        {
            strongest = parsed;
        }
        outgoing |= row_outgoing != 0;
        last_seen = last_seen.max(row_seen);
    }

    let Some(email) = email else {
        tx.execute(
            "DELETE FROM contact_addresses WHERE account_id = ?1 AND address_key = ?2",
            params![account.0, key.0],
        )?;
        return Ok(());
    };

    tx.execute(
        "INSERT INTO contact_addresses
             (account_id, address_key, email, display_name, trust, outgoing,
              last_seen, evidence_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(account_id, address_key) DO UPDATE SET
             email = excluded.email,
             display_name = excluded.display_name,
             trust = excluded.trust,
             outgoing = excluded.outgoing,
             last_seen = excluded.last_seen,
             evidence_count = excluded.evidence_count",
        params![
            account.0,
            key.0,
            email,
            display_name,
            strongest.as_str(),
            bool_int(outgoing),
            last_seen,
            evidence.len() as i64,
        ],
    )?;
    Ok(())
}

fn learned_from_row(row: &rusqlite::Row<'_>) -> Result<LearnedAddress, StoreError> {
    let key: String = row.get(0)?;
    let email: String = row.get(1)?;
    let display_name: Option<String> = row.get(2)?;
    let trust: String = row.get(3)?;
    let outgoing: i64 = row.get(4)?;
    let last_seen: i64 = row.get(5)?;
    let evidence_count: i64 = row.get(6)?;
    Ok(LearnedAddress {
        key: AddressKey(key),
        address: Address::new(display_name, email),
        evidence_count: evidence_count.max(0) as u32,
        outgoing: outgoing != 0,
        trust: ObservationTrust::from_db(&trust).unwrap_or(ObservationTrust::Ordinary),
        last_seen,
    })
}

fn bool_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
