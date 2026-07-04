#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORIGINAL_HOME="${HOME:-}"

ICP_CLI_HOME="${ICP_CLI_HOME:-$ROOT/.icp/home}"
ICP_COREPACK_HOME="${ICP_COREPACK_HOME:-$ROOT/.icp/corepack-home}"
ICP_XDG_CACHE_HOME="${ICP_XDG_CACHE_HOME:-$ROOT/.icp/xdg-cache}"
ICP_XDG_DATA_HOME="${ICP_XDG_DATA_HOME:-$ROOT/.icp/xdg-data}"
RUSTUP_HOME="${RUSTUP_HOME:-$ORIGINAL_HOME/.rustup}"
CARGO_HOME="${CARGO_HOME:-$ORIGINAL_HOME/.cargo}"

COMMITTED_DID="$ROOT/crates/social-demo-gateway/social_demo_gateway.did"
ARTIFACT_DIR="$ROOT/.icp/cache/artifacts"
WASM="$ARTIFACT_DIR/gleaph-social-demo-gateway"

# Normalize: strip trailing blank lines so a final newline difference does not fail the check.
normalize_candid() {
  sed -e 's/[[:space:]]*$//' -e :a -e '/^\n*$/{$d;N;};/\n$/ba'
}

cd "$ROOT"

mkdir -p "$ARTIFACT_DIR"

env \
  HOME="$ICP_CLI_HOME" \
  COREPACK_HOME="$ICP_COREPACK_HOME" \
  XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME" \
  XDG_DATA_HOME="$ICP_XDG_DATA_HOME" \
  RUSTUP_HOME="$RUSTUP_HOME" \
  CARGO_HOME="$CARGO_HOME" \
  DO_NOT_TRACK="${DO_NOT_TRACK:-1}" \
  icp build gleaph-social-demo-gateway

if [[ ! -f "$WASM" ]]; then
  printf 'Expected wasm artifact not found: %s\n' "$WASM" >&2
  exit 1
fi

EXTRACTED="$(env \
  HOME="$ICP_CLI_HOME" \
  COREPACK_HOME="$ICP_COREPACK_HOME" \
  XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME" \
  XDG_DATA_HOME="$ICP_XDG_DATA_HOME" \
  RUSTUP_HOME="$RUSTUP_HOME" \
  CARGO_HOME="$CARGO_HOME" \
  DO_NOT_TRACK="${DO_NOT_TRACK:-1}" \
  ic-wasm "$WASM" metadata candid:service | normalize_candid)"

COMMITTED="$(normalize_candid < "$COMMITTED_DID")"

if [[ "$COMMITTED" != "$EXTRACTED" ]]; then
  diff -u "$COMMITTED_DID" <(printf '%s\n' "$EXTRACTED") >&2 || true
  printf '\nGateway Candid drift detected.\n' >&2
  printf 'Committed: %s\n' "$COMMITTED_DID" >&2
  printf 'Update the committed .did file or the Rust interface so they match.\n' >&2
  exit 1
fi

printf 'Gateway Candid matches committed interface.\n'
