# Native Proton Mail Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver direct Proton Mail on Android/Linux with protected native sessions, safe encrypted delivery, contacts and historical autocomplete.
**Architecture:** A narrowly maintained S3-approved Rust library layer owns Proton protocol/crypto inside `providers/proton`; Pige keeps all cache, secrets, jobs and outbox ownership. Fail closed when recipient security policy or authentication cannot be honored.
**Tech Stack:** S3-approved extracted/adapted proton-core or narrow REST adapter, official MIT SRP/crypto with Rust backend, existing provider/contact/cache services.
**Spec:** `docs/superpowers/specs/2026-11-07-mail-providers-and-contacts-design.md`; exact contracts in `2026-11-07-mail-providers.md`.

## Global Constraints

- Proton must work directly on Android: no Bridge, Go service, or hosted mail proxy.
- Keep the project distributable under its existing MIT licensing direction.
- Never silently downgrade to plaintext.
- No impersonating official Proton clients.
- Do not claim Proton-client security parity.
- Full encrypted contact fields and editing are deferred, except security-critical recipient policy required for safe sending.
- Prerequisites: S3 native/device/security gate, F1-F10 and R1-R6. If extracting a safe maintained Rust layer is not practical, stop and revise scope with the user.

## Review Focus

1. Two-password/multiple-address-key accounts must unlock the correct data, not silently fall back to only the primary key (P1/P3).
2. Invalid signature, unsupported encryption format, or untrusted required key must not appear as verified plaintext (P3/P4).
3. An external recipient requiring encryption must never receive ordinary cleartext because key lookup/contact parsing failed (P4).
4. Draft creation can succeed before a lost response; recovery must not create duplicate draft/send attempts blindly (P4).
5. Event reset/pagination churn and CAPTCHA must behave on Android, not only through a CLI helper (P1/P2).

## File structure

- New core `providers/proton/{mod,client,auth,keys,normalize,mailbox,sync,contacts,policy,submission}.rs`.
- S3 chooses a pinned upstream fork/revision with its own patch series, not an untracked copy of a repository. If vendoring is required, use `rust/vendor/proton-mail-api/` with license/provenance/update instructions and no CLI/cache/keyring packages; add it to the workspace only when needed.
- New `auth/proton.rs`; Android `ProtonVerification.kt` only for supported S3 human-verification flow; Rust/host auth callbacks as needed.
- Core integration tests `proton_auth`, `proton_mailbox`, `proton_crypto`, `proton_submission`, `live_proton` and synthetic/licensed fixture corpus under `tests/fixtures/proton/`.
- New `docs/proton-support.md`, `docs/proton-dependency-updates.md`; modify Cargo manifests/lock/features, app factory/onboarding/security-state rendering.

### Task P1: Pinned library integration and native authentication

**Files:** Create proton `client.rs`/`auth.rs`/`keys.rs`, `auth/proton.rs`, `tests/proton_auth.rs`; add S3 dependency/feature and host verification bridge; register Proton auth provider.

**Interfaces:** `ProtonAuthentication` implements AuthenticationService; `ProtonClient::new(tokens: Arc<dyn TokenSource>, keys: Arc<UnlockedKeyStore>, endpoints: ProtonEndpoints) -> Self`; `UnlockedKeyStore::lock(&self)` wipes/releases unlocked material and invalidates dependent operations. `ProtonProviderFactory` creates services sharing account session/key state; `ProtonEndpoints` is adapter-private with configurable mock URL only for tests/approved deployments. Protected secret fields and honest app version/headers are exactly those accepted in S3.

- [ ] Write RED tests `invalid_server_proof_or_modulus_rejected`, `totp_challenge_requires_same_attempt`, `two_password_login_unlocks_mailbox`, `secondary_address_keys_are_available`, `captcha_can_resume_bound_attempt`, `fido_only_returns_unsupported`, `refresh_rotation_survives_restart`, `logout_locks_keys_and_cancels_work`, `late_refresh_after_removal_cannot_save`, `secrets_never_enter_debug_or_cache`.
- [ ] Run `cargo test -p pige_core --features provider-proton --test proton_auth`; expect RED.
- [ ] Implement S3-tested SRP/login/challenge state machine with typed UI challenges, existing OS secret store, single-flight refresh and protected key-passphrase material. No additional SQLite/keyring owned by Proton library. Keep CPU-heavy unlock/crypto off the UI executor via bounded blocking work and cancel/release results after account invalidation. Unsupported challenge mode is actionable, never bypassed.
- [ ] Re-run proton_auth; expect GREEN. Run S1 dependency gate to reject Go features/extra native SQLite/AGPL additions; build APK and verify two-factor/human verification/cold resume on actual device using the accepted flow.
- [ ] Commit named files: `feat: authenticate native Proton sessions and protect key state`.

### Task P2: Mail metadata, collections, event sync and history

**Files:** Create proton `normalize.rs`, `mailbox.rs`, `sync.rs`, `mod.rs`, `tests/proton_mailbox.rs`, fixtures; register provider feature.

**Interfaces:** `ProtonMailbox`, `ProtonSync`, `ProtonMutations` implement shared services (`ProtonMutations` may live in `sync.rs` initially, split into `mutations.rs` when substantial). Private `ProtonCursor` stages bootstrap anchor, snapshot page, event page, completed event ID and reset/reconciliation sweep. Account-wide message/label/conversation IDs are opaque strings. Mailbox.history returns normalized metadata without decrypting bodies just to learn correspondents.

- [ ] Write RED tests `proton_ids_are_not_numeric_uids`, `folder_and_labels_share_message_content`, `bootstrap_replays_event_anchor`, `more_events_page_commits_only_complete_cursor`, `event_reset_resnapshots_without_losing_cache`, `message_deleted_mid_fetch_is_tombstone`, `live_added_message_is_not_historical_notification`, `history_reaches_oldest_page`, `paging_churn_reconciles_without_skipping_addresses`, `mutation_partial_results_preserve_successes`.
- [ ] Run `cargo test -p pige_core --features provider-proton --test proton_mailbox`; expect RED.
- [ ] Implement S3-verified endpoints and events/reset semantics with bounded page fetches and normalized full records. Keep per-message flags/membership clear; server deletion and label removal differ. Verify actual Proton pagination/event behavior from fixtures/live evidence rather than inventing Gmail history semantics. Offset-based unstable listing requires overlapping/repeated reconciliation with dedup and an event anchor; never mark historical coverage complete on a failed/partial page.
- [ ] Re-run proton_mailbox and provider_sync/history_learning/provider_mutations; expect GREEN. Confirm metadata crawling never invokes body-decrypt/download APIs.
- [ ] Commit: `feat: synchronize native Proton messages and historical metadata`.

### Task P3: Normalized decrypted reading, attachments and contact sync

**Files:** Implement proton `keys.rs`/`mailbox.rs` body methods, create `contacts.rs`, `tests/proton_crypto.rs` and fixtures; modify reader/snapshot display only for signature/unsupported states already introduced in F8.

**Interfaces:** `ProtonAddressBook` implements AddressBookService for `proton.contacts`; only names/emails public contact DTOs. `ProtonMailbox::body` returns BodyContent, not fabricated original RFC822; `attachment` resolves tested remote packet/key references and returns checked plaintext bytes. `SignatureStatus` exactly follows master: Unsupported/Invalid/Unverified are distinct from Verified. Contact encrypted policy remains adapter-private for P4 rather than discarded by read-only DTO projection.

- [ ] Write RED tests `decrypt_internal_and_external_supported_formats`, `unsigned_is_not_verified`, `bad_signature_is_invalid`, `unknown_format_is_unsupported`, `multiple_keys_can_decrypt_old_mail`, `attachment_packet_integrity_failure_returns_error`, `oversize_plaintext_is_rejected_before_cache_growth`, `html_still_passes_sanitizer`, `contact_names_and_emails_sync_offline`, `contact_delete_does_not_erase_learned_address`.
- [ ] Run `cargo test -p pige_core --features provider-proton --test proton_crypto`; expect RED.
- [ ] Implement S3-supported crypto format matrix through reusable primitives; preserve raw only if genuinely available. Bound encrypted input, decrypted output and decompression to current body/attachment caps. Return clear reader errors/security verdicts, not empty successful bodies for unsupported formats. Preserve attachment Content-ID/disposition and existing remote-image policy. Sync contacts with R6 checkpoints; if only full snapshots are available advertise incremental_sync=false.
- [ ] Re-run proton_crypto, attachments, contact_sync, recipient_lookup; expect GREEN. Verify decrypted payload never leaks into logs/config or OS-purgeable cache as the sole source of learned addresses.
- [ ] Commit: `feat: read Proton encrypted mail and sync contact metadata`.

### Task P4: Recipient policy, encrypted submission and reconciliation

**Files:** Create proton `policy.rs`, `submission.rs`, `tests/proton_submission.rs`, fixtures; modify crypto dependency wrappers as required by S3 and factory submission wiring.

**Interfaces:** `DeliverySecurity = ProtonEncrypted | ExternalOpenPgp | ExternalOrdinary`; `RecipientDelivery { address: Address, security: DeliverySecurity }`; `ProtonRecipientPolicy::resolve(message: &OutgoingMessage, ctx: &OperationContext) -> Result<Vec<RecipientDelivery>, ProviderError>` also retains verified key/policy material privately, not in public DTOs/outbox. `ProtonSubmission` implements SubmissionService with ServerManaged Sent policy; durable preparation contains draft/reference/operation metadata only, no unlocked keys. A distinct safe error reports unsupported mandatory encryption/key-verification policy.

- [ ] Write RED tests `internal_recipients_are_encrypted`, `external_required_pgp_is_honored`, `unavailable_required_key_blocks_all_submission`, `encrypted_contact_policy_cannot_be_ignored`, `lookup_failure_never_means_external_cleartext`, `ordinary_external_is_explicitly_classified`, `mixed_to_cc_bcc_packages_do_not_leak_bcc`, `wrong_sender_alias_is_rejected`, `attachment_parts_are_encrypted_and_recoverable`, `draft_created_before_timeout_is_reconciled`, `send_timeout_is_uncertain_not_retry`, `draft_reference_is_not_delivery_receipt`, `sent_is_not_appended`, `accepted_receipt_trains_only_once`.
- [ ] Run `cargo test -p pige_core --features provider-proton --test proton_submission`; expect RED.
- [ ] Implement policy resolution before any delivery attempt, recipient-key trust rules, fresh content session keys and Proton-compatible sign/encrypt/package handling through S3-approved library functions. Reject unsupported required policy for the whole unsent operation, preserving draft for correction. Ordinary external delivery is valid only after affirmative policy resolution, never catch-all error fallback. Store immutable logical content/remote draft identity before submit, handle abandoned preparation without blind duplicate creation and never infer delivered from draft presence.
- [ ] Re-run proton_submission/provider_outbox/contact_learning; expect GREEN. Authorized live cases must inspect controlled internal, PGP external, ordinary external and Bcc recipient outcomes plus server Sent state. Record unsupported cases as unsupported, not passing.
- [ ] Commit: `feat: submit Proton mail with enforced recipient encryption policy`.

### Task P5: Proton account UX, limitations and live/device gate

**Files:** Modify account screens/messages/update/platform construction/JNI as needed; create `tests/live_proton.rs`, `docs/proton-support.md`, `docs/proton-dependency-updates.md`; add auth/reader integration assertions.

**Interfaces:** F10 auth APIs drive Proton sign-in/reconnect/TOTP/mailbox password/verification. Reader exposes normalized security status; compose shows external ordinary vs encrypted delivery classification and actionable blocked-policy error without claiming full Proton-client parity. Factory exposes only S3-proven supported capabilities.

- [ ] Write RED tests `proton_setup_has_no_imap_or_oauth_promise`, `cancel_verification_stores_no_account`, `unsupported_mode_is_actionable`, `mailbox_locked_prompts_unlock_not_empty_inbox`, `invalid_signature_status_is_visible`, `external_ordinary_warning_is_not_encryption_badge`, `config_restore_requires_secret_reauth`, `removed_account_late_login_is_discarded`.
- [ ] Run proton_auth/proton_crypto and account/reader unit test filters; expect new assertions RED.
- [ ] Wire UI states with redacted input handling; explain app-private decrypted cache (no new encryption at rest), Key Transparency/FIDO2/format limitations and third-party API stability. Document upstream source hashes, maintained patches, crypto feature rules, upgrade fixtures/live checks, and rollback on upstream breakage. Never expose raw server challenge JSON to users/logs.
- [ ] Re-run all Proton/core/workspace tests, dependency gate, desktop build and APK build. With explicit disposable-account authorization run ignored live_proton tests and on-device cold login/resume, background delta, offline autocomplete, attachment open, signature failures, contacts, send and logout/removal. Missing prerequisites are BLOCKED evidence, not success.
- [ ] Commit: `feat: expose native Proton accounts with documented security limits`.

## Exit gate

- [ ] No Bridge/Go/local proxy and no vendor-owned duplicate credential/cache layer.
- [ ] Supported Proton operations work on Android and Linux with honest client identity.
- [ ] Security gaps are documented and unsupported encryption policies fail closed.
- [ ] Exact dependency and fork maintenance procedure are committed; not just an opaque pinned archive.
