# Native mail providers, contacts, and recipient learning

Date: 2026-11-07
Status: Approved by the user in conversation; detailed implementation planning requested.

## Intent and agreed scope

Add native Gmail and Proton Mail access to Pige on Linux and Android, retaining
IMAP/SMTP. Own the application-facing contracts so dependencies can be replaced
and future services added without rewriting the application. Proton must work
directly on Android: no Bridge, Go service, or hosted mail proxy.

The user selected replaceable adapters around reusable libraries rather than
owning every protocol implementation. Version one includes synced contacts and
To/Cc/Bcc autocomplete, not an editable address-book application. Autocomplete
must learn as many email addresses as practical and persist that knowledge in
the user's local cache, including history beyond the visible mail window.

Gmail should have easy OAuth setup. Proton should have similarly straightforward
native login and remembered sessions; available Proton integrations use SRP,
not a verified public third-party mailbox OAuth authorization-code flow.

## Goals, non-goals, and constraints

- Preserve existing accounts, drafts, queued mail, cached reader content, local
  search, threading, attachment handling, and desktop/Android behavior.
- Keep provider SDK types, protocol identifiers, crypto, and wire errors inside
  their adapters. Pige owns domain types, persistence, scheduling, and the outbox.
- Support optional capabilities instead of pretending all services behave alike.
- Keep the project distributable under its existing MIT licensing direction.
  Audit transitive licenses; do not copy AGPL implementation code into Pige.
- Build and test the actual Android target before accepting a dependency.
- No Bridge; no credential collection by a hosted intermediary; no Gmail web
  scraping, authentication bypass, or impersonating official Proton clients.
- Do not claim exact Gmail-web suggestion parity, complete remote history before
  indexing finishes, exactly-once delivery, or Proton-client security parity.
- Defer contact editing, contact/group management UI, CardDAV implementation,
  calendar support, remote draft synchronization, and a dynamic plugin ABI.
  Existing local drafts and compose behavior remain supported.
- This is a multi-milestone refactor, not a single adapter addition. Each stage
  must leave existing IMAP/SMTP functionality working.

## Current implementation and refactor boundaries

`rust/pige_core/src/transport/mod.rs` already has object-safe `MailTransport`
and `SmtpSender` contracts and a fake. These are useful protocol boundaries,
but `MailTransport` exposes selected folders, numeric UIDs, UIDVALIDITY, and
IMAP-specific operations. Do not implement REST providers by inventing IMAP UIDs.

`sync/engine.rs` implements per-folder IMAP high-water sync; `api/mutations.rs`
contains Gmail-vs-generic IMAP policy. `api/mod.rs` directly selects folders,
fetches bodies, and constructs transports through `platform::TransportFactory`.
All those call sites need to move above a provider-neutral boundary.

`account.rs`, `store/accounts.rs`, and portable `config/mod.rs` currently require
IMAP/SMTP endpoints and password auth. JSON is authoritative account configuration;
SQLite mirrors it. Both shapes need backward-compatible upgrades.

`store/schema.rs` stores one message row per mailbox UID and one `folder_id` per
message. REST services need account-wide message identities and multiple label
memberships. Bodies, attachments, threading, snapshots, and search depend on
those local IDs, so schema migration cannot be an afterthought.

`send/outbox.rs` assumes SMTP acceptance followed by IMAP Sent APPEND. Gmail and
Proton native submission save Sent server-side and must not receive a second
APPEND. Preserve conservative handling of ambiguous/interrupted sends.

`platform/mod.rs` has a password-shaped credential abstraction; Android delegates
to Keystore-backed host methods. `pige/src/screens/compose.rs` currently stores
recipient fields as strings and has no autocomplete. Preserve address parsing
while adding token-aware suggestions through the core API.

## Approaches considered

1. **Selected: Pige-owned capabilities with replaceable SDK adapters.** Reuse
   libraries for protocol work, keep Pige's cache and orchestration. Initial
   refactoring is larger, but adding or replacing a provider stays localized.
2. **Own both REST implementations completely.** Maximum dependency control,
   but more authentication, encryption, protocol compatibility, and maintenance
   responsibility. A focused REST implementation remains a fallback within an
   adapter, not the default for every provider.
3. **Extend the existing IMAP trait to cover REST.** Less initial churn, but
   fictitious UID/folder semantics would spread throughout the cache and sync
   engine. Rejected for future extensibility and correctness.

## Dependency research and selection gates

Research below is documentation/manifest/source review, not proof of Android
compatibility or successful live-account operation. Registry searches covered
Gmail, Proton, and ProtonMail; this is not a claim to have tested every crate.
Pin exact accepted versions after compatibility spikes; repository HEAD may
contain functionality absent from a published release.

### Gmail and Google contacts

| Candidate | Assessment |
| --- | --- |
| `google-gmail1` | Recommended initial mail client: broad generated async API, MIT, history/labels/send support. Community generated, not an official Google Rust Gmail SDK. Supply authentication through Pige's token boundary. |
| `io-gmail` | Strong MIT/Apache alternative: I/O-free operations, batching, history watching. High-level client is blocking; coroutine integration requires an async driver. Current manifest declares Rust 1.89. |
| `gmail` (libninja) | Generated MIT fluent client; extra `httpclient` auth/transport abstraction needs evaluation. Not selected over the clearer initial choice. |
| `rightkit-google-apis` | Broad Gmail and People modules, including recipient lookup; recent extraction and desktop-oriented auth. Source contains request details that need API conformance review. Reference/candidate, not trusted turnkey integration. |
| `google-workspace-apis` | Documented Gmail coverage is incomplete for this client. |
| `rust-gmail` | Sending-focused/service-account example; not a full mailbox backend. |
| `threatflux-google-sdk` | Discovered, insufficient verified coverage to rank above the shortlist. |
| `mxr-provider-gmail`, `pidge-gmail`, CLI/MCP wrappers | Application-coupled rather than clear drop-in SDKs. Reference material only, subject to licenses. Apps Script Gmail types are not mailbox API clients. |

Contacts use Google's separate People API, whether mail uses a generated Gmail
SDK or another HTTP client. Evaluate `google-people1` as the matching generated
client or implement a narrow People REST adapter if this is simpler. Keep this
choice replaceable independently of Gmail mail access.

### Proton

| Candidate | Assessment |
| --- | --- |
| `proton-core` (`filippofinke/protonmail-rs`) | Closest high-level MIT candidate: SRP, crypto, mail, events, and read-only contact names/email entries. Manifest requires Rust 1.96, unconditional keyring and bundled rusqlite 0.32. Pige has bundled rusqlite 0.41: native SQLite linkage is a likely resolver blocker, not merely duplicate Rust types. Do not depend on it unchanged. |
| Official `ProtonMail/clients` mail Rust workspace | Real mail implementation and Android integration, but `mail-api` is AGPL-3.0-only and workspace-coupled. Do not link/copy under the current licensing direction without a separate licensing decision. |
| `proton-api-rs` | AGPL-3.0-only and Go SRP/FFI build dependency; reject for this direction. |
| `proton-sdk` (`narrrl`) | Pure-Rust account/session/crypto foundation for Drive; not a Mail implementation. Possible fallback foundation, not a ready Mail SDK. |
| `proton-sdk-rs2` | Account/session foundation attached to Drive functionality; same Mail gap. |
| `proton-sdk-rs` (`4tkbytes`) | WIP Drive SDK bindings; not a Mail solution. |
| `protonmail-client` / Bridge MCP wrappers | Bridge-based; excluded. |

Preferred evaluation: separate `proton-core` network/auth/mail operations from
its cache, keyring, CLI, and search ownership through upstream changes or a small
MIT-preserving maintained fork. Accept a fork only after documenting its patch
surface, update procedure, and test obligations. If extraction is impractical,
use a narrow Pige-owned REST layer with reusable official MIT crypto primitives.
Do not let that fallback turn into implementing cryptographic primitives.

Official `proton-srp`, `proton-crypto`, and related crypto-account components have
MIT manifests. `proton-crypto` defaults to a Go backend: explicitly disable it
and validate the Rust backend and all transitive dependencies on Android.

`proton-core` documents missing Key Transparency and FIDO2/WebAuthn support.
These require a security/compatibility decision and honest unsupported-account
handling, not a hidden downgrade. Live success claims in its README are upstream
claims until reproduced. Confirm third-party access expectations and honest
application identification with Proton documentation/maintainers; private API
changes and anti-abuse challenges remain ongoing operational risks.

## Provider architecture

Introduce a `providers` module in `pige_core`, with contracts, domain DTOs,
registry/factory, and adapter submodules. Start within the existing workspace;
split adapters into crates if build isolation warrants it. Register providers
statically with Cargo features, not a runtime plugin system.

A provider descriptor advertises optional capabilities; a factory creates an
account session with supported services. Conceptual contracts:

| Capability | Responsibilities |
| --- | --- |
| Mailbox | List collections; retrieve messages, displayable bodies, and attachments; paginated history/header enumeration. |
| Sync | Initial snapshot and paged changes with opaque checkpoints; explicit invalid-cursor/full-resync outcome. |
| Mutations | Semantic read/star/label/move/archive/trash/delete commands with explicit collection/message scope and per-item results. |
| Submission | Submit structured outgoing content, return accepted/rejected/unknown outcome and optional remote receipt; state Sent-copy policy. |
| Authentication | Begin/resume/cancel login, expose challenges, refresh/revoke sessions, persist rotated credentials. |
| AddressBook | Read/list/sync contacts; optional remote search; advertise future writes/groups without implementing editable UI now. |

SMTP implements submission only. An IMAP+SMTP account bundles its mailbox,
mutations, sync, and submission. An independent future CardDAV source can provide
contacts without a mailbox. Capabilities may be account/permission-specific;
permission denial disables that service without breaking mail.

Adapters translate SDK errors into typed categories: offline/transient,
rate-limited with retry time, expired auth/re-auth required, forbidden scope,
unsupported, cursor invalid, conflict, not found, and unknown write outcome.
Use bounded concurrency, cancellation, timeouts, and backoff. Never blindly
retry submissions or other non-idempotent writes after an ambiguous result.

Provider-specific archive/label/move semantics stay inside adapters. UI policy
uses capabilities, not `if gmail`. A command operates on explicit message IDs
and source collection, not an implicit mutable selected folder. IMAP sessions
can still SELECT internally. Expose native thread IDs as optional mappings;
Pige's existing RFC-header thread assignment remains the v1 display authority.

Bodies return normalized text/html, attachment descriptors, size information,
and signature status where available. Raw RFC822 is optional: Proton need not
pretend decrypted data is an original raw message. Reuse existing sanitizer,
rendering, attachment caps, and remote-content policy.

Submission accepts Pige-owned structured content and stable operation identity.
SMTP/Gmail can build MIME; Proton can encrypt and create provider drafts/packages.
Keep generated MIME or provider preparation references durable as appropriate.
Never place unlocked keys or session tokens in outbox rows. Provider preparation
must not change the logical outgoing message across retries.

## Identity, storage, and compatibility

Preserve `AccountId` and local `MessageId` independently of provider IDs. Store:

- Versioned account backend configuration: provider key plus typed, non-secret
  settings, with independent contact source configuration where needed.
- Provider remote message/collection/thread mappings scoped to account/backend.
  IMAP identity is mailbox identity + UIDVALIDITY + UID. Do not equate messages
  solely by RFC Message-ID; it can be absent or duplicated.
- One content/message identity per native API message, collection memberships
  separately. Gmail labels are many-to-many; IMAP folder copies remain distinct
  unless an adapter has a trustworthy global identity. Deduplicate display and
  learned evidence without assuming IMAP supports exact cross-folder identity.
- Folder-specific IMAP flags where needed, versus account-wide API message flags;
  label removal and deletion must not be conflated.
- Separate foreground sync, history-indexing, and contact checkpoints, with
  adapter/schema versioning and resumable page boundaries.

Use additive, transactional migrations followed by a controlled message-table
transition. Backfill old rows one-to-one, preserving IDs and foreign-key targets
for bodies, attachments, threading, and FTS. Compatibility projections can keep
existing snapshot shapes stable while reads are migrated. Snapshot queries must
count distinct local messages where multiple memberships exist.

Upgrade portable config explicitly; legacy accounts map to IMAP+SMTP and retain
secrets. Make readers handle the old config before new writes occur. Exclude
tokens, mailbox unlock material, contacts, and learned history from configuration
backups. Account edits and restore must invalidate stale jobs safely.

Replacing a library for the same provider preserves remote identifiers and
versioned checkpoints, or deliberately resets incompatible cursors. Switching
an account from IMAP to Gmail REST is not just replacing a crate: v1 requires a
controlled user-triggered backend switch, stops old workers, preserves drafts
and outbox, and rebuilds remote cache mappings rather than guessing identity.
Existing Gmail IMAP accounts do not silently convert during application upgrade.

For v1, a backend switch is blocked while any queued, preparing, submitting,
accepted-but-unfinalized, or uncertain submission remains. Resolve it using its
original backend first; never reinterpret an SMTP Sent-copy repair as a native
API send. Terminal rejected/failed rows may remain as history only when delivery
is known not to be pending; an ambiguous legacy failure is not treated as safe
without explicit user resolution. Drafts survive the switch and acquire the
new backend only when submitted. This intentionally avoids retaining two active
backend credential sets or transferring in-flight operations in v1.

## Authentication and platform boundary

Separate network/token management from platform interaction. Persist a versioned
secret envelope per account through the existing secure store, supporting legacy
password reads during migration. Serialize refresh per account, atomically save
rotated refresh tokens, and prevent late tasks from restoring removed credentials.
If persistence fails, report session-only behavior or require re-authentication;
never silently write secrets to the mail cache or config.

Google: desktop system-browser authorization code + PKCE/state validation;
Android must use a currently supported Google authorization mechanism, not a
desktop loopback listener, deprecated OOB flow, or embedded WebView. Evaluate
the Android host authorization bridge early. Prefer no mandatory Play-services
dependency (a design recommendation, not a user requirement); if that cannot be
achieved with a supported flow, return for a distribution/product decision.
Production OAuth client registration, consent verification, least-privilege
scopes, and refresh behavior are delivery work, not solved by choosing a crate.

Proton: SRP login with verified server proofs/modulus; TOTP, human-verification
challenge, and two-password/key-unlock states as needed. Persist only the session
and required protected key-unlock material, not the login password unnecessarily.
Keep unlocked keys in memory and wipe when practical. Unsupported authentication
modes must be explicit, never bypassed or downgraded.

Request Google contacts permissions only when enabling that capability. Gmail
mail scopes and contact/Other-contact scopes remain distinct. Lack of contacts
consent does not stop mail or local history autocomplete. Treat a later grant as
a supported new authorization operation rather than assuming installed-app
incremental authorization works identically everywhere.

## Contacts and persistent autocomplete

Google contact sources: People API saved contacts (`contacts.readonly`) and
Other contacts (`contacts.other.readonly`). Other contacts can be listed and
incrementally synchronized; they do not represent every email or the full Gmail
web ranking model. Workspace directory lookup is an optional future capability,
not required for v1. Proton's initial supported surface is contact names/emails;
full encrypted contact fields and editing are deferred.

That deferral applies to address-book presentation, not security-critical send
policy. Proton submission must resolve recipients through a supported Proton
flow and honor applicable encryption/signing preferences and trusted recipient
key material, including encrypted contact policy where required. Internal
recipients use Proton-compatible encryption. Ordinary external mail without an
encryption requirement may use Proton's normal unencrypted-to-recipient delivery,
with its security level made explicit; this is not a fallback for crypto failure.
If an external PGP preference, key verification requirement, or mandatory
encryption cannot be honored, block sending to that recipient with an actionable
unsupported-policy error. Never silently downgrade to plaintext. Password-link
delivery and advanced crypto controls are not required in v1.

Reading includes supported internal and external OpenPGP/MIME forms through
the selected implementation, with tested signature verdicts; unsupported forms
must be identified rather than rendered as verified or silently discarded.
During feasibility, inventory representative fixtures and the candidate's
actual format/policy coverage. Missing Key Transparency must be disclosed and
evaluated as a release limitation, not equated with an invalid-signature success.
If basic safe delivery needs a large encrypted-contact/crypto expansion beyond
the candidate, stop and bring that scope/security decision back to the user.

Store synced contacts separately from learned correspondents. Retain source ID,
account, remote ID/version, names, addresses, and tombstones. Future field-rich
contacts and writes can extend this model without treating learned addresses as
editable provider contacts. Never upload learned addresses automatically.

Learn from From, Reply-To, To, Cc, and available Bcc fields in message headers,
plus successful local submission envelopes. Add Reply-To to the normalized
header model; incoming mail usually cannot reveal Bcc. Do not infer hidden Bcc
recipients or scrape message bodies. Self identities/aliases are not default
suggestions. Do not train on unsent drafts, failed sends, or arbitrary keystrokes.

Maintain per-account address observations: original deliverable address,
normalized lookup key, observed names/provenance, incoming/outgoing counts,
last-seen/last-sent times, source confidence, and explicit user selection signals.
Use conservative normalization: lowercase/canonicalize domains and retain local
parts; do not strip dots, plus tags, or assume all local parts are case-insensitive.
Search may case-fold without rewriting the delivery address.

Learning must be replay-safe: durable per-message/per-submission evidence keys
or an equivalent deduplication ledger prevent count inflation from resync,
repeated pages, overlapping jobs, and Gmail multi-label membership. Reconcile
local-send and later Sent-message evidence when reliable receipts exist; otherwise
cap/rank imperfect duplicate evidence conservatively rather than claiming exact
interaction counts. IMAP UID resets must not erase accumulated learned history.

Default ranking proposal: deliberate prior selections and outgoing interactions,
saved contacts, Other contacts, then incoming-only observations; combine recency
and bounded frequency with current-account preference. Preserve trusted saved
names over untrusted incoming display names, always show actual email addresses,
and never auto-insert a recipient. Exclude blocked senders from suggestions;
retain Spam/Trash observations only as low-trust evidence suppressed unless a
trusted contact or deliberate outgoing/selection signal exists. These defaults
are assistant recommendations, not separately requested user settings.

Autocomplete queries are local-only, indexed and bounded. Return source/account
metadata, deduplicate exact-address suggestions, and support all configured
accounts with current sending-account preference. Remote contact search is not
required per keystroke. Compose tracks the active recipient token and request
generation so stale responses cannot replace suggestions after typing, changing
fields, or switching account. Keyboard and Android touch selection use the same
core results. Existing completed recipients remain unchanged.

### Historical coverage and scheduling

The ordinary initial mailbox window must not cap learning. Run a distinct
restart-safe metadata/header crawl, newest first, across accessible history and
collections, without body/attachment downloads solely for learning. Gmail should
use account-wide IDs rather than crawl every label as independent content;
Proton uses its paginated message metadata; IMAP enumerates real UIDs rather than
assuming dense ranges. Index available header Bcc only where authorized/exposed.

Catch up on live changes while historical pages proceed. Commit observations and
the next checkpoint together; use idempotent replay and periodic reconciliation
where enumeration is not a stable snapshot. Resume safely after cursor expiry,
UID epoch change, cancellation, or process death. Bound page size, concurrency,
memory, and work per scheduling slice; allow pause/resume and expose coverage
progress. No fixed message-count cutoff: full accessible history is the target,
not an immediate guarantee. Android WorkManager constraints mean eventual,
opportunistic progress; foreground indexing can continue while the app is open.
Recent mail, body-on-open, submission, and interactive operations take priority.

Keep learned observations in the persistent app-private SQLite cache under the
data directory, not an OS-purgeable temp/cache directory. They survive restart,
routine mail-body eviction, and remote message deletion. Provide clearing of
learned suggestions; clearing suppresses automatic historical relearning until
the user explicitly requests a rebuild (new observations may still be learned).
Account removal/factory reset remove account-scoped contacts/history and job
checkpoints; canceled tasks must not recreate them. Config backups exclude them.

Contacts, addresses, metadata, and decrypted Proton mail are sensitive. The
initial storage proposal follows Pige's existing app-private SQLite/file model
with OS credential protection, not a new promise of encrypted mail/contacts at
rest. Make this explicit in Proton setup and documentation. A mandatory encrypted
local cache would be an additional security/storage workstream requiring approval.
Never log tokens, keys, message plaintext, contact payloads, or full suggestion
queries; clearing data is logical deletion, not guaranteed forensic erasure.

## Sync, mutation, and outbox consistency

Adapters emit normalized changes. Pige applies each page and its durable cursor
in one transaction; incomplete pages do not advance checkpoints. Distinguish
membership removal from message deletion. Gmail expired history and Proton event
reset conditions cause a bounded reconciliation/full sync, not invented deltas.
Initial bootstrap must establish an anchor and replay changes to cover arrivals
and mutations during pagination. IMAP keeps UIDVALIDITY/high-water logic internal.

Serialize account writes/sync application or use version checks so a late sync
does not revert an optimistic mutation. Apply confirmed per-item outcomes and
resync uncertain items rather than rolling back an entire partially applied
batch. Preserve current v1 behavior that move/archive/trash/delete require a
connection; durable offline mutation queues are not added by this project.

Persist submission intent and a stable operation ID before network work. Distinct
states cover queued, preparing, submitting, accepted/awaiting-Sent-copy, complete,
rejected, and uncertain. On native API acceptance record its receipt and skip
SMTP-style Sent APPEND. Keep SMTP Sent-copy repair independent of delivery.
After a crash/timeout, reconcile through provider evidence if supported; absent
proof, surface uncertainty and require deliberate retry rather than automatically
resending. Provider draft creation is not delivery acceptance. Train local send
history only after acceptance; do not infer acceptance from a draft alone.

## Phased delivery and acceptance gates

These are rollout milestones, not the detailed implementation task plan. After
design approval, write the file-by-file implementation plan with test commands.

1. **Feasibility and dependency decisions.** Minimal isolated probes (only after
   approval): Android builds for chosen Gmail/People and Proton paths, dependency
   and license audit, Google authorization flow and registration feasibility,
   Proton login/challenges/resume/key unlock/read/send/attachments/events using
   disposable accounts. Document security gaps, upstream/fork strategy, and honest
   application identification. No broad schema refactor until Proton viability
   and Android auth are established. Stop for a user decision if these fail.
2. **Contracts and compatibility scaffolding.** Pige DTOs, typed errors, optional
   services, provider registry, fake provider and contract suite, account/config
   upgrade, secret-envelope and cancellation/account-generation safeguards.
3. **Cache and existing protocol migration.** Transactional identity/membership
   schema, preserved local IDs, migrate IMAP sync/body/mutations/submission behind
   the contracts. Existing desktop/Android behavior and migration fixtures pass
   before native accounts are exposed.
4. **Provider-neutral recipient cache.** Learn current cached headers and accepted
   sends, historical crawling interfaces/checkpoints, local lookup, clear/rebuild
   semantics, and compose autocomplete. Validate on existing IMAP accounts.
5. **Gmail + Google contacts.** Native OAuth onboarding, snapshot/history sync,
   labels/mutations/body/attachments/send, People saved + Other contacts sync,
   history crawl, and production consent/release checklist. Existing IMAP accounts
   stay as-is unless the user explicitly switches backends.
6. **Proton + contacts.** Native Android/Linux login, protected sessions/keys,
   normalized decrypted reader content, send packaging/attachments, event sync,
   supported contact metadata, full history crawling, explicit security and
   unsupported-account handling. Reuse contracts, not Gmail-specific policies.
7. **Hardening and release.** Large-mailbox measurements, interruptions, quota and
   expiry recovery, regression checks, migration recovery instructions, account
   removal/privacy behavior, on-device runs, and provider maintenance playbook.

Do not attach calendar/CardDAV/editable contacts to these milestones; the
interfaces should leave room for those later without delaying v1 parity.

## Verification strategy

- Establish current workspace tests/build baseline when implementation begins;
  do not assume historical test counts in `docs/technical-notes.md` are current.
- Contract tests run against fakes and each adapter's mocked protocol transport:
  capabilities, pagination, invalid cursor, stable identity, labels, deletion,
  body/attachment limits, typed errors, partial batches, and Sent policy.
- Fixture-based old-config and old-database migrations preserve accounts, local
  IDs, bodies, attachments, FTS hits, drafts, and every outbox state. Interrupt
  migration and reopen; reject newer schema safely. Test backend-switch recovery.
- Auth tests: PKCE/state, cancellation, denied scopes, challenge progression,
  refresh rotation and concurrency, persistence failures, process death, logout,
  account removal races, and redaction. Crypto fixtures include proof verification,
  multiple/address keys, encrypted attachments, MIME, and signature verdicts.
- Autocomplete tests: all header roles and available Bcc, header-only crawling,
  all-history resume, deduped label evidence, UID epoch reset, trusted names,
  blocked/spam suppression, normalization, account isolation/removal, clearing,
  stale compose responses, offline operation, and no implicit recipient insertion.
- Benchmark indexed local lookups at 100k unique addresses; target p95 under
  50 ms on the reference Android device, measured rather than assumed. Also
  measure mailbox UI responsiveness, memory, disk growth, APK size, and indexing
  progress under throttling. Adjust batch sizes without silently truncating scope.
- Run `cargo fmt --all --check`, `cargo test --workspace`,
  `cargo clippy --workspace --all-targets`, and the repository Android release
  build (`JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release`) where available.
- Opt-in live tests require disposable Google/Proton accounts, OAuth registrations,
  and device access. Explicitly record missing prerequisites, unsupported account
  types, and tests not run; mocked success is not live interoperability proof.

## Decisions left to implementation and approval checkpoints

The builder may choose exact Rust trait signatures, module splits, migration
numbers, SQL/index layouts, coroutine vs generated-client mechanics, batching
constants, and ranking weights within this design. Select final crate versions
and Proton extraction strategy only from feasibility evidence.

Return to the user if supported Android OAuth requires a new distribution
dependency/server, Proton requires unacceptable access/security/licensing
concessions, the fork is no longer small, or encrypted local storage becomes a
release requirement. Do not silently substitute Bridge or drop Android support.

## Primary references (reviewed 2026-11-07)

- Gmail sync: https://developers.google.com/workspace/gmail/api/guides/sync
- Google native auth: https://developers.google.com/identity/protocols/oauth2/native-app
- Android authorization: https://developer.android.com/identity/authorization
- People API: https://developers.google.com/people
- Other contacts: https://developers.google.com/people/v1/other-contacts
- Other-contact search/scope: https://developers.google.com/people/api/rest/v1/otherContacts/search
- Gmail generated client: https://github.com/Byron/google-apis-rs/tree/main/gen/gmail1
- I/O Gmail: https://github.com/pimalaya/io-gmail
- Gmail fluent client: https://github.com/libninjacom/gmail-rs
- RightKit: https://github.com/bogusyogi/rightkit/tree/main/crates/rightkit-google-apis
- Proton high-level candidate: https://github.com/filippofinke/protonmail-rs
- Candidate contacts source: https://github.com/filippofinke/protonmail-rs/blob/main/crates/proton-core/src/api/contacts.rs
- Official Proton mail: https://github.com/ProtonMail/clients/tree/main/project/mail/rust
- Official Proton crypto: https://github.com/ProtonMail/proton-crypto-rs
- Proton SRP explanation: https://proton.me/blog/encrypted-email-authentication
- Proton API alternative: https://github.com/LeanderBB/proton-api-rs
- Proton account alternatives: https://github.com/narrrl/proton-sdk-rs and https://github.com/tirbofish/proton-sdk-rs2

## Design workflow checklist

- [x] Inspect existing code and integration options.
- [x] Clarify Android, no Bridge, extensibility, contacts scope, and history learning.
- [x] Present alternatives and record the user's adapter approach selection.
- [x] Write the full proposed design and phased rollout.
- [x] Fresh builder review; resolve significant planning gaps.
- [x] User approval of this written design and recommended defaults.
- [x] Write detailed implementation plan after approval: `../plans/2026-11-07-mail-providers.md` and linked subplans.

Builder review identified two gaps, now resolved as proposed conservative defaults:
fail closed on unsupported Proton recipient encryption policy, and block backend
switching until original-backend submissions and Sent-copy repairs are resolved.
