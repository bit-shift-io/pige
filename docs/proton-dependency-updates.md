# Maintaining the vendored Proton dependency

Pige does not use the published `proton-core` crate directly. It vendors a
narrow, source-only fork under `rust/vendor/proton-core/` because the upstream
crate owns persistence Pige must own itself (a second bundled SQLite link, an
OS-keyring session store, and on-disk session files).

## Pin and provenance

| Item | Value |
| --- | --- |
| Upstream repository | `https://github.com/filippofinke/protonmail-rs` |
| Upstream crate/version | `proton-core 0.1.2` |
| Upstream commit | `67044505aa2931f387b262f37e3c8ad64a6a126c` |
| Vendored path | `rust/vendor/proton-core/` |
| License | MIT (`rust/vendor/proton-core/LICENSE`) |
| Source record | `rust/vendor/proton-core/PROVENANCE.md` |
| Cryptography backend | `proton-crypto 0.14.1`, `default-features = false`, feature `rustpgp` |
| SRP backend | `proton-srp 0.8.4` |
| Workspace membership | excluded (`rust/Cargo.toml` `exclude`) |

Only `src/`, `Cargo.toml`, `README.md` and `LICENSE` are forked. `Cargo.lock`,
`target/`, probe scripts, CLI/MCP crates and upstream live/wiremock tests are not
vendored. The generated vendor `Cargo.lock` is ignored by Git; the committed
workspace lock (`rust/Cargo.lock`) is authoritative for product builds.

## Maintained patch inventory

Re-apply exactly this list when moving the pin. Each patch exists for a security
or ownership reason; do not "simplify" one away during an upgrade.

| File | Patch | Reason |
| --- | --- | --- |
| `src/cache.rs` | deleted | Pige owns SQLite cache/FTS; removes a second persistence owner. |
| `src/mail/sync.rs`, `src/mail/mod.rs` | deleted / no re-export | P2 owns event sync and cursors. |
| `src/lib.rs` | remove cache module, tracing initializer and sync re-export | No process-global logging or cache ownership. |
| `src/session/secret_store.rs` | OS-keyring store removed | F3 `SecretRepository` is the only credential writer. |
| `src/session/mod.rs` | `directories`-based `Paths::system` removed | A host supplies paths; Pige never calls this module. |
| `Cargo.toml` | remove `directories`, `keyring`, `rusqlite`, `tracing-subscriber`; pin `proton-crypto`/`proton-srp`; remove upstream test targets | No second native `links` slot, no unused persistence, smaller Android artifact. |
| `src/api/contacts.rs` | retain cards, UID and unknown policy fields | Encrypted contact policy must be readable adapter-privately (P3/P4). |
| `src/crypto/message.rs` | bounded `decrypt_body_bounded` / `decrypt_attachment_bounded` | Streaming `Read::take(max+1)` rejects decompression bombs before materializing plaintext. |
| `src/mail/send.rs` | remove the silent `Type 4` cleartext fallback for keyless external recipients | P4 resolves policy itself and never downgrades to cleartext. |

The fork's high-level `session` module remains for API completeness but is dead
code for Pige. No Pige token, key or passphrase is ever written by the vendored
crate.

## Upgrade procedure

1. Record the last known-good commit and fixture result:
   ```sh
   git rev-parse HEAD
   cargo test --manifest-path rust/vendor/proton-core/Cargo.toml
   ```
2. Check out the candidate upstream revision in a scratch directory
   (`/tmp/opencode` is the approved scratch root). Diff `src/` against the fork
   and re-apply every row in the patch inventory. Do not copy `Cargo.lock`,
   `target/`, tests that require live Proton credentials, or any new persistence
   dependency.
3. Verify the dependency surface *before* running the app:
   ```sh
   cd rust
   cargo metadata --format-version 1 --filter-platform aarch64-linux-android \
     --features provider-proton >/tmp/opencode/proton-metadata.json
   python3 scripts/check-provider-boundaries.py /tmp/opencode/proton-metadata.json
   ```
   Stop if `gopenpgp`, a second `rusqlite`/SQLite link owner, `keyring`, or a
   new target/keyring path appears.
4. Build and test the fork and the adapter:
   ```sh
   cargo test --manifest-path rust/vendor/proton-core/Cargo.toml
   cargo test -p pige_core --features provider-proton \
     --test proton_auth --test proton_crypto --test proton_mailbox --test proton_submission
   cargo test -p pige_core --features provider-proton
   ```
   The synthetic SRP/OpenPGP/contact fixtures must stay green. They are the
   compatibility gate; live tests are additional evidence, not a substitute.
5. Build both release targets:
   ```sh
   cargo build -p pige --features provider-proton
   cargo check -p pige_core --features provider-proton --target aarch64-linux-android
   cargo check -p pige --features provider-proton --target aarch64-linux-android
   ```
6. Update `rust/vendor/proton-core/PROVENANCE.md` with the new upstream commit
   and any patch-row changes. Commit the vendor source and record the exact
   hash in the commit message.
7. Only then run the ignored live checklist in
   [`proton-support.md`](proton-support.md) with a disposable account.

## Crypto feature rules

- Keep `proton-crypto` on the S1-approved version with
  `default-features = false, features = ["rustpgp"]`. The `gopenpgp` backend is
  not permitted (extra native surface, license/boundary risk).
- Do not enable `proton-crypto` defaults transitively. Check
  `cargo tree -p pige_core --features provider-proton`.
- Do not add a second SQLite link owner. If an upstream change makes `rusqlite`
  mandatory, stop and revise the fork or scope with the user.
- Keep decrypted body/attachment output caps before any cache write. A backend
  that cannot stream should still be bounded with `Read::take(max + 1)` where
  the API allows it.

## Rollback on upstream breakage

If a candidate fails SRP/modulus verification, key unlock, contact-card policy,
body decryption, attachment integrity, the boundary gate, or the aarch64 check:

1. Do **not** keep a partial fork.
2. `git checkout -- rust/vendor/proton-core` (or revert the vendor commit) back
   to the last green pin.
3. Re-run the synthetic fixture suites and the boundary gate.
4. Record the failure and the pinned hash in the task/release notes. A format
   the fork cannot read fails closed as `Unsupported`; never add a cleartext
   fallback to make a live probe pass.
