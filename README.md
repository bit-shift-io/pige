# pige
Lightweight rust &amp; iced email client for desktop and mobile

## Native providers

Native Gmail and Proton adapters sit behind the shared provider registry; the
default IMAP/SMTP account path is unchanged. Deterministic verification never
uses credentials or sends mail:

```sh
bash scripts/verify-providers.sh default   # no provider features
bash scripts/verify-providers.sh all       # every provider feature
```

Release evidence, including the PASS/FAIL/BLOCKED live and on-device matrix, is
in [`docs/provider-release-verification.md`](docs/provider-release-verification.md).
Maintenance, migration, and local-plaintext limits are in
[`docs/provider-maintenance.md`](docs/provider-maintenance.md); the extension
recipe for a new backend is
[`docs/adding-a-mail-provider.md`](docs/adding-a-mail-provider.md).

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
