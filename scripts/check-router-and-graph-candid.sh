#!/usr/bin/env bash
set -euo pipefail

# Checks the deployed Router/Graph Candid surfaces against the L1/L2 contract.
#
# The social frontend talks to the Router through @gleaph/sdk (no committed actor
# bindings), so this script extracts the live .did from freshly built wasm and
# asserts the public-surface names and call kinds. It is the live counterpart of
# the source-level `scripts/router-l1-source-surface.contract.test.mjs` (which
# covers Router client.rs / types.rs and the SDK/CDK sources).

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

mkdir -p "$ARTIFACT_DIR"

router_did="$(extract_did gleaph-router)"

# The Router Candid is the live public surface for the SDK-direct frontend. Keep these
# checks scoped to service/type declarations: historical ADR prose and migration notes
# may retain old names, while the deployed wire surface must not expose a second path.
assert_router_symbol_present() {
  local symbol="$1"
  local pattern="$2"
  if ! grep -Eq "$pattern" <<<"$router_did"; then
    echo "ERROR: Router Candid is missing required symbol $symbol" >&2
    exit 1
  fi
}

assert_router_symbol_absent() {
  local symbol="$1"
  local pattern="$2"
  if grep -Eq "$pattern" <<<"$router_did"; then
    echo "ERROR: superseded Router Candid symbol remains: $symbol" >&2
    exit 1
  fi
}

assert_router_call_kind() {
  local symbol="$1"
  local kind="$2"
  local line
  line="$(grep -E "^[[:space:]]*${symbol}[[:space:]]*:" <<<"$router_did" | head -n 1)"
  if [[ -z "$line" ]]; then
    echo "ERROR: Router Candid is missing required symbol $symbol" >&2
    exit 1
  fi
  case "$kind" in
    composite_query)
      if [[ "$line" != *"composite_query;" ]]; then
        echo "ERROR: Router Candid symbol $symbol is not a composite_query" >&2
        exit 1
      fi
      ;;
    query)
      if [[ "$line" != *"query;" || "$line" == *"composite_query;" ]]; then
        echo "ERROR: Router Candid symbol $symbol is not a query" >&2
        exit 1
      fi
      ;;
    update)
      if [[ "$line" == *"query;" ]]; then
        echo "ERROR: Router Candid symbol $symbol is not an update" >&2
        exit 1
      fi
      ;;
  esac
}

for symbol in gql_query gql_mutate prepared_query prepared_mutate atomic_insert \
  mutation_status atomic_insert_status bulk_load bulk_load_status ensure_properties; do
  assert_router_symbol_present "$symbol" "^[[:space:]]*${symbol}[[:space:]]*:"
done

assert_router_call_kind ensure_properties update
assert_router_call_kind gql_query composite_query
assert_router_call_kind prepared_query composite_query
assert_router_call_kind mutation_status query
assert_router_call_kind atomic_insert_status query
assert_router_call_kind bulk_load_status query
for symbol in gql_mutate prepared_mutate atomic_insert bulk_load; do
  assert_router_call_kind "$symbol" update
done

assert_router_symbol_absent "gql_execute" '^[[:space:]]*gql_execute[[:space:]]*:'
assert_router_symbol_absent "ensure_property" '^[[:space:]]*ensure_property[[:space:]]*:'
assert_router_symbol_absent "execute_prepared" '^[[:space:]]*execute_prepared(_update)?[[:space:]]*:'
assert_router_symbol_absent "prepared_update" '^[[:space:]]*prepared_update(_idempotent)?[[:space:]]*:'
assert_router_symbol_absent "batch_insert" '^[[:space:]]*batch_insert[[:space:]]*:'
assert_router_symbol_absent "get_mutation_status" '^[[:space:]]*get_mutation_status[[:space:]]*:'
assert_router_symbol_absent "BatchRequest" '^type[[:space:]]+Batch(Request|Response|Receipt|Operation|Vertex|Edge|Endpoint|Property)'
retired_gql_method="gql_execute""_batch"
retired_gql_type="GqlExecuteIdempotent""Batch"
assert_router_symbol_absent "$retired_gql_method" "^[[:space:]]*${retired_gql_method}[[:space:]]*:"
assert_router_symbol_absent "$retired_gql_type" "^type[[:space:]]+$retired_gql_type"

graph_did="$(extract_did gleaph-graph-shard-0)"
retired_graph_pattern='ExecutePlanBat''ch(Typed|Shared)|ExecutePlanBulkBat''ch|execute_plan_update_bat''ch_bulk'
if grep -Eq "$retired_graph_pattern" <<<"$graph_did"; then
  echo "ERROR: retired Graph GQL list transport remains in generated Candid" >&2
  exit 1
fi

echo "Router and Graph Candid surfaces match the L1/L2 contract."
