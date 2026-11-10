# Provider release verification

This page separates **deterministic, already-executed evidence** from **live
provider and device evidence**. A row is `PASS` only when the stated command or
checklist step was actually run for this release candidate. An unexecuted row is
`BLOCKED`, even when the mocked suite covers the same code path.

No credentials, account data, tokens, or message content belong in this page or
in the repository. Synthetic fixtures prove redaction and wiring; they do not
prove interoperability with a live provider.

## Release candidate

- Branch: `feat/mail-providers`
- Workspace version: `2026.11.5`
- Evidence directory: `.superpowers/sdd/2026-11-07-mail-providers/h3-evidence/`
- Device used for the non-destructive launch check: Android `6bfcc911`
  (attached, holds the user's real mail; no message or account data was
  modified).

## Deterministic matrix

Commands run from the repository root. Full output is retained in the evidence
directory.

| Check | Command | Result | Evidence |
| --- | --- | --- | --- |
| Rust formatting | `cd rust && cargo fmt --all --check` | PASS | `format.txt` |
| Default workspace tests | `bash scripts/verify-providers.sh default` | PASS | `verify-default.log` |
| All-features workspace tests | `bash scripts/verify-providers.sh all` | PASS | `verify-all.log` |
| Clippy (default) | inside `verify-providers.sh default` | PASS | `verify-default.log` |
| Clippy (all features) | inside `verify-providers.sh all` | PASS | `verify-all.log` |
| No provider features | `cargo check -p pige_core --no-default-features` | PASS | `verify-default.log` |
| Google feature | `cargo check -p pige_core --features provider-google` | PASS | `verify-default.log` |
| Proton feature | `cargo check -p pige_core --features provider-proton` | PASS | `verify-default.log` |
| Both providers | `cargo check -p pige_core --features provider-google,provider-proton` | PASS | `verify-all.log` |
| All features | `cargo check -p pige_core --all-features` | PASS | `verify-all.log` |
| Dependency/license gate | `scripts/check-provider-boundaries.py` through the verify script | PASS | `verify-all.log` |
| Privacy regressions | `cargo test -p pige_core --all-features --test provider_privacy` | PASS | `provider-privacy.log` |
| Desktop release build | `cargo build --locked -p pige --bin pige` (release profile through `scripts/release.sh desktop`) | PASS | `desktop-release.log` |
| Android release build | `JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release` | PASS | `android-release.log` |
| Released desktop artifact | `dist/pige-2026.11.5-linux-x86_64` | PASS | `release-artifacts.txt` |
| Released Android artifact | `dist/pige-2026.11.5-android-arm64.apk` | PASS | `release-artifacts.txt` |

The verify script is intentionally credential-free and never runs an ignored
live test. It refuses `PIGE_TEST_ALLOW_SEND=1`, so the ordinary default run can
never send mail.

## Live provider matrix

Each row requires a disposable account, explicit authorization, and the listed
environment. Missing prerequisites are recorded as `BLOCKED`; they are not
inferred from mocks.

| Provider case | Prerequisite | Result |
| --- | --- | --- |
| Google read/sync (`live_gmail_read_and_sync`) | `PIGE_TEST_GOOGLE_CLIENT_ID`, `PIGE_TEST_GOOGLE_REFRESH_TOKEN`, `PIGE_TEST_GOOGLE_ACCOUNT_EMAIL`, disposable test-user account | BLOCKED |
| Google contacts denial (`live_contacts_denial_keeps_mail`) | mail-only token without People scopes and `PIGE_TEST_GOOGLE_CONTACTS_DENIED=1` | BLOCKED |
| Google send (`live_gmail_send_requires_explicit_opt_in`) | the read prerequisites plus `PIGE_TEST_ALLOW_SEND=1` and a verified self-recipient | BLOCKED |
| Proton read/sync (`live_proton_read_and_sync`) | `PIGE_TEST_PROTON_EMAIL`, `PIGE_TEST_PROTON_ACCOUNT_PASSWORD`, optional mailbox password/TOTP/human-verification token, disposable account | BLOCKED |
| Proton send (`live_proton_send_requires_explicit_opt_in`) | Proton read prerequisites plus `PIGE_TEST_ALLOW_SEND=1`; recipient is the test account itself | BLOCKED |
| Proton logout/key lock (`live_proton_logout_locks_keys`) | Proton read prerequisites | BLOCKED |
| IMAP/SMTP live read and send (`live_imap`) | `PIGE_TEST_IMAP_HOST`, `PIGE_TEST_IMAP_USER`, `PIGE_TEST_IMAP_PASSWORD`; send also needs `PIGE_TEST_SMTP_HOST`, `PIGE_TEST_TO`, `PIGE_TEST_SEND=1` | BLOCKED |
| Google OAuth registration | Cloud project, Desktop + Android clients, consented scopes, verification status, release SHA-1 registration | BLOCKED |
| Proton API compatibility | Current disposable account and completed fork-upgrade checks | BLOCKED |

A live test invoked with `--ignored` but missing its environment fails loudly.
Do not run live sends against a personal or production mailbox. Do not report a
mocked response as live evidence.

## On-device checklist

The attached device holds real mail. Only non-destructive install/launch checks
were performed; every row that changes accounts, messages, contacts, or
credentials is `BLOCKED` until the user authorizes a disposable-account run.

| Device case | Status | Note |
| --- | --- | --- |
| Install release APK and launch/smoke (no data change) | PASS | `device-smoke.log`; app started, process observed, no message/account mutation |
| Google/Proton login and verification | BLOCKED | needs disposable live account and consent |
| Token refresh/rotation and re-authorization | BLOCKED | needs live grant |
| Process kill and resume | BLOCKED | needs disposable account |
| Offline reader and recipient autocomplete | BLOCKED | needs disposable account data |
| Long-history progress | BLOCKED | needs disposable account |
| Contacts denial and source deletion | BLOCKED | needs live contacts grant |
| Alias/Bcc send inspection | BLOCKED | needs authorized live send |
| Failed/tampered signature states | BLOCKED | needs disposable Proton fixture account |
| Token revocation | BLOCKED | needs live grant |
| Worker deadline/cancellation | BLOCKED | needs live sync workload |
| Clear, rebuild, remove account, and reset | BLOCKED | destructive to real data; run only on disposable account |

Performance evidence for the large address cache and history indexing is in
[`provider-performance.md`](provider-performance.md) and
[`provider-performance-device.log`](provider-performance-device.log).

## Known limitations to disclose

- Cached mail is plaintext at rest; clearing is logical deletion, not forensic
  erasure.
- Native Gmail and Proton interoperability is proven by the ignored live tests
  when those accounts are available, not by the deterministic suite.
- Google distribution requires OAuth verification/consent upkeep; Proton uses a
  private/unsupported API surface and does not claim official-client security
  parity.
- The deterministic matrix proves the built code and dependency boundary, not
  that Google or Proton currently accepts a real login.
