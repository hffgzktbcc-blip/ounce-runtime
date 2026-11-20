#!/usr/bin/env python3
"""Unit tests for scripts/check_action_pins.py."""

from __future__ import annotations

import pathlib
import tempfile
import unittest

import check_action_pins as cap


class CheckActionPinsTests(unittest.TestCase):
    def test_accepts_pinned_shas_local_paths_and_docker_digests(self) -> None:
        self.assertTrue(
            cap.is_pinned_action("actions/checkout@11d5960a326750d5838078e36cf38b85af677262")
        )
        self.assertTrue(
            cap.is_pinned_action(
                "github/codeql-action/init@33d7b85da1a04171e211befe2c739303b9a4c066"
            )
        )
        self.assertTrue(cap.is_pinned_action("./.github/actions/local"))
        self.assertTrue(
            cap.is_pinned_action("docker://alpine@" + "sha256:" + "a" * 64)
        )

    def test_rejects_tags_branches_short_shas_and_uppercase_hex(self) -> None:
        for bad in [
            "actions/checkout@v4",
            "dtolnay/rust-toolchain@stable",
            "actions/checkout@main",
            "actions/checkout@11d5960",
            "actions/checkout@" + "A" * 40,
            "actions/checkout",
            "docker://alpine:latest",
        ]:
            self.assertFalse(cap.is_pinned_action(bad), f"should reject {bad}")

    def test_workflow_file_ignores_comments_and_flags_unpinned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            wf = pathlib.Path(tmp) / "sample.yml"
            wf.write_text(
                "\n".join([
                    "jobs:",
                    "  build:",
                    "    steps:",
                    "      # uses: actions/checkout@v4",
                    "      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4",
                    "      - uses: Swatinem/rust-cache@v2",
                ]),
                encoding="utf-8",
            )
            checked, errors = cap.check_workflow_file(wf)
            self.assertEqual(checked, 2)
            self.assertEqual(len(errors), 1)
            self.assertIn("Swatinem/rust-cache@v2", errors[0])

    def test_repo_workflows_are_all_pinned(self) -> None:
        repo_workflows = pathlib.Path(__file__).resolve().parent.parent / ".github" / "workflows"
        files, refs, errors = cap.check_workflows(repo_workflows)
        self.assertGreater(files, 0)
        self.assertGreater(refs, 0)
        self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main()
