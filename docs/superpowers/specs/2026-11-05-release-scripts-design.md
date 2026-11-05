# Release and publish scripts — design

This is the same design as qlaunch's
`docs/superpowers/specs/2026-11-05-release-scripts-design.md`, applied to pige.
Only the named deltas differ.

## Intent

Give pige two small, separate scripts: one that builds versioned desktop and
Android release artifacts into a single folder, and one that publishes those
artifacts as a GitHub release. The Cargo crate version is the single source of
truth for the version, and the version scheme is calendar-based (year.month.day).

## Deltas from qlaunch

- Crate/package names: `pige`, `pige_core`; Android namespace/applicationId
  `bitshift.pige`; library `libpige.so`; binary `pige`.
- Android signing config is named `shared` (guarded by `hasSharedKey`) rather
  than `release`; this does not affect the version plumbing.
- `scripts/build.sh` already defaults `JAVA_HOME` to `/opt/jbr-21`.
- `README.md` is a single line; the "Releasing" section is added in full.
- `.gitignore` gains `/dist/`.

## Goals

- `scripts/release.sh [all|android|desktop]` (default `all`) builds the Linux
  desktop binary and the arm64 Android APK into `dist/` at the repo root, named
  with the version.
- `scripts/publish.sh` creates a GitHub release from the contents of `dist/`.
- The version comes only from the `pige` crate in `rust/`.
- Keep the existing `scripts/` bash style; reuse `scripts/build.sh` for Android.

## Non-goals

- Desktop packaging beyond the bare binary (no AppImage, `.deb`, tarball).
- Code signing beyond the committed test-phone keystore already in use.
- CI / GitHub Actions; both scripts run locally.
- Building for ABIs or desktop architectures other than what the machine and
  Gradle already target (Android arm64-v8a; desktop `uname -m`).
- More than one release per day.

## Anti-goals

- A second source of truth for the version. `android/app/build.gradle.kts`
  must stop hardcoding `0.1.0`.
- A script that publishes implicitly, or that pushes tags without the user
  asking.

## Constraints

- Android release builds already go through `scripts/build.sh release`, which
  sets up the NDK toolchain and calls Gradle.
- GNU bash, `python3`, and `cargo` are already project dependencies.
- `gh` may be absent; `publish.sh` will check for it and fail clearly.

## Version scheme

Calendar versioning, `YYYY.MM.DD`, e.g. `2026.11.5`.

- **Cargo requires valid semver, which forbids leading zeros**, so the day is
  not zero-padded: `2026.11.5`, never `2026.11.05`.
- Defined once at the workspace level: `[workspace.package] version` in
  `rust/Cargo.toml`, with `pige` and `pige_core` both using
  `version.workspace = true`.
- Bumped by hand in `rust/Cargo.toml` and committed before `release.sh` runs;
  the script reads the version, it does not stamp the date.
- Tags and filenames use the crate version verbatim (`v2026.11.5`,
  `pige-2026.11.5-...`).
- Android `versionCode` is `major*10000 + minor*100 + patch`, which for
  `YYYY.MM.DD` is exactly `YYYYMMDD` (`2026.11.5` → `20261105`).

## Approaches

1. **Single orchestrator + separate publish script (chosen).** `release.sh`
   reads the version, builds both targets, and copies into `dist/`;
   `publish.sh` handles the GitHub release.
2. Separate per-target build scripts plus a wrapper — more files for no gain.
3. Rust `cargo xtask` — adds a workspace member and diverges from the bash
   scripts already in use.

## Design

### Version helper — `scripts/version.sh`

Prints the `pige` package version by parsing `cargo metadata`. `release.sh`,
`build.sh` and `publish.sh` all use it, so there is exactly one parser.

### `scripts/release.sh [all|android|desktop]`

- Deletes files in `dist/` that are not for the current version, so the folder
  never mixes versions while the two targets can still be built separately.
- Desktop: `cargo build --locked --release -p pige --bin pige`, then copy
  `rust/target/release/pige` to `dist/pige-$VERSION-linux-$(uname -m)`.
- Android: `bash scripts/build.sh release`, then copy
  `build/app/outputs/apk/release/app-release.apk` to
  `dist/pige-$VERSION-android-arm64.apk`.
- Echo the resulting file list.

### `scripts/build.sh` + `android/app/build.gradle.kts`

`build.sh` derives the version and `versionCode` with `scripts/version.sh` and
passes them to Gradle as `-PqlaunchVersionName` / `-PqlaunchVersionCode`.
Gradle reads those properties (with `0.0.0` / `1` fallbacks) instead of the
hardcoded `0.1.0` / `1`.

### `scripts/publish.sh [--draft]`

- Fails clearly if `gh` is missing.
- Validates the local artifacts (`dist/pige-$VERSION-*`) before any auth/network
  call; refuses if none are present.
- Fails if `gh` is not authenticated.
- Ensures tag `v$VERSION` exists: creates it locally at HEAD if missing, pushes
  it, then `gh release create "v$VERSION" <assets> --title "pige $VERSION"
  --generate-notes`.

### Housekeeping

- Change the workspace version and the two crate versions to the CalVer scheme
  (start at the current date).
- Add `/dist/` to `.gitignore`.
- Add a "Releasing" section to `README.md`.
