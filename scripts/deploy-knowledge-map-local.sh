#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORIGINAL_HOME="${HOME:-}"

GRAPH_NAME="${GLEAPH_DEMO_GRAPH_NAME:-gleaph.pocket_ic}"
SHARD_ID="${GLEAPH_DEMO_SHARD_ID:-0}"
INSTALL_MODE="${GLEAPH_DEMO_INSTALL_MODE:-auto}"
SEED_MUTATION_KEY="${GLEAPH_DEMO_SEED_MUTATION_KEY:-knowledge-map-seed-person-knows-project-weight5-v1}"

ICP_CLI_HOME="${ICP_CLI_HOME:-$ROOT/.icp/home}"
ICP_COREPACK_HOME="${ICP_COREPACK_HOME:-$ROOT/.icp/corepack-home}"
ICP_XDG_CACHE_HOME="${ICP_XDG_CACHE_HOME:-$ROOT/.icp/xdg-cache}"
ICP_XDG_DATA_HOME="${ICP_XDG_DATA_HOME:-$ROOT/.icp/xdg-data}"
RUSTUP_HOME="${RUSTUP_HOME:-$ORIGINAL_HOME/.rustup}"
CARGO_HOME="${CARGO_HOME:-$ORIGINAL_HOME/.cargo}"

log() {
  printf '[knowledge-map] %s\n' "$*" >&2
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

ensure_local_network() {
  log "Checking local IC network"
  if icp_cmd network status local --json >/dev/null 2>&1; then
    log "Local IC network is already running"
    return
  fi
  if [[ "${GLEAPH_DEMO_SKIP_NETWORK_START:-0}" == "1" ]]; then
    log "Local IC network is not running and GLEAPH_DEMO_SKIP_NETWORK_START=1 was set"
    log "Start it first with: icp network start local -d"
    exit 1
  fi
  log "Starting local IC network"
  icp_cmd network start local -d
}

ensure_canister() {
  local name="$1"
  local id

  log "Resolving canister id for $name"
  if id="$(icp_cmd canister status -e local -i "$name" 2>/dev/null | head -n 1)" && [[ -n "$id" ]]; then
    log "Using existing $name canister $id"
    printf '%s\n' "$id"
    return
  fi

  log "Creating $name canister"
  icp_cmd canister create -e local --quiet "$name"
}

local_gateway_url() {
  icp_cmd network status local --json | node -e '
const fs = require("node:fs");
const raw = fs.readFileSync(0, "utf8");
const status = JSON.parse(raw);
console.log(status.gateway_url || status.api_url || "");
'
}

icp_call_expect_ok() {
  local description="$1"
  local allowed_error="$2"
  shift 2

  local output
  if ! output="$(icp_cmd canister call "$@" 2>&1)"; then
    printf '%s\n' "$output"
    log "$description failed"
    exit 1
  fi

  printf '%s\n' "$output"
  if [[ "$output" == *"variant {"*"Err"* ]]; then
    if [[ -n "$allowed_error" && "$output" == *"$allowed_error"* ]]; then
      log "$description returned expected existing-state response"
      return
    fi
    log "$description returned an error variant"
    exit 1
  fi
}

main() {
  cd "$ROOT"

  mkdir -p "$ICP_CLI_HOME" "$ICP_COREPACK_HOME" "$ICP_XDG_CACHE_HOME" "$ICP_XDG_DATA_HOME"

  ensure_local_network

  local admin
  log "Resolving local deploy principal"
  admin="$(icp_cmd identity principal)"

  log "Building all canisters"
  icp_cmd build

  local router_id index_id graph_id frontend_id
  router_id="$(ensure_canister gleaph-router)"
  index_id="$(ensure_canister gleaph-graph-index)"
  graph_id="$(ensure_canister gleaph-graph-shard-0)"
  frontend_id="$(ensure_canister knowledge-map)"

  log "Installing gleaph-router"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-router --args "(
    record {
      issuing_principal = principal \"$admin\";
      initial_admins = vec {};
      controllers = vec { principal \"$admin\" };
    }
  )"

  log "Installing gleaph-graph-index"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-graph-index --args "(
    record {
      controllers = vec { principal \"$admin\" };
      router_canister = principal \"$router_id\";
    }
  )"

  log "Registering demo graph in Router"
  icp_call_expect_ok "Registering demo graph in Router" "Conflict = \"$GRAPH_NAME\"" -e local gleaph-router admin_register_graph "(
    record {
      graph_id = 0 : nat32;
      graph_name = \"$GRAPH_NAME\";
      canister_id = principal \"$graph_id\";
      owner = principal \"$admin\";
      admins = vec {};
      status = variant { Active };
      version = 1 : nat64;
      updated_at_ns = 0 : nat64;
      provisioning_state = variant { None };
      is_home = false;
    }
  )"

  log "Registering graph shard in Router"
  icp_call_expect_ok "Registering graph shard in Router" "" -e local gleaph-router admin_register_shard "(
    record {
      shard_id = $SHARD_ID : nat32;
      graph_canister = principal \"$graph_id\";
      index_canister = principal \"$index_id\";
      logical_graph_name = \"$GRAPH_NAME\";
    }
  )"

  log "Installing gleaph-graph-shard-0"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-graph-shard-0 --args "(
    record {
      logical_graph_name = opt \"$GRAPH_NAME\";
      router_canister = opt principal \"$router_id\";
      shard_id = opt ($SHARD_ID : nat32);
      index_canister = opt principal \"$index_id\";
    }
  )"

  log "Seeding knowledge-map relationship through Router GQL"
  icp_call_expect_ok "Seeding knowledge-map relationship through Router GQL" "" -e local gleaph-router gql_execute_idempotent \
    "(\"INSERT (:Person)-[:KNOWS {weight: 5}]->(:Project)\", vec {}, \"$SEED_MUTATION_KEY\")"

  if [[ "${GLEAPH_DEMO_VERIFY_QUERY:-0}" == "1" ]]; then
    log "Verifying knowledge-map relationship query through Router GQL"
    icp_call_expect_ok "Verifying knowledge-map relationship query through Router GQL" "" -e local gleaph-router gql_query \
      '("MATCH (a)-[e:KNOWS {weight: 5}]->(b) RETURN ELEMENT_ID(a) AS source_id, ELEMENT_ID(e) AS edge_id, ELEMENT_ID(b) AS target_id, e.weight AS edge_weight", vec {})' \
      --query
  else
    log "Skipping Router GQL query verification; set GLEAPH_DEMO_VERIFY_QUERY=1 to enable it"
  fi

  log "Deploying knowledge-map asset canister"
  icp_cmd deploy -e local -y knowledge-map

  local gateway
  log "Resolving local gateway URL"
  gateway="$(local_gateway_url)"

  printf '\nKnowledge map local deployment is ready.\n'
  printf '  Router:        %s\n' "$router_id"
  printf '  Graph index:   %s\n' "$index_id"
  printf '  Graph shard 0: %s\n' "$graph_id"
  printf '  Frontend:      %s\n' "$frontend_id"
  if [[ -n "$gateway" ]]; then
    printf '  Gateway:       %s\n' "$gateway"
    printf '  Frontend URL:  %s://%s.%s%s\n' \
      "$(node -e 'console.log(new URL(process.argv[1]).protocol.slice(0, -1))' "$gateway")" \
      "$frontend_id" \
      "$(node -e 'console.log(new URL(process.argv[1]).hostname)' "$gateway")" \
      "$(node -e 'const u = new URL(process.argv[1]); console.log(u.port ? `:${u.port}` : "")' "$gateway")"
  fi
}

main "$@"
