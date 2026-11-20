#!/usr/bin/env python3
"""Generate a deterministic SPDX 2.3 JSON SBOM from Cargo.toml and Cargo.lock (stdlib only)."""

from __future__ import annotations

import argparse
import datetime
import json
import os
import pathlib
import re

try:
    import tomllib  # type: ignore[import-not-found]
except ModuleNotFoundError:  # Python < 3.11 fallback
    tomllib = None  # type: ignore[assignment]

REPO_URL = "https://github.com/hffgzktbcc-blip/ounce-runtime"
TOOL_CREATOR = "Tool: ounce-runtime/scripts/generate_sbom.py"
SPDX_ID_SAFE_RE = re.compile(r"[^A-Za-z0-9.-]+")


def _parse_cargo_toml_fallback(text: str) -> dict:
    in_package = False
    pkg: dict[str, str] = {}
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("["):
            in_package = line == "[package]"
            continue
        if in_package and "=" in line:
            k, v = line.split("=", 1)
            k = k.strip()
            v = v.strip().split("#", 1)[0].strip()
            if len(v) >= 2 and v[0] == '"' and v[-1] == '"':
                pkg[k] = v[1:-1]
    return {"package": pkg}


def _parse_cargo_lock_fallback(text: str) -> dict:
    packages: list[dict] = []
    cur: dict | None = None
    in_deps = False
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line == "[[package]]":
            if cur is not None:
                packages.append(cur)
            cur = {}
            in_deps = False
            continue
        if cur is None:
            continue
        if in_deps:
            if "]" in line:
                for m in re.findall(r'"([^"]+)"', line):
                    cur.setdefault("dependencies", []).append(m)
                in_deps = False
            else:
                for m in re.findall(r'"([^"]+)"', line):
                    cur.setdefault("dependencies", []).append(m)
            continue
        if "=" in line:
            k, v = line.split("=", 1)
            k = k.strip()
            v = v.strip()
            if k == "dependencies":
                cur["dependencies"] = re.findall(r'"([^"]+)"', v)
                if "]" not in v:
                    in_deps = True
            elif len(v) >= 2 and v[0] == '"' and v[-1] == '"':
                cur[k] = v[1:-1]
    if cur is not None:
        packages.append(cur)
    return {"package": packages}


def _load_cargo_toml(path: pathlib.Path) -> dict:
    text = path.read_text(encoding="utf-8")
    if tomllib is not None:
        return tomllib.loads(text)
    return _parse_cargo_toml_fallback(text)


def _load_cargo_lock(path: pathlib.Path) -> dict:
    text = path.read_text(encoding="utf-8")
    if tomllib is not None:
        return tomllib.loads(text)
    return _parse_cargo_lock_fallback(text)


def _spdx_id(name: str, version: str, seen: set[str]) -> str:
    base = f"SPDXRef-Package-{SPDX_ID_SAFE_RE.sub('-', name)}-{SPDX_ID_SAFE_RE.sub('-', version)}"
    candidate = base
    suffix = 2
    while candidate in seen:
        candidate = f"{base}-{suffix}"
        suffix += 1
    seen.add(candidate)
    return candidate


def _created_timestamp(override: str | None = None) -> str:
    if override:
        return override
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if epoch and epoch.isdigit():
        dt = datetime.datetime.fromtimestamp(int(epoch), tz=datetime.timezone.utc)
    else:
        dt = datetime.datetime.now(tz=datetime.timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def _parse_dep_spec(dep: str) -> tuple[str, str | None]:
    parts = dep.split()
    name = parts[0]
    version = parts[1] if len(parts) >= 2 and not parts[1].startswith("(") else None
    return name, version


def generate_sbom(
    repo_root: pathlib.Path,
    target: str = "generic",
    created: str | None = None,
) -> dict:
    cargo_toml = _load_cargo_toml(repo_root / "Cargo.toml")
    cargo_lock = _load_cargo_lock(repo_root / "Cargo.lock")

    root_pkg = cargo_toml["package"]
    root_name = root_pkg["name"]
    root_version = root_pkg["version"]
    root_license = root_pkg.get("license", "GPL-3.0-or-later")

    lock_packages = sorted(
        cargo_lock.get("package", []),
        key=lambda p: (p["name"].lower(), p["version"]),
    )

    seen_ids: set[str] = set()
    id_by_name_version: dict[tuple[str, str], str] = {}
    ids_by_name: dict[str, list[str]] = {}
    packages: list[dict] = []

    for pkg in lock_packages:
        name = pkg["name"]
        version = pkg["version"]
        is_root = name == root_name and "source" not in pkg
        spdx_id = "SPDXRef-Package-ounce-runtime" if is_root else _spdx_id(name, version, seen_ids)
        seen_ids.add(spdx_id)
        id_by_name_version[(name, version)] = spdx_id
        ids_by_name.setdefault(name, []).append(spdx_id)

        source = pkg.get("source")
        if is_root:
            download_location = f"{REPO_URL}/releases/tag/v{root_version}"
            declared_license = root_license
        elif not source:
            download_location = f"{REPO_URL}"
            declared_license = "MIT" if name == "ounce-recompiler" else "NOASSERTION"
        elif source.startswith("registry+https://github.com/rust-lang/crates.io-index"):
            download_location = f"https://crates.io/api/v1/crates/{name}/{version}/download"
            declared_license = "NOASSERTION"
        else:
            download_location = source
            declared_license = "NOASSERTION"

        entry: dict = {
            "SPDXID": spdx_id,
            "name": name,
            "versionInfo": version,
            "downloadLocation": download_location,
            "filesAnalyzed": False,
            "licenseConcluded": "NOASSERTION",
            "licenseDeclared": declared_license,
            "copyrightText": "NOASSERTION",
            "externalRefs": [
                {
                    "referenceCategory": "PACKAGE-MANAGER",
                    "referenceType": "purl",
                    "referenceLocator": f"pkg:cargo/{name}@{version}",
                }
            ],
        }
        checksum = pkg.get("checksum")
        if checksum:
            entry["checksums"] = [{"algorithm": "SHA256", "checksumValue": checksum}]
        packages.append(entry)

    root_id = id_by_name_version.get((root_name, root_version), "SPDXRef-Package-ounce-runtime")
    relationships: list[dict] = [
        {
            "spdxElementId": "SPDXRef-DOCUMENT",
            "relationshipType": "DESCRIBES",
            "relatedSpdxElement": root_id,
        }
    ]

    for pkg in lock_packages:
        src_id = id_by_name_version[(pkg["name"], pkg["version"])]
        for dep in pkg.get("dependencies", []):
            dep_name, dep_ver = _parse_dep_spec(dep)
            target_id: str | None = None
            if dep_ver is not None:
                target_id = id_by_name_version.get((dep_name, dep_ver))
            if target_id is None:
                candidates = ids_by_name.get(dep_name, [])
                if len(candidates) == 1:
                    target_id = candidates[0]
                elif candidates:
                    target_id = candidates[0]
            if target_id:
                relationships.append(
                    {
                        "spdxElementId": src_id,
                        "relationshipType": "DEPENDS_ON",
                        "relatedSpdxElement": target_id,
                    }
                )

    return {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": f"{root_name}-{root_version}-{target}",
        "documentNamespace": f"{REPO_URL}/spdx/{root_name}-{root_version}-{target}",
        "creationInfo": {
            "created": _created_timestamp(created),
            "creators": [TOOL_CREATOR],
        },
        "documentDescribes": [root_id],
        "packages": packages,
        "relationships": relationships,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, help="Output path for the SPDX 2.3 JSON file")
    parser.add_argument("--target", default="generic", help="Rust target triple for document naming")
    parser.add_argument(
        "--repo-root",
        default=None,
        help="Repository root containing Cargo.toml and Cargo.lock",
    )
    parser.add_argument("--created", default=None, help="Explicit ISO-8601 UTC creation timestamp")
    args = parser.parse_args(argv)

    repo_root = (
        pathlib.Path(args.repo_root)
        if args.repo_root
        else pathlib.Path(__file__).resolve().parent.parent
    )
    sbom = generate_sbom(repo_root, target=args.target, created=args.created)

    out_path = pathlib.Path(args.output)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(sbom, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
