#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORIGINAL_HOME="${HOME:-}"

GRAPH_NAME="${GLEAPH_DEMO_GRAPH_NAME:-gleaph.pocket_ic}"
SHARD_ID="${GLEAPH_DEMO_SHARD_ID:-0}"
INSTALL_MODE="${GLEAPH_DEMO_INSTALL_MODE:-auto}"
VECTOR_INDEX_ID="${GLEAPH_DEMO_VECTOR_INDEX_ID:-1}"
EMBEDDING_NAME="${GLEAPH_DEMO_EMBEDDING_NAME:-post_vec}"
EMBEDDING_DIMS="${GLEAPH_DEMO_EMBEDDING_DIMS:-8}"

ICP_CLI_HOME="${ICP_CLI_HOME:-$ROOT/.icp/home}"
ICP_COREPACK_HOME="${ICP_COREPACK_HOME:-$ROOT/.icp/corepack-home}"
ICP_XDG_CACHE_HOME="${ICP_XDG_CACHE_HOME:-$ROOT/.icp/xdg-cache}"
ICP_XDG_DATA_HOME="${ICP_XDG_DATA_HOME:-$ROOT/.icp/xdg-data}"
RUSTUP_HOME="${RUSTUP_HOME:-$ORIGINAL_HOME/.rustup}"
CARGO_HOME="${CARGO_HOME:-$ORIGINAL_HOME/.cargo}"

log() {
  printf '[social-demo] %s\n' "$*" >&2
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
  local call_args=("$@")
  if [[ -n "${ICP_DEPLOYER_IDENTITY:-}" ]]; then
    # Inject --identity immediately after the first positional <CANISTER> arg
    # (icp canister call requires --identity to follow any leading options but
    # precede the positional <CANISTER> argument).  We also need --environment to
    # be present; if not provided as a leading flag, append it from the default
    # local env so the call never errors out.
    if [[ " ${call_args[*]:-} " != *" -e "* && " ${call_args[*]:-} " != *" --environment "* ]]; then
      call_args=("-e" "local" "${call_args[@]}")
    fi
    local injected=()
    local inserted=0
    for arg in "${call_args[@]}"; do
      if [[ $inserted -eq 0 && "$arg" != -* && "$arg" != "-e" && "$arg" != "local" ]]; then
        injected+=("--identity" "$ICP_DEPLOYER_IDENTITY")
        inserted=1
      fi
      injected+=("$arg")
    done
    if [[ $inserted -eq 0 ]]; then
      injected+=("--identity" "$ICP_DEPLOYER_IDENTITY")
    fi
    call_args=("${injected[@]}")
  fi
  if ! output="$(icp_cmd canister call "${call_args[@]}" 2>&1)"; then
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

seed_social_graph() {
  log "Generating social seed manifest"
  node "$ROOT/frontend/apps/knowledge-map/scripts/generate-seeds.mjs" \
    "$ROOT/frontend/apps/knowledge-map/seeds/social-graph.json" \
    "$ROOT/frontend/apps/knowledge-map/seeds/social-seeds.json"

  log "Seeding social graph through Router GQL"
  env \
    HOME="$ICP_CLI_HOME" \
    COREPACK_HOME="$ICP_COREPACK_HOME" \
    XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME" \
    XDG_DATA_HOME="$ICP_XDG_DATA_HOME" \
    RUSTUP_HOME="$RUSTUP_HOME" \
    CARGO_HOME="$CARGO_HOME" \
    DO_NOT_TRACK="${DO_NOT_TRACK:-1}" \
    ICP_IDENTITY_NAME="$deployer_id" \
    node "$ROOT/frontend/apps/knowledge-map/scripts/apply-knowledge-map-seeds.mjs" \
      "$ROOT/frontend/apps/knowledge-map/seeds/social-seeds.json"
}

setup_vector_index() {
  local vector_id="$1"

  log "Registering vector index $EMBEDDING_NAME with target $vector_id"
  icp_call_expect_ok "Register post_vec vector index" "" -e local gleaph-router admin_register_vector_index \
    '(record { logical_graph_name = "'"$GRAPH_NAME"'"; embedding_name = "'"$EMBEDDING_NAME"'"; index_id = '"$VECTOR_INDEX_ID"' : nat32; dims = '"$EMBEDDING_DIMS"' : nat16; metric = opt variant { L2Squared }; target = opt principal "'"$vector_id"'"; if_not_exists = true })'

  log "Activating vector dispatch"
  icp_call_expect_ok "Activate vector dispatch" "" -e local gleaph-router admin_set_vector_dispatch_activation \
    '(true)'

  log "Attaching vector index shard"
  icp_call_expect_ok "Attach vector index shard" "" -e local gleaph-router admin_attach_vector_index_shard \
    '(record { logical_graph_name = "'"$GRAPH_NAME"'"; shard_id = '"$SHARD_ID"' : nat32; vector_index_canister = principal "'"$vector_id"'" })'
}

ingest_social_embeddings() {
  if [[ "${GLEAPH_DEMO_SKIP_EMBEDDINGS:-0}" == "1" ]]; then
    log "Skipping Post embeddings ingest (GLEAPH_DEMO_SKIP_EMBEDDINGS=1)"
    return
  fi
  log "Ingesting Post embeddings through Router"
  if env \
      HOME="$ICP_CLI_HOME" \
      COREPACK_HOME="$ICP_COREPACK_HOME" \
      XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME" \
      XDG_DATA_HOME="$ICP_XDG_DATA_HOME" \
      RUSTUP_HOME="$RUSTUP_HOME" \
      CARGO_HOME="$CARGO_HOME" \
      GLEAPH_DEMO_GRAPH_NAME="$GRAPH_NAME" \
      GLEAPH_DEMO_ROUTER_CANISTER=gleaph-router \
      GLEAPH_DEMO_EMBEDDING_NAME="$EMBEDDING_NAME" \
      ICP_IDENTITY_NAME="$deployer_id" \
      DO_NOT_TRACK="${DO_NOT_TRACK:-1}" \
      node "$ROOT/frontend/apps/social-demo/scripts/ingest-social-embeddings.mjs" \
        "$ROOT/frontend/apps/knowledge-map/seeds/social-seeds.json"
  then
    log "Embeddings ingest complete"
  else
    local rc=$?
    log "WARN: embeddings ingest failed (rc=$rc); continuing without embeddings."
    log "      Set GLEAPH_DEMO_SKIP_EMBEDDINGS=1 to silence this warning."
  fi
}

register_social_prepared_queries() {
  log "Registering social demo prepared queries"

  icp_call_expect_ok "Register public timeline prepared query" "Conflict" -e local gleaph-router prepared_register \
    '("public_timeline", "MATCH (p:Post) WHERE p.is_public = 1 RETURN p.demo_id AS post_id, p.created_at AS created_at ORDER BY created_at DESC")'

  icp_call_expect_ok "Register Alice home feed prepared query" "Conflict" -e local gleaph-router prepared_register \
    '("alice_home_feed", "MATCH (u:User)-[:FOLLOWS]->(author:User)-[:POSTED]->(p:Post) WHERE u.demo_id = '\''alice'\'' AND p.is_public = 1 RETURN p.demo_id AS post_id, p.created_at AS created_at ORDER BY created_at DESC")'

  icp_call_expect_ok "Register topic path prepared query" "Conflict" -e local gleaph-router prepared_register \
    '("topic_path_explanation", "MATCH (p:Post)-[has_topic:HAS_TOPIC]->(t:Topic) WHERE t.demo_id = '\''topic-graph'\'' MATCH (u:User)-[follows:FOLLOWS]->(author:User)-[posted:POSTED]->(p) WHERE u.demo_id = '\''alice'\'' RETURN p.demo_id AS post_id, follows.demo_edge_id AS follows_edge_id, posted.demo_edge_id AS posted_edge_id, t.demo_id AS topic_id, has_topic.demo_edge_id AS topic_edge_id, p.created_at AS created_at ORDER BY created_at DESC")'

  icp_call_expect_ok "Register semantic discovery prepared query" "Conflict" -e local gleaph-router prepared_register \
    '("semantic_discovery", "MATCH (p:Post) WHERE p.is_public = 1 SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, distance ORDER BY distance ASC")'

  icp_call_expect_ok "Register Alice semantic feed prepared query" "Conflict" -e local gleaph-router prepared_register \
    '("alice_semantic_feed", "MATCH (u:User)-[:FOLLOWS]->(author:User)-[:POSTED]->(p:Post) WHERE u.demo_id = '\''alice'\'' AND p.is_public = 1 SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, distance ORDER BY distance ASC")'
}

verify_social_demo_scenarios() {
  log "Verifying all five Gateway scenarios"
  for scenario in PublicTimeline AliceHomeFeed TopicPath SemanticDiscovery AliceSemanticFeed; do
    icp_call_expect_ok "Verify $scenario scenario" "" -e local gleaph-social-demo-gateway execute_social_demo_scenario \
      "(variant { $scenario })" --query
  done
}

main() {
  cd "$ROOT"

  mkdir -p "$ICP_CLI_HOME" "$ICP_COREPACK_HOME" "$ICP_XDG_CACHE_HOME" "$ICP_XDG_DATA_HOME"

  ensure_local_network

  local admin
  log "Resolving local deploy principal"
  local deployer_id="gleaph-demo-deployer"
  if ! icp_cmd identity list -q 2>/dev/null | grep -qx "$deployer_id"; then
    log "Creating local deployer identity '$deployer_id' in sandbox (plaintext PEM storage)"
    icp_cmd identity new --storage plaintext "$deployer_id" >/dev/null
  fi
  admin="$(icp_cmd identity principal --identity "$deployer_id" | head -n 1)"
  admin="${admin//[$'\r\n ']/}"
  if [[ -z "$admin" || "$admin" == "2vxsx-fae" ]]; then
    log "ERROR: deployer identity '$deployer_id' resolved to an empty/anonymous principal"
    exit 1
  fi
  if ! [[ "$admin" =~ ^[a-z0-9]{1,5}(-[a-z0-9]{1,5})+$ ]]; then
    log "ERROR: deployer principal does not look like a valid Principal textual form: '$admin'"
    exit 1
  fi
  log "Using deployer identity '$deployer_id' (principal: $admin)"

  # Subsequent admin / prepared / execute / register calls must be signed by the
  # same identity that was registered as the issuing principal, otherwise Router
  # rejects them as NotAuthorized.
  ICP_DEPLOYER_IDENTITY="$deployer_id"

  log "Building all canisters"
  icp_cmd build

  # The Gateway canister must be created before graph registration so its principal is known when
  # adding graph admins. It is installed after Router, Index, Graph, and Vector are registered/wired
  # because its init args need the Router principal.
  local router_id index_id graph_id gateway_id frontend_id vector_id
  router_id="$(ensure_canister gleaph-router)"
  index_id="$(ensure_canister gleaph-graph-index)"
  graph_id="$(ensure_canister gleaph-graph-shard-0)"
  gateway_id="$(ensure_canister gleaph-social-demo-gateway)"
  vector_id="$(ensure_canister gleaph-vector)"
  frontend_id="$(ensure_canister social-demo)"

  log "Installing gleaph-router"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-router --args "(
    record {
      issuing_principal = principal \"$admin\";
      initial_admins = vec {};
    }
  )"

  log "Installing gleaph-graph-index"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-graph-index --args "(
    record {
      router_canister = principal \"$router_id\";
    }
  )"

  log "Registering demo graph in Router with Gateway as graph admin"
  icp_call_expect_ok "Registering demo graph in Router" "Conflict = \"$GRAPH_NAME\"" -e local gleaph-router admin_register_graph "(
    record {
      graph_id = 0 : nat32;
      graph_name = \"$GRAPH_NAME\";
      canister_id = principal \"$graph_id\";
      owner = principal \"$admin\";
      admins = vec { principal \"$gateway_id\" };
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

  log "Installing gleaph-vector"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-vector --args "(
    record {
      router_canister = principal \"$router_id\";
    }
  )"

  log "Installing gleaph-social-demo-gateway"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-social-demo-gateway --args "(
    record {
      router_canister = principal \"$router_id\";
    }
  )"

  seed_social_graph
  setup_vector_index "$vector_id"
  ingest_social_embeddings
  register_social_prepared_queries

  if [[ "${GLEAPH_DEMO_VERIFY_QUERY:-0}" == "1" ]]; then
    verify_social_demo_scenarios
  else
    log "Skipping Gateway scenario verification; set GLEAPH_DEMO_VERIFY_QUERY=1 to enable it"
  fi

  log "Deploying social-demo asset canister"
  icp_cmd deploy -e local -y social-demo

  local gateway
  log "Resolving local gateway URL"
  gateway="$(local_gateway_url)"

  printf '\nSocial demo local deployment is ready.\n'
  printf '  Router:        %s\n' "$router_id"
  printf '  Graph index:   %s\n' "$index_id"
  printf '  Graph shard 0: %s\n' "$graph_id"
  printf '  Vector index:  %s\n' "$vector_id"
  printf '  Gateway:       %s\n' "$gateway_id"
  printf '  Frontend:      %s\n' "$frontend_id"
  if [[ -n "$gateway" ]]; then
    printf '  Gateway URL:   %s\n' "$gateway"
    printf '  Frontend URL:  %s://%s.%s%s\n' \
      "$(node -e 'console.log(new URL(process.argv[1]).protocol.slice(0, -1))' "$gateway")" \
      "$frontend_id" \
      "$(node -e 'console.log(new URL(process.argv[1]).hostname)' "$gateway")" \
      "$(node -e 'const u = new URL(process.argv[1]); console.log(u.port ? `:${u.port}` : "")' "$gateway")"
  fi
}

main "$@"
