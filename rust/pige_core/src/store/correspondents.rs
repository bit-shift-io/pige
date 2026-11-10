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

use std::collections::{BTreeMap, HashSet};

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, StatementStatus, params, params_from_iter};

use crate::account::AccountId;
use crate::contacts::learn;
use crate::contacts::normalize::normalize_address;
use crate::contacts::rank::{
    MAX_FREQUENCY, OTHER_CONTACT_KIND, RecipientCandidate, prefix_upper_bound,
    recipient_account_search_sql, recipient_account_token_search_sql, recipient_search_tokens,
};
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
    /// Monotonic fence for crawl pages. Clear, rebuild, pause, and resume bump
    /// it so a page produced under an older epoch cannot commit.
    pub epoch: u64,
    /// The user's persistent pause switch. Existing suggestions are untouched.
    pub paused: bool,
    /// Clear disabled the provider history crawl until an explicit rebuild.
    pub history_disabled: bool,
    /// Clear disabled the local ExistingCache backfill until an explicit rebuild.
    pub cache_backfill_disabled: bool,
}

/// Restart-safe progress of the provider header-history crawl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryProgress {
    pub scanned: u64,
    pub total_hint: Option<u64>,
    pub complete: bool,
    pub paused: bool,
}

/// Durable history progress plus any pending rate-limit delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryState {
    pub progress: HistoryProgress,
    /// Epoch milliseconds before which no provider request may be made.
    pub retry_after_ms: i64,
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

    /// Record one deliberate recipient choice made in compose.
    ///
    /// This is deliberately a stable evidence id per address: repeated picks
    /// refresh `last_seen` but cannot inflate frequency, while arbitrary typing
    /// never reaches this method. The user-authored display name (if any) is
    /// learned as deliberate evidence.
    pub fn record_selection(
        &self,
        ctx: &OperationContext,
        address: &Address,
    ) -> Result<(), StoreError> {
        let key = normalize_address(address)
            .map_err(|error| StoreError::corrupt("contacts.address", error.to_string()))?;
        let observation = Observation {
            key: key.clone(),
            address: address.clone(),
            role: HeaderRole::To,
            last_seen: now_unix(),
            outgoing: true,
            trust: ObservationTrust::Deliberate,
        };
        let evidence = EvidenceId(format!("selection:{}", key.0));
        self.record_observations(
            ctx,
            &evidence,
            LearningOrigin::Selection,
            None,
            &[observation],
        )
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
    ///
    /// This convenience form stamps the page with the *current* learning epoch.
    /// A crawl that fetched its page before a clear/rebuild/pause must call
    /// [`Self::apply_history_page_at_epoch`] with the epoch it captured instead.
    pub fn apply_history_page(
        &self,
        ctx: &OperationContext,
        expected_previous: Option<&ProviderCursor>,
        page: &HistoryPage,
    ) -> Result<(), StoreError> {
        let epoch = self.learning_epoch(&ctx.account)?;
        self.apply_history_page_at_epoch(ctx, epoch, expected_previous, page)
    }

    /// Learn up to `limit` already-cached messages that have not yet been
    /// backfilled, then durably advance the local cursor.
    ///
    /// The cursor is a `messages.id` stored in `contact_learning_state`, so a
    /// crash before commit replays the same batch and a restart resumes after
    /// the last committed row. `ExistingCache` reads cached headers only, so it
    /// learns From/To/Cc; `Reply-To` and `Bcc` are not persisted by F5 and are
    /// deliberately excluded here (provider history/live sync still see them).
    ///
    /// Clear disables this path until an explicit rebuild, and pause stops it.
    /// A backfill that captured an older epoch must use
    /// [`Self::learn_existing_cache_page_at_epoch`].
    pub fn learn_existing_cache_page(
        &self,
        ctx: &OperationContext,
        limit: u32,
    ) -> Result<ExistingCachePage, StoreError> {
        let epoch = self.learning_epoch(&ctx.account)?;
        self.learn_existing_cache_page_at_epoch(ctx, limit, epoch)
    }

    /// Epoch-fenced form of [`Self::learn_existing_cache_page`].
    pub fn learn_existing_cache_page_at_epoch(
        &self,
        ctx: &OperationContext,
        limit: u32,
        expected_epoch: u64,
    ) -> Result<ExistingCachePage, StoreError> {
        let limit = limit.max(1);
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        let state = learning_state_in(&tx, &ctx.account)?;
        check_learning_write_in(&state, expected_epoch)?;
        if state.cache_backfill_disabled {
            return Err(StoreError::LearningDisabled);
        }

        let after = state.existing_cache_cursor.clone();
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
            .or_else(|| state.existing_cache_cursor.map(|cursor| cursor.0));
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

    /// Current learning epoch for one account. Bumped by clear, rebuild, and
    /// pause/resume so a crawl that started earlier cannot commit.
    pub fn learning_epoch(&self, account: &AccountId) -> Result<u64, StoreError> {
        Ok(self.learning_state(account)?.epoch)
    }

    /// Pause or resume all learning ingestion for one account.
    ///
    /// Pausing keeps every existing suggestion and leaves the history
    /// checkpoint in place so a resume continues where it stopped. Both edges
    /// advance the learning epoch: a page that was already in flight must not
    /// commit while paused, nor immediately after a resume.
    pub fn pause_learning(&self, account: &AccountId, paused: bool) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        ensure_learning_state_in(&tx, account)?;
        tx.execute(
            "UPDATE contact_learning_state
             SET learning_paused = ?2,
                 learning_epoch = learning_epoch + 1,
                 updated_at = ?3
             WHERE account_id = ?1",
            params![account.0, bool_int(paused), now_unix()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Remove this account's learned evidence/projection/selections and disable
    /// automatic history and cached-header ingestion.
    ///
    /// Synced contacts and their independently derived suggestions are kept. On
    /// disk this is a logical deletion of the account's learning rows; it is not
    /// a secure forensic erase of SQLite pages.
    pub fn clear_learned_recipients(&self, account: &AccountId) -> Result<(), StoreError> {
        self.reset_learning_in(account, false)
    }

    /// Clear learned data and re-enable automatic history under a new epoch.
    ///
    /// A rebuild is deliberately a fresh learning epoch: it discards what was
    /// learned before and lets a new full-history crawl start from the
    /// beginning. The persistent pause switch is left exactly as the user set
    /// it, and synced contacts are preserved.
    pub fn rebuild_learned_recipients(&self, account: &AccountId) -> Result<(), StoreError> {
        self.reset_learning_in(account, true)
    }

    fn reset_learning_in(
        &self,
        account: &AccountId,
        enable_automatic_learning: bool,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        ensure_learning_state_in(&tx, account)?;

        // Deletes evidence rows (and `evidence_addresses` through its foreign
        // key) and the aggregated learned projection. Selections are ordinary
        // evidence rows with origin `selection`, so they disappear too.
        tx.execute(
            "DELETE FROM correspondence_evidence WHERE account_id = ?1",
            [&account.0],
        )?;
        tx.execute(
            "DELETE FROM contact_addresses WHERE account_id = ?1",
            [&account.0],
        )?;
        // Rebuild the derived projection so the account's synced contacts remain
        // suggestible even though every learned signal for it is gone. Only this
        // account's projection rows are touched; another account's learned
        // suggestions are completely independent of this clear.
        rebuild_recipient_index_for_account_in(&tx, account)?;

        let disabled = if enable_automatic_learning { 0 } else { 1 };
        tx.execute(
            "UPDATE contact_learning_state
             SET existing_cache_cursor = NULL,
                 updated_at = ?2,
                 history_scanned = 0,
                 history_total_hint = NULL,
                 history_complete = 0,
                 history_paused = 0,
                 history_retry_after = 0,
                 learning_epoch = learning_epoch + 1,
                 history_disabled = ?3,
                 cache_backfill_disabled = ?4
             WHERE account_id = ?1",
            params![account.0, now_unix(), disabled, disabled],
        )?;

        // The history checkpoint is not learning evidence, but deleting it
        // makes a new epoch start a full crawl instead of resuming a cursor that
        // may already be past everything the user asked to forget.
        let job_key = serde_json::to_string(&history_job(account))?;
        tx.execute("DELETE FROM job_checkpoints WHERE job_key = ?1", [job_key])?;

        tx.commit()?;
        Ok(())
    }

    /// Epoch-fenced form of [`Self::apply_history_page`]. A page that was
    /// produced before clear/rebuild/pause is rejected instead of committing.
    pub fn apply_history_page_at_epoch(
        &self,
        ctx: &OperationContext,
        expected_epoch: u64,
        expected_previous: Option<&ProviderCursor>,
        page: &HistoryPage,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        let state = learning_state_in(&tx, &ctx.account)?;
        check_learning_write_in(&state, expected_epoch)?;
        if state.history_disabled {
            return Err(StoreError::LearningDisabled);
        }
        let key = history_job(&ctx.account);
        check_checkpoint_in(&tx, &key, expected_previous)?;

        // Recompute each affected address once *after* the whole page's
        // evidence is inserted. Recomputing inside the per-message loop makes a
        // page quadratic in the number of accumulated observations, which a
        // full-history crawl would feel as steadily slower pages.
        let mut affected = HashSet::new();
        for message in &page.messages {
            let observations =
                learn::observe_message(&ctx.account, message, LearningOrigin::History);
            let fingerprint = learn::header_fingerprint(&message.header);
            let evidence = EvidenceId::for_message(&message.remote_id);
            affected.extend(record_observations_collect_in(
                &tx,
                &ctx.account,
                &evidence,
                LearningOrigin::History,
                Some(&fingerprint),
                &observations,
            )?);
        }
        for key in affected {
            recompute_contact_address_in(&tx, &ctx.account, &key)?;
        }

        // The history job checkpoint (above, via `job_checkpoints`) is the single
        // durable history cursor. `contact_learning_state` only tracks the local
        // ExistingCache backfill; there is no second, unread history copy.
        set_checkpoint_in(&tx, &key, ctx.generation, &page.checkpoint)?;
        tx.commit()?;
        Ok(())
    }

    /// Restart-safe local learning progress for one account.
    pub fn learning_state(&self, account: &AccountId) -> Result<LearningState, StoreError> {
        learning_state_in(&self.connection(), account)
    }

    /// Durable header-history progress for one account, without network work.
    pub fn history_state(&self, account: &AccountId) -> Result<HistoryState, StoreError> {
        let row: Option<(i64, Option<i64>, i64, i64, i64)> = self
            .connection()
            .query_row(
                "SELECT history_scanned, history_total_hint, history_complete,
                        history_paused, history_retry_after
                 FROM contact_learning_state WHERE account_id = ?1",
                [&account.0],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((scanned, total_hint, complete, paused, retry_after)) => HistoryState {
                progress: HistoryProgress {
                    scanned: scanned.max(0) as u64,
                    total_hint: total_hint.map(|value| value.max(0) as u64),
                    complete: complete != 0,
                    paused: paused != 0,
                },
                retry_after_ms: retry_after,
            },
            None => HistoryState::default(),
        })
    }

    /// Persist one slice's progress under the account's generation fence.
    /// Progress is monotonic: `scanned` never decreases, and once a crawl is
    /// complete a late tail write cannot transiently mark it incomplete again.
    /// A history retry delay is independent of this write and is preserved.
    ///
    /// This convenience form stamps the write with the *current* learning epoch.
    /// A crawl that captured its epoch before fetching a page must call
    /// [`Self::record_history_progress_at_epoch`] instead, otherwise a page from
    /// before a clear/rebuild could still mark the new epoch complete.
    pub fn record_history_progress(
        &self,
        ctx: &OperationContext,
        progress: HistoryProgress,
    ) -> Result<(), StoreError> {
        let epoch = self.learning_epoch(&ctx.account)?;
        self.record_history_progress_at_epoch(ctx, epoch, progress)
    }

    /// Epoch-fenced form of [`Self::record_history_progress`]. Progress produced
    /// under an older learning epoch is rejected with
    /// [`StoreError::StaleLearningEpoch`] instead of completing or rewinding the
    /// new epoch that clear/rebuild/pause just started.
    pub fn record_history_progress_at_epoch(
        &self,
        ctx: &OperationContext,
        expected_epoch: u64,
        progress: HistoryProgress,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        let state = learning_state_in(&tx, &ctx.account)?;
        check_learning_write_in(&state, expected_epoch)?;
        if state.history_disabled {
            return Err(StoreError::LearningDisabled);
        }
        tx.execute(
            "INSERT INTO contact_learning_state
                 (account_id, existing_cache_cursor, updated_at, history_scanned,
                  history_total_hint, history_complete, history_paused)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account_id) DO UPDATE SET
                 updated_at = excluded.updated_at,
                 history_scanned = MAX(contact_learning_state.history_scanned,
                                       excluded.history_scanned),
                 history_total_hint = COALESCE(excluded.history_total_hint,
                                               contact_learning_state.history_total_hint),
                 -- A late tail write (budget/gate exit) must never undo a
                 -- concurrent completion. Completion is monotonic until the
                 -- learning row itself is cleared.
                 history_complete = CASE
                     WHEN contact_learning_state.history_complete <> 0 THEN 1
                     ELSE excluded.history_complete
                 END,
                 history_paused = CASE
                     WHEN contact_learning_state.history_complete <> 0 THEN 0
                     ELSE excluded.history_paused
                 END",
            params![
                ctx.account.0,
                now_unix(),
                progress.scanned.min(i64::MAX as u64) as i64,
                progress
                    .total_hint
                    .map(|value| value.min(i64::MAX as u64) as i64),
                bool_int(progress.complete),
                bool_int(progress.paused),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Persist a rate-limit delay so a restart does not immediately retry.
    ///
    /// This convenience form stamps the write with the *current* learning epoch;
    /// a crawl must use [`Self::record_history_retry_after_at_epoch`] with the
    /// epoch it captured before the request.
    pub fn record_history_retry_after(
        &self,
        ctx: &OperationContext,
        until_ms: i64,
    ) -> Result<(), StoreError> {
        let epoch = self.learning_epoch(&ctx.account)?;
        self.record_history_retry_after_at_epoch(ctx, epoch, until_ms)
    }

    /// Epoch-fenced form of [`Self::record_history_retry_after`]. A delay
    /// produced under an older epoch must not delay the crawl that a rebuild
    /// just restarted.
    pub fn record_history_retry_after_at_epoch(
        &self,
        ctx: &OperationContext,
        expected_epoch: u64,
        until_ms: i64,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        let state = learning_state_in(&tx, &ctx.account)?;
        check_learning_write_in(&state, expected_epoch)?;
        if state.history_disabled {
            return Err(StoreError::LearningDisabled);
        }
        tx.execute(
            "INSERT INTO contact_learning_state
                 (account_id, existing_cache_cursor, updated_at, history_retry_after)
             VALUES (?1, NULL, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET
                 updated_at = excluded.updated_at,
                 history_retry_after = excluded.history_retry_after",
            params![ctx.account.0, now_unix(), until_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Clear any pending rate-limit delay.
    pub fn clear_history_retry_after(&self, ctx: &OperationContext) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        check_generation_in(&tx, &ctx.account, ctx.generation)?;
        tx.execute(
            "UPDATE contact_learning_state
             SET history_retry_after = 0, updated_at = ?2
             WHERE account_id = ?1",
            params![ctx.account.0, now_unix()],
        )?;
        tx.commit()?;
        Ok(())
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

    /// Search the indexed recipient projection for every account.
    ///
    /// Each account contributes two independently capped pages: exact
    /// folded-name/address matches and the remaining prefix matches. Both are
    /// driven by `recipient_index_account_rank`, so a broad prefix stops after
    /// `cap` rows instead of materializing and sorting the whole token range.
    /// Within one account the query-dependent exact/current tie-breakers are
    /// constant, so the union of the per-account pages is a superset of the true
    /// global top candidates; the caller applies the final deterministic order.
    /// No evidence rows are touched.
    pub fn recipient_candidates(
        &self,
        sending_account: &AccountId,
        folded_query: &str,
        tokens: &[String],
        cap: usize,
    ) -> Result<Vec<RecipientCandidate>, StoreError> {
        Ok(self
            .recipient_candidates_with_steps(sending_account, folded_query, tokens, cap)?
            .0)
    }

    /// SQLite VM steps consumed by the candidate lookup. Test-only hook used by
    /// the 100k-address fixture to prove a short prefix is bounded work.
    #[doc(hidden)]
    pub fn recipient_search_vm_steps(
        &self,
        sending_account: &AccountId,
        folded_query: &str,
        tokens: &[String],
        cap: usize,
    ) -> Result<i64, StoreError> {
        Ok(self
            .recipient_candidates_with_steps(sending_account, folded_query, tokens, cap)?
            .1)
    }

    fn recipient_candidates_with_steps(
        &self,
        _sending_account: &AccountId,
        folded_query: &str,
        tokens: &[String],
        cap: usize,
    ) -> Result<(Vec<RecipientCandidate>, i64), StoreError> {
        if tokens.is_empty() || cap == 0 {
            return Ok((Vec::new(), 0));
        }
        let conn = self.connection();
        let accounts = store_account_ids(&conn)?;

        let mut candidates = Vec::new();
        let mut steps = 0i64;
        for account in accounts {
            for exact in [true, false] {
                let sql = if exact {
                    recipient_account_search_sql(tokens.len(), true)
                } else {
                    let (drive, probe_steps) = choose_prefix_drive(&conn, &account, tokens)?;
                    steps += probe_steps;
                    match drive {
                        PrefixDrive::NoMatches => continue,
                        PrefixDrive::Token(driving) => {
                            recipient_account_token_search_sql(tokens.len(), driving)
                        }
                        PrefixDrive::Rank => recipient_account_search_sql(tokens.len(), false),
                    }
                };
                let values = recipient_query_values(&account, folded_query, tokens, cap);
                let mut statement = conn.prepare(&sql)?;
                {
                    let mut rows = statement.query(params_from_iter(values))?;
                    while let Some(row) = rows.next()? {
                        candidates.push(candidate_from_row(row)?);
                    }
                }
                steps += i64::from(statement.get_status(StatementStatus::VmStep));
            }
        }
        Ok((candidates, steps))
    }

    /// `EXPLAIN QUERY PLAN` for the real recipient lookup. Exposed for the
    /// 100k-address fixture test; it uses exactly the production SQL and values.
    #[doc(hidden)]
    pub fn explain_recipient_search(
        &self,
        _sending_account: &AccountId,
        folded_query: &str,
        tokens: &[String],
        cap: usize,
    ) -> Result<Vec<String>, StoreError> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.connection();
        let mut plan = Vec::new();
        for account in store_account_ids(&conn)? {
            for exact in [true, false] {
                let label = if exact { "exact" } else { "prefix" };
                let sql = if exact {
                    recipient_account_search_sql(tokens.len(), true)
                } else {
                    match choose_prefix_drive(&conn, &account, tokens)?.0 {
                        PrefixDrive::NoMatches => continue,
                        PrefixDrive::Token(driving) => {
                            recipient_account_token_search_sql(tokens.len(), driving)
                        }
                        PrefixDrive::Rank => recipient_account_search_sql(tokens.len(), false),
                    }
                };
                let sql = format!("EXPLAIN QUERY PLAN {sql}");
                let values = recipient_query_values(&account, folded_query, tokens, cap);
                let mut statement = conn.prepare(&sql)?;
                let mut rows = statement.query(params_from_iter(values))?;
                while let Some(row) = rows.next()? {
                    plan.push(format!("{account} {label}: {}", row.get::<_, String>(3)?));
                }
            }
        }
        Ok(plan)
    }

    /// Rebuild both derived recipient tables from their durable sources. Used
    /// after migration and by repair tooling; ordinary writes refresh only the
    /// affected address key.
    pub fn rebuild_recipient_index(&self) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        rebuild_recipient_index_in(&tx)?;
        tx.commit()?;
        Ok(())
    }
}

/// How one account's prefix branch should be executed.
enum PrefixDrive {
    /// The rank-index scan is the right plan (broad or short prefix).
    Rank,
    /// Drive from this token's prefix because it matches a small bounded set.
    Token(usize),
    /// At least one query token has no matches, so the prefix branch is empty.
    NoMatches,
}

/// Tokens whose prefixes match at most this many rows are cheap enough to drive
/// the lookup. Bigger ranges use the rank-index scan, which stops at the cap.
const PREFIX_DRIVE_LIMIT: usize = 4_096;

/// One- and two-character prefixes are common while typing, so they probe a
/// smaller range. A short prefix matching within this bound is still selective
/// enough to drive from `recipient_tokens_token`; above it almost every row
/// matches and the rank-index scan is cheaper.
const SHORT_PREFIX_DRIVE_LIMIT: usize = 256;

/// Pick the cheapest bounded prefix strategy without scanning an unbounded
/// token range. Each probe stops after its limit + 1 rows, so even a broad
/// token costs bounded index work. Very short tokens use the smaller
/// [`SHORT_PREFIX_DRIVE_LIMIT`] probe: a broad one- or two-character prefix
/// stays on the historical rank-index plan, while a rare short prefix (for
/// example a single contact whose name starts with a distinctive two-character
/// pair) is driven from the token index instead of scanning the rank order.
fn choose_prefix_drive(
    conn: &rusqlite::Connection,
    account: &str,
    tokens: &[String],
) -> Result<(PrefixDrive, i64), StoreError> {
    if tokens.is_empty() {
        return Ok((PrefixDrive::NoMatches, 0));
    }
    // A pathological number of query words is cheaper to answer with the
    // rank-index scan than with one bounded probe per word.
    if tokens.len() > 8 {
        return Ok((PrefixDrive::Rank, 0));
    }

    let mut steps = 0i64;
    let mut exact_one = None;

    // First pass: detect any token with no matches and any token that matches
    // exactly one projection row. A unique token is automatically the most
    // selective drive, so the common selective query stops after two rows per
    // token instead of probing every prefix up to the large bound.
    for (index, token) in tokens.iter().enumerate() {
        let (count, probe_steps) = probe_prefix(conn, account, token, 2)?;
        steps += probe_steps;
        if count == 0 {
            return Ok((PrefixDrive::NoMatches, steps));
        }
        if count == 1 && exact_one.is_none() {
            exact_one = Some(index);
        }
    }
    if let Some(index) = exact_one {
        return Ok((PrefixDrive::Token(index), steps));
    }

    // Second pass: find the rarest token whose bounded range fits its drive
    // limit. If even the rarest fitting prefix is too large, use the rank index
    // scan, which stops at the result cap instead of sorting the range.
    let mut best: Option<(usize, usize)> = None;
    for (index, token) in tokens.iter().enumerate() {
        let limit = if token.chars().count() <= 2 {
            SHORT_PREFIX_DRIVE_LIMIT
        } else {
            PREFIX_DRIVE_LIMIT
        };
        let (count, probe_steps) = probe_prefix(conn, account, token, limit as i64 + 1)?;
        steps += probe_steps;
        if count == 0 {
            return Ok((PrefixDrive::NoMatches, steps));
        }
        if count <= limit && best.is_none_or(|(_, best_count)| count < best_count) {
            best = Some((index, count));
        }
    }

    match best {
        Some((index, _)) => Ok((PrefixDrive::Token(index), steps)),
        None => Ok((PrefixDrive::Rank, steps)),
    }
}

/// Count one token's prefix range, stopping at `limit`. A fresh statement is
/// used so `VmStep` reports this probe rather than a cached statement's total.
fn probe_prefix(
    conn: &rusqlite::Connection,
    account: &str,
    token: &str,
    limit: i64,
) -> Result<(usize, i64), StoreError> {
    let mut statement = conn.prepare(
        "SELECT 1 FROM recipient_tokens INDEXED BY recipient_tokens_token
         WHERE account_id = ?1 AND token >= ?2 AND token < ?3
         LIMIT ?4",
    )?;
    let mut count = 0usize;
    {
        let mut rows =
            statement.query(params![account, token, prefix_upper_bound(token), limit])?;
        while rows.next()?.is_some() {
            count += 1;
        }
    }
    Ok((
        count,
        i64::from(statement.get_status(StatementStatus::VmStep)),
    ))
}

fn store_account_ids(conn: &rusqlite::Connection) -> Result<Vec<String>, StoreError> {
    let mut statement = conn.prepare("SELECT id FROM accounts ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut accounts = Vec::new();
    while let Some(row) = rows.next()? {
        accounts.push(row.get(0)?);
    }
    Ok(accounts)
}

fn candidate_from_row(row: &rusqlite::Row<'_>) -> Result<RecipientCandidate, StoreError> {
    let email: String = row.get(2)?;
    let display_name: Option<String> = row.get(3)?;
    let last_seen: i64 = row.get(10)?;
    let frequency: i64 = row.get(11)?;
    Ok(RecipientCandidate {
        account: AccountId(row.get(0)?),
        key: AddressKey(row.get(1)?),
        address: Address::new(display_name, email),
        saved: row.get::<_, i64>(4)? != 0,
        other: row.get::<_, i64>(5)? != 0,
        outgoing: row.get::<_, i64>(6)? != 0,
        selected: row.get::<_, i64>(7)? != 0,
        incoming: row.get::<_, i64>(8)? != 0,
        low_trust_only: row.get::<_, i64>(9)? != 0,
        last_seen,
        frequency: frequency.max(0) as u32,
    })
}

fn recipient_query_values(
    account: &str,
    folded_query: &str,
    tokens: &[String],
    cap: usize,
) -> Vec<Value> {
    let mut values = Vec::with_capacity(tokens.len() * 2 + 3);
    values.push(Value::Text(account.to_string()));
    values.push(Value::Text(folded_query.to_string()));
    for token in tokens {
        values.push(Value::Text(token.clone()));
        values.push(Value::Text(crate::contacts::rank::prefix_upper_bound(
            token,
        )));
    }
    values.push(Value::Integer(cap as i64));
    values
}

/// Contact rows visible for one account/address, split by source kind.
#[derive(Debug, Default)]
struct ContactSignals {
    saved: bool,
    other: bool,
    saved_name: Option<String>,
    saved_email: Option<String>,
    other_name: Option<String>,
    other_email: Option<String>,
}

/// Deliberate/incoming provenance for one account/address.
#[derive(Debug, Default)]
struct EvidenceSignals {
    selected: bool,
    outgoing: bool,
    incoming: bool,
}

fn learned_address_on(
    conn: &rusqlite::Connection,
    account: &AccountId,
    key: &AddressKey,
) -> Result<Option<LearnedAddress>, StoreError> {
    conn.query_row(
        "SELECT address_key, email, display_name, trust, outgoing, last_seen, evidence_count
         FROM contact_addresses
         WHERE account_id = ?1 AND address_key = ?2",
        params![account.0, key.0],
        learned_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

fn contact_signals_on(
    conn: &rusqlite::Connection,
    account: &AccountId,
    key: &AddressKey,
) -> Result<ContactSignals, StoreError> {
    let mut statement = conn.prepare(
        "SELECT ce.email, ce.display_name, COALESCE(cs.kind, 'saved')
         FROM contact_emails ce
         LEFT JOIN contact_sources cs
           ON cs.account_id = ce.account_id AND cs.source_id = ce.source_id
         WHERE ce.account_id = ?1 AND ce.address_key = ?2
         ORDER BY ce.source_id, ce.remote_contact_id, ce.email",
    )?;
    let mut rows = statement.query(params![account.0, key.0])?;
    let mut signals = ContactSignals::default();
    while let Some(row) = rows.next()? {
        let email: String = row.get(0)?;
        let name: Option<String> = row.get(1)?;
        let kind: String = row.get(2)?;
        if kind.eq_ignore_ascii_case(OTHER_CONTACT_KIND) {
            signals.other = true;
            if signals.other_email.is_none() {
                signals.other_email = Some(email);
            }
            if signals.other_name.is_none()
                && let Some(name) = name.filter(|name| !name.trim().is_empty())
            {
                signals.other_name = Some(name);
            }
        } else {
            signals.saved = true;
            if signals.saved_email.is_none() {
                signals.saved_email = Some(email);
            }
            if signals.saved_name.is_none()
                && let Some(name) = name.filter(|name| !name.trim().is_empty())
            {
                signals.saved_name = Some(name);
            }
        }
    }
    Ok(signals)
}

fn evidence_signals_on(
    conn: &rusqlite::Connection,
    account: &AccountId,
    key: &AddressKey,
) -> Result<EvidenceSignals, StoreError> {
    let mut statement = conn.prepare(
        "SELECT ce.origin, ea.trust
         FROM evidence_addresses ea
         JOIN correspondence_evidence ce
           ON ce.account_id = ea.account_id AND ce.evidence_id = ea.evidence_id
         WHERE ea.account_id = ?1 AND ea.address_key = ?2",
    )?;
    let mut rows = statement.query(params![account.0, key.0])?;
    let mut signals = EvidenceSignals::default();
    while let Some(row) = rows.next()? {
        let origin: String = row.get(0)?;
        let trust: String = row.get(1)?;
        match origin.as_str() {
            "selection" => signals.selected = true,
            "accepted_send" => signals.outgoing = true,
            "existing_cache" | "history" | "live_arrival"
                if trust != ObservationTrust::SpamOrTrash.as_str() =>
            {
                signals.incoming = true;
            }
            _ => {}
        }
    }
    Ok(signals)
}

/// Recompute one address's projection row and token set from its durable
/// contact and evidence rows. Called inside the caller's write transaction.
pub(crate) fn refresh_recipient_index_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    key: &AddressKey,
) -> Result<(), StoreError> {
    let learned = learned_address_on(conn, account, key)?;
    let contact = contact_signals_on(conn, account, key)?;
    let evidence = evidence_signals_on(conn, account, key)?;
    write_recipient_index_in(conn, account, key, learned.as_ref(), &contact, &evidence)
}

fn write_recipient_index_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    key: &AddressKey,
    learned: Option<&LearnedAddress>,
    contact: &ContactSignals,
    evidence: &EvidenceSignals,
) -> Result<(), StoreError> {
    if learned.is_none() && !contact.saved && !contact.other {
        delete_recipient_index_in(conn, account, key)?;
        return Ok(());
    }

    let email = contact
        .saved_email
        .clone()
        .or_else(|| contact.other_email.clone())
        .or_else(|| learned.map(|learned| learned.address.email.clone()));
    let Some(email) = email else {
        delete_recipient_index_in(conn, account, key)?;
        return Ok(());
    };
    let display_name = contact
        .saved_name
        .clone()
        .or_else(|| contact.other_name.clone())
        .or_else(|| {
            learned.and_then(|learned| {
                learned
                    .address
                    .name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
            })
        });

    let saved = contact.saved;
    let other = contact.other;
    let selected = evidence.selected;
    let outgoing = evidence.outgoing;
    let incoming = evidence.incoming
        || learned.is_some_and(|learned| learned.trust == ObservationTrust::Ordinary);
    let trusted = saved || other || selected || outgoing;
    let low_trust_only =
        !trusted && learned.is_some_and(|learned| learned.trust == ObservationTrust::SpamOrTrash);
    let last_seen = learned.map(|learned| learned.last_seen).unwrap_or(0);
    let frequency = learned
        .map(|learned| learned.evidence_count.min(MAX_FREQUENCY))
        .unwrap_or(0);
    let tier = if outgoing || selected {
        0
    } else if saved {
        1
    } else if other {
        2
    } else {
        3
    };

    conn.prepare_cached(
        "INSERT INTO recipient_index
             (account_id, address_key, email, display_name, email_folded, name_folded,
              saved_contact, other_contact, outgoing, selected, incoming, low_trust_only,
              last_seen, frequency, tier)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(account_id, address_key) DO UPDATE SET
             email = excluded.email,
             display_name = excluded.display_name,
             email_folded = excluded.email_folded,
             name_folded = excluded.name_folded,
             saved_contact = excluded.saved_contact,
             other_contact = excluded.other_contact,
             outgoing = excluded.outgoing,
             selected = excluded.selected,
             incoming = excluded.incoming,
             low_trust_only = excluded.low_trust_only,
             last_seen = excluded.last_seen,
             frequency = excluded.frequency,
             tier = excluded.tier",
    )?
    .execute(params![
        account.0,
        key.0,
        email,
        display_name,
        email.trim().to_lowercase(),
        display_name.as_deref().unwrap_or("").trim().to_lowercase(),
        bool_int(saved),
        bool_int(other),
        bool_int(outgoing),
        bool_int(selected),
        bool_int(incoming),
        bool_int(low_trust_only),
        last_seen,
        i64::from(frequency),
        i64::from(tier),
    ])?;

    conn.prepare_cached("DELETE FROM recipient_tokens WHERE account_id = ?1 AND address_key = ?2")?
        .execute(params![account.0, key.0])?;
    if !low_trust_only {
        let search = Address::new(display_name, email);
        for token in recipient_search_tokens(&search) {
            conn.prepare_cached(
                "INSERT OR IGNORE INTO recipient_tokens (account_id, address_key, token)
                 VALUES (?1, ?2, ?3)",
            )?
            .execute(params![account.0, key.0, token])?;
        }
    }
    Ok(())
}

fn delete_recipient_index_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    key: &AddressKey,
) -> Result<(), StoreError> {
    conn.prepare_cached("DELETE FROM recipient_index WHERE account_id = ?1 AND address_key = ?2")?
        .execute(params![account.0, key.0])?;
    conn.prepare_cached("DELETE FROM recipient_tokens WHERE account_id = ?1 AND address_key = ?2")?
        .execute(params![account.0, key.0])?;
    Ok(())
}

/// Rebuild the derived recipient tables for every account. Used after a
/// migration or by repair tooling; ordinary writes refresh only the affected
/// address key.
pub(crate) fn rebuild_recipient_index_in(conn: &rusqlite::Connection) -> Result<(), StoreError> {
    for account in store_account_ids(conn)? {
        rebuild_recipient_index_for_account_in(conn, &AccountId(account))?;
    }
    Ok(())
}

/// Rebuild one account's derived recipient tables from its durable contact and
/// evidence rows. Clearing learning for one account must not churn another
/// account's independent projection.
pub(crate) fn rebuild_recipient_index_for_account_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM recipient_tokens WHERE account_id = ?1",
        [&account.0],
    )?;
    conn.execute(
        "DELETE FROM recipient_index WHERE account_id = ?1",
        [&account.0],
    )?;

    let mut entries: BTreeMap<String, (Option<LearnedAddress>, ContactSignals, EvidenceSignals)> =
        BTreeMap::new();

    {
        let mut statement = conn.prepare(
            "SELECT address_key, email, display_name, trust, outgoing, last_seen, evidence_count
             FROM contact_addresses
             WHERE account_id = ?1",
        )?;
        let mut rows = statement.query([&account.0])?;
        while let Some(row) = rows.next()? {
            let address_key: String = row.get(0)?;
            let display_name: Option<String> = row.get(2)?;
            let trust: String = row.get(3)?;
            let outgoing: i64 = row.get(4)?;
            let last_seen: i64 = row.get(5)?;
            let evidence_count: i64 = row.get(6)?;
            let learned = LearnedAddress {
                key: AddressKey(address_key.clone()),
                address: Address::new(display_name, row.get::<_, String>(1)?),
                evidence_count: evidence_count.max(0) as u32,
                outgoing: outgoing != 0,
                trust: ObservationTrust::from_db(&trust).unwrap_or(ObservationTrust::Ordinary),
                last_seen,
            };
            entries.entry(address_key).or_default().0 = Some(learned);
        }
    }

    {
        let mut statement = conn.prepare(
            "SELECT ce.address_key, ce.email, ce.display_name, COALESCE(cs.kind, 'saved')
             FROM contact_emails ce
             LEFT JOIN contact_sources cs
               ON cs.account_id = ce.account_id AND cs.source_id = ce.source_id
             WHERE ce.account_id = ?1
             ORDER BY ce.address_key, ce.source_id, ce.remote_contact_id",
        )?;
        let mut rows = statement.query([&account.0])?;
        while let Some(row) = rows.next()? {
            let address_key: String = row.get(0)?;
            let email: String = row.get(1)?;
            let name: Option<String> = row.get(2)?;
            let kind: String = row.get(3)?;
            let signals = &mut entries.entry(address_key).or_default().1;
            if kind.eq_ignore_ascii_case(OTHER_CONTACT_KIND) {
                signals.other = true;
                if signals.other_email.is_none() {
                    signals.other_email = Some(email);
                }
                if signals.other_name.is_none()
                    && let Some(name) = name.filter(|name| !name.trim().is_empty())
                {
                    signals.other_name = Some(name);
                }
            } else {
                signals.saved = true;
                if signals.saved_email.is_none() {
                    signals.saved_email = Some(email);
                }
                if signals.saved_name.is_none()
                    && let Some(name) = name.filter(|name| !name.trim().is_empty())
                {
                    signals.saved_name = Some(name);
                }
            }
        }
    }

    {
        let mut statement = conn.prepare(
            "SELECT ea.address_key,
                    MAX(CASE WHEN ce.origin = 'selection' THEN 1 ELSE 0 END),
                    MAX(CASE WHEN ce.origin = 'accepted_send' THEN 1 ELSE 0 END),
                    MAX(CASE WHEN ce.origin IN ('existing_cache', 'history', 'live_arrival')
                              AND ea.trust <> 'spam_or_trash' THEN 1 ELSE 0 END)
             FROM evidence_addresses ea
             JOIN correspondence_evidence ce
               ON ce.account_id = ea.account_id AND ce.evidence_id = ea.evidence_id
             WHERE ea.account_id = ?1
             GROUP BY ea.address_key",
        )?;
        let mut rows = statement.query([&account.0])?;
        while let Some(row) = rows.next()? {
            let address_key: String = row.get(0)?;
            let signals = &mut entries.entry(address_key).or_default().2;
            signals.selected = row.get::<_, i64>(1)? != 0;
            signals.outgoing = row.get::<_, i64>(2)? != 0;
            signals.incoming = row.get::<_, i64>(3)? != 0;
        }
    }

    for (key, (learned, contact, evidence)) in entries {
        write_recipient_index_in(
            conn,
            account,
            &AddressKey(key),
            learned.as_ref(),
            &contact,
            &evidence,
        )?;
    }
    Ok(())
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
    let affected =
        record_observations_collect_in(tx, account, evidence, origin, fingerprint, observations)?;
    for key in affected {
        recompute_contact_address_in(tx, account, &key)?;
    }
    Ok(())
}

/// Insert evidence without refreshing the aggregated projection. Returns the
/// affected address keys so a multi-message page can recompute each once.
fn record_observations_collect_in(
    tx: &rusqlite::Connection,
    account: &AccountId,
    evidence: &EvidenceId,
    origin: LearningOrigin,
    fingerprint: Option<&str>,
    observations: &[Observation],
) -> Result<Vec<AddressKey>, StoreError> {
    // Pause is a global stop for every ingestion path. Clear also blocks the
    // two automatic origins even if a future caller bypasses the dedicated
    // history/backfill entry points. A caller inside an already-running
    // transaction (live arrival, accepted send) still commits its own
    // non-learning state, but adds no evidence.
    let controls = learning_state_in(tx, account)?;
    if controls.paused
        || (origin == LearningOrigin::History && controls.history_disabled)
        || (origin == LearningOrigin::ExistingCache && controls.cache_backfill_disabled)
    {
        return Ok(Vec::new());
    }

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
        return Ok(Vec::new());
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

    Ok(affected)
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
        refresh_recipient_index_in(tx, account, key)?;
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
    refresh_recipient_index_in(tx, account, key)?;
    Ok(())
}

fn learned_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LearnedAddress> {
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

/// Make sure an account has a durable learning-controls row without changing
/// any existing value.
fn ensure_learning_state_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO contact_learning_state (account_id, updated_at)
         VALUES (?1, 0)",
        [&account.0],
    )?;
    Ok(())
}

fn learning_state_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
) -> Result<LearningState, StoreError> {
    let row: Option<(Option<String>, i64, i64, i64, i64, i64)> = conn
        .query_row(
            "SELECT existing_cache_cursor, updated_at, learning_epoch, learning_paused,
                    history_disabled, cache_backfill_disabled
             FROM contact_learning_state WHERE account_id = ?1",
            [&account.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    Ok(match row {
        Some((cursor, updated_at, epoch, paused, history_disabled, cache_disabled)) => {
            LearningState {
                existing_cache_cursor: cursor.map(MessageId),
                updated_at,
                epoch: epoch.max(0) as u64,
                paused: paused != 0,
                history_disabled: history_disabled != 0,
                cache_backfill_disabled: cache_disabled != 0,
            }
        }
        None => LearningState::default(),
    })
}

/// Reject a page that was produced before the account's learning controls last
/// moved. Clear/rebuild also set the source-specific disabled flags, which the
/// caller checks separately.
fn check_learning_write_in(
    state: &LearningState,
    expected_epoch: u64,
) -> Result<(), StoreError> {
    if state.epoch != expected_epoch {
        return Err(StoreError::StaleLearningEpoch {
            expected: expected_epoch,
            actual: state.epoch,
        });
    }
    if state.paused {
        return Err(StoreError::LearningPaused);
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
