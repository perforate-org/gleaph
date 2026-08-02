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

ROUTER_DID="$ROOT/frontend/apps/social-demo/src/generated/gleaph_router/declarations/gleaph_router.did"

# The Router Candid file is the generated public-surface source for the social frontend. Keep this
# check scoped to service/type declarations: historical ADR prose and migration notes may retain
# old names, while runtime bindings must not expose a second wire path.
assert_router_symbol_present() {
  local symbol="$1"
  local pattern="$2"
  if ! grep -Eq "$pattern" "$ROUTER_DID"; then
    echo "ERROR: Router Candid is missing required symbol $symbol" >&2
    exit 1
  fi
}

assert_router_symbol_absent() {
  local symbol="$1"
  local pattern="$2"
  if grep -Eq "$pattern" "$ROUTER_DID"; then
    echo "ERROR: superseded Router Candid symbol remains: $symbol" >&2
    exit 1
  fi
}

assert_router_symbol_present "gql_query" '^[[:space:]]*gql_query[[:space:]]*:'
assert_router_symbol_present "gql_mutate" '^[[:space:]]*gql_mutate[[:space:]]*:'
assert_router_symbol_present "prepared_query" '^[[:space:]]*prepared_query[[:space:]]*:'
assert_router_symbol_present "prepared_mutate" '^[[:space:]]*prepared_mutate[[:space:]]*:'
assert_router_symbol_present "atomic_insert" '^[[:space:]]*atomic_insert[[:space:]]*:'
assert_router_symbol_present "mutation_status" '^[[:space:]]*mutation_status[[:space:]]*:'
assert_router_symbol_present "atomic_insert_status" '^[[:space:]]*atomic_insert_status[[:space:]]*:'
assert_router_symbol_present "bulk_load" '^[[:space:]]*bulk_load[[:space:]]*:'
assert_router_symbol_present "bulk_load_status" '^[[:space:]]*bulk_load_status[[:space:]]*:'

assert_router_symbol_absent "gql_execute" '^[[:space:]]*gql_execute[[:space:]]*:'
assert_router_symbol_absent "execute_prepared" '^[[:space:]]*execute_prepared(_update)?[[:space:]]*:'
assert_router_symbol_absent "prepared_update" '^[[:space:]]*prepared_update(_idempotent)?[[:space:]]*:'
assert_router_symbol_absent "batch_insert" '^[[:space:]]*batch_insert[[:space:]]*:'
assert_router_symbol_absent "get_mutation_status" '^[[:space:]]*get_mutation_status[[:space:]]*:'
assert_router_symbol_absent "BatchRequest" '^type[[:space:]]+Batch(Request|Response|Receipt|Operation|Vertex|Edge|Endpoint|Property)'
retired_gql_method="gql_execute""_batch"
retired_gql_type="GqlExecuteIdempotent""Batch"
assert_router_symbol_absent "$retired_gql_method" "^[[:space:]]*$retired_gql_method[[:space:]]*:"
assert_router_symbol_absent "$retired_gql_type" "^type[[:space:]]+$retired_gql_type"

GRAPH_DID="$ROOT/frontend/apps/social-demo/src/generated/gleaph_graph/declarations/gleaph_graph.did"
retired_graph_pattern='ExecutePlanBat''ch(Typed|Shared)|ExecutePlanBulkBat''ch|execute_plan_update_bat''ch_bulk'
if grep -Eq "$retired_graph_pattern" "$GRAPH_DID"; then
  echo "ERROR: retired Graph GQL list transport remains in generated Candid" >&2
  exit 1
fi

echo "Router and Graph Candid interfaces match committed bindings."
