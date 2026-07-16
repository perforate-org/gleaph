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

ARTIFACT_DIR="$ROOT/.icp/cache/artifacts"

export HOME="$ICP_CLI_HOME"
export COREPACK_HOME="$ICP_COREPACK_HOME"
export XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME"
export XDG_DATA_HOME="$ICP_XDG_DATA_HOME"
export RUSTUP_HOME="$RUSTUP_HOME"
export CARGO_HOME="$CARGO_HOME"
export DO_NOT_TRACK="${DO_NOT_TRACK:-1}"

icp_cmd() {
  env \
    HOME="$ICP_CLI_HOME" \
    COREPACK_HOME="$ICP_COREPACK_HOME" \
    XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME" \
    XDG_DATA_HOME="$ICP_XDG_DATA_HOME" \
    RUSTUP_HOME="$RUSTUP_HOME" \
    CARGO_HOME="$CARGO_HOME" \
    DO_NOT_TRACK="${DO_NOT_TRACK:-1}" \
    icp "$@"
}

normalize_candid() {
  # Strip trailing whitespace and collapse trailing blank lines to a single
  # newline so a final newline difference does not fail the check.
  awk '{
    gsub(/[[:space:]]+$/, "")
    print
  }' | awk 'NF || printed { printed=1; print }' | sed -e '$a\'
}

extract_did() {
  local name="$1"
  local wasm="$ARTIFACT_DIR/$name"
  icp_cmd build "$name"
  if [[ ! -f "$wasm" ]]; then
    echo "ERROR: wasm artifact not found: $wasm" >&2
    exit 1
  fi
  candid-extractor "$wasm" | normalize_candid
}

compare_did() {
  local name="$1"
  local committed="$2"
  local extracted
  extracted="$(extract_did "$name")"
  local normalized_committed
  normalized_committed="$(normalize_candid < "$committed")"
  if [[ "$extracted" != "$normalized_committed" ]]; then
    diff -u "$committed" <(printf '%s\n' "$extracted") >&2 || true
    echo "ERROR: Candid drift detected for $name" >&2
    echo "Committed: $committed" >&2
    echo "Run scripts/generate-router-and-graph-bindings.sh to regenerate." >&2
    exit 1
  fi
}

mkdir -p "$ARTIFACT_DIR"

compare_did gleaph-router "$ROOT/frontend/apps/social-demo/src/generated/gleaph_router/declarations/gleaph_router.did"
compare_did gleaph-graph-shard-0 "$ROOT/frontend/apps/social-demo/src/generated/gleaph_graph/declarations/gleaph_graph.did"

echo "Router and Graph Candid interfaces match committed bindings."
