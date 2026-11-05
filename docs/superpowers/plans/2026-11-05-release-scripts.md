# Release and Publish Scripts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build versioned Linux and Android release artifacts into `dist/` from a single Cargo-sourced CalVer version, and publish them as a GitHub release.

**Architecture:** Three small bash scripts. `scripts/version.sh` is the one place that reads the workspace version from Cargo. `scripts/release.sh` builds the desktop binary and the Android APK and copies them into `dist/` under versioned names. `scripts/publish.sh` tags and pushes a GitHub release from `dist/`. Gradle receives the version as properties so `build.gradle.kts` stops hardcoding it.

**Tech Stack:** bash, `cargo` (workspace), Gradle/AGP, `python3` (JSON parsing), `gh` (publish only).

**Spec:** `docs/superpowers/specs/2026-11-05-release-scripts-design.md`

## Global Constraints

- Version scheme is CalVer `YYYY.MM.DD` with the day **not** zero-padded (`2026.11.5`); Cargo rejects leading zeros.
- The single version source is `[workspace.package] version` in `rust/Cargo.toml`; both member crates use `version.workspace = true`. No `VERSION=` override anywhere.
- Android `versionCode` is `major*10000 + minor*100 + patch` (→ `2026.11.5` = `20261105`).
- Artifact names: `dist/pige-<version>-android-arm64.apk` and `dist/pige-<version>-linux-$(uname -m)`.
- Desktop artifact is the bare binary (no packaging). Android is arm64-v8a only.
- `dist/` is gitignored. A release run removes `dist/` files that are not for the current version.
- `bash`, `cargo`, and `python3` are present. `gh` may be absent; `publish.sh` must fail clearly if it is.
- Bumping the version invalidates `rust/Cargo.lock`; the bump commit must include a refreshed lock (running `cargo check` refreshes it).
- `publish.sh` must validate local artifacts before any `gh` auth/network call.
- The repo has no shell test harness, so each script's "test" is a runnable command with expected output.

## Review Focus

Inputs/conditions the spec implies but no single step's happy path exercises, most likely first:

1. `publish.sh` when `gh` is missing or unauthenticated — must stop with a clear message and exit non-zero, touching neither git nor GitHub.
2. `publish.sh` when `dist/` is empty or holds a different version — must refuse before any network call.
3. `release.sh` with an unknown target argument — must print usage, exit 2, and write nothing.
4. `release.sh` version changes and single-target runs — stale-version files are dropped, but a sibling artifact of the same version survives.
5. A hand-edited version with a leading zero (`2026.11.05`) — Cargo refuses to load the workspace, so `version.sh` fails loudly instead of emitting a bad version.

---

### Task 1: CalVer workspace version and `scripts/version.sh`

**Files:**
- Modify: `rust/Cargo.toml`
- Modify: `rust/pige/Cargo.toml:3`
- Modify: `rust/pige_core/Cargo.toml:3`
- Modify (generated): `rust/Cargo.lock`
- Create: `scripts/version.sh`

**Interfaces:**
- Consumes: nothing.
- Produces: `scripts/version.sh` — writes the `pige` crate version to stdout and exits 0; exits non-zero with Cargo's error if the manifest is invalid or the lock is stale.

- [ ] **Step 1: Confirm the helper does not exist yet (red)**

Run: `bash scripts/version.sh`
Expected: `No such file or directory`, exit non-zero.

- [ ] **Step 2: Define the version once in the workspace manifest**

In `rust/Cargo.toml`, after the `[workspace]` table, add:

```toml
[workspace.package]
version = "2026.11.5"
```

In `rust/pige/Cargo.toml` and `rust/pige_core/Cargo.toml`, replace `version = "0.1.0"` with:

```toml
version.workspace = true
```

- [ ] **Step 3: Refresh the lockfile**

Run: `cd rust && cargo check --workspace`
Expected: succeeds and rewrites `rust/Cargo.lock` so both workspace packages show `version = "2026.11.5"`. Confirm with `grep -A1 '^name = "pige"' rust/Cargo.lock`.

- [ ] **Step 4: Create `scripts/version.sh`**

```bash
#!/usr/bin/env bash
# Print the pige workspace version. Single source of truth: rust/Cargo.toml.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo metadata --locked --format-version 1 --no-deps --manifest-path "$ROOT/rust/Cargo.toml" \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "pige"))'
```

Then: `chmod +x scripts/version.sh`

- [ ] **Step 5: Verify the helper and the invalid-version guard**

Run: `bash scripts/version.sh`
Expected: `2026.11.5`

Run (leading-zero guard, using a throwaway manifest):

```bash
mkdir -p /tmp/bad && touch /tmp/bad/lib.rs
printf '[package]\nname = "x"\nversion = "2026.11.05"\nedition = "2021"\n[lib]\npath = "lib.rs"\n' > /tmp/bad/Cargo.toml
cargo metadata --format-version 1 --no-deps --manifest-path /tmp/bad/Cargo.toml
```
Expected: fails with `invalid leading zero in patch version number`.

- [ ] **Step 6: Stage** (commits are handled by the human at the end)

```bash
git add rust/Cargo.toml rust/pige/Cargo.toml rust/pige_core/Cargo.toml rust/Cargo.lock scripts/version.sh
```

---

### Task 2: Pass the version into Gradle

**Files:**
- Modify: `android/app/build.gradle.kts:36-37`
- Modify: `scripts/build.sh` (final Gradle invocation)

**Interfaces:**
- Consumes: `scripts/version.sh` from Task 1.
- Produces: `scripts/build.sh` passes `-PqlaunchVersionName=<version>` and `-PqlaunchVersionCode=<YYYYMMDD>` to Gradle.

- [ ] **Step 1: Make Gradle read the properties with visible fallbacks**

In `android/app/build.gradle.kts`, in `defaultConfig`, replace:

```kotlin
        versionCode = 1
        versionName = "0.1.0"
```

with:

```kotlin
        // Supplied by scripts/build.sh from the workspace Cargo version.
        // Fallbacks are deliberately obvious so a bare Gradle build is unversioned.
        versionCode = providers.gradleProperty("qlaunchVersionCode").getOrElse("1").toInt()
        versionName = providers.gradleProperty("qlaunchVersionName").getOrElse("0.0.0")
```

- [ ] **Step 2: Confirm the fallback shows through (red)**

Run: `bash scripts/build.sh debug`
Then: `AAPT2="$(ls -1 "${ANDROID_HOME:-$HOME/Android/Sdk}"/build-tools/*/aapt2 | sort -V | tail -1)"; "$AAPT2" dump badging build/app/outputs/apk/debug/app-debug.apk | head -1`
Expected: `versionName='0.0.0'`.

- [ ] **Step 3: Compute and pass the version from `build.sh`**

Before the final Gradle invocation, add:

```bash
VERSION="$(bash "$ROOT/scripts/version.sh")"
IFS=. read -r V_MAJOR V_MINOR V_PATCH <<<"$VERSION"
VERSION_CODE=$((10#$V_MAJOR * 10000 + 10#$V_MINOR * 100 + 10#$V_PATCH))
```

and append the two properties to the Gradle call:

```bash
"${GRADLE[@]}" -p "$ROOT/android" "-PicedAndroidJava=$JAVA_SRC" \
    "-PqlaunchVersionName=$VERSION" "-PqlaunchVersionCode=$VERSION_CODE" "$TASK"
```

- [ ] **Step 4: Verify the built APK carries the Cargo version (green)**

Run: `bash scripts/build.sh debug` (then the same `aapt2 dump badging` command)
Expected: `versionName='2026.11.5'` and `versionCode='20261105'`.

- [ ] **Step 5: Stage**

```bash
git add android/app/build.gradle.kts scripts/build.sh
```

---

### Task 3: `scripts/release.sh` and `dist/`

**Files:**
- Create: `scripts/release.sh`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: `scripts/version.sh`; `scripts/build.sh release`; cargo desktop binary at `rust/target/release/pige`.
- Produces: `dist/pige-<version>-android-arm64.apk` and `dist/pige-<version>-linux-$(uname -m)`.

- [ ] **Step 1: Confirm the script does not exist yet (red)**

Run: `bash scripts/release.sh desktop`
Expected: `No such file or directory`, exit non-zero.

- [ ] **Step 2: Create `scripts/release.sh`**

```bash
#!/usr/bin/env bash
# Build versioned release artifacts into dist/. Usage: release.sh [all|android|desktop]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:-all}"
case "$MODE" in all|android|desktop) ;; *) echo "Usage: $0 [all|android|desktop]" >&2; exit 2;; esac

VERSION="$(bash "$ROOT/scripts/version.sh")"
mkdir -p "$ROOT/dist"
# Keep only the current version, but allow building each target separately.
find "$ROOT/dist" -maxdepth 1 -type f ! -name "pige-$VERSION-*" -delete

if [[ "$MODE" == all || "$MODE" == desktop ]]; then
    cargo build --locked --release --manifest-path "$ROOT/rust/Cargo.toml" -p pige --bin pige
    install -m 0755 "$ROOT/rust/target/release/pige" \
        "$ROOT/dist/pige-$VERSION-linux-$(uname -m)"
fi

if [[ "$MODE" == all || "$MODE" == android ]]; then
    bash "$ROOT/scripts/build.sh" release
    cp "$ROOT/build/app/outputs/apk/release/app-release.apk" \
        "$ROOT/dist/pige-$VERSION-android-arm64.apk"
fi

echo "release: $VERSION"
ls -1 "$ROOT/dist"
```

Then: `chmod +x scripts/release.sh`

- [ ] **Step 3: Ignore `dist/`**

Add to `.gitignore` after the Android build output block:

```gitignore
# Release artifacts
/dist/
```

- [ ] **Step 4: Verify the desktop artifact**

Run: `bash scripts/release.sh desktop`
Expected: lists `pige-2026.11.5-linux-x86_64`; `file` shows an ELF executable and it is executable.

- [ ] **Step 5: Verify a single-target run keeps the sibling artifact**

Run: `bash scripts/release.sh android`
Expected: lists both `pige-2026.11.5-android-arm64.apk` **and** the desktop binary.

- [ ] **Step 6: Verify stale-version files are removed**

Run: `touch dist/pige-0.0.0-stale; bash scripts/release.sh desktop; ls -1 dist`
Expected: `pige-0.0.0-stale` is gone; only the two `2026.11.5` files remain.

- [ ] **Step 7: Verify the default builds both, and the bad target is rejected**

Run: `bash scripts/release.sh` → both artifacts.
Run: `bash scripts/release.sh bogus`; echo "exit=$?" → usage, `exit=2`, nothing written.

- [ ] **Step 8: Stage**

```bash
git add scripts/release.sh .gitignore
```

---

### Task 4: `scripts/publish.sh` and README

**Files:**
- Create: `scripts/publish.sh`
- Modify: `README.md`

**Interfaces:**
- Consumes: `scripts/version.sh`; files matching `dist/pige-<version>-*`.
- Produces: a GitHub release tagged `v<version>` with the `dist/` files attached.

- [ ] **Step 1: Confirm the script does not exist yet (red)**

Run: `bash scripts/publish.sh`
Expected: `No such file or directory`, exit non-zero.

- [ ] **Step 2: Create `scripts/publish.sh`**

```bash
#!/usr/bin/env bash
# Publish dist/ artifacts as a GitHub release. Usage: publish.sh [--draft]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

DRAFT=0
for arg in "$@"; do
    case "$arg" in
        --draft) DRAFT=1 ;;
        -h|--help) echo "Usage: $0 [--draft]"; exit 0 ;;
        *) echo "Usage: $0 [--draft]" >&2; exit 2 ;;
    esac
done

if ! command -v gh >/dev/null 2>&1; then
    echo "publish: gh not installed; see https://cli.github.com" >&2
    exit 1
fi

# Validate everything local before any network/auth call, so an empty or
# wrong-version dist/ fails with an actionable message even offline.
VERSION="$(bash "$ROOT/scripts/version.sh")"
TAG="v$VERSION"
shopt -s nullglob
ASSETS=("$ROOT/dist/pige-$VERSION-"*)
if [ "${#ASSETS[@]}" -eq 0 ]; then
    echo "publish: no dist/ artifacts for $VERSION; run scripts/release.sh first" >&2
    exit 1
fi

if ! gh auth status >/dev/null 2>&1; then
    echo "publish: gh is not authenticated; run 'gh auth login'" >&2
    exit 1
fi

if ! git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    git -C "$ROOT" tag -a "$TAG" -m "pige $VERSION"
fi
git -C "$ROOT" push origin "$TAG"

ARGS=("$TAG" "${ASSETS[@]}" --title "pige $VERSION" --generate-notes --verify-tag)
if [ "$DRAFT" -eq 1 ]; then ARGS+=(--draft); fi
gh release create "${ARGS[@]}"
```

Then: `chmod +x scripts/publish.sh`

- [ ] **Step 3: Verify the no-`gh` guard**

Run: `PATH=/usr/bin:/bin bash scripts/publish.sh; echo "exit=$?"`
Expected: `publish: gh not installed...`, `exit=1`, no tag created.

- [ ] **Step 4: Verify the empty-`dist/` guard makes no `gh` call**

Run (stub `gh` logs its calls; empty `dist/`):

```bash
STUB="$(mktemp -d)"; export GH_LOG="$STUB/log"; : >"$GH_LOG"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >>"$GH_LOG"\nexit 0\n' >"$STUB/gh"; chmod +x "$STUB/gh"
mv dist dist.bak && mkdir dist
PATH="$STUB:$PATH" bash scripts/publish.sh; echo "exit=$?"
cat "$GH_LOG"; grep -q "auth status" "$GH_LOG" && echo BAD || echo GOOD
mv dist.bak/* dist/ && rmdir dist.bak; rm -rf "$STUB"
```
Expected: `publish: no dist/ artifacts for 2026.11.5...`, `exit=1`, `GOOD` (no gh call).

- [ ] **Step 5: Verify the happy path with stub `gh` and `git`**

```bash
STUB="$(mktemp -d)"; export GH_LOG="$STUB/log"; : >"$GH_LOG"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >>"$GH_LOG"\nexit 0\n' >"$STUB/gh"; chmod +x "$STUB/gh"
cat >"$STUB/git" <<'EOF'
#!/usr/bin/env bash
case "$*" in *rev-parse*) exit 1;; *) exit 0;; esac
EOF
chmod +x "$STUB/git"
PATH="$STUB:$PATH" bash scripts/publish.sh --draft; cat "$GH_LOG"; rm -rf "$STUB"
```
Expected: `release create v2026.11.5 <both dist assets> --title pige 2026.11.5 --generate-notes --verify-tag --draft`.

- [ ] **Step 6: Document releasing in the README**

Replace the one-line `README.md` with a title, the description, and:

````markdown
## Releasing

The version lives in `rust/Cargo.toml` (`[workspace.package] version`, `YYYY.MM.DD`,
day not zero-padded). To cut a release, bump it, refresh the lock, and commit:

```sh
# edit rust/Cargo.toml, then:
(cd rust && cargo check --workspace)   # updates Cargo.lock
git commit -am "release: 2026.11.5"
```

Build artifacts into `dist/`:

```sh
bash scripts/release.sh            # both targets
bash scripts/release.sh desktop    # or one
```

- `pige-<version>-android-arm64.apk`
- `pige-<version>-linux-<arch>` (bare executable)

Publish to GitHub (needs the authenticated GitHub CLI, `gh`):

```sh
bash scripts/publish.sh            # publish immediately
bash scripts/publish.sh --draft    # or push the tag and open a draft to review first
```

To publish a draft after reviewing it:

```sh
gh release edit "v$(bash scripts/version.sh)" --draft=false
```
````

- [ ] **Step 7: Stage**

```bash
git add scripts/publish.sh README.md
```
