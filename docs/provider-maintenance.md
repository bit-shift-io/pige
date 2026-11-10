# Provider maintenance: migration, recovery, and local data

This document describes what happens to existing accounts when pige moves
between provider backends, how protected credentials are stored, what is (and is
not) encrypted locally, and which caches the lifecycle operations clear.

The provider foundation keeps the existing IMAP/SMTP behaviour: an upgraded
installation continues to read and send through the provider boundary with the
same accounts, drafts, queued mail, cached reader content, search, threading,
attachments, and snapshots. Native Gmail/Proton actions stay gated until their
adapters are registered in the process-wide provider registry.

## Versioned configuration migration

`pige.json` is the source of truth for accounts and settings. Its format is
versioned:

- **v1** stored `username`/`imap`/`smtp`/`auth` inline on each account.
- **v2** stores a typed `backend` plus `contact_sources`.

A v1 document is read through its own wire reader and migrated to v2 in memory
before the first write. The account id, label, identity, endpoints, server
security mode, sort order, and colour are preserved exactly. An account that was
manually configured to use Gmail's IMAP servers stays an IMAP/SMTP account; it is
never silently converted into a native Gmail account. A native backend never
receives an invented IMAP or SMTP endpoint.

A document written by a newer build is rejected before anything is saved. The
file is left byte-for-byte intact and configuration writes are blocked until the
user runs a build that understands it.

The SQLite cache is migrated stepwise and transactionally. Upgrades preserve
local `MessageId`s, body and attachment links, FTS entries, and thread
membership. A schema newer than the running build is rejected rather than
downgraded. The current schema includes the durable account generation/lease,
generation-fenced sync checkpoints, provider remote mappings, structured outbox
submission, and recoverable backend-switch intents.

## Credential storage

Passwords, refresh tokens, and other protected material live only in the
platform credential store: Secret Service on Linux, the Keystore on Android, or
the in-memory fake used by tests. They never appear in `pige.json`, SQLite, FTS,
logs, or snapshots.

Stored credentials use a versioned `SecretEnvelope`. A bare string written by an
older build is read back as a legacy password without loss and is rewritten as
an envelope only when the account next saves a credential. An unreadable or
newer-than-supported envelope is a hard error rather than a second guess.

Credential writes are fenced by the account's durable generation and
cross-process lease, so a refresh that lost its session cannot persist a rotated
secret after removal or re-authentication. Interactive authentication stages a
candidate under a `pending_<attempt>` secure-store reference; it is committed
only by account creation or a guarded backend switch, and it is discarded on
completion, cancellation, expiry, or process startup sweep.

When the OS credential store is unavailable, the UI may store a session-only
credential. That value lives in process memory only: it is gone after a restart,
and the user must sign in again. A persistent write is never silently downgraded
to session-only.

## Local plaintext cache limitation

The mail cache is **not encrypted at rest**. Cached headers, plain-text and HTML
bodies, the FTS index, attachment bytes, and outbox drafts are stored in
`pige.db` and below the platform cache directory. On desktop these files are
protected by the operating-system user account and file permissions, not by an
application-level key. On Android they live in app-private storage and are
protected by the platform sandbox and device encryption.

This means a local attacker who can read the app's data directory can read cached
mail. Removal, restore, and factory reset delete the corresponding database rows;
factory reset also clears the cache directory. A backend switch removes only the
provider-derived cache described below, so any residual attachment file whose
row was already missing should be treated as local plaintext until the cache is
cleared manually or by a reset.

## Backend switch

A user-triggered backend switch is the only way an existing account changes
provider. It is refused while any submission for the account is not final:

- `queued`, `preparing`, and `submitting`;
- `accepted` but not fully finalized (the Sent copy is still pending);
- `uncertain` (the write may have reached the provider);
- legacy `sending`, `failed`, and `sent_local` rows, which are decoded
  conservatively.

A draft, a completed delivery, and an evidenced rejection do not block the
switch. Resolve a pending submission with its original backend first; an SMTP
Sent-copy repair is never reinterpreted as a native API send.

A successful switch:

1. cancels every live context for the account and advances its durable
   generation;
2. waits for the account's work gate to drain;
3. writes a non-secret switch intent and a rollback copy of the previous
   credential in the secure store, then records `BackedUp`;
4. writes the new active credential and records `CredentialWritten` only after
   that write returned success;
5. publishes the new account backend in `pige.json`, then records
   `ConfigWritten`;
6. clears only provider-derived cache and checkpoints, then records `Completed`;
7. removes the intent *before* discarding the rollback copy, so a crash in
   between can only leave a temporary reference for the startup sweep. If
   restoring the old credential fails, the intent, backup, and candidate are
   kept so startup can repair the pair.

The persisted phase records what has actually been **confirmed**, never what is
about to be attempted. In particular, a phase below `CredentialWritten` is never
treated as evidence that the new credential is active.

A session-only switch deletes the previous persistent OS entry before storing the
new session credential: `get` prefers the persistent entry, so leaving it would
make the new backend read the old secret. The rollback copy is written first, so
the old credential is never lost.

The account id, drafts, outbox rows, and learned data in other tables survive.
The provider remote mappings (`remote_messages`, `remote_collections`), cached
folders, messages, threads, bodies, attachments, FTS rows derived from that mail,
and sync/contact checkpoints are rebuilt from the new backend. A thread row that
a surviving draft still references is retained rather than left dangling.

A same-backend re-authentication keeps the endpoints and contact sources and
does not clear the mail cache. Replacing one contact source clears only that
source's checkpoint; other sources' checkpoints and all mail remain.

### Crash recovery

The switch intent is written before any credential or configuration is
activated, and its phase is advanced only after the matching step is confirmed:

- **`BackedUp`** — the rollback reference exists (or the account had no
  credential). Startup can safely roll back, but must not assume the active
  credential changed.
- **`CredentialWritten`** — the new active credential write returned success.
  A crash before config publication restores the previous credential from the
  rollback reference and keeps the old backend.
- **`ConfigWritten`** — the target backend is published and matches the new
  credential. Startup completes forward and clears the previous provider cache
  (or only the replaced contact-source checkpoint for a same-backend switch).
- **`Completed`** — the cache clear finished; startup only retires the intent.
- **After config publication with a session-only credential** — the session
  secret cannot survive a restart, so startup restores the previous backend and
  credential rather than leaving a new backend with no or stale credentials.

Forward recovery verifies that the target credential is actually present (or
reactivates a still-staged candidate); if it cannot, it rolls the configuration
back instead of pairing the new backend with the old secret.

The invariant is that a new backend is never paired with old credentials (or the
inverse) without a surviving intent and a retry path. If a restore, delete, or
cache clear fails — including `NoStorageAccess` where the entry may still exist —
the intent and backup are kept and the operation is retried at the next start.
Only a delete that returned success (or a verified absence) counts as cleanup.
Startup reports a warning for any incomplete recovery. Removing the intent before
discarding the rollback reference guarantees a crash cannot leave an intent whose
recovery data is already gone.

The startup orphan sweep retries rollback-reference cleanup for every account id
still named by `pige.json` or the SQLite mirror. A reference whose account id has
already disappeared from both stores cannot be enumerated through the OS
credential store; that residual is documented rather than silently assumed gone.
Factory reset reports a warning when an explicit cleanup cannot be completed.

## Restore, removal, and factory reset

- **Restore** cancels the current generation of every existing account before
  rebuilding the account mirror, then reconciles any interrupted switch against
  the restored account shapes: a restored pre-switch backend gets the old
  credential back, a restored target backend keeps the new one, and an unrelated
  backend has its active credential removed and must sign in again. Remaining
  intents are cleared only after that pairing is confirmed. Accounts dropped by
  the backup lose their credentials, jobs, cache, and rollback backups.
  Surviving accounts keep their cache but any work that started before the
  restore is fenced out.
- **Removal** discards a pending switch, advances the generation, fences
  credential writers, deletes the secret and any rollback backup, removes the
  account row (cascading its cache), and clears its jobs and pending-auth
  references. The generation tombstone remains so a number is never reused and a
  late writer cannot win.
- **Factory reset** discards switch intents, cancels every generation, deletes
  accounts, secrets, rollback backups, jobs, leases, pending-auth references, and
  the cache directory. It leaves no credential or job record behind for the
  previous configuration.

At startup, a rollback backup whose intent no longer exists is swept for every
account the configuration or SQLite mirror still names, so a crash between
cache clearing and reference cleanup cannot leave an unreachable secret.

Account edits advance the generation too: a sync, refresh, prefetch, or crawl
that started before the edit cannot persist its result, and a credential write
from the old generation is rejected.

## Operational checklist after an upgrade

1. Start the app once with the new build so the schema and configuration
   migration run.
2. Confirm the account list, folder counts, drafts, and cached reader content
   are unchanged.
3. Send one test message through an ordinary IMAP/SMTP account (live-send tests
   are separate from the offline suite and require explicit authorization and a
   working server).
4. If startup reports an interrupted switch, follow its instruction: sign in
   again only when the warning says a credential could not be restored; never
   manually copy a password into `pige.json` or the database.
5. Treat the local cache as plaintext when assessing device backup and
   compromise risk; factory reset is the documented way to remove all local
   mail data.

## Privacy regression suite

`tests/provider_privacy.rs` is the repeatable guard for the boundaries above.
It uses synthetic markers and asserts:

- logs, `ProviderError` `Display`/`Debug`, configuration, and backups never
  contain a stored secret or provider `source_detail`;
- a configuration backup contains account settings but no synced contact names,
  contact addresses, or learned correspondent addresses;
- clearing learned recipients survives a simulated process restart with history
  and cache backfill still disabled, and a later foreground request does not
  open the provider;
- removing an account cascades its contact, correspondent, recipient-index,
  learning-state, contact-source-state, job, and pending-auth rows;
- the outbox and its prepared artifact contain no session token or private key;
- a forbidden contact scope disables only that source and is not retried by the
  background scheduler.

These tests do not claim that cached mail is encrypted at rest or that a
logical clear is a secure erase. They prove that private payloads do not cross a
log/config/backup/outbox boundary.

## Deterministic verification and release evidence

Run the release gate from the repository root:

```sh
bash scripts/verify-providers.sh default
bash scripts/verify-providers.sh all
```

The script checks formatting, the workspace tests, clippy, every provider
feature combination, `--no-default-features`, and the dependency/license
boundary gate. It uses `--locked`, never reads credentials, never runs an
ignored live test, and refuses `PIGE_TEST_ALLOW_SEND=1`. Building the desktop
and Android release artifacts is a separate authorized step; see
[`provider-release-verification.md`](provider-release-verification.md) for the
PASS/FAIL/BLOCKED matrix and the current device/live status.

## Adding or replacing a provider

[`adding-a-mail-provider.md`](adding-a-mail-provider.md) is the extension
recipe: capability advertisement, versioned remote ids and cursors, initial/
delta/history recovery, Sent-copy and unknown-outcome policy, shared tests,
auth/privacy/license checks, and safe SDK replacement. It also documents the
Proton fork upgrade checks and the OAuth signing/consent upkeep that a release
requires.
