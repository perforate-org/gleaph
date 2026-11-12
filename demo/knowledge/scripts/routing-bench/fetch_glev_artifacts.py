#!/usr/bin/env python3
"""Download and verify the pinned Glev release assets for the routing canister.

The manifest (`crates/gleaph-routing-canister/artifacts.toml`) pins the release tag, runtime git
revision and the SHA-256 of the tokenizer asset. This script verifies that the manifest
agrees with the canister's Cargo.toml revision, downloads the assets from the Glev release (using
the `gh` CLI, so set GH_TOKEN/GLEV_TOKEN for the private repository), and fails if an asset is
unavailable or its checksum/size differs.

It prints shell assignments for the verified files:

    eval "$(python3 demo/knowledge/scripts/routing-bench/fetch_glev_artifacts.py)"
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
DEFAULT_MANIFEST = ROOT / "crates" / "gleaph-routing-canister" / "artifacts.toml"
CANISTER_MANIFEST = ROOT / "crates" / "gleaph-routing-canister" / "Cargo.toml"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def canister_revision() -> str:
    text = CANISTER_MANIFEST.read_text()
    match = re.search(r'glev-general-router\s*=\s*\{[^}]*rev\s*=\s*"([0-9a-f]{40})"', text)
    if not match:
        raise SystemExit(f"cannot find the pinned glev-general-router revision in {CANISTER_MANIFEST}")
    return match.group(1)


def gh() -> str:
    binary = shutil.which("gh")
    if not binary:
        raise SystemExit(
            "the gh CLI is required to download private Glev releases; install gh and set GH_TOKEN"
        )
    return binary


def download(repository: str, tag: str, name: str, out: Path) -> Path:
    target = out / name
    if target.exists():
        return target
    out.mkdir(parents=True, exist_ok=True)
    command = [
        gh(),
        "release",
        "download",
        tag,
        "--repo",
        repository,
        "--pattern",
        name,
        "--dir",
        str(out),
        "--clobber",
    ]
    subprocess.run(command, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return target


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--out", type=Path, default=ROOT / "target" / "glev-artifacts")
    parser.add_argument("--check", action="store_true", help="verify existing files without downloading")
    args = parser.parse_args()

    manifest = tomllib.loads(args.manifest.read_text())
    release = manifest["release"]
    wire_version = manifest["wire"]["version"]
    revision = release["runtime_revision"]
    pinned = canister_revision()
    if revision != pinned:
        raise SystemExit(
            f"manifest runtime_revision {revision} does not match Cargo.toml revision {pinned}"
        )

    verified: dict[str, Path] = {}
    for slot in ("tokenizer",):
        asset = manifest["assets"][slot]
        name, expected = asset["file"], asset["sha256"]
        path = args.out / name
        if not path.exists():
            if args.check:
                raise SystemExit(f"missing {path}; run without --check to download")
            print(f"downloading {name} from {release['repository']}@{release['tag']}", file=sys.stderr)
            path = download(release["repository"], release["tag"], name, args.out)
        actual = sha256(path)
        if actual != expected:
            path.unlink(missing_ok=True)
            raise SystemExit(f"{name}: sha256 {actual} does not match the pinned {expected}")
        expected_bytes = asset.get("bytes")
        if expected_bytes is not None and path.stat().st_size != expected_bytes:
            raise SystemExit(
                f"{name}: size {path.stat().st_size} does not match the pinned {expected_bytes}"
            )
        verified[slot] = path
        print(f"verified {name} ({actual})", file=sys.stderr)

    print(f"GLEV_TOKENIZER={verified['tokenizer']}")
    print(f"GLEV_WIRE_VERSION={wire_version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
