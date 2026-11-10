#!/usr/bin/env bash
# Deterministic provider verification. Usage: verify-providers.sh [default|all]
#
# This script never uses credentials, never runs an ignored live test, never
# sends mail, and never publishes or tags a release. `default` exercises the
# no-feature product shape; `all` exercises every provider feature. Both modes
# also check each provider feature on its own so a feature combination cannot
# silently stop compiling.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:-default}"
case "$MODE" in
    default|all) ;;
    *)
        echo "Usage: $0 [default|all]" >&2
        exit 2
        ;;
esac

if [[ "${PIGE_TEST_ALLOW_SEND:-}" == "1" ]]; then
    echo "verify-providers: refusing to run with PIGE_TEST_ALLOW_SEND=1; this script never sends mail" >&2
    exit 2
fi

cd "$ROOT/rust"

echo "==> cargo fmt --all --check"
cargo fmt --all --check

# The vendored Proton fork is deliberately excluded from the workspace, so the
# workspace fmt check cannot see it. Format it through its own manifest rather
# than silently allowing drift in dependency code we publish.
echo "==> cargo fmt --check (vendored proton-core)"
cargo fmt --manifest-path vendor/proton-core/Cargo.toml --all --check

if [[ "$MODE" == "all" ]]; then
    TEST_FLAGS=(--workspace --all-features)
    CLIPPY_FLAGS=(--workspace --all-targets --all-features)
else
    TEST_FLAGS=(--workspace)
    CLIPPY_FLAGS=(--workspace --all-targets)
fi

echo "==> cargo test ${TEST_FLAGS[*]} --locked"
cargo test --locked "${TEST_FLAGS[@]}"

echo "==> cargo clippy ${CLIPPY_FLAGS[*]} --locked"
cargo clippy --locked "${CLIPPY_FLAGS[@]}"

# Feature combinations. `--no-default-features` is explicit even though the
# workspace default is empty, because a future default must not enable a
# provider or its optional dependency without this check noticing.
echo "==> cargo check -p pige_core --no-default-features"
cargo check --locked -p pige_core --no-default-features

for features in provider-google provider-proton provider-google,provider-proton; do
    echo "==> cargo check -p pige_core --features $features"
    cargo check --locked -p pige_core --features "$features"
done

echo "==> cargo check -p pige_core --all-features"
cargo check --locked -p pige_core --all-features

# Dependency and license boundary gate. Inspect the *resolved* graph with every
# provider feature enabled: an unfiltered cargo metadata still lists inactive
# optional dependencies such as gopenpgp-sys and would false-positive the gate.
# Check both the Android target and the host desktop target so the AGPL/link
# rules are not silently Android-only. cargo tree is a second, independent view
# of which optional backend features actually build.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
HOST="$(rustc -vV | awk '/^host:/ {print $2}')"

echo "==> cargo metadata/tree + provider dependency gate (Android graph)"
cargo metadata --locked --format-version 1 --all-features \
    --filter-platform aarch64-linux-android > "$TMP/metadata-android.json"
cargo tree --locked --all-features --target aarch64-linux-android \
    --edges features > "$TMP/tree-android.txt"
python3 "$ROOT/scripts/check-provider-boundaries.py" \
    --metadata "$TMP/metadata-android.json" --tree "$TMP/tree-android.txt"

echo "==> cargo metadata/tree + provider dependency gate (desktop graph: $HOST)"
cargo metadata --locked --format-version 1 --all-features \
    --filter-platform "$HOST" > "$TMP/metadata-desktop.json"
cargo tree --locked --all-features --target "$HOST" \
    --edges features > "$TMP/tree-desktop.txt"
python3 "$ROOT/scripts/check-provider-boundaries.py" \
    --metadata "$TMP/metadata-desktop.json" --tree "$TMP/tree-desktop.txt"

# The gate's own regression tests are deterministic fixtures; they do not touch
# Cargo, the network, or any credential.
echo "==> provider dependency gate unit tests"
python3 -m unittest discover -s "$ROOT/scripts/tests" \
    -p 'test_provider_boundaries.py'

echo "verify-providers: OK ($MODE; no live tests, no send, no publish)"
