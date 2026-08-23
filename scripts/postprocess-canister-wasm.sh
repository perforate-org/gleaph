#!/usr/bin/env bash
set -euo pipefail

# Canonical post-processing for every Gleaph canister wasm artifact, on every
# build path (root icp.yaml script adapters, scripts/deploy-demo-local.sh's
# instrumented router, crates/pocket-ic-tests/build.rs fixtures,
# scripts/check-codegen-local-e2e.sh). Each input file is rewritten in place.
#
# Pipeline (the order is the contract and must stay identical everywhere):
#   1. ic-wasm metadata "cargo:version"   (build-tool provenance)
#   2. ic-wasm metadata "template:type"   (rust)
#   3. candid-extractor -> ic-wasm metadata "candid:service" (public surface)
#   4. ic-wasm shrink                     (unused functions + debug info)
#
# The name section is kept deliberately (--keep-name-section) so stack traces
# and profiling output stay symbolized; it is a custom section and does not
# count against the IC's install-time code-section limit. If a size lever ever
# forces dropping it, record that decision in design/implementation-gaps.md
# (GAP-2026-08-23-002 owns the standing measurements).
#
# Usage: postprocess-canister-wasm.sh MODULE.wasm [MODULE.wasm ...]

fail_missing_tool() {
  case "$1" in
    ic-wasm)
      echo >&2 'ic-wasm not found. To install ic-wasm, see https://github.com/dfinity/ic-wasm'
      ;;
    candid-extractor)
      echo >&2 'candid-extractor not found. Run `cargo install candid-extractor` to install it.'
      ;;
  esac
  exit 1
}

command -v ic-wasm >/dev/null 2>&1 || fail_missing_tool ic-wasm
command -v candid-extractor >/dev/null 2>&1 || fail_missing_tool candid-extractor
command -v cargo >/dev/null 2>&1 || { echo >&2 'cargo not found on PATH'; exit 1; }

if [[ $# -lt 1 ]]; then
  echo "usage: $0 MODULE.wasm [MODULE.wasm ...]" >&2
  exit 1
fi

for wasm in "$@"; do
  if [[ ! -f "$wasm" ]]; then
    echo "ERROR: missing wasm artifact: $wasm" >&2
    exit 1
  fi
done

for wasm in "$@"; do
  ic-wasm "$wasm" -o "$wasm" metadata "cargo:version" -d "$(cargo --version)" --keep-name-section
  ic-wasm "$wasm" -o "$wasm" metadata "template:type" -d "rust" --keep-name-section
  did="$(mktemp)"
  # shellcheck disable=SC2064  # the temp path is fixed for this invocation
  trap 'rm -f "$did"' EXIT
  candid-extractor "$wasm" >"$did"
  ic-wasm "$wasm" -o "$wasm" metadata "candid:service" -f "$did" -v public --keep-name-section
  rm -f "$did"
  ic-wasm "$wasm" -o "$wasm" shrink --keep-name-section
  printf '[postprocess-canister-wasm] %s: %s bytes\n' "$wasm" "$(wc -c <"$wasm" | tr -d ' ')"
done
