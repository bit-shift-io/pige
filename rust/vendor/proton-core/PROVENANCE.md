# Vendored `proton-core` provenance

Pige vendors a narrow, source-only copy of the `proton-core` crate because the
published crate owns local persistence we must not delegate (a second bundled
SQLite link, an OS-keyring session store, and on-disk session files). Pige owns
all cache, secrets, jobs, and cursors; this vendor copy is used only for its
protocol, SRP, and OpenPGP implementation.

## Upstream

- Repository: <https://github.com/filippofinke/protonmail-rs>
- Crate: `proton-core`
- Version: `0.1.2`
- Commit: `67044505aa2931f387b262f37e3c8ad64a6a126c`
- License: MIT (see `LICENSE`; copyright Filippo Finke and contributors)
- Vendored path in this repository: `rust/vendor/proton-core/`

Only the upstream crate's `src/` and `Cargo.toml` (plus its `README.md` for
reference) are copied. `Cargo.lock`, `target/`, probe scripts, and the CLI/MCP
crates are deliberately not vendored.

## Modifications from upstream 0.1.2

This copy matches the S3-approved extraction patch inventory in
`docs/provider-feasibility.md` §10.4:

- Deleted `src/cache.rs` (SQLite cache + FTS). Pige owns persistence.
- Deleted `src/mail/sync.rs` and its `pub mod sync;`; Pige will own event sync
  and cursors in P2.
- `src/lib.rs`: dropped `pub mod cache;`, the tracing-subscriber initializer,
  and the `SyncReport` re-export.
- `src/session/secret_store.rs`: dropped the OS-keyring `KeyringStore`; kept the
  `SecretStore` trait and the in-memory implementation.
- `src/session/mod.rs`: dropped the `directories`-based `Paths::system`; a host
  supplies a base directory explicitly. Pige does **not** call this module.
- `src/mail/mod.rs`: removed `pub mod sync;` and uses the injected store/paths.
- `Cargo.toml`: removed `directories`, `keyring`, `rusqlite`, and
  `tracing-subscriber`; pinned `proton-crypto 0.14.1` with `rustpgp` and
  `default-features = false`, and `proton-srp 0.8`; removed upstream live /
  wiremock test targets.
- `src/api/contacts.rs` (P3): `Contact`/`ContactEmail` retain `Cards`, `UID`
  and unknown policy fields so encrypted contact policy can be read
  adapter-privately rather than discarded by the public name/email projection.
- `src/crypto/message.rs` (P3): added bounded body/attachment decryption helpers
  used by Pige to reject decompression bombs before materializing plaintext.
- `src/mail/send.rs` (P4): removed the silent `Type 4` cleartext fallback for
  keyless external recipients. A recipient with no usable key and no explicit
  encrypted-outside password now fails with an error instead of being sent in
  cleartext. Pige's submission path resolves contact/key policy itself and never
  calls this high-level send pipeline.

No further behavioural changes are made. The vendored `session` module is left
in the source for API completeness but Pige's Proton adapter never calls it:
Pige writes its own `ProtonSessionSecret` envelope through F3's
`SecretRepository`, and no Pige secret is ever written by this crate.

## Updating

Follow `docs/provider-feasibility.md` §10.4 ("Upkeep budget"): re-apply the
patch set above on the new pinned upstream revision, keep `proton-crypto` on the
S1-approved version until a checked bump, re-run the aarch64 build, the
dependency boundary gate, and the synthetic crypto fixtures, and retain a
rollback to the last green revision. Any upstream change that reintroduces
persistence ownership or a second native `links` slot is a stop-and-return
trigger.
