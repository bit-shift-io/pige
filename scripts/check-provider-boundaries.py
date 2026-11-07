#!/usr/bin/env python3
"""Provider dependency boundary gate for Pige's native mail providers.

The native Gmail/Proton work may only add dependencies that keep the project
buildable on Android and distributable under Pige's existing MIT direction.
Three hazards motivate this gate (see docs/provider-feasibility.md):

1. ``proton-crypto`` defaults to the Go ``gopenpgp-sys`` backend.  Android must
   use the pure-Rust backend (``proton-rpgp``), so the Go backend must never be
   activated in the resolved graph.
2. Official Proton mail code (``ProtonMail/clients``) is ``AGPL-3.0-only`` and
   incompatible with Pige's licensing direction.  No unapproved AGPL crate may
   enter the graph.
3. ``rusqlite`` already owns the native SQLite link (``links = "sqlite3"`` via
   ``libsqlite3-sys``).  A second crate claiming that link slot breaks the
   native build, so exactly one package may own it.

Identity checks are performed against Cargo's package ``name``/``version``/
``source`` (and therefore the metadata ``id``) rather than bare name
substrings, so look-alike names such as ``gopenpgp-sys-stub`` are ignored.

CLI::

    python3 scripts/check-provider-boundaries.py \
        --metadata <cargo-metadata.json> --tree <cargo-tree.txt>

Exits 0 only when every approved-dependency rule holds; exit 1 on violation and
exit 2 for malformed input.
"""

from __future__ import annotations

import argparse
import json
import re
import sys

# --- Policy constants -------------------------------------------------------

# Package that provides the Go build of the Proton OpenPGP backend.
GO_CRYPTO_PACKAGES = frozenset({"gopenpgp-sys"})

# Crate whose default feature activates the Go backend.
PROTON_CRYPTO_PACKAGE = "proton-crypto"
# Features that must never be enabled on proton-crypto: `default` implies
# `gopgp`; `multi_be` pulls both backends.
GO_BACKEND_FEATURES = frozenset({"default", "gopgp", "multi_be"})
# Feature that selects the pure-Rust backend we approve.
RUST_BACKEND_FEATURE = "rustpgp"

# Native link slot owned by the existing bundled SQLite stack.
SQLITE_LINK = "sqlite3"

# SPDX tokens that represent unacceptable strong-copyleft licenses.
FORBIDDEN_LICENSE_PREFIXES = ("AGPL",)

# Packages explicitly approved despite a forbidden license expression.  Empty
# today: nothing in Pige's graph is AGPL.  This is the escape hatch for a
# deliberate, documented licensing decision (never silently added to).
APPROVED_LICENSE_PACKAGES: frozenset[str] = frozenset()


def _is_forbidden_license(expression: str | None) -> tuple[bool, str | None]:
    """Return (is_forbidden, token) for an SPDX license expression.

    Only AGPL-family tokens are rejected.  The expression is tokenised so that
    a permissive OR-branch (``MIT OR AGPL-3.0-only``) still trips the gate: the
    dependency could be used under AGPL terms or the ambiguity is unacceptable.
    """
    if not expression:
        return False, None
    for token in re.split(r"[\s()]+", expression):
        if not token:
            continue
        upper = token.upper()
        for prefix in FORBIDDEN_LICENSE_PREFIXES:
            if upper.startswith(prefix):
                return True, token
    return False, None


def check_go_crypto_backend(metadata: dict, tree_text: str, violations: list[str]) -> None:
    """Reject any activation of the Go Proton crypto backend."""
    packages = {pkg["id"]: pkg for pkg in metadata.get("packages", [])}
    nodes = metadata.get("resolve", {}).get("nodes", [])

    proton_features: set[str] = set()
    for node in nodes:
        pkg = packages.get(node["id"])
        if pkg is None:
            continue
        if pkg["name"] in GO_CRYPTO_PACKAGES:
            violations.append(
                f"Go crypto backend package present in resolved graph: {pkg['id']}"
            )
        if pkg["name"] == PROTON_CRYPTO_PACKAGE:
            proton_features.update(node.get("features", []))

    go_features = sorted(proton_features & GO_BACKEND_FEATURES)
    if go_features:
        violations.append(
            "proton-crypto has Go backend features enabled: "
            + ", ".join(go_features)
            + " (use default-features = false, features = [\""
            + RUST_BACKEND_FEATURE
            + "\"])"
        )

    # The feature-resolved tree is an independent view of what actually builds.
    for feature in re.findall(r'(?<![\w-])proton-crypto feature "([^"]+)"', tree_text):
        if feature in GO_BACKEND_FEATURES:
            violations.append(
                f'resolved cargo tree enables proton-crypto Go feature "{feature}"'
            )
    for match in re.finditer(r"(?:^|[\s│├└─])gopenpgp-sys\s+v?\d", tree_text):
        line = tree_text[match.start():tree_text.find("\n", match.start())].strip()
        violations.append(f"resolved cargo tree contains Go backend package: {line}")


def check_licenses(metadata: dict, violations: list[str]) -> None:
    """Reject forbidden (AGPL) licenses anywhere in the resolved graph."""
    nodes = metadata.get("resolve", {}).get("nodes", [])
    packages = {pkg["id"]: pkg for pkg in metadata.get("packages", [])}
    resolved_ids = {node["id"] for node in nodes} or set(packages)

    for pkg_id in sorted(resolved_ids):
        pkg = packages.get(pkg_id)
        if pkg is None:
            continue
        if pkg_id in APPROVED_LICENSE_PACKAGES:
            continue
        forbidden, token = _is_forbidden_license(pkg.get("license"))
        if forbidden:
            violations.append(
                f"unapproved {token} license in resolved graph: {pkg['id']}"
            )


def check_sqlite_links(metadata: dict, violations: list[str]) -> None:
    """Reject more than one crate owning the native SQLite link slot."""
    nodes = metadata.get("resolve", {}).get("nodes", [])
    packages = {pkg["id"]: pkg for pkg in metadata.get("packages", [])}
    resolved_ids = {node["id"] for node in nodes} or set(packages)

    owners = sorted(
        pkg["id"]
        for pkg_id, pkg in packages.items()
        if pkg.get("links") == SQLITE_LINK and pkg_id in resolved_ids
    )
    if len(owners) > 1:
        violations.append(
            f'multiple crates own links = "{SQLITE_LINK}" in the resolved graph: '
            + ", ".join(owners)
        )


def license_gaps(metadata: dict) -> list[str]:
    """Advisory: registry packages whose SPDX license cannot be resolved.

    Some candidates (for example ``proton-crypto``, which publishes only a
    ``license-file``) expose no ``license`` field in ``cargo metadata``.  That is
    not by itself a violation, but it means the AGPL rule could not evaluate the
    package, so it is surfaced for manual provenance review.
    """
    nodes = metadata.get("resolve", {}).get("nodes", [])
    packages = {pkg["id"]: pkg for pkg in metadata.get("packages", [])}
    resolved_ids = {node["id"] for node in nodes} or set(packages)
    gaps = []
    for pkg_id in sorted(resolved_ids):
        pkg = packages.get(pkg_id)
        if pkg is None or not pkg.get("source"):
            continue  # workspace/path packages need no registry license.
        if not pkg.get("license"):
            gaps.append(f"no SPDX license metadata for registry package: {pkg['id']}")
    return gaps


def evaluate(metadata: dict, tree_text: str = "") -> list[str]:
    """Run every rule and return the list of human-readable violations."""
    violations: list[str] = []
    check_go_crypto_backend(metadata, tree_text, violations)
    check_licenses(metadata, violations)
    check_sqlite_links(metadata, violations)
    return violations


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Gate Pige provider dependencies (Go crypto, AGPL, SQLite links)."
    )
    parser.add_argument("--metadata", required=True, help="cargo metadata JSON path")
    parser.add_argument("--tree", required=True, help="cargo tree output path")
    args = parser.parse_args(argv)

    try:
        with open(args.metadata, encoding="utf-8") as fh:
            metadata = json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        print(f"error: cannot read cargo metadata: {exc}", file=sys.stderr)
        return 2

    try:
        with open(args.tree, encoding="utf-8") as fh:
            tree_text = fh.read()
    except OSError as exc:
        print(f"error: cannot read cargo tree: {exc}", file=sys.stderr)
        return 2

    if "packages" not in metadata:
        print("error: metadata JSON is missing 'packages'", file=sys.stderr)
        return 2

    for gap in license_gaps(metadata):
        print(f"warning: {gap}", file=sys.stderr)

    violations = evaluate(metadata, tree_text)
    if violations:
        print("provider boundary check: FAILED", file=sys.stderr)
        for violation in violations:
            print(f"  - {violation}", file=sys.stderr)
        return 1

    resolved = len(metadata.get("resolve", {}).get("nodes", [])) or len(
        metadata.get("packages", [])
    )
    print(f"provider boundary check: OK ({resolved} resolved packages checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
