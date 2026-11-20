#!/usr/bin/env python3
"""Verify that every remote GitHub Action in .github/workflows/ is pinned to a 40-char commit SHA."""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

USES_RE = re.compile(r"^\s*(?:-\s*)?uses:\s*['\"]?([^'\"\s#]+)['\"]?")
SHA40_RE = re.compile(r"^[0-9a-f]{40}$")
DOCKER_DIGEST_RE = re.compile(r"^docker://.+@sha256:[0-9a-f]{64}$")


def is_pinned_action(ref: str) -> bool:
    if ref.startswith("./"):
        return True
    if ref.startswith("docker://"):
        return bool(DOCKER_DIGEST_RE.match(ref))
    if "@" not in ref:
        return False
    _, rev = ref.rsplit("@", 1)
    return bool(SHA40_RE.match(rev))


def check_workflow_file(path: pathlib.Path) -> tuple[int, list[str]]:
    checked = 0
    errors: list[str] = []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("#"):
            continue
        match = USES_RE.match(line)
        if not match:
            continue
        ref = match.group(1)
        checked += 1
        if not is_pinned_action(ref):
            errors.append(f"{path}:{lineno}: unpinned action reference: {ref}")
    return checked, errors


def check_workflows(workflows_dir: pathlib.Path) -> tuple[int, int, list[str]]:
    files = sorted(
        p for p in workflows_dir.iterdir()
        if p.is_file() and p.suffix in {".yml", ".yaml"}
    )
    total_refs = 0
    all_errors: list[str] = []
    for wf in files:
        refs, errors = check_workflow_file(wf)
        total_refs += refs
        all_errors.extend(errors)
    return len(files), total_refs, all_errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "workflows_dir",
        nargs="?",
        default=None,
        help="Path to .github/workflows directory (defaults to repo .github/workflows)",
    )
    args = parser.parse_args(argv)
    if args.workflows_dir:
        workflows_dir = pathlib.Path(args.workflows_dir)
    else:
        workflows_dir = pathlib.Path(__file__).resolve().parent.parent / ".github" / "workflows"

    if not workflows_dir.is_dir():
        print(f"ERROR: workflows directory not found: {workflows_dir}", file=sys.stderr)
        return 1

    file_count, ref_count, errors = check_workflows(workflows_dir)
    for err in errors:
        print(err, file=sys.stderr)
    if errors:
        print(
            f"FAILED: {len(errors)} unpinned action reference(s) across {file_count} workflow file(s).",
            file=sys.stderr,
        )
        return 1

    print(
        f"OK: checked {file_count} workflow file(s), {ref_count} action reference(s), 0 unpinned."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
