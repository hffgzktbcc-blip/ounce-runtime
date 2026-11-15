#!/usr/bin/env python3
"""Generate consolidated third-party license notices and GPL-3.0 Corresponding Source notice."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess


NOTICE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")
REPO_URL = "https://github.com/hffgzktbcc-blip/ounce-runtime"


def load_package_metadata(repo_root: pathlib.Path) -> tuple[dict, list[dict]]:
    raw = subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=repo_root,
    )
    meta = json.loads(raw)
    root_pkg = next(
        pkg for pkg in meta["packages"] if pkg["name"] == "ounce-runtime" and not pkg.get("source")
    )
    third_party = sorted(
        (pkg for pkg in meta["packages"] if pkg.get("source")),
        key=lambda pkg: (pkg["name"].lower(), pkg["version"]),
    )
    return root_pkg, third_party


def find_notice_files(manifest_path: str) -> list[pathlib.Path]:
    pkg_dir = pathlib.Path(manifest_path).parent
    if not pkg_dir.is_dir():
        return []
    files = [
        entry
        for entry in sorted(pkg_dir.iterdir(), key=lambda p: p.name.lower())
        if entry.is_file() and entry.name.upper().startswith(NOTICE_PREFIXES)
    ]
    return files


def render_third_party_licenses(
    pkg_info: dict,
    third_party: list[dict],
    target: str,
) -> str:
    lines: list[str] = [
        f"Third-Party Licenses and Notices for {pkg_info['name']} {pkg_info['version']}",
        f"Target: {target}",
        f"Project License: {pkg_info.get('license', 'GPL-3.0-or-later')} (see LICENSE)",
        "Workspace crate crates/ounce-recompiler is licensed under MIT.",
        "",
        f"Resolved third-party crates in Cargo.lock: {len(third_party)}",
        "",
        "=== Crate Inventory ===",
    ]
    for pkg in third_party:
        spdx = pkg.get("license") or "SEE_CRATE_FILE"
        repo = pkg.get("repository") or "-"
        lines.append(f"- {pkg['name']} {pkg['version']} | {spdx} | {repo}")

    lines.extend(["", "=== Included License and Notice Texts ==="])
    for pkg in third_party:
        notice_files = find_notice_files(pkg["manifest_path"])
        if not notice_files:
            continue
        for notice_file in notice_files:
            text = notice_file.read_text(encoding="utf-8", errors="replace").strip()
            lines.extend([
                "",
                "-" * 72,
                f"Crate: {pkg['name']} {pkg['version']} ({pkg.get('license') or 'unspecified'})",
                f"File: {notice_file.name}",
                "-" * 72,
                text,
            ])

    lines.append("")
    return "\n".join(lines)


def render_corresponding_source(
    pkg_info: dict,
    target: str,
    git_ref: str,
    git_sha: str,
) -> str:
    version = pkg_info["version"]
    tag = git_ref or f"v{version}"
    sha_note = git_sha or "unknown"
    return "\n".join([
        f"{pkg_info['name']} {version} ({target})",
        f"License: {pkg_info.get('license', 'GPL-3.0-or-later')}",
        "",
        "Corresponding Source Notice (GPL-3.0 Sections 4 and 6)",
        "----------------------------------------------------",
        f"This binary distribution of {pkg_info['name']} is licensed under the GNU",
        "General Public License, version 3 or later (see LICENSE).",
        "",
        "Complete Corresponding Source for this exact build is available at:",
        f"  Repository: {REPO_URL}",
        f"  Tag / Ref:  {tag}",
        f"  Commit SHA: {sha_note}",
        "",
        "To reproduce the build from source:",
        f"  git clone {REPO_URL}.git",
        "  cd ounce-runtime",
        f"  git checkout {sha_note if git_sha else tag}",
        f"  cargo build --release --locked --target {target}",
        "",
    ])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="Rust target triple")
    parser.add_argument("--output-dir", required=True, help="Directory for generated notice files")
    args = parser.parse_args()

    repo_root = pathlib.Path(__file__).resolve().parent.parent
    out_dir = pathlib.Path(args.output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    pkg_info, third_party = load_package_metadata(repo_root)
    (out_dir / "THIRD_PARTY_LICENSES.txt").write_text(
        render_third_party_licenses(pkg_info, third_party, args.target),
        encoding="utf-8",
    )
    (out_dir / "CORRESPONDING_SOURCE.txt").write_text(
        render_corresponding_source(
            pkg_info,
            args.target,
            os.environ.get("GITHUB_REF_NAME", ""),
            os.environ.get("GITHUB_SHA", ""),
        ),
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
