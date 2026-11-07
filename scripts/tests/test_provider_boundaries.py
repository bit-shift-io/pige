#!/usr/bin/env python3
"""Unit tests for scripts/check-provider-boundaries.py.

These are TDD tests: they describe the dependency boundary gate for the native
mail provider work before the gate exists.  The gate must match on Cargo
metadata package ids/source (not bare name substrings) for all three rules:

  * no Go/gopenpgp crypto backend in the resolved graph;
  * no unapproved AGPL-3.0-only package;
  * no second crate claiming the native SQLite ``links = "sqlite3"`` slot.

Run from the repository root:

    python3 -m unittest discover -s scripts/tests -p 'test_provider_boundaries.py'
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPT = os.path.join(REPO_ROOT, "scripts", "check-provider-boundaries.py")

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"


def make_pkg(
    name: str,
    version: str = "1.0.0",
    *,
    license: str | None = "MIT",
    source: str | None = CRATES_IO,
    links: str | None = None,
    features: dict | None = None,
) -> dict:
    """Build a package record shaped like ``cargo metadata`` output."""
    pkg_id = f"{name} {version}" + (f" ({source})" if source else "")
    return {
        "id": pkg_id,
        "name": name,
        "version": version,
        "license": license,
        "source": source,
        "links": links,
        "features": features or {},
        "manifest_path": f"/nonexistent/{name}-{version}/Cargo.toml",
    }


def make_node(pkg: dict, features: list[str] | None = None) -> dict:
    """Build a resolve node for a package, enabling ``features``."""
    return {"id": pkg["id"], "features": list(features or []), "deps": []}


def make_metadata(packages: list[dict], nodes: list[dict]) -> dict:
    return {
        "packages": packages,
        "resolve": {"nodes": nodes, "root": nodes[0]["id"] if nodes else None},
        "workspace_members": [],
    }


def run_gate(metadata: dict, tree: str = "") -> subprocess.CompletedProcess:
    """Invoke the CLI exactly as CI does and return the completed process."""
    with tempfile.TemporaryDirectory() as tmp:
        meta_path = os.path.join(tmp, "cargo-metadata.json")
        tree_path = os.path.join(tmp, "cargo-tree.txt")
        with open(meta_path, "w", encoding="utf-8") as fh:
            json.dump(metadata, fh)
        with open(tree_path, "w", encoding="utf-8") as fh:
            fh.write(tree)
        return subprocess.run(
            [sys.executable, SCRIPT, "--metadata", meta_path, "--tree", tree_path],
            capture_output=True,
            text=True,
        )


# Baseline graph pieces shared by the accept case.
LIB_SQLITE = make_pkg("libsqlite3-sys", "0.30.1", links="sqlite3")
RUSQLITE = make_pkg("rusqlite", "0.41.0")
PROTON_CRYPTO_RUST = make_pkg("proton-crypto", "0.14.1")
PROTON_RPGP = make_pkg("proton-rpgp", "0.9.0")
GMAIL = make_pkg("google-gmail1", "7.0.0")
PEOPLE = make_pkg("google-people1", "7.0.0")


class ProviderBoundaryGateTests(unittest.TestCase):
    def test_rejects_go_crypto_backend(self) -> None:
        """The Go gopenpgp backend must fail even when its own build compiles."""
        gopenpgp = make_pkg("gopenpgp-sys", "0.3.9")
        packages = [RUSQLITE, LIB_SQLITE, PROTON_CRYPTO_RUST, gopenpgp]
        nodes = [
            make_node(RUSQLITE),
            make_node(LIB_SQLITE),
            # Default features pull in the Go backend (feature "gopgp").
            make_node(PROTON_CRYPTO_RUST, ["default", "gopgp"]),
            make_node(gopenpgp),
        ]
        metadata = make_metadata(packages, nodes)
        tree = (
            "pige v2026.11.5 (/repo)\n"
            "└── proton-crypto feature \"gopgp\"\n"
            "    └── gopenpgp-sys v0.3.9\n"
        )
        result = run_gate(metadata, tree)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        combined = (result.stdout + result.stderr).lower()
        self.assertIn("gopenpgp-sys", combined)
        self.assertIn("go", combined)

    def test_rejects_unapproved_agpl_package(self) -> None:
        """A new AGPL-3.0-only crate anywhere in the graph must fail."""
        # Decoy: a name containing "agpl" with a permissive license is fine and
        # must not be what triggers the failure (id/license based, not name).
        decoy = make_pkg("agpl-notes-helper", "1.0.0", license="MIT")
        evil = make_pkg("proton-mail-api", "1.2.3", license="AGPL-3.0-only")
        packages = [RUSQLITE, LIB_SQLITE, PROTON_CRYPTO_RUST, decoy, evil]
        nodes = [
            make_node(RUSQLITE),
            make_node(LIB_SQLITE),
            make_node(PROTON_CRYPTO_RUST, ["rustpgp"]),
            make_node(decoy),
            make_node(evil),
        ]
        result = run_gate(make_metadata(packages, nodes))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        combined = result.stdout + result.stderr
        self.assertIn("AGPL-3.0-only", combined)
        self.assertIn(evil["id"], combined)
        # The permissive decoy must not be reported as a violation.
        self.assertNotIn(decoy["id"], combined)

    def test_rejects_multiple_sqlite_links(self) -> None:
        """Two crates owning links = "sqlite3" break native SQLite linking."""
        intruder = make_pkg("sqlite3-native-bundle", "0.32.1", links="sqlite3")
        packages = [RUSQLITE, LIB_SQLITE, PROTON_CRYPTO_RUST, intruder]
        nodes = [
            make_node(RUSQLITE),
            make_node(LIB_SQLITE),
            make_node(PROTON_CRYPTO_RUST, ["rustpgp"]),
            make_node(intruder),
        ]
        result = run_gate(make_metadata(packages, nodes))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        combined = result.stdout + result.stderr
        self.assertIn("sqlite3", combined)
        self.assertIn(LIB_SQLITE["id"], combined)
        self.assertIn(intruder["id"], combined)

    def test_accepts_approved_rust_backend(self) -> None:
        """Rust-only Proton crypto plus one SQLite owner is the approved shape.

        Includes decoys whose names contain "gopenpgp-sys"/"go" and "sqlite3"
        but which are not the actual packages, proving identity-based matching.
        """
        go_decoy = make_pkg("gopenpgp-sys-stub", "0.0.1", license="MIT")
        sqlite_decoy = make_pkg("sqlite3-sys-fake", "0.0.1", license="MIT")
        packages = [
            RUSQLITE,
            LIB_SQLITE,
            PROTON_CRYPTO_RUST,
            PROTON_RPGP,
            GMAIL,
            PEOPLE,
            go_decoy,
            sqlite_decoy,
        ]
        nodes = [
            make_node(RUSQLITE),
            make_node(LIB_SQLITE),
            make_node(PROTON_CRYPTO_RUST, ["rustpgp"]),
            make_node(PROTON_RPGP),
            make_node(GMAIL),
            make_node(PEOPLE),
            make_node(go_decoy),
            make_node(sqlite_decoy),
        ]
        tree = (
            "pige v2026.11.5 (/repo)\n"
            "└── proton-crypto v0.14.1\n"
            "    └── proton-rpgp v0.9.0\n"
            # Decoy: a look-alike token whose "proton-crypto" is preceded by a
            # word char/hyphen, so the (?<![\w-]) feature-line lookbehind in the
            # gate must not treat this as the real proton-crypto feature.
            "    └── my-proton-crypto feature \"gopgp\"\n"
        )
        result = run_gate(make_metadata(packages, nodes), tree)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # Assert the exact documented success line, not a loose "ok" substring.
        self.assertIn(
            "provider boundary check: OK (8 resolved packages checked)",
            result.stdout,
        )

    def test_rejects_missing_spdx_license_for_new_registry_package(self) -> None:
        """A new registry crate with no SPDX field must fail, not merely warn.

        This closes the hole where an AGPL crate shipping only a ``license-file``
        would otherwise pass the MIT-distribution gate on a warning.
        """
        mystery = make_pkg("mystery-mail-sdk", "1.0.0", license=None)
        packages = [RUSQLITE, LIB_SQLITE, PROTON_CRYPTO_RUST, mystery]
        nodes = [
            make_node(RUSQLITE),
            make_node(LIB_SQLITE),
            make_node(PROTON_CRYPTO_RUST, ["rustpgp"]),
            make_node(mystery),
        ]
        result = run_gate(make_metadata(packages, nodes))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        combined = result.stdout + result.stderr
        self.assertIn(mystery["id"], combined)
        self.assertIn("SPDX", combined)

    def test_accepts_allow_listed_license_file_package(self) -> None:
        """The deliberately allow-listed ``proton-crypto`` may omit SPDX metadata."""
        proton_no_spdx = make_pkg("proton-crypto", "0.14.1", license=None)
        packages = [RUSQLITE, LIB_SQLITE, proton_no_spdx]
        nodes = [
            make_node(RUSQLITE),
            make_node(LIB_SQLITE),
            make_node(proton_no_spdx, ["rustpgp"]),
        ]
        result = run_gate(make_metadata(packages, nodes))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("provider boundary check: OK", result.stdout)
        # Provenance is still surfaced as an advisory warning on stderr.
        self.assertIn("allow-listed without SPDX metadata", result.stderr)

    def test_rejects_non_object_metadata_json(self) -> None:
        """Malformed (non-object) metadata must exit 2, never raise TypeError."""
        result = run_gate(5)  # type: ignore[arg-type]
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("must be an object", result.stderr)


if __name__ == "__main__":
    unittest.main()
