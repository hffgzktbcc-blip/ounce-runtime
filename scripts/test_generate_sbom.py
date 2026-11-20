#!/usr/bin/env python3
"""Unit tests for scripts/generate_sbom.py."""

from __future__ import annotations

import json
import pathlib
import tempfile
import unittest

import generate_sbom as gs


class GenerateSbomTests(unittest.TestCase):
    def test_generates_valid_spdx_2_3_from_synthetic_lockfile(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "Cargo.toml").write_text(
                '[package]\nname = "ounce-runtime"\nversion = "0.6.0"\nlicense = "GPL-3.0-or-later"\n',
                encoding="utf-8",
            )
            (root / "Cargo.lock").write_text(
                "\n".join([
                    "version = 4",
                    "",
                    "[[package]]",
                    'name = "ounce-runtime"',
                    'version = "0.6.0"',
                    'dependencies = ["serde 1.0.229"]',
                    "",
                    "[[package]]",
                    'name = "serde"',
                    'version = "1.0.229"',
                    'source = "registry+https://github.com/rust-lang/crates.io-index"',
                    'checksum = "' + "a" * 64 + '"',
                ]),
                encoding="utf-8",
            )

            out_path = root / "out" / "sbom.spdx.json"
            rc = gs.main([
                "--repo-root",
                str(root),
                "--target",
                "aarch64-apple-darwin",
                "--created",
                "2026-11-20T00:00:00Z",
                "--output",
                str(out_path),
            ])
            self.assertEqual(rc, 0)
            doc = json.loads(out_path.read_text(encoding="utf-8"))
            self.assertEqual(doc["spdxVersion"], "SPDX-2.3")
            self.assertEqual(doc["dataLicense"], "CC0-1.0")
            self.assertEqual(doc["SPDXID"], "SPDXRef-DOCUMENT")
            self.assertEqual(doc["creationInfo"]["created"], "2026-11-20T00:00:00Z")
            self.assertEqual(doc["documentDescribes"], ["SPDXRef-Package-ounce-runtime"])
            self.assertEqual(len(doc["packages"]), 2)

            by_name = {p["name"]: p for p in doc["packages"]}
            self.assertEqual(by_name["ounce-runtime"]["licenseDeclared"], "GPL-3.0-or-later")
            self.assertEqual(
                by_name["serde"]["checksums"],
                [{"algorithm": "SHA256", "checksumValue": "a" * 64}],
            )
            self.assertEqual(
                by_name["serde"]["externalRefs"][0]["referenceLocator"],
                "pkg:cargo/serde@1.0.229",
            )
            rel_types = {(r["spdxElementId"], r["relationshipType"], r["relatedSpdxElement"]) for r in doc["relationships"]}
            self.assertIn(
                ("SPDXRef-DOCUMENT", "DESCRIBES", "SPDXRef-Package-ounce-runtime"),
                rel_types,
            )
            self.assertIn(
                ("SPDXRef-Package-ounce-runtime", "DEPENDS_ON", by_name["serde"]["SPDXID"]),
                rel_types,
            )

    def test_generates_spdx_for_real_repository_lockfile(self) -> None:
        repo_root = pathlib.Path(__file__).resolve().parent.parent
        doc = gs.generate_sbom(
            repo_root,
            target="x86_64-unknown-linux-gnu",
            created="2026-11-20T00:00:00Z",
        )
        self.assertEqual(doc["spdxVersion"], "SPDX-2.3")
        self.assertGreater(len(doc["packages"]), 20)
        ids = [p["SPDXID"] for p in doc["packages"]]
        self.assertEqual(len(ids), len(set(ids)), "SPDXIDs must be unique")
        self.assertIn("SPDXRef-Package-ounce-runtime", ids)


if __name__ == "__main__":
    unittest.main()
