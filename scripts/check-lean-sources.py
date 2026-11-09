#!/usr/bin/env python3
"""Check Lean fixtures' transcribed Rust sources against committed anchor manifests.

Each fixture carries `crates/<crate>/formal/SOURCES.sha256`:

    # schema: 1
    # status: anchored | pending-reanchor | obsolete
    # anchor: <git commit the model was transcribed at>
    # reviewed: <YYYY-MM-DD>
    <sha256 of the blob at HEAD>  <repo-relative source path>

The manifest pins every Rust source file the Lean model cites. The checker compares
the pinned hash with the `HEAD:` blob and reports changed/missing paths:

* `anchored`        - a changed/missing source is a blocking failure (exit 1);
* `pending-reanchor`- changed/missing sources are printed as advisories (exit 0),
                      so known drift does not break CI before the model is re-anchored;
* `obsolete`        - the fixture models deleted code; it is reported and skipped.

`--strict` treats pending fixtures as blocking; `--update` rewrites the hashes at HEAD
without touching status/anchor/reviewed; `--manifest` checks only the given manifests.

Usage:
    python3 scripts/check-lean-sources.py
    python3 scripts/check-lean-sources.py --strict
    python3 scripts/check-lean-sources.py --manifest /tmp/anchored.sha256
    python3 scripts/check-lean-sources.py --update
"""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import subprocess
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
SCHEMA = "1"
STATUSES = ("anchored", "pending-reanchor", "obsolete")
HEX = set("0123456789abcdef")


def git_blob(path: str) -> bytes | None:
    """Return the `HEAD:<path>` blob bytes, or None when the path is not in HEAD."""
    proc = subprocess.run(
        ["git", "-C", str(REPO_ROOT), "show", f"HEAD:{path}"],
        capture_output=True,
    )
    return proc.stdout if proc.returncode == 0 else None


def parse_manifest(manifest: pathlib.Path):
    headers: dict[str, str] = {}
    entries: list[tuple[str, str]] = []
    errors: list[str] = []
    for lineno, raw in enumerate(manifest.read_text().splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        if line.startswith("#"):
            body = line[1:].strip()
            if ":" in body:
                key, value = body.split(":", 1)
                headers[key.strip().lower()] = value.strip()
            continue
        parts = line.split()
        if len(parts) != 2:
            errors.append(f"{manifest}:{lineno}: expected '<sha256>  <path>'")
            continue
        digest, source = parts
        digest = digest.lower()
        if len(digest) != 64 or not set(digest) <= HEX:
            errors.append(f"{manifest}:{lineno}: malformed sha256 for {source}")
            continue
        source_path = pathlib.PurePosixPath(source)
        if source_path.is_absolute() or ".." in source_path.parts:
            errors.append(f"{manifest}:{lineno}: source path must be repo-relative: {source}")
            continue
        entries.append((digest, source))
    if headers.get("schema") != SCHEMA:
        errors.append(f"{manifest}: schema must be {SCHEMA}")
    if headers.get("status") not in STATUSES:
        errors.append(f"{manifest}: status must be one of {', '.join(STATUSES)}")
    if not headers.get("anchor"):
        errors.append(f"{manifest}: missing '# anchor:' header")
    return headers, entries, errors


def check_one(manifest: pathlib.Path, strict: bool) -> dict:
    headers, entries, errors = parse_manifest(manifest)
    status = headers.get("status", "?")
    result = {
        "manifest": manifest,
        "status": status,
        "anchor": headers.get("anchor", "?"),
        "errors": errors,
        "changed": [],
        "missing": [],
        "unchanged": 0,
    }
    if status == "obsolete":
        return result
    for digest, source in entries:
        blob = git_blob(source)
        if blob is None:
            result["missing"].append(source)
            continue
        if hashlib.sha256(blob).hexdigest() == digest:
            result["unchanged"] += 1
        else:
            result["changed"].append(source)
    return result


def blocking(result: dict, strict: bool) -> bool:
    if result["errors"]:
        return True
    if result["status"] == "obsolete":
        return False
    if result["status"] == "anchored" or strict:
        return bool(result["changed"] or result["missing"])
    return False


def display_path(manifest: pathlib.Path) -> str:
    try:
        return str(manifest.relative_to(REPO_ROOT))
    except ValueError:
        return str(manifest)


def print_result(result: dict, strict: bool) -> None:
    rel = display_path(result["manifest"])
    print(f"fixture {rel} [{result['status']} anchor {result['anchor']}]")
    for error in result["errors"]:
        print(f"  ERROR {error}")
    if result["status"] == "obsolete":
        print("  skipped: fixture models deleted code (see its SCOPE.md)")
        return
    if result["status"] == "pending-reanchor":
        for source in result["changed"]:
            print(f"  ADVISORY changed: {source} (re-anchor before flipping to anchored)")
        for source in result["missing"]:
            print(f"  ADVISORY missing: {source}")
    else:
        for source in result["changed"]:
            print(f"  ERROR changed: {source}")
        for source in result["missing"]:
            print(f"  ERROR missing: {source}")
    if result["unchanged"]:
        print(f"  ok: {result['unchanged']} source(s) unchanged")
    if result["status"] == "pending-reanchor" and strict and (result["changed"] or result["missing"]):
        print("  (--strict: pending drift is blocking)")


def update_manifest(manifest: pathlib.Path) -> int:
    headers, entries, errors = parse_manifest(manifest)
    if errors:
        for error in errors:
            print(f"ERROR {error}", file=sys.stderr)
        return 1
    if headers.get("status") == "obsolete":
        print(f"skip obsolete manifest {display_path(manifest)}")
        return 0
    hashes: dict[str, str] = {}
    for _digest, source in entries:
        blob = git_blob(source)
        if blob is None:
            print(f"ERROR missing at HEAD: {source}", file=sys.stderr)
            return 1
        hashes[source] = hashlib.sha256(blob).hexdigest()
    out_lines: list[str] = []
    updated = 0
    for raw in manifest.read_text().splitlines():
        line = raw.strip()
        parts = line.split()
        if line and not line.startswith("#") and len(parts) == 2 and parts[1] in hashes:
            out_lines.append(f"{hashes[parts[1]]}  {parts[1]}")
            updated += 1
        else:
            out_lines.append(raw)
    manifest.write_text("\n".join(out_lines) + "\n")
    print(f"updated {display_path(manifest)}: {updated} hash(es)")
    return 0


def discover() -> list[pathlib.Path]:
    return sorted(REPO_ROOT.glob("crates/*/formal/SOURCES.sha256"))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Check Lean fixture transcription anchors against HEAD.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--manifest", action="append", default=[], help="check only this manifest (repeatable)")
    parser.add_argument("--strict", action="store_true", help="treat pending-reanchor drift as blocking")
    parser.add_argument("--update", action="store_true", help="rewrite hashes at HEAD (keeps status/anchor)")
    args = parser.parse_args(argv)

    manifests = [pathlib.Path(m).resolve() for m in args.manifest] if args.manifest else discover()
    if not manifests:
        print("ERROR no crates/*/formal/SOURCES.sha256 manifests found", file=sys.stderr)
        return 1

    if args.update:
        failures = 0
        for manifest in manifests:
            failures += update_manifest(manifest)
        return 1 if failures else 0

    blocking_results = 0
    pending_results = 0
    obsolete_results = 0
    for manifest in manifests:
        result = check_one(manifest, args.strict)
        print_result(result, args.strict)
        if result["status"] == "obsolete":
            obsolete_results += 1
        elif blocking(result, args.strict):
            blocking_results += 1
        else:
            pending_results += 1
    print(
        f"summary: {len(manifests)} manifest(s), {blocking_results} blocking, "
        f"{pending_results} advisory/anchored-clean, {obsolete_results} obsolete"
    )
    return 1 if blocking_results else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
