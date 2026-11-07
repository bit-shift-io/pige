# Contacts and Persistent Recipient Learning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver persistent offline autocomplete learned from contacts, successful sends, and all accessible mail history.
**Architecture:** Cache synced contacts separately from a replay-safe correspondent evidence ledger. An indexed local projection serves compose, while bounded resumable jobs learn headers without loading historical messages into the visible inbox cache.
**Tech Stack:** Rust/SQLite, provider contracts from plan 01, Iced, existing Android WorkManager.
**Spec:** `docs/superpowers/specs/2026-11-07-mail-providers-and-contacts-design.md`; shared DTOs and constraints in `2026-11-07-mail-providers.md`.

## Global Constraints

- No fixed message-count cutoff: full accessible history is the target, not an immediate guarantee.
- Never upload learned addresses automatically.
- Do not infer hidden Bcc recipients or scrape message bodies.
- Self identities/aliases are not default suggestions.
- Keep learned observations in the persistent app-private SQLite cache under the data directory, not an OS-purgeable temp/cache directory.
- Defer contact editing, contact/group management UI, CardDAV implementation, calendar support, remote draft synchronization, and a dynamic plugin ABI.
- Prerequisite: F1-F10 completed. Contacts can be implemented/tested against a fake before native contact adapters exist.

## Review Focus

1. Display-name spoofing, Unicode domains, case-sensitive local parts and plus-tags must not redirect an intended recipient (R1/R2).
2. A suggestion response after switching fields/account must not replace a completed address (R5).
3. Repeated pages, UID resets, and a message in several labels must not multiply observed frequency (R1/R3).
4. Clear-history followed by background resync must not immediately recreate all forgotten suggestions (R4).
5. Contacts permission denial/deletion must not stop local mail-derived suggestions or delete independent learned evidence (R2/R6).

## File structure

- New `rust/pige_core/src/contacts/{mod,types,normalize,learn,rank,sync}.rs`.
- New `store/{contacts,correspondents}.rs`, next free additive migrations, and `api/contacts.rs`.
- New `sync/history.rs`; modify `sync/scheduler.rs` and headless/foreground scheduling entry points.
- New `rust/pige/src/components/recipient_suggestions.rs`; modify compose/account/advanced settings state/messages/update plus components module.
- Modify Android `SyncWorker.kt`, `PigeBridge.kt`, Rust `pige/src/jni.rs` for bounded maintenance and cancellation.
- Integration binaries `contact_learning`, `recipient_lookup`, `history_learning`, `contact_lifecycle`, `contact_sync` under core tests; compose unit tests in existing screen module.

### Task R1: Contact schema and replay-safe address observations

**Files:** Create `contacts/{mod,types,normalize,learn}.rs`, `store/{contacts,correspondents}.rs`, next free migration, `tests/contact_learning.rs`; modify core `lib.rs`, `store/mod.rs`, `send/submission.rs`, `sync/provider.rs`.

**Interfaces:** `AddressKey(String)` preserves local part and canonicalizes domain; `normalize_address(address: &Address) -> Result<AddressKey, AddressError>`. `EvidenceId(String)`; `Observation { key: AddressKey, address: Address, role: HeaderRole, last_seen: i64, outgoing: bool, trust: ObservationTrust }`; `HeaderRole = From | ReplyTo | To | Cc | Bcc`; `ObservationTrust = Ordinary | SpamOrTrash | Deliberate`. `LearningOrigin = ExistingCache | History | LiveArrival | AcceptedSend | Selection`.

`contacts::learn::observe_message(account: &AccountId, message: &MessageRecord, origin: LearningOrigin) -> Vec<Observation>` is pure. `Store::record_observations(ctx: &OperationContext, evidence: &EvidenceId, origin: LearningOrigin, observations: &[Observation]) -> Result<(), StoreError>` and `apply_history_page(ctx, page)` persist observations/checkpoint atomically. Create source contacts, contact emails, correspondence evidence, evidence-address roles, aggregated address/name projection, and per-account learning-state tables with cascade account ownership; evidence survives remote message deletion and is not FK-cascaded from messages.

- [ ] Write RED tests `all_header_roles_and_available_bcc`, `absent_bcc_is_not_invented`, `unsent_draft_has_no_evidence`, `duplicate_page_is_idempotent`, `labels_count_one_message`, `uid_reset_fingerprint_keeps_frequency_bounded`, `domain_normalization_preserves_local_case_dots_and_tags`, `invalid_address_is_skipped_without_losing_page`, `message_eviction_keeps_learned_address`, `accepted_send_and_receipt_reconcile_once`.
- [ ] Run `cargo test -p pige_core --test contact_learning`; expect RED.
- [ ] Implement evidence uniqueness by account + stable native message/operation identity; retain a normalized full-header fingerprint as a secondary dedup hint for IMAP UID epochs/copies and local-send/Sent replay. Never identify mail content solely by Message-ID. Where equivalence is ambiguous, prefer undercounted bounded frequency over invented exact counts. Coalesce roles per message/address; store evidence/role sets rather than increment counters on every upsert. Preserve strongest trusted name provenance and original deliverable address.
- [ ] Wire accepted-send observations into F9 acceptance transaction and live-header observations into F7 apply transaction. Only explicit new-arrival changes train as `LiveArrival`; flag/label replays are not arrivals. `ExistingCache` uses a restart-safe local backfill cursor. Run the binary again and existing outbox/provider_sync tests; expect GREEN.
- [ ] Commit named files: `feat: persist replay-safe correspondent observations`.

### Task R2: Indexed local recipient search and ranking

**Files:** Create `contacts/rank.rs`, `api/contacts.rs`, `tests/recipient_lookup.rs`; modify `store/correspondents.rs`, `api/mod.rs`, `contacts/types.rs`, blocked-sender lookup.

**Interfaces:** `RecipientQuery { sending_account: AccountId, text: String, limit: u16 }`; `RecipientSuggestion { address: Address, sources: Vec<SuggestionSource>, accounts: Vec<AccountId> }`; `SuggestionSource = SavedContact | OtherContact | Outgoing | Incoming | Selected`. `api::suggest_recipients(query: RecipientQuery) -> Result<Vec<RecipientSuggestion>, String>` is local-only. `api::record_recipient_selection(account: &AccountId, address: &Address) -> Result<(), String>` records deliberate choice, not arbitrary typing. Public lookup caps requested limit at 20 and query input at 256 Unicode characters without splitting UTF-8.

- [ ] Write RED tests `suggestions_work_with_network_disabled`, `outgoing_and_selected_rank_above_incoming`, `saved_name_beats_incoming_spoof`, `same_exact_address_combines_sources`, `case_distinct_local_parts_stay_distinct`, `blocked_and_spam_only_are_suppressed`, `self_aliases_are_suppressed`, `active_account_breaks_ties`, `denied_contacts_still_returns_history`, `name_or_address_prefix_matches`, `query_and_limit_are_bounded`.
- [ ] Run `cargo test -p pige_core --test recipient_lookup`; expect RED.
- [ ] Implement indexed token-prefix matching over case-folded name/address search keys, while the returned address remains unchanged. Use a projection/table plus suitable prefix indexes or FTS5, not a full evidence-ledger scan per keypress. Rank lexicographically by deliberate/outgoing tier, saved contacts, Other contacts, incoming-only; then current-account preference, latest meaningful interaction, capped frequency, deterministic address key. Exact match outranks a broad prefix within its tier. Suppress blocked addresses globally; low-trust-only evidence cannot surface unless independently trusted. No remote requests or logging queries.
- [ ] Re-run recipient_lookup; expect GREEN. Add a 100k-address fixture and verify the query plan uses projection/search indexes rather than correspondence evidence scans; final device timing is H2.
- [ ] Commit: `feat: rank local recipient suggestions across contact sources`.

### Task R3: Resumable full-history header learning

**Files:** Create `sync/history.rs`, `tests/history_learning.rs`; modify `sync/scheduler.rs`, `store/jobs.rs`, `contacts/learn.rs`, existing foreground/core background entry points.

**Interfaces:** `history::learn_slice(store: &Store, mailbox: &mut dyn MailboxService, ctx: &OperationContext, budget: WorkBudget) -> Result<LearningProgress, ProviderError>`; `LearningProgress { scanned: u64, total_hint: Option<u64>, complete: bool, paused: bool }`; `api::learning_progress(account: &AccountId) -> Result<LearningProgress, String>`. Default history budget: 100 records/page, at most 5 pages or 5 seconds per background slice (whichever first), at most 2 simultaneous account jobs; foreground can schedule additional slices. A deadline cancels pending work; no body request solely to learn addresses.

- [ ] Write RED tests `learns_beyond_initial_300`, `resume_after_restart_without_duplicate_counts`, `sparse_imap_uids_reach_oldest_message`, `expired_cursor_replays_safely`, `new_arrival_during_crawl_is_not_lost`, `history_does_not_fill_visible_mail_cache`, `body_and_attachment_calls_remain_zero`, `transient_429_preserves_checkpoint_and_retry_after`, `partial_page_does_not_mark_complete`, `high_priority_send_preempts_between_pages`. Use 1,205 synthetic header messages and assert all accessible addresses eventually indexed, not just the first page/window.
- [ ] Run `cargo test -p pige_core --test history_learning`; expect RED.
- [ ] Implement slices and durable progress/checkpoint CAS. Crawl accessible history newest first; adapters own pagination/rebaseline. Apply only complete fetched pages; retry partial fetch failures without committing an advanced cursor. Yield between pages to interactive work; preserve rate-limit delay across restarts. Finish history only on adapter end-of-enumeration; changes arriving during a crawl are caught through live delta plus later bounded reconciliation. Never emit new-mail notifications for history pages.
- [ ] Run history_learning and provider_sync; expect GREEN. With an instrumented fake confirm memory is bounded by page size and incomplete work resumes, not restarts every worker run.
- [ ] Commit: `feat: learn recipients through resumable header history jobs`.

### Task R4: Clear, pause, rebuild and lifecycle privacy

**Files:** Create `tests/contact_lifecycle.rs`; modify `api/contacts.rs`, `contacts/learn.rs`, `store/{contacts,correspondents,jobs}.rs`, `api/accounts.rs`, `settings.rs`, config/backup tests and advanced/settings screen handlers.

**Interfaces:** `api::pause_learning(account: &AccountId, paused: bool) -> Result<(), String>`; `clear_learned_recipients(account: &AccountId) -> Result<(), String>`; `rebuild_learned_recipients(account: &AccountId) -> Result<(), String>`. Clear is account-scoped, removes learned evidence/projection/selections but not synced contacts, disables `ExistingCache`/`History` ingestion and cancels existing history generation. New `AcceptedSend`/`Selection` and adapter-confirmed `LiveArrival` may train; metadata-only replays/bootstrap are not arrivals. Rebuild explicitly starts a new learning epoch and re-enables history. Pause stops all learning ingestion/crawling but does not remove existing suggestions.

- [ ] Write RED tests `clear_then_resync_does_not_restore_history`, `clear_then_new_send_learns`, `stale_crawl_after_clear_cannot_commit`, `pause_preserves_existing_suggestions`, `explicit_rebuild_relearns_history`, `remove_one_account_preserves_other_evidence`, `factory_reset_deletes_contacts_and_jobs`, `config_backup_excludes_contacts_and_learned_addresses`, `synced_contact_survives_clear`.
- [ ] Run `cargo test -p pige_core --test contact_lifecycle`; expect RED.
- [ ] Implement separate account generation and learning epoch fences, persistent pause/rebuild settings and clear semantics. Clearing a cache is logical deletion, not secure forensic erasure. Use cascade ownership for account removal; rebuild and pause must not restore user-blocked senders. Add settings actions with confirmation only for clearing/rebuilding, not an entire address-book screen.
- [ ] Run contact_lifecycle and config/backup tests; expect GREEN.
- [ ] Commit: `feat: control recipient learning and clear account history safely`.

### Task R5: Compose token-aware autocomplete

**Files:** Create `pige/src/components/recipient_suggestions.rs`; modify `components/mod.rs`, `screens/compose.rs`, `message.rs`, `update.rs`, and compose unit tests. Re-read concurrent UI edits before applying any changes.

**Interfaces:** `RecipientRequestKey { generation: u64, account_id: AccountId, field: ComposeField, token_start: usize, token_end: usize, query: String }`; `ComposeState::recipient_request(&self, caret: usize) -> Option<RecipientRequestKey>`; `apply_suggestions(&mut self, key: RecipientRequestKey, suggestions: Vec<RecipientSuggestion>) -> bool`; `accept_suggestion(&mut self, key: &RecipientRequestKey, index: usize) -> Result<Address, String>`. Use UTF-8 byte boundaries consistently with the widget's selection/caret adapter. Messages: `RecipientQueryReady`, `RecipientSuggestionsLoaded`, `RecipientSuggestionChosen`, `RecipientSuggestionDismissed` with request keys.

- [ ] Write RED unit tests `autocomplete_replaces_only_active_token`, `quoted_comma_name_keeps_completed_recipient`, `stale_field_account_or_generation_is_rejected`, `unicode_caret_never_slices_invalid_boundary`, `bcc_suggestions_do_not_modify_to_or_cc`, `escape_dismisses_without_insertion`, `arrows_and_enter_match_touch_choice`, `typing_alone_does_not_record_selection`. Preserve existing parse-address tests.
- [ ] Run `cargo test -p pige --lib screens::compose::tests`; expect RED for missing autocomplete behavior.
- [ ] Add generation-keyed query scheduling on recipient input/caret changes; debounce 100 ms, default 8 results, skip remote network and empty/invalid token queries. Ensure the selected Iced text-input API exposes actual cursor position; if a custom field wrapper is required, keep it scoped to recipient fields and test its caret adapter. Render display name plus actual email and optional source/account hint; no implicit selection on blur. Use existing UI style, keyboard navigation and Android touch targets. Record selection only after explicit acceptance.
- [ ] Re-run compose unit tests and `cargo test --workspace`; expect GREEN. Manually test middle-token edits, semicolons, back navigation and Android soft keyboard with no accidental send/recipient insertion.
- [ ] Commit: `feat: add offline compose recipient autocomplete`.

### Task R6: Generic contact sync and Android maintenance scheduling

**Files:** Create `contacts/sync.rs`, `tests/contact_sync.rs`; modify `api/contacts.rs`, `store/contacts.rs`, `sync/scheduler.rs`, `api/events.rs`, `pige/src/jni.rs`, Android `SyncWorker.kt`/`PigeBridge.kt`, settings progress display.

**Interfaces:** `contacts::sync::sync_slice(store: &Store, service: &mut dyn AddressBookService, source: &ContactSourceId, ctx: &OperationContext, budget: WorkBudget) -> Result<LearningProgress, ProviderError>`; `api::set_contact_sync(account: &AccountId, source: &ContactSourceId, enabled: bool) -> Result<(), String>`. Contact page changes + checkpoint commit atomically. `api::run_maintenance(budget: Duration) -> Result<(), String>` gives interactive mail/outbox priority, then contacts/history slices; background JNI uses this without requiring an activity.

- [ ] Write RED tests `contact_full_sync_sweeps_only_after_last_page`, `contact_tombstone_leaves_history_evidence`, `contacts_forbidden_disables_only_that_source`, `independent_contact_source_works_without_mailbox`, `background_deadline_returns_with_checkpoint`, `worker_cancel_stops_native_slice`, `contacts_are_usable_offline_after_restart`. Fake a two-page snapshot with a failure on page two; prior cache must remain until successful completion.
- [ ] Run `cargo test -p pige_core --test contact_sync`; expect RED.
- [ ] Implement source-scoped cursors, full snapshot generations, tombstones and local projection rebuild. Add bounded native maintenance entry point, WorkManager stop/cancellation bridge and progress snapshots. Keep WorkManager minimum periodic interval 15 minutes; no promise of continuous background sync. Allow foreground opportunistic slices. Network constraints apply to jobs, not local lookup. Disabling source sync cancels jobs, keeps existing local source data unless account/source removal is explicit.
- [ ] Re-run contact_sync and full workspace tests; build Android release and verify cached suggestions headlessly survive a killed/restarted app. No native provider needed: fake-contact tests and an existing IMAP account are the first independent delivery.
- [ ] Commit: `feat: synchronize address books and schedule bounded history learning`.

## Exit gate

- [ ] IMAP header history and accepted sends produce persistent offline compose suggestions.
- [ ] Clear/pause/rebuild/removal and source provenance are observable and tested.
- [ ] Generic contact sync passes fake conformance tests; production Google/Proton adapters plug into it in the next plans.
