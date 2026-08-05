#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
E2E_ROOT="$ROOT/crates/codegen/e2e"
SDK_ROOT="$ROOT/sdk/client/js"
GRAPH_NAME="gleaph.codegen.e2e"
INSTALL_MODE="${GLEAPH_CODEGEN_E2E_INSTALL_MODE:-install}"
DEPLOYER_IDENTITY="gleaph-codegen-e2e-deployer"
LOCAL_URL="${GLEAPH_CODEGEN_E2E_LOCAL_URL:-http://localhost:8000}"

ICP_CLI_HOME="${ICP_CLI_HOME:-$ROOT/.icp/codegen-e2e-home}"
ICP_COREPACK_HOME="${ICP_COREPACK_HOME:-$ROOT/.icp/codegen-e2e-corepack-home}"
ICP_XDG_CACHE_HOME="${ICP_XDG_CACHE_HOME:-$ROOT/.icp/codegen-e2e-xdg-cache}"
ICP_XDG_DATA_HOME="${ICP_XDG_DATA_HOME:-$ROOT/.icp/codegen-e2e-xdg-data}"
ORIGINAL_HOME="${HOME:-}"
RUSTUP_HOME="${RUSTUP_HOME:-$ORIGINAL_HOME/.rustup}"
CARGO_HOME="${CARGO_HOME:-$ORIGINAL_HOME/.cargo}"
GENERATED="$SDK_ROOT/.codegen-local-e2e.generated.js"
IDENTITY_PEM="$ICP_CLI_HOME/Library/Application Support/org.dfinity.icp-cli/identity/keys/$DEPLOYER_IDENTITY.pem"

cleanup() {
  rm -f "$GENERATED"
}
trap cleanup EXIT

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

ensure_local_network() {
  curl --silent --show-error --fail "$LOCAL_URL/api/v2/status" >/dev/null 2>&1 || {
    echo "local IC network is not reachable at $LOCAL_URL" >&2
    exit 1
  }
}

create_canister() {
  icp_cmd canister create \
    --network "$LOCAL_URL" \
    --root-key fetch \
    --identity anonymous \
    --controller "$1" \
    --detached \
    --quiet
}

install_canister() {
  local canister="$1"
  local wasm="$2"
  local args="$3"
  icp_cmd canister install \
    --network "$LOCAL_URL" \
    --root-key fetch \
    --identity "$DEPLOYER_IDENTITY" \
    -y \
    --mode "$INSTALL_MODE" \
    --wasm "$wasm" \
    "$canister" \
    --args "$args"
}

call_ok() {
  local output
  output="$(icp_cmd canister call \
    --network "$LOCAL_URL" \
    --root-key fetch \
    --identity "$DEPLOYER_IDENTITY" \
    "$@" 2>&1)"
  printf '%s\n' "$output"
  [[ "$output" != *"variant { Err"* ]]
}

main() {
  cd "$E2E_ROOT"
  mkdir -p "$ICP_CLI_HOME" "$ICP_COREPACK_HOME" "$ICP_XDG_CACHE_HOME" "$ICP_XDG_DATA_HOME"
  ensure_local_network

  if [[ ! -f "$IDENTITY_PEM" ]]; then
    icp_cmd identity new --storage plaintext "$DEPLOYER_IDENTITY" >/dev/null
  fi
  local admin
  admin="$(icp_cmd identity principal --identity "$DEPLOYER_IDENTITY" | head -n 1)"
  admin="${admin//[$'\r\n ']/}"
  if [[ -z "$admin" || "$admin" == "2vxsx-fae" ]]; then
    echo "codegen local E2E deployer identity resolved to anonymous" >&2
    exit 1
  fi
  icp_cmd build
  local router_wasm index_wasm graph_wasm
  router_wasm="$ROOT/target/wasm32-unknown-unknown/release/gleaph_router.wasm"
  index_wasm="$ROOT/target/wasm32-unknown-unknown/release/gleaph_graph_index.wasm"
  graph_wasm="$ROOT/target/wasm32-unknown-unknown/release/gleaph_graph.wasm"
  for wasm in "$router_wasm" "$index_wasm" "$graph_wasm"; do
    [[ -f "$wasm" ]] || { echo "missing wasm artifact: $wasm" >&2; exit 1; }
  done

  local router index graph
  router="$(create_canister "$admin")"
  index="$(create_canister "$admin")"
  graph="$(create_canister "$admin")"

  install_canister "$router" "$router_wasm" "(
    record {
      issuing_principal = principal \"$admin\";
      initial_admins = vec {};
    }
  )"
  install_canister "$index" "$index_wasm" "(
    record {
      router_canister = principal \"$router\";
    }
  )"

  call_ok "$router" register_graph "(
    record {
      graph_name = \"$GRAPH_NAME\";
      owner = principal \"$admin\";
      admins = vec {};
      is_home = true;
      shards = vec { record {
        shard_id = 0 : nat32;
        graph_canister = principal \"$graph\";
        index_canister = principal \"$index\";
      } };
      requested_resources = vec {};
    }
  )"
  install_canister "$graph" "$graph_wasm" "(
    record {
      logical_graph_name = opt \"$GRAPH_NAME\";
      router_canister = opt principal \"$router\";
      shard_id = opt (0 : nat32);
      index_canister = opt principal \"$index\";
    }
  )"

  local prepared_dir="$E2E_ROOT/prepared"
  mkdir -p "$prepared_dir"
  cp "$E2E_ROOT/manifest/empty-query.gql" "$prepared_dir/list-vertices.gql"
  cat > "$prepared_dir/list-vertices.toml" <<'TOML'
description = "List vertices in the default graph."
[[allowed_sorts]]
key = "name"
label = "Name"
TOML
  cargo run -p gleaph-cli -- \
    prepared apply \
    --dir "$prepared_dir" \
    --canister "$router" \
    -n local \
    --identity "$IDENTITY_PEM"

  pnpm --dir "$ROOT" sdk:build
  cargo run -p gleaph-codegen -- \
    --canister "$router" \
    --graph "$GRAPH_NAME" \
    --target javascript \
    --network local \
    --identity "$IDENTITY_PEM" \
    --output "$GENERATED"

  (
    cd "$SDK_ROOT"
    GLEAPH_CODEGEN_OUTPUT="$GENERATED" \
      GLEAPH_ROUTER_CANISTER="$router" \
      GLEAPH_CODEGEN_IDENTITY_PEM="$IDENTITY_PEM" \
      node "$E2E_ROOT/test.mjs"
  )
}

main "$@"
