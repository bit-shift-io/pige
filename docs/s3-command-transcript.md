# S3 native-Proton feasibility — sanitized command transcript

Committed, sanitized raw evidence for the build/link/gate claims in
`docs/provider-feasibility.md` §10 (Task S3). It records exact commands and
concise captured outcomes only. It contains **no** account identifiers,
passwords, TOTP seeds, tokens, keys, mailbox contents, or personal home paths;
secret values appear as shell placeholders and host paths as variables.

Two capture sessions are recorded:

- **A — Extraction build re-verification** (2026-11-08, during the S3 review
  fix). Re-run so the extraction build, link and gate are reproducible from this
  file instead of from disposable `/tmp` logs.
- **B — Committed-workspace gates** (2026-11-08, branch HEAD `8589d8a` after the
  fix). The provider boundary gate and the full workspace test suite.

Environment: host `x86_64`; `rustc`/`cargo` 1.98.1; Android NDK **r28c**
(`28.2.13676358`); target `aarch64-linux-android` (API 34). `$PROBE_ROOT` is a
disposable scratch directory outside the repository; the extracted fork is the
§10.4 7-file extraction patch plus probe-only synthetic tests added under
`crypto/message.rs` (`proton-core 0.1.2` on `proton-crypto 0.14.1`, `rustpgp`).

---

## A. Extraction build re-verification

### A1. Extracted fork — host unit tests

```bash
cd "$PROBE_ROOT/proton-core-fork"
cargo test --lib
```

Captured outcome (tail):

```
test result: ok. 48 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.50s
```

### A2. Extracted fork — `aarch64-linux-android` release build

```bash
export ANDROID_NDK_HOME="$ANDROID_SDK_ROOT/ndk/28.2.13676358"
TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TOOLCHAIN/aarch64-linux-android34-clang"
export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
export AR_aarch64_linux_android="$TOOLCHAIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"

cd "$PROBE_ROOT/proton-core-fork"
cargo build --release --target aarch64-linux-android
```

Captured outcome (tail):

```
warning: `proton-core` (lib) generated 2 warnings (run `cargo fix --lib -p proton-core` to apply 2 suggestions)
    Finished `release` profile [optimized] target(s) in 0.26s
```

### A3. Android shared object (cdylib link)

```bash
cd "$PROBE_ROOT/proton-fork-cdylib"   # path-depends on proton-core-fork
cargo build --release --target aarch64-linux-android
file target/aarch64-linux-android/release/libproton_fork_probe.so
stat -c '%s bytes' target/aarch64-linux-android/release/libproton_fork_probe.so
```

Captured outcome:

```
target/aarch64-linux-android/release/libproton_fork_probe.so: ELF 64-bit LSB shared object, ARM aarch64, version 1 (SYSV), dynamically linked, for Android 34, built by NDK r28c (13676358), not stripped
460712 bytes
```

### A4. Boundary gate on the extracted graph

The gate must be evaluated against the target's **resolved** graph. An
*unfiltered* `cargo metadata` also lists inactive *optional* dependencies
(`gopenpgp-sys`) and therefore false-positives the gate; pass
`--filter-platform <target>` (matching the build) so only the real graph is
checked.

```bash
cd "$PROBE_ROOT/proton-core-fork"
cargo metadata --format-version 1 --filter-platform aarch64-linux-android \
  > "$PROBE_ROOT/evidence/fork-metadata.json"
cargo tree -e features > "$PROBE_ROOT/evidence/fork-tree.txt"

cd <repo>
python3 scripts/check-provider-boundaries.py \
  --metadata "$PROBE_ROOT/evidence/fork-metadata.json" \
  --tree "$PROBE_ROOT/evidence/fork-tree.txt"
```

Captured outcome (stderr provenance warnings omitted):

```
provider boundary check: OK (338 resolved packages checked)
```

The feature-resolved tree contains `proton-crypto feature "rustpgp"` only and
zero occurrences of `gopenpgp-sys`; the extracted graph also has no `rusqlite`,
`sqlite3` or `keyring` package. (An earlier disposable run of the same gate
recorded `326` resolved packages from a different manifest/feature snapshot; the
count here differs only because this re-run keeps the manifest's
`dev-dependencies`.)

---

## B. Committed-workspace gates at branch HEAD `8589d8a`

### B1. Provider boundary gate

```bash
cd rust
cargo metadata --format-version 1 > "$PROBE_ROOT/evidence/workspace-metadata.json"
cargo tree -e features > "$PROBE_ROOT/evidence/workspace-tree.txt"
cd ..
python3 scripts/check-provider-boundaries.py \
  --metadata "$PROBE_ROOT/evidence/workspace-metadata.json" \
  --tree "$PROBE_ROOT/evidence/workspace-tree.txt"
```

Captured outcome:

```
provider boundary check: OK (619 resolved packages checked)
```

### B2. Full workspace test suite

```bash
cd rust
cargo test --workspace
```

Captured outcome (aggregate over the 20 test binaries):

```
20 suites
360 passed; 0 failed; 22 ignored
```

Representative tail:

```
     Running tests/threading.rs
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
   Doc-tests pige
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
   Doc-tests pige_core
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

---

## Capture note: tmpfs link failure

Running the full workspace build with `CARGO_TARGET_DIR` pointed at the small
`/tmp` tmpfs (16 GB) fails to link the `pige` GUI target when the tmpfs fills
(`No space left on device`); `cargo` reports it as a link failure. It is an
environment/space artifact, not a code defect. Keep the workspace target
directory on the larger `/home` filesystem (as done for B2).
