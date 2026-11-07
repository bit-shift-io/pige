# Native Mail Providers and Contacts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver replaceable native Gmail/Proton and IMAP/SMTP backends, synced contacts, and persistent history-based recipient autocomplete on Linux and Android.

**Architecture:** Pige owns capability contracts, domain types, account configuration, cache, jobs, and outbox. SDKs stay inside adapters; protocol state uses opaque versioned cursors and IDs. Contacts and learned correspondents are independent data sources combined by local autocomplete.

**Tech Stack:** Rust 2024, Tokio, async-trait, rusqlite, existing Iced/Kotlin/WorkManager shells, async-imap, lettre; conditional google-gmail1/People and native Proton Rust dependencies selected by feasibility evidence.

**Spec:** `docs/superpowers/specs/2026-11-07-mail-providers-and-contacts-design.md` (approved).

## Global Constraints

- Proton must work directly on Android: no Bridge, Go service, or hosted mail proxy.
- Keep the project distributable under its existing MIT licensing direction.
- Do not implement REST providers by inventing IMAP UIDs.
- Keep provider SDK types, protocol identifiers, crypto, and wire errors inside their adapters.
- Never silently downgrade to plaintext.
- No fixed message-count cutoff: full accessible history is the target, not an immediate guarantee.
- Existing Gmail IMAP accounts do not silently convert during application upgrade.
- Preserve local IDs, accounts, drafts, queued mail, bodies, attachments, threading, and search during upgrades.
- No implementation in this planning change. Do not treat the detailed plan request as permission to run live probes/send mail.

## Review Focus

1. Process death during token rotation or send acceptance must not lose usable credentials silently or resend an accepted message (01 Tasks F3/F9; 03 G1; 04 P1/P4).
2. Arrivals/deletions during historical pagination must not silently end indexing early or resurrect removed mail (01 F7; 02 R3; 03 G2; 04 P2).
3. Untrusted incoming display names must not overwrite saved names, expose Bcc, or trick users into selecting another address (02 R1/R2/R5; 03 G4; 04 P4).
4. Restore/removal concurrent with a worker must not recreate accounts, secrets, cached contacts, or learned history (01 F3/F10; 02 R4).
5. Mixed-version config/cache or a failed schema rebuild must preserve the old installation's data and reject newer formats safely (01 F2/F4/F9; 05 H1).

## Execution map

This is one ordered program with separately executable plans. Each subplan has
its own file map, exact interfaces, red/green tests, and commits. Do not parallelize
tasks that modify the same domain/config/schema interfaces.

| Order | Plan | Deliverable / exit gate |
| --- | --- | --- |
| 0 | [Feasibility](2026-11-07-mail-providers-00-feasibility.md) | Supported Android Google auth and native Proton interoperability proved, or explicit stop report. |
| 1 | [Provider foundation](2026-11-07-mail-providers-01-foundation.md) | Existing IMAP/SMTP runs through the new boundary with migrated cache/outbox and no UI regression. |
| 2 | [Contacts and recipient learning](2026-11-07-mail-providers-02-recipients.md) | Offline autocomplete and resumable full-history learning work on existing IMAP accounts. |
| 3 | [Gmail and People](2026-11-07-mail-providers-03-google.md) | OAuth native Gmail, saved/Other contacts, labels/history/submission, safe explicit backend switching. |
| 4 | [Proton](2026-11-07-mail-providers-04-proton.md) | Native Android/Linux Proton auth, encrypted mail, events, contacts, and historical learning. |
| 5 | [Hardening and release](2026-11-07-mail-providers-05-release.md) | Measured performance, migration recovery, live/on-device evidence, maintenance documentation. |

Native Google and Proton adapter implementation can proceed independently only
after foundation and recipient contracts are stable. Keep Proton feasibility
first even though Gmail production implementation comes earlier.

## Shared interface vocabulary (owned by F1 unless noted)

These names are the cross-plan contract, not complete implementation code. All
network capability traits use `#[async_trait]`, are object-safe and `Send`, and
take `&mut self`. Synchronous storage does not hold a lock over network awaits.

- In `providers/types.rs`: string newtypes `ProviderId`, `RemoteMessageId`,
  `RemoteCollectionId`, `RemoteThreadId`, `RemoteAttachmentId`, `OperationId`,
  `ContactSourceId`, `RemoteContactId`, `AuthAttemptId`; local IDs remain in `mail`.
- `ProviderCursor { version: u32, payload: String }`: opaque JSON envelope, never
  a token/key. `JobKey { account: AccountId, kind: JobKind, source: String }`;
  `JobKind = MailSync | History | Contacts`.
- `OperationContext { account: AccountId, generation: u64, cancellation:
  CancellationToken, deadline: tokio::time::Instant }`. Generation is checked
  before network work and again in every durable write transaction (F3/F7).
- `ProviderError { kind: ErrorKind, safe_message: String, retry_after:
  Option<Duration> }`; `ErrorKind = Offline | Transient | RateLimited |
  Reauthenticate | Forbidden | Unsupported | CursorInvalid | Conflict |
  NotFound | Cancelled | UnknownWriteOutcome`.
- `ProviderCapabilities { mailbox: bool, sync: bool, mutations:
  MutationCapabilities, submission: bool, contacts: ContactCapabilities,
  server_search: bool }`. Mutation capability booleans: `read`, `star`, `labels`,
  `move_messages`, `archive`, `trash`, `permanent_delete`. Contacts:
  `read`, `incremental_sync`, `remote_search`, `write`, `groups` (last two false in v1).
- `CollectionRecord { remote_id, name, kind: FolderKind, delimiter:
  Option<String>, total: Option<u32>, unread: Option<u32> }`.
- `MessageRecord { remote_id, remote_thread: Option<RemoteThreadId>, header:
  MessageHeader, flags: Flags, collections: Vec<RemoteCollectionId>, snippet:
  String, has_attachments: bool }`. `MessageHeader` gains `reply_to` and `bcc`.
- `MailChange = UpsertCollection(CollectionRecord) | RemoveCollection(RemoteCollectionId)
  | UpsertMessage { message: MessageRecord, is_new_arrival: bool } | RemoveMessage(RemoteMessageId) |
  SetMembership { message: RemoteMessageId, collection: RemoteCollectionId,
  present: bool }`. Partial flag updates must be normalized by adapter to a full
  `MessageRecord` rather than erasing unmentioned membership/header data.
- `SyncRequest { cursor: Option<ProviderCursor>, initial_limit: u32 }`;
  `ChangePage { changes: Vec<MailChange>, checkpoint: ProviderCursor,
  has_more: bool }`. Checkpoint encodes bootstrap/delta/reconciliation stage.
  Snapshot sweeps retire absent records only after a completed scoped generation,
  never after one bounded recent-mail page.
- `HistoryRequest { cursor: Option<ProviderCursor>, page_size: u32 }`;
  `HistoryPage { messages: Vec<MessageRecord>, checkpoint: ProviderCursor,
  has_more: bool, coverage: HistoryCoverage }`;
  `HistoryCoverage { scanned: u64, total_hint: Option<u64> }`. History records are
  learned without being inserted into the visible mailbox cache automatically.
- `SignatureStatus = NotApplicable | Verified | Unsigned | Unverified | Invalid |
  Unsupported`; `BodyContent { plain_text: Option<String>, html: Option<String>,
  raw: Option<Vec<u8>>, attachments: Vec<AttachmentDescriptor>, signature:
  SignatureStatus }`; `AttachmentDescriptor { remote_id: RemoteAttachmentId,
  part_id: String, filename: String, content_type: String, size: u64,
  content_id: Option<String>, disposition: AttachmentDisposition }`.
- `MutationRequest { operation: OperationId, targets: Vec<RemoteMessageId>,
  source: Option<RemoteCollectionId>, action: MutationAction }`;
  `MutationAction = SetRead(bool) | SetStar(bool) | AddLabel(RemoteCollectionId) |
  RemoveLabel(RemoteCollectionId) | Move(RemoteCollectionId) | Archive | Trash |
  DeletePermanently`. `MutationResult { items: Vec<MutationItemResult> }`;
  `MutationItemResult { message: RemoteMessageId, outcome: MutationOutcome }`;
  `MutationOutcome = Applied | Rejected(ProviderError) | Unknown`.
- `SubmissionRequest { operation: OperationId, message: OutgoingMessage,
  attachments: Vec<SubmissionAttachment>, prepared: Option<PreparedSubmission> }`;
  `SubmissionAttachment { id: String, filename: String, content_type: String,
  bytes: Vec<u8>, content_id: Option<String>, disposition: AttachmentDisposition }`.
  These are size-bounded structured parts for the submission API; the existing
  plain-text compose UI passes an empty vector (no new attachment-picker UI).
  `PreparedSubmission { version: u32, backend: ProviderId, raw: Option<Vec<u8>>,
  reference: Option<String> }`; no passwords, tokens or private keys in this value.
  `SubmissionReceipt { operation, remote_message: Option<RemoteMessageId>,
  remote_thread: Option<RemoteThreadId> }`.
- `SubmissionOutcome = Accepted(SubmissionReceipt) | Rejected(ProviderError) |
  Unknown`; `SentCopyPolicy = ServerManaged | ClientAppend`; `ReconcileOutcome =
  Accepted(SubmissionReceipt) | NotAccepted | Unknown`.
- `ContactRecord { remote_id: RemoteContactId, version: Option<String>, names:
  Vec<String>, emails: Vec<Address> }`; `ContactChange = Upsert(ContactRecord) |
  Delete(RemoteContactId)`; `ContactPage { changes: Vec<ContactChange>,
  checkpoint: ProviderCursor, has_more: bool }`. Full contact snapshot cursors
  encode a sweep generation; only complete snapshots remove absent contacts.

Exact services in `providers/traits.rs` (`Result<T, ProviderError>` on methods):

```rust
trait MailboxService: Send {
    async fn collections(&mut self, ctx: &OperationContext) -> Result<Vec<CollectionRecord>, ProviderError>;
    async fn body(&mut self, id: &RemoteMessageId, ctx: &OperationContext) -> Result<BodyContent, ProviderError>;
    async fn attachment(&mut self, message: &RemoteMessageId, id: &RemoteAttachmentId, ctx: &OperationContext) -> Result<Vec<u8>, ProviderError>;
    async fn history(&mut self, request: HistoryRequest, ctx: &OperationContext) -> Result<HistoryPage, ProviderError>;
}
trait SyncService: Send {
    async fn next_page(&mut self, request: SyncRequest, ctx: &OperationContext) -> Result<ChangePage, ProviderError>;
}
trait MutationService: Send {
    async fn apply(&mut self, request: MutationRequest, ctx: &OperationContext) -> Result<MutationResult, ProviderError>;
}
trait SubmissionService: Send {
    fn sent_copy_policy(&self) -> SentCopyPolicy;
    async fn prepare(&mut self, request: &SubmissionRequest, ctx: &OperationContext) -> Result<PreparedSubmission, ProviderError>;
    async fn submit(&mut self, request: &SubmissionRequest, ctx: &OperationContext) -> Result<SubmissionOutcome, ProviderError>;
    async fn reconcile(&mut self, request: &SubmissionRequest, ctx: &OperationContext) -> Result<ReconcileOutcome, ProviderError>;
    async fn finalize_sent(&mut self, request: &SubmissionRequest, receipt: &SubmissionReceipt, ctx: &OperationContext) -> Result<(), ProviderError>;
}
trait AddressBookService: Send {
    async fn next_page(&mut self, source: &ContactSourceId, cursor: Option<ProviderCursor>, ctx: &OperationContext) -> Result<ContactPage, ProviderError>;
}
```

`ProviderSession { capabilities, mailbox: Option<Box<dyn MailboxService>>, sync:
Option<Box<dyn SyncService>>, mutations: Option<Box<dyn MutationService>>,
submission: Option<Box<dyn SubmissionService>>, address_book:
Option<Box<dyn AddressBookService>> }`. Shared account token/client state may be
held by adapter-internal `Arc`s; do not borrow one mutable service from another.
`ProviderFactory::open(&self, account: &Account, ctx: &OperationContext) ->
Result<ProviderSession, ProviderError>` is async, `Send + Sync`, implemented by
`ProviderRegistry`. Auth-specific interfaces and public API wrappers are in F3.

### Shared execution invariants

- Every committed change includes the expected account generation and checkpoint.
  Cross-process operations use a durable account lease/fencing token as well as
  an in-process mutex; CAS alone cannot prevent stale remote reads before a mutation.
- Context deadlines and cancellation apply to all capability calls; no account
  lease is held indefinitely while waiting on interactive authentication.
- `UpsertMessage.is_new_arrival` is false for snapshot/history/metadata-only
  updates and true only for adapter-confirmed newly arrived mail; this distinction
  prevents clear-history from being undone by ordinary label/flag resyncs.
- Auth completion stages a candidate account and protected secret reference.
  Only account creation or the F10 recoverable switch commits it to the active
  account; authenticating a replacement backend never overwrites old credentials
  while submissions or Sent-copy repairs still need them.

## Reading / execution conventions

- Read the spec, this master, then the relevant subplan before editing.
- Run Rust commands from `rust/`; root shell/Android commands explicitly say root.
- New integration test binaries serialize global `api` state with one test mutex,
  as existing `tests/api.rs` does. Prefer Store/unit tests for parallelism.
- Each task's RED test list specifies exact names and assertions, not just topics.
  For grouped tests, the command intentionally runs the entire named binary.
- Each checkbox is a separate action; code follows RED. Commit only named task
  files after GREEN and inspect `git diff --cached` first. Suggested commit
  messages are not permission to stage unrelated work.
- Re-read current source before execution: concurrent UI edits existed while this
  plan was written. Don't overwrite them or rely on the old line numbers.
- Schema version was 4 and config version 1 when planned. Allocate the next free
  migration/version on execution; never reuse another change's version number.
- The current host reports Rust 1.98.1; dependencies requiring 1.96 are feasible
  here but still require explicit CI/Android toolchain verification.
- All new DTOs have redacted Debug where they contain sensitive fields. Adapter
  fixtures are synthetic; no real mail, auth tokens, keys, or contact lists in Git.
- All wire types and credentials stay private to adapters/auth. The v1
  `server_search` and contact `remote_search`, `write`, `groups` flags remain
  false until matching contract methods and implementations exist.
- Native-provider integration test files use crate-level feature guards; default,
  each-provider-only, no-default-features, and all-features builds must compile.
- Preserve existing plain-text compose UX; internal submission attachment parts
  prepare for provider API encryption/forwarding without adding a file picker.

## Plan tracking and self-review

- [x] Read approved spec and relevant Rust/Kotlin/build/test entry points.
- [x] Define ordered subplans and shared signatures before task decomposition.
- [x] Finish detailed subplans and self-review spec coverage, types, tests, scope.
- [ ] User reviews execution plan and selects execution method.

### Spec-to-task coverage

| Approved design area | Owning tasks |
| --- | --- |
| Library comparison, Android link/live verification, licenses, fork upkeep | S1-S3, G5, P5, H3 |
| Optional mailbox/submission/auth/contact contracts and no UID emulation | F1, F5-F8 |
| Account/config/secret upgrade, cancellation and rotated tokens | F2-F3, F10, G1, P1 |
| Remote IDs, multi-label memberships, existing local IDs/FTS/body preservation | F4, F8, H1 |
| Semantic operations, scoped flags, partial writes and sync races | F6-F8, G2, P2, H1 |
| Normalized reader/attachments and signature/error states | F8, G2, P3 |
| Durable send preparation/acceptance/Sent repair/unknown result | F9, G4, P4, H1 |
| Safe explicit backend switching and recovery | F10, G5, H1 |
| Saved/Other contacts, independent contact sources, optional scopes | R1-R2, R6, G3, P3 |
| Learning provenance, roles/Bcc, normalization, trusted names, dedup/ranking | R1-R2, G4, P4 |
| Full-history header indexing, bootstrap/live catch-up, deadlines/backoff | F7, R3, R6, G2, P2 |
| Persistent cache, clear/pause/rebuild, remove/reset/backup privacy | R4, F10, H3 |
| Token-aware keyboard/touch compose suggestions and stale-response rejection | R5 |
| Device performance target, memory/disk/APK measurements and extension docs | H2-H3 |

Self-review corrected shared type ownership, explicit new-arrival vs metadata
learning, cross-process fencing, and staged replacement credentials before backend
switch. All five Review Focus cases map to named tests. No spec requirement is
intentionally omitted; unknown supported-auth and Proton security details are
explicit S2/S3 stop/go inputs, not assumed implementation success.

Concrete ranking limits, 100 ms debounce, 100-record/5-page/5-second crawl slices,
four Google HTTP requests/account, and 25 MiB total outgoing attachment cap are
implementation defaults chosen for this plan, not claims about provider limits.
Measure/tune them within the approved boundedness/performance constraints.
Existing plain-text compose remains; structured attachment parts are internal
submission support, not an added attachment-picker feature.

The detailed tasks are an implementation roadmap, not evidence that dependencies
build, Google approval is obtained, Proton access is supported, or tests pass.
