# Provider Hardening and Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Verify recoverability, privacy and measured desktop/Android behavior before exposing native providers broadly.
**Architecture:** Reuse conformance suites and failure-injection fixtures, then run explicit live/device tests. Release evidence separates mocked protocol behavior from actual interoperability and registration/distribution prerequisites.
**Tech Stack:** Rust test harness, Python dependency gate, SQLite fixtures, Android instrumentation/ADB, current release scripts.
**Spec:** `docs/superpowers/specs/2026-11-07-mail-providers-and-contacts-design.md`; plan master `2026-11-07-mail-providers.md`.

## Global Constraints

- Do not claim exact Gmail-web suggestion parity, complete remote history before indexing finishes, exactly-once delivery, or Proton-client security parity.
- Never log tokens, keys, message plaintext, contact payloads, or full suggestion queries.
- Clearing data is logical deletion, not guaranteed forensic erasure.
- Build and test the actual Android target before accepting a dependency.
- No release publishing, tagging, credentialed writes or external API test sends without user authorization.

## Review Focus

1. Old installation plus interrupted migration/config switch must reopen without losing outbox or IDs (H1).
2. A 100k-address cache and years of mail must not freeze compose or grow per-keystroke memory without bound (H2).
3. Android worker cancellation/process death must checkpoint progress and not resubmit mail (H1/H3).
4. Disabled provider features and revoked grants must not destroy readable cache or force infinite worker retries (H1/H3).
5. Telemetry/logging/backup/error reports must not disclose sensitive payloads (H3).

## File structure

- New `rust/pige_core/tests/provider_recovery.rs`, `tests/recipient_perf.rs`, `tests/provider_privacy.rs`.
- Shared fake harness refinements under `tests/common/`; optional shell view performance tests in existing app tests.
- New `scripts/verify-providers.sh`, `docs/provider-release-verification.md`, `docs/provider-maintenance.md` (extend F10), `docs/adding-a-mail-provider.md`.
- Update README, Google OAuth setup and Proton support docs; no publishing automation changes required.

### Task H1: Cross-provider conformance and crash/migration recovery

**Interfaces:** A shared `tests/common/provider_contract.rs::run_suite(factory: impl ProviderTestHarness)` drives fake and mocked adapter instances; `ProviderTestHarness` builds an account/session and supplies fixture/event controls without leaking SDK types into app tests. Recovery uses subprocess kill/reopen against synthetic database/config/secure-store fakes. Existing API function signatures remain stable.

- [ ] Write RED tests `v1_to_latest_config_and_v4_cache_recover`, `every_outbox_state_survives_upgrade`, `migration_fault_before_swap_rolls_back`, `switch_fault_between_secret_and_config_recovers`, `worker_kill_after_submission_never_resends`, `two_process_leases_do_not_regress_cursor`, `disabled_provider_keeps_cached_reader_available`, `backend_swap_preserves_drafts_not_uid_mappings`, `revoke_one_account_does_not_block_others`.
- [ ] Run `cargo test -p pige_core --all-features --test provider_recovery`; expect RED until any uncovered integration defects are reproduced. If tests already pass, record GREEN and do not invent product changes merely to force a failure; any actual fix needs its own reproducing RED case.
- [ ] Implement only fixes proven by those tests; exercise failed credential persistence and database-full/locked conditions without deleting user state. Migrations are forward-only: rollback instructions use a pre-upgrade consistent backup, not running an older binary on the new schema. Freeze compatibility expectations for secrets/config/newer-schema handling.
- [ ] Run provider_recovery, all adapter contract binaries and `cargo test --workspace --all-features`; expect GREEN with ignored live tests identified separately.
- [ ] Commit fixes/tests: `test: verify provider migration and crash recovery`.

### Task H2: Performance and bounded indexing measurements

**Interfaces:** `tests/recipient_perf.rs` seeds exactly 100,000 unique addresses with mixed source/provenance and exposes ignored test `recipient_lookup_100k` using the public R2 query. Target: p95 under 50 ms on reference Android device; no claim from desktop-only timing. Header crawl fixture contains at least 1,205 messages and a large generated case exceeding one million sparse UID positions without allocating one million fake messages.

- [ ] Write tests `lookup_uses_bounded_projection`, `history_memory_scales_with_page_not_mailbox`, `cancellation_releases_background_slot`, `index_restart_eventually_reaches_oldest_header`; assert bounded result sizes, page concurrency <= configured 2 account jobs, no body fetch and all fixture pages eventually learned.
- [ ] Run `cargo test -p pige_core --test recipient_perf`; these deterministic assertions must pass before benchmarking; any failure becomes a reproducing test before optimization.
- [ ] Add ignored timed `recipient_lookup_100k` with warmup and >=200 representative name/email prefix queries; record p50/p95/max, device/build/data size, SQLite file size, peak resident memory, indexing rate, battery/network conditions and quota backoff. Run root Android release build and measure on device with an instrumentation/native fixture runner; cross-compiling alone is not timing evidence.
- [ ] Measure cold cached inbox, compose keystrokes during background crawl, body-on-open, APK size and restart progress. Optimize SQL indexes/batches only with failing functional tests or measured regressions; retain full-history target and stable ranking. No silently truncating learned addresses to meet the target.
- [ ] Re-run deterministic tests, desktop timing `cargo test --release -p pige_core --test recipient_perf recipient_lookup_100k -- --ignored --nocapture` and device runner; save raw sanitized results in release evidence. If p95 target misses, report it and stop short of a performance-success claim.
- [ ] Commit tests/measurement docs and justified tuning: `perf: measure and bound recipient lookup and history indexing`.

### Task H3: Privacy, live/on-device matrix and maintenance handoff

**Files:** Create privacy tests, verification script and release/add-provider docs; update existing support/setup/maintenance docs and README.

**Interfaces:** `scripts/verify-providers.sh [default|all]` runs format/tests/clippy and dependency gates without credentials or publishing. Live test binaries are explicit opt-ins, require disposable-account environment and separate `PIGE_TEST_ALLOW_SEND=1` for sending; missing env in an explicitly invoked ignored live test fails clearly. The ordinary default test run never sends mail.

- [ ] Write RED tests `logs_and_debug_omit_secret_sentinels`, `backup_has_no_contact_or_correspondent_data`, `clear_then_process_restart_keeps_history_disabled`, `remove_account_deletes_all_private_projection_rows`, `outbox_artifacts_have_no_session_or_private_key`, `denied_scope_is_not_infinite_background_retry`. Capture error paths with synthetic secret/name/query markers and assert logs/config/backup lack them; never assert real plaintext is encrypted at rest.
- [ ] Run `cargo test -p pige_core --all-features --test provider_privacy`; expect RED for uncovered leaks or missing harness; then implement narrowly scoped fixes and script/docs.
- [ ] Run root `bash scripts/verify-providers.sh all`; implementation runs `cargo fmt --all --check`, `cargo test --workspace --all-features`, `cargo clippy --workspace --all-targets --all-features`, and dependency gate from `rust/` or with explicit manifests. Also verify `cargo check -p pige_core --no-default-features` and each provider feature separately; update lockfile deliberately, never as accidental verification work.
- [ ] Run `cargo build --locked -p pige --bin pige` from `rust/` and root `JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release` (or the build script's detected valid JDK). Confirm release manifest enables intended provider features; feature-disabled accounts retain cached read access with an unavailable-provider notice.
- [ ] With authorization, run Google/Proton/IMAP live tests and actual device checklist: login/verification, rotation, process kill/resume, offline reader/autocomplete, long history progress, contacts denial/deletion, alias/Bcc send inspection, failed signatures, token revocation, worker deadline, clear/rebuild/remove/reset. Record each PASS/FAIL/BLOCKED and prerequisites; no unexecuted row is PASS.
- [ ] Write add-provider recipe: implement only supported capabilities, add private wire normalization/fixtures, register versioned IDs/cursors, demonstrate initial/delta/history recovery and Sent policy, pass shared tests, audit auth/privacy/licenses and target builds. Record how to replace an SDK while preserving mappings or resetting only incompatible cursors; document Proton fork upgrade checks and OAuth release signing/consent upkeep.
- [ ] Commit named files: `docs: document provider verification and extension workflow`. Ask user before tagging/publishing any release.

## Final acceptance

- [ ] User-visible functionality matches approved design and unsupported security/account types are documented.
- [ ] All deterministic tests/builds pass with fresh command evidence; live/device gaps explicitly remain blockers or disclosed limitations approved by the user.
- [ ] Detailed docs show data migration/recovery, credential storage, local plaintext cache limitation, clearing behavior and provider maintenance obligations.
- [ ] Request whole-branch code review before integration; do not equate plan completion with implementation completion.
