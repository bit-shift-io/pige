# Gmail and Google People Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship native OAuth Gmail with saved/Other contacts and full-history recipient learning on Android and Linux.
**Architecture:** Share an account token source across independent Gmail and People adapters. Normalize Gmail IDs, label memberships and history into the established Pige contracts; no SDK types escape `providers/google`.
**Tech Stack:** S1/S2-approved generated Google clients (initial recommendation google-gmail1), platform-supported Google authorization, Tokio, existing cache/contact/outbox services.
**Spec:** `docs/superpowers/specs/2026-11-07-mail-providers-and-contacts-design.md`; exact contracts in `2026-11-07-mail-providers.md`.

## Global Constraints

- Existing Gmail IMAP accounts do not silently convert during application upgrade.
- Do not implement REST providers by inventing IMAP UIDs.
- Request Google contacts permissions only when enabling that capability.
- Lack of contacts consent does not stop mail or local history autocomplete.
- No Gmail web scraping, authentication bypass, or impersonating official clients.
- Prerequisites: S1/S2 evidence, foundation F1-F10, and recipients R1-R6. No production Android auth shortcut replaces S2.

## Review Focus

1. A history cursor can expire mid-sync; preserve cache and reconcile without losing arrivals or deleting a partial snapshot (G2).
2. Denied/revoked contacts scope must not break mail, retry auth forever, or expose stale removed-account results (G1/G3).
3. Bcc delivery and alias selection must survive MIME generation without leaking Bcc to unrelated recipients (G4).
4. Gmail labels are memberships, not independent duplicate message bodies (G2).
5. Gmail modify scope cannot permanently delete mail; do not present a nonfunctional destructive action (G2/G5).

## File structure

- New core `providers/google/{mod,client,auth,normalize,mailbox,sync,mutations,submission,people}.rs`.
- New `auth/google_desktop.rs` and `auth/google_android.rs`: host flow wrappers selected by S2.
- Modify core/app manifests and lockfile only for S1/S2-approved exact dependencies and features; feature `provider-google`.
- New Android `GoogleAuthorization.kt` and lifecycle callbacks in `MainActivity.kt`/`PigeBridge.kt`; conditional Gradle/manifest additions follow S2 decision.
- New core tests `google_auth`, `google_mailbox`, `google_people`, `google_submission`, `live_google`; synthetic `tests/fixtures/google/` response/header fixtures.
- New docs `docs/google-oauth-setup.md`; modify account settings/onboarding and app/platform construction.

### Task G1: Google tokens and supported platform login

**Files:** Create google `client.rs`/`auth.rs`, `auth/google_{desktop,android}.rs`, `tests/google_auth.rs`, `GoogleAuthorization.kt`; modify Android host bridge/manifest/Gradle, Rust JNI bridge and auth registry registration.

**Interfaces:** `GoogleAuthentication` implements AuthenticationService from F3. `GoogleClient::new(tokens: Arc<dyn TokenSource>, endpoints: GoogleEndpoints) -> Self`; `GoogleEndpoints { gmail_base: Url, people_base: Url }` is adapter-internal and supports local mock endpoints only in tests. `GoogleProviderFactory::new(auth: Arc<AccountSessionManager>, endpoints: GoogleEndpoints) -> Self`; factory opens independent services sharing authenticated state. JNI callback payload is `GoogleAuthorizationResult { attempt: AuthAttemptId, result: platform-specific validated result }`; freeze concrete token/code fields in S2 rather than accepting arbitrary unvalidated browser parameters.

- [ ] Write RED tests `desktop_pkce_state_mismatch_rejected`, `cancelled_attempt_cannot_store_secret`, `android_result_after_activity_recreation_is_bound`, `google_tokens_survive_restart`, `refresh_rotation_saved_before_reuse`, `refresh_failure_reauths_without_password_prompt`, `denied_contacts_keeps_mail_scope`, `expired_attempt_callback_rejected`, `sensitive_http_errors_are_redacted`.
- [ ] Run `cargo test -p pige_core --features provider-google --test google_auth`; expect RED for missing feature/implementation.
- [ ] Implement S2-approved flow: system browser + PKCE/state on desktop; supported Android host mechanism with no deprecated redirects. Platform-owned token flows remain behind TokenSource rather than forcing exportable refresh tokens. Serialize refresh, save rotating credentials through F3, and bind callback/attempt/account generation. Main worker never launches a login activity; return reauthentication-required.
- [ ] Re-run google_auth; expect GREEN. Build APK and manually verify supported device sign-in/cancel/rotation/cold resume; record real failures rather than replacing the flow silently.
- [ ] Commit named files: `feat: authorize Google mail with supported platform flows`.

### Task G2: Gmail mailbox, history and semantic mutations

**Files:** Create google `normalize.rs`, `mailbox.rs`, `sync.rs`, `mutations.rs`, `mod.rs`; create `tests/google_mailbox.rs` and fixtures; modify provider registry/Cargo feature wiring.

**Interfaces:** `GmailMailbox`, `GmailSync`, `GmailMutations` implement existing traits. Use Gmail message IDs as opaque strings, label IDs as collection IDs, optional threadId as mapping only. `GmailCursor` (private Serde enum) stages `Bootstrap { anchor, collection, page_token, sweep }`, `CatchUp { anchor, page_token }`, `Delta { history_id, page_token }`, `Reconcile { anchor, sweep, page_token }`. History crawl uses a separate cursor and account-wide message listing. UpsertMessage marks `is_new_arrival` only for a genuine messagesAdded delta outside initial/historical bootstrap.

- [ ] Write RED tests `two_labels_share_one_message_and_body`, `bootstrap_replays_anchor_changes`, `history_404_reconciles_without_emptying_cache`, `history_pages_do_not_skip_added_then_deleted_message`, `deleted_get_404_during_page_is_tombstone`, `partial_metadata_batch_does_not_advance_cursor`, `user_label_rename_preserves_collection_id`, `archive_removes_inbox_not_unrelated_labels`, `trash_is_not_permanent_delete`, `modify_scope_disables_permanent_delete`, `all_history_not_limited_to_inbox`, `other_folder_on_demand_is_supported`.
- [ ] Run `cargo test -p pige_core --features provider-google --test google_mailbox`; expect RED.
- [ ] Implement generated-client calls with bounded concurrency (initial 4 requests/account), metadata field masks including From/Reply-To/To/Cc/Bcc/Message-ID/In-Reply-To/References/Date, authoritative internal date/size, pagination and retry categories. Bootstrap recent 300 per syncable system collection using deduplicated IDs, anchor before listing, then replay all history pages; skipped older messages are not snapshot deletions. Respect rate limits and use response `historyId`, not sorted-ID arithmetic.
- [ ] Implement bodies/attachments with size caps and normalized content; history enumeration stays header-only. Represent All Mail as a provider-owned virtual collection if needed, not a nonexistent Gmail label ID; keep API listing exclusion/includeSpamTrash policy explicit. Full history crawl includes accessible Spam/Trash as suppressed low-trust observations. Native archive removes INBOX; label removal only changes that label, actual delete tombstones the message. Use gmail.modify by default, advertise permanent_delete=false unless a separately approved full-mail grant exists.
- [ ] Re-run google_mailbox plus provider_contract/provider_sync/provider_mutations/history_learning; expect GREEN and no UID conversions in google modules.
- [ ] Commit: `feat: add native Gmail mailbox and history synchronization`.

### Task G3: People saved contacts and Other contacts

**Files:** Create google `people.rs`, `tests/google_people.rs`, fixtures; modify factory capability/source configuration and `docs/google-oauth-setup.md`.

**Interfaces:** `GoogleAddressBook` implements AddressBookService for source kinds `google.saved` and `google.other`. Saved contacts list via `people.connections.list`, Other contacts via `otherContacts.list`; read masks request only allowed names/email/metadata fields. Source cursors contain snapshot/sync/page token, immutable request parameters and sweep generation. Query-time compose still calls local R2, not People search.

- [ ] Write RED tests `saved_and_other_contacts_keep_distinct_sources`, `saved_person_and_other_person_share_suggestion_not_remote_id`, `deleted_person_removes_contact_source_only`, `expired_sync_token_triggers_full_sweep`, `missing_permission_keeps_mail_and_local_history`, `partial_snapshot_does_not_delete_cached_contacts`, `all_pages_keep_identical_read_parameters`, `other_contacts_scope_is_not_assumed_from_contacts_scope`.
- [ ] Run `cargo test -p pige_core --features provider-google --test google_people`; expect RED.
- [ ] Implement S1-selected People SDK or narrow REST calls through shared tokens; no copy of unchecked third-party request masks. Follow actual API expiry semantics (e.g. People HTTP 410/token-expired), paginated sync token rules and deletion metadata. Never use Gmail historyId for contacts. Request authorization through a fresh supported grant operation when contacts are enabled later; 403 disables/reports only affected sources.
- [ ] Re-run google_people and contact_sync/recipient_lookup; expect GREEN. Live probe confirms saved plus automatically populated Other contacts where the user account has them; empty Other contacts is valid, not a reason to scrape Gmail web.
- [ ] Commit: `feat: sync Google saved and Other contacts`.

### Task G4: Gmail native submission and ambiguous send recovery

**Files:** Create google `submission.rs`, `tests/google_submission.rs`; modify `send/compose.rs` only if MIME/alias requirements need it, and Google factory wiring.

**Interfaces:** `GmailSubmission` implements SubmissionService with ServerManaged Sent policy. Prepare persists immutable MIME containing the correct API recipient information and stable RFC Message-ID/operation evidence; submit uses `users.messages.send` or an S2-evidenced draft-send path. `reconcile` returns Accepted only with sufficiently strong operation/account/message evidence; a mere matching subject is not proof. `finalize_sent` never APPENDs.

- [ ] Write RED tests `accepted_receipt_records_message_and_thread`, `gmail_never_calls_smtp_or_append`, `timeout_after_send_is_uncertain`, `same_subject_is_not_reconciliation_proof`, `retry_after_draft_preparation_does_not_duplicate_send`, `from_alias_is_validated`, `to_cc_bcc_recipient_sets_are_preserved`, `reply_references_and_thread_hint_are_consistent`, `structured_parts_are_encoded_in_mime`, `successful_send_trains_once`.
- [ ] Run `cargo test -p pige_core --features provider-google --test google_submission`; expect RED.
- [ ] Implement MIME/base64url payload and receipt mapping. Native send relies on Gmail's documented Bcc processing: do not strip Bcc before the API has recipient information; verify with authorized multi-recipient live test that delivered mail does not expose it. Use selected alias only if allowed; map provider rejection distinctly from connection loss. Searches after timeout can establish acceptance only if reliable; otherwise require explicit retry decision. Never assume RFC Message-ID is a server idempotency key.
- [ ] Run google_submission and provider_outbox/compose; expect GREEN. With permission, send to controlled To/Cc/Bcc recipients and inspect separate delivered copies and Sent placement.
- [ ] Commit: `feat: submit Gmail messages through native API`.

### Task G5: Native Gmail onboarding, explicit switching and release setup

**Files:** Modify account UI/update/message/platform construction, Google registry wiring and docs; create `tests/live_google.rs` and provider lifecycle additions.

**Interfaces:** Existing F10 begin/answer/cancel auth API handles new Gmail accounts and explicit IMAP-to-Gmail switch after `can_switch_backend`. Contact-enable action invokes G1 authorization and R6 source registration. UI offers native Google separately from manual IMAP; existing Gmail IMAP settings remain editable as IMAP.

- [ ] Write RED tests `new_google_account_has_no_password_or_server_fields`, `cancel_login_creates_no_account`, `contacts_optional_at_setup`, `switch_refuses_sent_copy_repair_or_uncertain_send`, `native_delete_button_reflects_scope`, `restore_google_config_requires_reauth_not_embedded_tokens`, `same_email_accounts_are_not_silently_merged`.
- [ ] Run `cargo test -p pige_core --features provider-google --test provider_lifecycle` and affected account UI unit tests; expect new assertions RED.
- [ ] Wire onboarding/reconnect/contacts choices and backend switch recovery. Document console registrations, Android package/signing identity, minimal scopes, consent verification, testing-user limitations/refresh lifetimes and release signing updates. Feature off yields a readable unavailable-provider account state, not corrupt-config fallback.
- [ ] Re-run Google/core/workspace tests and Android release build. `live_google` ignored tests require explicit env plus send opt-in; missing credentials fail an explicitly requested live run, never silently pass. Record cold resume, sync/delta, contacts denial, offline autocomplete, send, revoke and switch recovery on Linux/Android.
- [ ] Commit: `feat: expose native Gmail accounts and optional Google contacts`.

## Exit gate

- [ ] Production registration/consent requirements documented separately from code completion.
- [ ] Gmail/native contacts integrate without IMAP-specific application branches.
- [ ] Old Gmail IMAP account continues working; any switch is deliberate and blocked for unfinished delivery.
