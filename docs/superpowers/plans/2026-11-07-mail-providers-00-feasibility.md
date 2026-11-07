# Mail Provider Feasibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decide supported dependencies/auth flows before migrating real account data.
**Architecture:** Isolated disposable probes establish Android builds and live interoperability; an evidence report freezes production choices. No probe becomes product code without subsequent tasks.
**Tech Stack:** Existing Rust/NDK/JDK toolchain; generated Google clients; native Proton Rust crypto.
**Spec:** `docs/superpowers/specs/2026-11-07-mail-providers-and-contacts-design.md`; shared vocabulary/order in `2026-11-07-mail-providers.md`.

## Global Constraints

- Proton must work directly on Android: no Bridge, Go service, or hosted mail proxy.
- Keep the project distributable under its existing MIT licensing direction.
- Never silently downgrade to plaintext.
- Build and test the actual Android target before accepting a dependency.
- No real credentials/mail in repository fixtures. User must supply disposable-account access and explicitly authorize test sends.

## Review Focus

1. A crate compiling alone but conflicting with Pige's native SQLite link must be rejected (S1).
2. Browser auth succeeding on desktop must not count as Android auth success (S2).
3. Proton internal-to-external mixed recipients must not silently lose encryption policy (S3).
4. A refreshed session must work after process death and cannot depend on a retained login password (S2/S3).
5. A CLI CAPTCHA helper requiring console snippets is not usable Android onboarding (S3).

## File structure

- Create `docs/provider-feasibility.md`: versions/features/licenses/build/live matrix, limitations, final decisions.
- Create `scripts/check-provider-boundaries.py`: dependency/feature gate, reused in final CI.
- Disposable Rust and Android probes live under `/tmp/opencode/pige-provider-probes/`, not the application workspace or committed dependencies.
- Read existing `rust/Cargo.toml`, both package manifests, `scripts/build.sh`, Android manifest/Gradle and credential/worker bridges.

### Task S1: Toolchain, dependency and licensing evidence

**Interfaces:** Consumes current locked workspace and NDK build flags; produces exact dependency versions/features/commit hashes and PASS/BLOCKED evidence in `docs/provider-feasibility.md`. Script CLI: `python3 scripts/check-provider-boundaries.py --metadata <cargo-metadata.json> --tree <cargo-tree.txt>` exits 0 only for the approved dependency rules.

- [ ] Record the pre-change baseline with `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo build --locked -p pige --bin pige`; run root `bash scripts/build.sh release` separately. Record failures as existing/new, never fix unrelated UI changes here.
- [ ] Write script unit tests in `scripts/tests/test_provider_boundaries.py`: `test_rejects_go_crypto_backend`, `test_rejects_unapproved_agpl_package`, `test_rejects_multiple_sqlite_links`, `test_accepts_approved_rust_backend`; assert exit/failure diagnostics, accounting for Cargo metadata's package IDs rather than name-substring matching.
- [ ] Run root `python3 -m unittest discover -s scripts/tests -p 'test_provider_boundaries.py'`; expect RED before script implementation.
- [ ] Implement the script; audit direct/transitive licenses and native `links`, distinguishing existing approved dependencies from new additions. In an isolated manifest using Pige's dependency versions, compare `google-gmail1`, `google-people1` or narrow People REST, and extracted `proton-core` with Go/default crypto disabled. Record extraction patch inventory and upstream source license provenance.
- [ ] Re-run the Python tests; expect GREEN. Use the linker/CC/AR/16-KiB flags from `scripts/build.sh` for real `cargo build --target aarch64-linux-android` of probe libraries and assemble a test APK. Save build commands and results (not secrets) in the report; `cargo check` alone is insufficient.
- [ ] Commit script/tests/report only: `test: add provider dependency compatibility gates`.

### Task S2: Supported Google authorization and API probes

**Interfaces:** Produces a frozen `GoogleAuthDecision` section: Desktop client registration, Android client/signing identity, supported authorization SDK/flow, refresh persistence ownership, required scopes, non-Play-services availability, and selected mail/People SDK injection points. No invented cross-platform callback URI.

- [ ] Add report checklist cases `desktop_pkce_cancel_and_state`, `android_consent_return_and_recreation`, `mail_without_contacts_scope`, `resume_after_process_restart`, `contacts_later_grant`, `revoked_grant_requires_login`; mark NOT RUN initially.
- [ ] In disposable probes reproduce these cases using Google-supported platform mechanisms; request `gmail.modify` for normal mail, `contacts.readonly` and `contacts.other.readonly` only for contacts. Confirm actual token/refresh ownership on Android before specifying the host bridge. No OOB, Android loopback listener, embedded WebView, or service-account workaround.
- [ ] Verify profile, message list/detail, history, one authorized test send, saved contacts and Other contacts against a disposable account. Confirm `gmail.modify` does not authorize immediate permanent deletion: expose that capability as false rather than request full-mail access silently.
- [ ] Test refresh after cold restart, disabled/absent Play services, cancellation and lost activity; record PASS/FAIL/BLOCKED per case. If supported Google auth requires Play services or an auth server, stop for the user's distribution decision; do not proceed by impersonating a desktop app on Android.
- [ ] Commit report update only: `docs: record Google Android authorization decision`.

### Task S3: Native Proton interoperability and security gate

**Interfaces:** Produces `ProtonDependencyDecision`: accepted source/version/features, honest app identification/access expectations, key-policy and format matrix, protected session fields, fork/upstream patch procedure, and live/device test evidence. Downstream P1-P4 cannot start without this gate.

- [ ] List synthetic/live cases in the report: one/two-password accounts, TOTP, human verification, invalid SRP proof/modulus, resumed rotating session, multiple address keys, internal/external read, MIME/attachments, mixed-recipient send, encrypted contact policy, unsupported FIDO2-only login, event expiry/reset, contact metadata, signed/unsigned/invalid signature.
- [ ] Run minimal native probes on Linux and the Android APK for each supported case. Prove that retained session plus protected unlock material resumes after a killed process. Verify CAPTCHA is solvable through an actual Android-host interaction rather than CLI console-token scraping. Record unsupported modes explicitly.
- [ ] Compare ordinary external delivery with external encryption-required/key-verification-required contacts; missing policy visibility is a blocker, not permission to send cleartext. Verify sender aliases and recipient packaging preserve privacy. Explicitly obtain user permission before test messages are sent.
- [ ] Document Key Transparency/FIDO2/format gaps and report API-access uncertainty using honest external application headers; seek maintainer clarification where required. Stop if only official-client impersonation works, policies cannot be honored, or the necessary fork is no longer small.
- [ ] Freeze exact dependencies and an upkeep budget/patch inventory in the report, or publish a BLOCKED decision with required user input. Do not relabel an upstream README claim as a reproduced test.
- [ ] Commit report update only: `docs: record native Proton feasibility and security limits`.

## Exit gate

- [ ] User reviews any product/security/distribution exceptions; unresolved gates block the broad schema migration.
- [ ] Final report links raw sanitized command evidence and distinguishes desktop build, Android link, device run, and live-account success.
- [ ] Continue to plan 01 only with supported auth, Rust-only Proton, and a maintainable/license-compatible dependency path. Crate selection may change, shared Pige contracts should not.
