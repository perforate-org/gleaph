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

BINDINGS_DIR="$ROOT/frontend/apps/social-demo/src/generated"
ARTIFACT_DIR="$ROOT/.icp/cache/artifacts"

export HOME="$ICP_CLI_HOME"
export COREPACK_HOME="$ICP_COREPACK_HOME"
export XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME"
export XDG_DATA_HOME="$ICP_XDG_DATA_HOME"
export RUSTUP_HOME="$RUSTUP_HOME"
export CARGO_HOME="$CARGO_HOME"
export DO_NOT_TRACK="${DO_NOT_TRACK:-1}"

log() {
  printf '[bindings] %s\n' "$*" >&2
}

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

extract_did() {
  local name="$1"
  local out_path="$2"
  local wasm="$ARTIFACT_DIR/$name"
  if [[ ! -f "$wasm" ]]; then
    log "Building $name to produce wasm artifact"
    icp_cmd build "$name"
  fi
  if [[ ! -f "$wasm" ]]; then
    log "ERROR: wasm artifact not found after build: $wasm"
    exit 1
  fi
  candid-extractor "$wasm" > "$out_path"
}

generate_bindings() {
  local did_path="$1"
  local out_dir="$2"
  rm -rf "$out_dir"
  mkdir -p "$out_dir"
  # Use the same @icp-sdk/bindgen version as the workspace.
  (
    cd "$ROOT/frontend/apps/social-demo"
    NPM_CONFIG_CACHE="${NPM_CONFIG_CACHE:-/tmp/.npm}" npx -y @icp-sdk/bindgen \
      --did-file "$did_path" \
      --out-dir "$out_dir" \
      --force \
      --declarations-flat
  )
  # The flat layout emits declarations next to the actor file. Move them into a
  # declarations/ subdir to match the existing social_demo_gateway layout, and
  # keep the .did file itself so CI can diff it against freshly-extracted wasm.
  local actor_name
  actor_name="$(basename "$out_dir")"
  mkdir -p "$out_dir/declarations"
  mv "$out_dir/${actor_name}.did.d.ts" "$out_dir/declarations/"
  mv "$out_dir/${actor_name}.did.js" "$out_dir/declarations/"
  cp "$did_path" "$out_dir/declarations/${actor_name}.did"
}

mkdir -p "$ARTIFACT_DIR"

log "Extracting Router Candid interface"
extract_did gleaph-router "$ARTIFACT_DIR/gleaph_router.did"
log "Generating Router TypeScript bindings"
generate_bindings "$ARTIFACT_DIR/gleaph_router.did" "$BINDINGS_DIR/gleaph_router"

log "Extracting Graph Candid interface"
extract_did gleaph-graph-shard-0 "$ARTIFACT_DIR/gleaph_graph.did"
log "Generating Graph TypeScript bindings"
generate_bindings "$ARTIFACT_DIR/gleaph_graph.did" "$BINDINGS_DIR/gleaph_graph"

log "Router and Graph bindings regenerated at $BINDINGS_DIR"
