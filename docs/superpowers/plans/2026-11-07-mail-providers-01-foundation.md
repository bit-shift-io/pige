# Provider Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate existing IMAP/SMTP onto provider-neutral storage and services without losing user data.
**Architecture:** Introduce contracts before adapters, migrate config/secrets and cache transactionally, then replace API protocol calls while keeping public UI operations stable. Existing protocol transports remain adapter internals.
**Tech Stack:** Rust, async-trait, Tokio, tokio-util CancellationToken, SQLite, async-imap, lettre; no native REST dependencies until feasibility passes.
**Spec:** `docs/superpowers/specs/2026-11-07-mail-providers-and-contacts-design.md`; exact shared DTOs/services in `2026-11-07-mail-providers.md`.

## Global Constraints

- Do not implement REST providers by inventing IMAP UIDs.
- Existing Gmail IMAP accounts do not silently convert during application upgrade.
- Keep provider SDK types, protocol identifiers, crypto, and wire errors inside their adapters.
- Preserve existing accounts, drafts, queued mail, cached reader content, local search, threading, attachment handling, and desktop/Android behavior.
- Never blindly retry submissions or other non-idempotent writes after an ambiguous result.
- Prerequisite: feasibility gates S1-S3 passed, or user approved a documented change of scope.

## Review Focus

1. Duplicate/missing RFC Message-ID must not collapse independent IMAP messages (F4/F5).
2. Migration failure must not leave bodies/attachments/FTS pointing to new or missing local IDs (F4).
3. A send accepted just before process death must become uncertain, not automatically queued (F9).
4. An account removed while refresh is waiting must not be recreated by rotated credentials (F3/F10).
5. A partial remote mutation followed by old sync data must not roll back successful server changes (F7/F8).

## File structure

- New `rust/pige_core/src/providers/{mod,types,error,traits,registry,fake}.rs`: contracts, factory, fake and adapter registration; add `pub mod providers` in `lib.rs`.
- New `providers/imap/{mod,identity,sync,mailbox,mutations,submission}.rs`: existing transport wrappers.
- New `auth/{mod,types,secrets,session}.rs` and `api/accounts.rs`: protected credentials/login lifecycle and backend switch; add `pub mod auth`.
- New `store/{remote_ids,memberships,jobs}.rs`, next free migration files, fixture builder `tests/common/legacy.rs`.
- New `sync/{provider,scheduler}.rs`, `send/submission.rs`, `api/{mailbox,body}.rs`: provider orchestration; extract only touched network responsibilities from large `api/mod.rs`.
- Modify account/config/store/mail definitions, IMAP/SMTP connection helpers, existing API/snapshot/mutation/outbox paths, platform factories, and affected UI account consumers.
- New integration binaries `provider_contract`, `provider_config`, `provider_auth`, `provider_store`, `provider_imap`, `provider_sync`, `provider_mutations`, `provider_outbox`, `provider_lifecycle` under `rust/pige_core/tests/`.

### Task F1: Provider contracts, fake and registry

**Files:** Create `providers/{mod,types,error,traits,registry,fake}.rs`, `tests/provider_contract.rs`; modify `lib.rs`, `account.rs`, `Cargo.toml`, `Cargo.lock`.

**Interfaces:** Produces all shared DTOs/service traits/`ProviderSession`/`ProviderFactory` in the master. `ProviderRegistry::register(id: ProviderId, factory: Arc<dyn ProviderFactory>) -> Result<(), ProviderError>` rejects duplicate IDs; `ProviderRegistry::open` dispatches by `Account::provider_id() -> ProviderId`. Introduce that helper returning `imap-smtp` on the existing account in this task; F2 extends it to typed backends. The registry is application-owned, not a process-global plugin loader.

- [ ] Write RED tests `opens_submission_only_provider` (mailbox/sync None, submission Some), `missing_capability_is_unsupported` (no panic), `duplicate_provider_is_rejected`, `cursor_is_not_interpreted_by_registry`, `cancelled_context_opens_no_network`, `errors_redact_wire_payloads` (secret marker absent from Display/Debug).
- [ ] Run `cargo test -p pige_core --test provider_contract`; expect missing-module/type RED.
- [ ] Implement contracts and scripted `FakeProviderFactory`, `FakeProviderSession` builders with call recording, delayed results, scripted failures and shared recorded submission attempts; use async-trait and `tokio-util` cancellation. Do not expose REST SDKs through these types. Native adapter feature names: `provider-google`, `provider-proton` (off until respective plans).
- [ ] Run the test binary again; expect all tests GREEN, then `cargo check --workspace`.
- [ ] Commit named files: `feat: define provider-neutral mail capabilities`.

### Task F2: Versioned backend account configuration

**Files:** Modify `account.rs`, `config/mod.rs`, `store/accounts.rs`, `account/presets.rs`, `api/mod.rs`, `platform/{linux,android,fake}.rs`, `transport/{imap,smtp}.rs`, affected `pige/src/screens/accounts.rs` and `update.rs`; create `tests/provider_config.rs` and next free account migration.

**Interfaces:** `MailBackend = ImapSmtp { username: String, imap: ServerConfig, smtp: ServerConfig, auth: AuthKind } | Gmail { user_id: String } | Proton { user_id: Option<String> }`; `ContactSourceConfig { id: ContactSourceId, provider: ProviderId, kind: String, enabled: bool }`. `Account` retains identity/label/display/email/order/color and replaces protocol fields with `backend: MailBackend`, `contact_sources: Vec<ContactSourceConfig>`. `Account::provider_id() -> ProviderId`; `Account::imap_smtp() -> Option<(&str, &ServerConfig, &ServerConfig, AuthKind)>`. Legacy `AccountInput` stays accepted as an IMAP-only onboarding wrapper. Config v2 has explicit v1/v2 wire readers, not fake empty endpoints for native accounts.

- [ ] Write RED tests `v1_round_trip_preserves_account_ids_and_endpoints`, `native_account_needs_no_imap_endpoint`, `newer_config_is_rejected_before_save`, `restore_v1_has_no_contacts_or_tokens`, `gmail_imap_stays_imap`, `store_mirror_matches_json_backend`. Use a v1 fixture with custom host/security/user/color and assert exact equality after migration.
- [ ] Run `cargo test -p pige_core --test provider_config`; expect RED for missing backend migration.
- [ ] Implement readers, writer/version checks and SQLite account mirror. Config remains authoritative; parse/validate before writing, use current atomic file writer and pre-restore copy. Update existing callers to match backend; legacy form remains functional and native accounts cannot be edited as fake IMAP accounts. Do not expose native add buttons yet.
- [ ] Run `cargo test -p pige_core --test provider_config`, then `cargo test --workspace`; existing config/credentials/account tests must remain GREEN.
- [ ] Commit named files and any updated constructors: `refactor: version account configuration by backend`.

### Task F3: Protected auth envelopes and generation-safe sessions

**Files:** Create `auth/{mod,types,secrets,session}.rs`, `store/jobs.rs` generation methods, `tests/provider_auth.rs`; modify `lib.rs`, `platform/{mod,linux,android,fake}.rs`, `api/mod.rs`, Android `CredentialStore.kt` only if atomic envelope size/persistence tests require it.

**Interfaces:** `SecretEnvelope { version: u32, credentials: StoredCredentials }`, `StoredCredentials = Password(SecretString) | Google(GoogleSessionSecret) | Proton(ProtonSessionSecret)`; concrete secret fields follow S2/S3 evidence and are confined to auth/adapter code. Use the `secrecy` crate's `SecretString`; pin version in S1. `SecretRepository::load(account: &AccountId) -> Result<Option<SecretEnvelope>, ProviderError>`, `save(ctx: &OperationContext, secret: &SecretEnvelope) -> Result<(), ProviderError>`, `delete(account: &AccountId) -> Result<(), ProviderError>` wrap `CredentialStore`, preserving legacy raw passwords. `StagedSecretRef(String)` is a non-secret reference to a separate protected pending-auth entry; `stage(attempt: &AuthAttemptId, secret: &SecretEnvelope) -> Result<StagedSecretRef, ProviderError>` and `discard_staged(reference: &StagedSecretRef) -> Result<(), ProviderError>` never overwrite active credentials. Expired/cancelled attempts clean up staged entries. `AccountSessionManager::context(account: &AccountId, timeout: Duration) -> Result<OperationContext, ProviderError>`; `invalidate(account: &AccountId) -> Result<(), ProviderError>` cancels old contexts and durably increments generation.

`AuthRequest { provider: ProviderId, account: Option<AccountId>, enable_contacts: bool }`; `AuthProgress = Challenge { attempt: AuthAttemptId, challenge: AuthChallenge } | Complete(AuthenticatedAccount) | Cancelled`; `AuthChallenge = Browser(BrowserChallenge) | Password { username: Option<String> } | Totp | MailboxPassword | HumanVerification(HumanChallenge) | Unsupported { reason: String }`. `AuthAnswer` is a matching redacted enum. Async AuthenticationService methods: `begin(&mut self, request: AuthRequest) -> Result<AuthProgress, ProviderError>`, `respond(&mut self, attempt: &AuthAttemptId, answer: AuthAnswer) -> Result<AuthProgress, ProviderError>`, `cancel(&mut self, attempt: &AuthAttemptId) -> Result<(), ProviderError>`, `resume(&mut self, account: &AccountId) -> Result<AuthProgress, ProviderError>`, `revoke(&mut self, account: &AccountId) -> Result<(), ProviderError>`. `AuthenticatedAccount { account: Account, persistence: SecretPersistence, credential_ref: StagedSecretRef }`; `SecretPersistence = Persistent | SessionOnly`. Browser/Human challenge payloads contain provider-validated URLs, expiry and attempt binding; concrete result fields are frozen by S2/S3, never arbitrary JS instructions. New-account attempts reserve a provisional account ID/generation without creating a visible account or active credentials. `TokenSource: Send + Sync` with async `access_token(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError>` supports synchronized refresh and platform-owned authorization tokens.

- [ ] Write RED tests `legacy_password_migrates_without_loss`, `refresh_is_single_flight`, `removed_account_rejects_rotated_secret`, `session_only_never_writes_sqlite`, `credential_store_failure_is_reported`, `late_attempt_answer_is_rejected`, `auth_debug_redacts_all_secret_variants`. Assert exactly one refresh and no durable write after invalidation.
- [ ] Run `cargo test -p pige_core --test provider_auth`; expect RED.
- [ ] Implement generation record and transactional checks, cancellation, serialized credential mutation, secret-envelope serialization and size validation. Existing host storage may store a JSON envelope as one encrypted string; do not move tokens to `pige.json`. Refresh failures require reauth, not password fallback. Serialize removal and refresh through the same account gate, including cross-process durable lease/fencing; write failures cannot be reported as persisted. Add StoreError variants for stale generation/checkpoint and preserve safe public error mapping. Use secure-store keys `pending_<attempt UUID>` (matching Android's existing `[A-Za-z0-9_-]{1,128}` whitelist) for staged attempts; pending-auth database records contain references, never secret contents.
- [ ] Re-run the auth binary and existing `credentials` tests; expect GREEN. Exercise interrupted host write with a fake store that preserves old value on failure; mark platform durability claims separately for device verification.
- [ ] Commit: `feat: protect provider sessions and cancel stale account work`.

### Task F4: Durable remote mappings and multi-membership migration

**Files:** Create `store/{remote_ids,memberships}.rs`, next free message migration, `tests/provider_store.rs`, `tests/common/legacy.rs`; modify `store/{migrations,mod,messages,folders,bodies,attachments,search}.rs`, `mail/{mod,message}.rs`, `lib.rs`, existing fixture constructors/tests.

**Interfaces:** `Store::remote_message(account: &AccountId, id: &MessageId) -> Result<RemoteMessageId, StoreError>`; `message_memberships(id: &MessageId) -> Result<Vec<FolderId>, StoreError>`; `Store::apply_change_page(ctx: &OperationContext, key: &JobKey, page: &ChangePage) -> Result<(), StoreError>`. Store `messages` content fields use local IDs; move folder membership to `message_collections(message_id, folder_id)`; `remote_messages(account_id, backend_key, remote_id, message_id)` and `remote_collections(...)` enforce scoped uniqueness. IMAP remote_id is a versioned serialized `(mailbox, uidvalidity, uid)` tuple, never colon-splitting arbitrary names. `StoredMessage` drops mandatory UID fields; any folder context supplied to UI is an explicit query projection, not content identity. Body/attachment/FTS references retain their exact MessageId.

- [ ] Build synthetic real v4 SQLite fixtures from the old schema including duplicate/missing RFC IDs, bodies, attachments, FTS, folders, all outbox states. Write RED tests `v4_ids_and_dependents_survive`, `same_remote_id_across_accounts_is_distinct`, `two_labels_share_one_native_message`, `same_rfc_id_is_not_identity`, `migration_rollback_reopens_v4`, `newer_schema_rejected`, `membership_removal_keeps_body`, `page_and_cursor_commit_atomically`.
- [ ] Run `cargo test -p pige_core --test provider_store`; expect RED.
- [ ] Implement additive mapping tables first, then rebuild the message table with mandatory old folder/UID constraints removed. If SQLite requires foreign-key checking disabled, do so only on a dedicated migration connection before the transaction, never on ordinary store operations; restore/check it after commit/rollback. Recreate dependent FTS triggers and indexes explicitly. Use fault injection before table swap/version update; run `PRAGMA foreign_key_check` and `integrity_check` in tests. Rollback must restore the old schema intact. Allocate migrations from the current execution-time version, not a hard-coded v5 assumption.
- [ ] Re-run provider_store, `cargo test -p pige_core --test store --test search --test threading --test attachments`; expect GREEN with exact old IDs and FTS results preserved.
- [ ] Commit: `refactor: separate remote message identity from collections`.

### Task F5: IMAP mailbox adapter and header-only enumeration

**Files:** Create `providers/imap/{mod,identity,mailbox}.rs`, `tests/provider_imap.rs`; modify `transport/{mod,imap,fake}.rs`, `mail/{message,parse}.rs`, affected `tests` header literals.

**Interfaces:** `ImapProviderFactory` consumes existing low-level `TransportFactory` and auth repository, produces `ProviderSession`. Add protocol-only `MailTransport::list_uids(&mut self) -> Result<Vec<Uid>, TransportError>` or a bounded paged equivalent with identical sparse-UID semantics; `fetch_learning_headers(&mut self, uids: &[Uid]) -> Result<Vec<RemoteMessage>, TransportError>`. `ImapMailbox` implements MailboxService and calls SELECT internally; `MessageHeader` gains `#[serde(default)] reply_to: Vec<Address>` and `bcc: Vec<Address>`. Use current MIME parsing for `BodyContent`, including optional raw bytes.

- [ ] Write RED tests `explicit_identity_selects_correct_mailbox`, `uid_epoch_is_part_of_identity`, `sparse_uids_do_not_truncate_history`, `reply_to_and_available_bcc_are_parsed`, `header_history_never_fetches_body`, `raw_body_uses_existing_sanitizer_pipeline`. Assert IDs for mailbox names containing colon/Unicode round-trip, all sparse UIDs `[1, 900, 50000]` are learned, and zero body commands for header enumeration.
- [ ] Run `cargo test -p pige_core --test provider_imap`; expect RED.
- [ ] Implement explicit identity mapping and bounded UID fetch batches; preserve modified-UTF7, TLS validation and attachment/body limits. History cursor records mailbox traversal/epoch plus UID boundary and cumulative progress; derive newest-first ordering from available UID metadata, not dense UID assumptions. Do not generate fake IDs for native APIs.
- [ ] Run provider_imap, `imap_folders`, `sync`, and `attachments` binaries; expect GREEN.
- [ ] Commit: `feat: wrap IMAP mailbox access in provider services`.

### Task F6: IMAP sync and semantic mutations

**Files:** Create `providers/imap/{sync,mutations}.rs`; modify `providers/imap/mod.rs`, `sync/engine.rs`, `tests/provider_imap.rs` and `tests/mutations.rs` protocol fixtures.

**Interfaces:** `ImapSync` implements SyncService and encodes existing UIDVALIDITY/high-water/reconciliation states inside ProviderCursor; `ImapMutations` implements MutationService using explicit remote IDs/source. Preserve Gmail IMAP label behavior in this adapter; `Capabilities.gmail` is no longer application-level policy.

- [ ] Write RED tests `epoch_reset_reconciles_only_affected_mailbox`, `imap_delete_is_not_api_label_removal`, `gmail_imap_archive_removes_source_label_only`, `copy_delete_fallback_reports_partial_outcome`, `initial_window_is_not_full_snapshot`, `folder_rename_does_not_merge_unrelated_uid_identity`. Assert no deletion of older cache merely because a 300-message window was fetched.
- [ ] Run `cargo test -p pige_core --test provider_imap`; expect RED.
- [ ] Implement per-mailbox cursors and protocol fallbacks, using existing engine algorithms where correct; retain Other-folder on-demand sync. If rename identity cannot be established reliably, retire old remote mapping and resync rather than matching UIDs by name heuristics.
- [ ] Run provider_imap, existing `sync` and `mutations`; expect GREEN.
- [ ] Commit: `feat: encapsulate IMAP sync and mutation semantics`.

### Task F7: Transactional provider sync and serialized scheduling

**Files:** Create `sync/{provider,scheduler}.rs`, `tests/provider_sync.rs`; modify `store/jobs.rs`, `sync/mod.rs`, `api/{mod,events,snapshot}.rs`.

**Interfaces:** `sync::provider::sync_account(store: &Store, session: &mut ProviderSession, ctx: &OperationContext, budget: WorkBudget) -> Result<SyncReport, SyncError>`; `WorkBudget { page_size: u32, max_pages: u32, deadline: Instant }`. `AccountScheduler::run_interactive` and `run_background` serialize account mutations and sync across the whole remote-read/apply sequence; interactive waits are bounded by context cancellation/deadline. Durable checkpoint writes use expected generation/previous checkpoint compare-and-swap so separate processes cannot regress state. `Store::job_checkpoint(key: &JobKey) -> Result<Option<ProviderCursor>, StoreError>`; `SyncReport` preserves notifications and per-account errors.

- [ ] Write RED tests `crash_before_page_commit_replays_safely`, `invalid_cursor_starts_scoped_reconciliation`, `bootstrap_anchor_replays_arrival`, `late_page_after_mutation_cannot_revert_flags`, `partial_window_does_not_sweep_older_messages`, `stale_checkpoint_cas_rejected`, `removed_account_page_is_discarded`. Use delayed fake requests to control ordering.
- [ ] Run `cargo test -p pige_core --test provider_sync`; expect RED.
- [ ] Implement apply transaction, job checkpoints and account coordination with leases/generation/CAS for background workers. Never hold a SQLite/API mutex over await. Page deadlines bound how long background work occupies the account gate; yield between pages. Notifications exclude historic crawl/backfill and initial import.
- [ ] Run provider_sync and `cargo test -p pige_core --test api`; expect GREEN.
- [ ] Commit: `feat: synchronize provider changes with durable checkpoints`.

### Task F8: Replace direct API mailbox and mutation calls

**Files:** Create `api/{mailbox,body}.rs`, `tests/provider_mutations.rs`; modify `api/{mod,mutations,snapshot}.rs`, `platform/{mod,linux,android,fake}.rs`, `pige/src/platform.rs`.

**Interfaces:** Public `api::{sync_all,sync_account,sync_folder,load_older,open_conversation,open_attachment,prefetch_bodies,archive_thread,move_thread,trash_thread,delete_permanently,set_read}` retain call signatures where possible; `api::Config` injects `providers: Arc<dyn ProviderFactory>` in place of protocol factory. Existing protocol factory remains only inside ImapProviderFactory. Add `api::archive_available(capabilities: &ProviderCapabilities, current: FolderKind, has_archive: bool) -> bool`. Snapshot account capabilities are provider-neutral.

- [ ] Write RED tests `mixed_accounts_open_without_protocol_branch`, `label_memberships_count_distinct_messages`, `body_fetch_preserves_signature_status`, `unsupported_delete_is_not_attempted`, `partial_mutation_reconciles_only_uncertain_items`, `attachment_fetch_uses_remote_descriptor`, `offline_move_retains_reconnect_error`. Verify stale pages cannot reverse a completed mutation using F7 scheduler.
- [ ] Run `cargo test -p pige_core --test provider_mutations`; expect RED.
- [ ] Extract touched network orchestration and use provider services; route normal bodies and Proton-normalized bodies through the same sanitizer/limits. Persist signature status and remote attachment references in next free additive migration. Keep UI FTS/thread IDs stable. Remove provider-name branches from API/snapshot code; existing `transport` modules remain protocol internals.
- [ ] Run provider_mutations and `cargo test --workspace`; expect GREEN. Search API modules for `.imap(`, `.smtp(`, `UidRange`, `capabilities.gmail`; expected no remaining application protocol calls outside compatibility/deprecation comments/tests.
- [ ] Commit: `refactor: route mail operations through provider sessions`.

### Task F9: Provider-independent durable submission

**Files:** Create `send/submission.rs`, `providers/imap/submission.rs`, `tests/provider_outbox.rs`; modify `send/{mod,outbox,compose}.rs`, `api/mod.rs`, next free outbox migration, `pige/src/screens` state-label consumers as needed.

**Interfaces:** `SubmissionState = Draft | Queued | Preparing | Submitting | Accepted | Complete | Rejected | Uncertain`; async `send::flush_provider_outbox(store: &Store, account: &Account, service: &mut dyn SubmissionService, ctx: &OperationContext) -> Result<OutboxReport, StoreError>`. Persist operation ID, backend identity, prepared artifact, receipt, retry/error metadata separately. Add `outbox_parts` with stable part IDs/metadata/content BLOBs and cascade on outbox deletion for master `SubmissionAttachment` parts; legacy/plain-text rows have zero parts. Enforce existing attachment cap per part and 25 MiB total attachment bytes per submission before persistence/prepare; provider limits can be stricter and return a safe pre-delivery rejection. The existing send_mail wrapper passes no parts; an internal `queue_submission(store: &Store, message: OutgoingMessage, attachments: Vec<SubmissionAttachment>) -> Result<OutboxId, StoreError>` atomically persists both. Adapter `finalize_sent` is idempotent only to the degree evidenced: never retry an ambiguous IMAP APPEND blindly; reconcile or mark uncertain repair without resending delivery.

- [ ] Write RED tests `server_managed_sent_never_appends`, `smtp_accepted_retries_only_sent_copy`, `killed_submitting_becomes_uncertain`, `killed_preparing_reconciles_draft_before_repeat`, `timeout_after_acceptance_never_auto_resends`, `v4_sending_is_uncertain`, `v4_sent_local_keeps_original_backend`, `accepted_learns_once`, `bcc_envelope_is_preserved`, `prepared_payload_has_no_auth_secret`, `attachment_parts_survive_outbox_restart`, `attachment_limits_reject_before_delivery`, `empty_parts_preserve_plaintext_compose`. Legacy Failed with unknown cause maps to Uncertain, not known rejection.
- [ ] Run `cargo test -p pige_core --test provider_outbox`; expect RED.
- [ ] Implement state transitions/atomic claim and restart recovery. Native prepare may create remote draft; interrupted preparation consults stable operation evidence before repeating. SMTP preserves exact MIME and envelope; extend MIME construction with optional structured parts without changing the existing `build_mime` no-part wrapper. State acceptance and future learning hook share a transaction; until R1 exists keep the hook inert behind an explicit observation callback. Server-managed finalize is no-op; preserve existing local draft API wrappers and report behavior.
- [ ] Run provider_outbox, `outbox`, `compose`, and `cargo test --workspace`; expect GREEN.
- [ ] Commit: `feat: generalize durable outbox submission and reconciliation`.

### Task F10: Account lifecycle, backend switch guard and baseline gate

**Files:** Create `api/accounts.rs`, `tests/provider_lifecycle.rs`; modify `api/mod.rs`, `auth/session.rs`, config restore/reset paths, platform construction and relevant account UI; document migration behavior in `docs/provider-maintenance.md`.

**Interfaces:** `api::can_switch_backend(account: &AccountId) -> Result<(), String>` checks queued/preparing/submitting/accepted/uncertain and ambiguous legacy failures. Async `api::switch_backend(account: &AccountId, authenticated: AuthenticatedAccount) -> Result<ModelSnapshot, String>` is recoverable at config boundary; async `api::begin_auth(request: AuthRequest) -> Result<AuthProgress, String>`, `answer_auth(attempt: &AuthAttemptId, answer: AuthAnswer) -> Result<AuthProgress, String>`, `cancel_auth(attempt: &AuthAttemptId) -> Result<(), String>` wrap F3. `api::finish_auth(authenticated: AuthenticatedAccount) -> Result<(AccountId, ModelSnapshot), String>` commits a newly authenticated account; existing-account completion uses guarded switch or reconnect logic instead. Switching drains/cancels workers, preserves AccountId/drafts/learned history, clears only provider remote cache/checkpoints and stores a recoverable switch intent before activating staged credentials/config. Startup either completes or rolls back the intent; never leaves a new backend with old credentials. Contact-source change clears only replaced source cache.

- [ ] Write RED tests `switch_blocked_for_each_pending_state`, `draft_survives_switch`, `switch_crash_recovers_config_and_secret_pair`, `restore_cancels_late_sync`, `remove_cancels_refresh_and_crawl`, `same_account_edit_rejects_stale_generation`, `factory_reset_leaves_no_job_or_secret`, `imap_account_still_sends_after_upgrade`.
- [ ] Run `cargo test -p pige_core --test provider_lifecycle`; expect RED.
- [ ] Implement generation-safe lifecycle with removal tombstones and switch intent containing references/non-secret config only, never clear old secrets before recovery path exists. Restore cancels current generations before rebuilding account mirror. Revoke sessions best effort; local cleanup still happens and errors are shown. Native account actions remain gated until provider plans enable them.
- [ ] Run provider_lifecycle, then full format/test/clippy/desktop and Android release commands from master. Verify one ordinary IMAP account and SMTP send with explicit live authorization; record unavailable prerequisites rather than claiming success.
- [ ] Commit: `refactor: finish provider foundation with safe account lifecycle`.

## Exit gate

- [ ] Existing protocol behavior works through provider sessions; no native provider implementation required for this deliverable.
- [ ] All old fixtures migrate, local IDs persist, body/search/threading/outbox regression tests pass.
- [ ] Removed/edited/restored accounts cannot be resurrected by in-flight work.
- [ ] Contract signatures in the master match implemented APIs before another subplan starts.
