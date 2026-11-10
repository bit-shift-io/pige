# Adding a mail provider

This recipe is the extension contract for a native backend. It describes the
smallest safe implementation, the mappings and cursors that must stay stable,
the recovery and Sent-copy policy, and the checks that must pass before the
provider is registered in a release build.

The existing Gmail and Proton adapters are the reference implementations. The
shared vocabulary is defined in
[`docs/provider-feasibility.md`](provider-feasibility.md) and the plan master
`2026-11-07-mail-providers.md`; use those names verbatim instead of inventing a
provider-specific type.

## 1. Implement only advertised capabilities

A provider session advertises `ProviderCapabilities` and attaches exactly the
services those booleans promise:

| Capability | Service | Notes |
| --- | --- | --- |
| `mailbox` | `MailboxService` | collections, body, attachment, history |
| `sync` | `SyncService` | one bounded change page at a time |
| `mutations` | `MutationService` | only the booleans set in `MutationCapabilities` |
| `submission` | `SubmissionService` | prepare/submit/reconcile/finalize |
| `contacts.read` | `AddressBookService` | source-scoped pages |
| `server_search` | none in v1 | leave false until a contract exists |

Every absent service must return `ErrorKind::Unsupported`, never a panic or an
empty success. `MutationCapabilities` must be exact: the UI uses it to decide
which actions exist. `ContactCapabilities.write`, `groups`, and
`remote_search` are false in v1 even when the wire API supports them.

Provider code lives behind the registry (`ProviderFactory`/`ProviderRegistry`).
The application opens a session with `ProviderFactory::open(&self, account,
ctx)`; it never reaches a protocol or SDK type directly.

## 2. Keep identity and cursors versioned

- Use the string newtypes `ProviderId`, `RemoteMessageId`,
  `RemoteCollectionId`, `RemoteThreadId`, `RemoteAttachmentId`,
  `OperationId`, `ContactSourceId`, and `RemoteContactId`. Never store a local
  `MessageId` in a remote field or the reverse.
- `User`-visible account ids and local message ids survive a backend switch.
  Remote ids are migration-scoped: the store keeps `(account, provider,
  remote id)` mappings so local ids, drafts, threads, and outbox rows are
  preserved.
- `ProviderCursor { version, payload }` is an opaque JSON envelope. It must
  never contain an access token, refresh token, password, private key, or
  plaintext header. Bump `version` when the payload semantics change; do not
  silently reinterpret an old payload.
- Persist enough in the cursor to resume bootstrap, delta, and reconciliation
  separately. A cursor from another account, job kind, source, or adapter
  version must be rejected as `CursorInvalid` and restarted from a safe point.
- `JobKey { account, kind, source }` identifies the durable stream. A sync
  cursor belongs to `MailSync`, a header-history cursor to `History`, and a
  contact snapshot to `Contacts` with its source id.
- Every durable write carries the expected account generation. Fence stale
  work with `OperationContext::generation`; use the shared account lease when a
  cross-process worker can write.

## 3. Demonstrate recovery and Sent policy

Implement the full page contract; partial or optimistic pages are not enough:

1. **Initial sync** (no cursor): return collections/messages with an initial
   checkpoint and `has_more`. A bounded recent-mail page may not retire absent
   messages.
2. **Delta sync**: resume from the checkpoint, include every changed membership
   and flag, and preserve unmentioned header fields. Only a completed scoped
   snapshot generation may remove records that are truly absent.
3. **History recovery**: implement `MailboxService::history` and return
   `HistoryPage` values with `HistoryCoverage`. History is learned for
   suggestions and is not inserted into the visible mailbox cache
   automatically. Respect the durable learning epoch, pause/clear state, and
   rate-limit retry time.
4. **Interrupted work**: after a process death, resume from the last committed
   page. Never replay an accepted non-idempotent write.
5. **Sent copy**: advertise `SentCopyPolicy::ServerManaged` only when the
   provider's submission API itself writes the Sent copy. Otherwise use
   `ClientAppend` and implement `finalize_sent` append semantics.
6. **Submission outcomes**: `Accepted`, `Rejected`, and `Unknown` are distinct.
   On a timeout or transport loss after a write may have reached the server,
   report `Unknown`/`UnknownWriteOutcome`, store the operation id and prepared
   artifact, and use `reconcile` to look for evidence. Never resend blindly.
   A retry must reuse the same `OperationId` and prepared MIME/Message-ID.
7. **Prepared artifacts** contain only the version, backend id, raw MIME, and an
   opaque reference. A session token, refresh token, password, or private key
   must never be copied into `PreparedSubmission`, the outbox, or a log.

The outbox owns durability; adapters must be idempotent with respect to the
operation id. A Sent-copy failure leaves the accepted row recoverable and must
not cause a second submission.

## 4. Wire normalization and fixtures

- Put wire parsing/normalization in a private module. The application boundary
  sees only provider-neutral records.
- Normalize partial flag updates into a complete `MessageRecord`; never erase
  unmentioned membership, `reply_to`, or `bcc`.
- Treat an unexpected protocol shape as a typed, redacted `ProviderError`
  (`Unsupported`, `Reauthenticate`, `CursorInvalid`, …), not a panic.
- Add deterministic fixtures for every wire shape the adapter accepts. Prefer
  a local mock server or transport fixture over a live account.
- Keep provider error `safe_message` presentation-ready. Put protocol payloads
  only in `source_detail`, which `Display` and `Debug` deliberately redact.

## 5. Pass the shared tests

Run from the repository root:

```sh
bash scripts/verify-providers.sh default
bash scripts/verify-providers.sh all
```

The script runs formatting, the workspace tests, clippy, every provider feature
combination, `--no-default-features`, and the dependency/license boundary gate.
It never uses credentials, never runs an ignored live test, and never sends
mail.

For a new adapter, also add and pass:

- the shared provider contract suite (`tests/common/provider_contract.rs`) via
  a harness: open/capabilities/service presence, cancellation, opaque cursors,
  and supplied fixtures;
- that provider's targeted mailbox, sync, mutation, submission, auth, and
  contacts tests;
- `tests/provider_privacy.rs`, which covers log/backup/outbox redaction,
  clear-and-restart history, account removal, and denied scopes;
- an ignored live test binary with one explicit opt-in per case. A live test
  invoked without its environment must fail clearly, never pass silently.
  Sending additionally requires `PIGE_TEST_ALLOW_SEND=1` and a disposable
  account; the ordinary test run must never send mail.

## 6. Audit auth, privacy, licenses, and target builds

Auth:

- Use the shared `AuthenticationService`/`TokenSource` contract. OAuth must use
  PKCE S256 and an exact `state` check on desktop, and bind an Android host
  result to one `AuthAttemptId`.
- Interactive authentication stages a candidate secret. Only account creation
  or a guarded backend switch commits it. A replacement credential never
  overwrites the active one while a submission or Sent-copy repair needs it.
- Rotate and revoke tokens generation-fenced. An unreadable or newer secret
  envelope is a hard error, not a second guess.

Privacy:

- Never log tokens, keys, message plaintext, contact payloads, or full
  suggestion queries. Capture a synthetic marker through log, config, and
  backup paths before adding a new diagnostic.
- The mail cache is plaintext at rest. Document that limitation; do not claim
  application-level encryption or forensic erasure.
- Clearing learned data is a logical deletion and disables automatic history
  until an explicit rebuild. Account removal must cascade every contact,
  correspondent, recipient-index, learning-state, job, and pending-auth row.
- Google contacts denial is source-scoped: mail and local history keep working,
  only that source is disabled, and a permission denial must not become an
  infinite background retry. Rate-limit/quota errors are retryable; a true
  permission denial is not.

Licenses and dependency gate:

- Run `scripts/check-provider-boundaries.py` through the verify script. It
  checks the resolved graphs for both the Android target and the host desktop
  target, rejecting the Go `gopenpgp-sys` backend, unapproved AGPL packages,
  and a second owner of the native SQLite link slot. `proton-crypto` must stay
  on the pure-Rust `rustpgp` backend with `default-features = false`.
- Update `Cargo.lock` deliberately in a normal change, never as a side effect
  of verification. The verify script uses `--locked`.
- Build the actual Android target before accepting a dependency or feature
  combination. `cargo check` on the host is not proof of an Android build.

## 7. Replace an SDK without losing local data

Use this sequence when the provider SDK/API changes:

1. Read the new SDK's identity and cursor semantics. Decide whether existing
   remote mappings and cursors are still valid.
2. If mappings remain valid, keep the current `(provider, remote id)` rows and
   bump only the `ProviderCursor` version where the payload changed. A stale
   cursor becomes `CursorInvalid` and the adapter restarts that job safely;
   local ids, drafts, outbox rows, and learned evidence survive.
3. If a remote id format is incompatible, migrate or drop only that provider's
   remote mappings and cursors. Do not delete local messages, drafts, outbox
   submissions, or contact evidence. Removing an account or switching backends
   remains the only operation that clears provider-derived cache.
4. Keep the outbox operation id and prepared artifact stable across the
   upgrade. An accepted-but-unfinalized submission must finish on its original
   backend before the adapter is swapped.
5. Add regression fixtures for both the old and new wire shapes during the
   transition. Run the full deterministic suite and the live checklist before
   deleting the compatibility path.

### Proton fork upgrades

The vendored library is a narrow MIT fork described in
[`proton-dependency-updates.md`](proton-dependency-updates.md). On every update:

- re-check provenance and license files, and keep `proton-crypto` on the
  pure-Rust backend;
- run `scripts/verify-providers.sh all` and the ignored live Proton checklist
  in [`proton-support.md`](proton-support.md);
- verify SRP group/proof handling, key unlocking, signature states, recipient
  encryption policy, and human-verification/TOTP challenges;
- confirm the app-version token and that a format change fails closed rather
  than downgrading to cleartext.

### OAuth release signing and consent upkeep

- Register the Android OAuth client for package `bitshift.pige` and the actual
  signing SHA-1. A new keystore or debug-key build needs a new fingerprint or
  consent returns `DEVELOPER_ERROR`.
- Keep the release keystore backed up and outside git; losing it prevents
  in-place upgrades.
- Keep the consent screen scopes minimal (`gmail.modify` plus identity, and the
  contacts scopes only on opt-in). Publish/verify the app before distributing to
  users outside the test list; restricted scopes may require a security
  assessment.
- Test-user refresh tokens can expire after seven days while the consent screen
  is in Testing. Re-authorize a disposable account for live checks.
- Record the release matrix in
  [`provider-release-verification.md`](provider-release-verification.md). Live
  rows stay BLOCKED until an authorized disposable account actually runs them.

## Final checklist

- [ ] Advertised capabilities exactly match attached services.
- [ ] Remote ids and cursors are versioned, opaque, and account/job scoped.
- [ ] Initial, delta, history, and interrupted-work recovery are demonstrated.
- [ ] Sent-copy policy and accepted/rejected/unknown reconciliation are tested.
- [ ] No secret, key, plaintext, contact payload, or query marker reaches logs,
      config, backup, or outbox artifacts.
- [ ] Shared contract, provider-specific, privacy, and ignored live tests pass.
- [ ] Dependency/license gate, `--locked` check, and desktop + Android release
      builds pass.
- [ ] Auth, privacy, license, and target-build notes are documented.
