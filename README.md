# pige
Lightweight rust &amp; iced email client for desktop and mobile

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
