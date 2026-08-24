#!/usr/bin/env bash
set -euo pipefail

# One-shot bring-up for the knowledge demo on its own managed local network
# (dev-mode topology: the documented operational fallback while Provision-driven
# provisioning has no CLI plumbing — GAP-2026-08-24-006 family).
#
#   1. fresh managed network (stop/start) for this demo's icp.yaml project
#   2. deployer identity (gleaph-demo-deployer) + local cycles funding
#   3. build + install the four platform canisters (Router/Index/Shard/Vector)
#   4. register_graph "knowledge" binding the installed shard+index (ADR 0056) so
#      `CREATE GRAPH` in migration 000001 short-circuits admission instead of
#      attempting Provision-driven provisioning
#   5. run `gleaph migration apply` (README quickstart step) — creates the graph type,
#      graph, property indexes, and the document_embedding vector index definition
#   6. attach the vector target (ensure definition → set target → enable dispatch →
#      attach shard), completing the ADR 0071 dev-mode fallback
#
# The final banner exports GLEAPH_CANISTER / GLEAPH_NETWORK / GLEAPH_FETCH_ROOT_KEY:
# that Router id is the single SSOT consumed by BOTH the CLI chain and
# `pnpm write-env` (which accepts it as --canister/GLEAPH_CANISTER).
#
# Long poles are the wasm32 release builds (~minutes each); progress renders inline.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DEMO="$ROOT/demo/knowledge"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

GRAPH_NAME="knowledge"
INDEX_NAME="document_embedding"
EMBEDDING_NAME="embedding"
EMBEDDING_DIMS=768
EMBEDDING_LABEL="Document"
SHARD_ID=0
INDEX_ID=0
DEPLOYER_ID="${KNOWLEDGE_DEPLOYER_ID:-gleaph-demo-deployer}"
DEPLOYER_CYCLES=1000000000000000 # 1_000t local fabricated cycles

ICP_CLI_HOME="${ICP_CLI_HOME:-$DEMO/.icp/home}"
ICP_COREPACK_HOME="${ICP_COREPACK_HOME:-$DEMO/.icp/corepack-home}"
ICP_XDG_CACHE_HOME="${ICP_XDG_CACHE_HOME:-$DEMO/.icp/xdg-cache}"
ICP_XDG_DATA_HOME="${ICP_XDG_DATA_HOME:-$DEMO/.icp/xdg-data}"

log() { printf '[knowledge-bootstrap] %s\n' "$*" >&2; }

icp_cmd() {
  env \
    HOME="$ICP_CLI_HOME" \
    COREPACK_HOME="$ICP_COREPACK_HOME" \
    XDG_CACHE_HOME="$ICP_XDG_CACHE_HOME" \
    XDG_DATA_HOME="$ICP_XDG_DATA_HOME" \
    DO_NOT_TRACK="${DO_NOT_TRACK:-1}" \
    icp "$@"
}

api_url() {
  icp_cmd network status local --json | node -e '
const fs = require("node:fs");
const status = JSON.parse(fs.readFileSync(0, "utf8"));
process.stdout.write((status.api_url || status.gateway_url || "").replace(/\/+$/, ""));
'
}

ensure_fresh_network() {
  log "Recreating the demo managed network (fresh-network requirement)"
  # stop tolerates an already-stopped network; start recreates the container so the
  # chain below always runs against a clean ledger.
  icp_cmd network stop local >/dev/null 2>&1 || true
  icp_cmd network start local -d
  local url
  url="$(api_url)"
  if [[ -z "$url" ]] || ! curl -fsS --max-time 3 -o /dev/null "$url/api/v2/status"; then
    log "ERROR: demo network not reachable at ${url:-<none>} after start"
    exit 1
  fi
  log "Demo network is running ($url)"
}

ensure_deployer() {
  if ! icp_cmd identity list -q 2>/dev/null | grep -qx "$DEPLOYER_ID"; then
    log "Creating deployer identity '$DEPLOYER_ID' (plaintext PEM)"
    icp_cmd identity new --storage plaintext "$DEPLOYER_ID" >/dev/null
  fi
  PRINCIPAL="$(icp_cmd identity principal --identity "$DEPLOYER_ID" | head -n 1)"
  PRINCIPAL="${PRINCIPAL//[$'\r\n ']/}"
  if [[ -z "$PRINCIPAL" || "$PRINCIPAL" == "2vxsx-fae" ]]; then
    log "ERROR: deployer principal resolved empty/anonymous"
    exit 1
  fi
  log "Deployer $DEPLOYER_ID = $PRINCIPAL"
}

fund_deployer() {
  local balance
  balance="$(icp_cmd cycles balance -e local --identity "$DEPLOYER_ID" --quiet | awk '{print $1}' | tr -d '_')"
  if [[ "$balance" =~ ^[0-9]+$ ]] && (( balance >= DEPLOYER_CYCLES )); then
    log "Deployer cycles sufficient ($balance)"
    return
  fi
  log "Funding deployer with 1_000t local cycles"
  icp_cmd cycles transfer "$DEPLOYER_CYCLES" "$PRINCIPAL" -e local --identity anonymous >/dev/null
}

ensure_canister() {
  local name="$1" id
  if id="$(icp_cmd canister status -e local -i "$name" 2>/dev/null | head -n 1)" && [[ -n "$id" ]]; then
    log "Using existing $name ($id)"
  else
    log "Creating $name"
    icp_cmd canister create -e local --identity "$DEPLOYER_ID" --quiet \
      --reserved-cycles-limit 100t "$name" >/dev/null
    id="$(icp_cmd canister status -e local -i "$name" | head -n 1)"
  fi
  icp_cmd canister top-up -e local --identity "$DEPLOYER_ID" --amount 100t "$name" >/dev/null \
    || log "WARN: top-up failed for $name"
  printf '%s\n' "$id"
}

call_ok() {
  local description="$1"; shift
  local out
  # icp-cli fetches the canister's embedded Candid interface by default, and the
  # router's extracted interface trips icp-cli's parser ("Unrecognized token Query").
  # When bootstrap has extracted it locally, pass --candid so calls type-check against
  # a file we know decodes truthfully.
  local candid_args=()
  if [[ -n "${ROUTER_DID:-}" && -f "$ROUTER_DID" ]]; then
    candid_args=(--candid "$ROUTER_DID")
  fi
  out="$(icp_cmd canister call -e local --identity "$DEPLOYER_ID" \
    "${candid_args[@]}" "$@")" || {
    log "ERROR: $description call failed"; exit 1;
  }
  if [[ "$out" == *"variant {"*"Err"* ]]; then
    log "ERROR: $description returned an error variant:"; printf '%s\n' "$out" >&2; exit 1
  fi
  log "$description: ok"
}

main() {
  cd "$DEMO"

  ensure_fresh_network
  ensure_deployer
  fund_deployer

  # wasm32 release builds, one package per invocation (build-budget observability).
  # KNOWLEDGE_SKIP_VECTOR=1 defers the vector canister (Track-2 style partial bring-up).
  local skip_vector="${KNOWLEDGE_SKIP_VECTOR:-0}"
  log "Building gleaph-router (wasm32 release; long)"
  icp_cmd build --debug gleaph-router
  # Extract the router's public interface for typed admin calls (call_ok above).
  ROUTER_DID="$DEMO/.icp/cache/router-full.did"
  mkdir -p "$(dirname "$ROUTER_DID")"
  if command -v ic-wasm >/dev/null 2>&1; then
    ic-wasm "$DEMO/.icp/cache/artifacts/gleaph-router" metadata candid:service \
      > "$ROUTER_DID" 2>/dev/null || log "WARN: candid extraction failed; falling back to network typing"
  fi
  log "Building gleaph-graph-index (wasm32 release)"
  icp_cmd build --debug gleaph-graph-index
  log "Building gleaph-graph-shard-0 (wasm32 release)"
  icp_cmd build --debug gleaph-graph-shard-0
  if [[ "$skip_vector" != "1" ]]; then
    log "Building gleaph-vector (wasm32 release)"
    icp_cmd build --debug gleaph-vector
  fi

  local router_id index_id shard_id vector_id
  router_id="$(ensure_canister gleaph-router)"
  index_id="$(ensure_canister gleaph-graph-index)"
  shard_id="$(ensure_canister gleaph-graph-shard-0)"
  if [[ "$skip_vector" != "1" ]]; then
    vector_id="$(ensure_canister gleaph-vector)"
  fi

  log "Installing gleaph-router (issuing principal: $PRINCIPAL)"
  icp_cmd canister install -e local -y --identity "$DEPLOYER_ID" --mode reinstall gleaph-router --args "(
    record {
      issuing_principal = principal \"$PRINCIPAL\";
      initial_admins = vec {};
    }
  )" >/dev/null

  log "Installing gleaph-graph-index"
  icp_cmd canister install -e local -y --identity "$DEPLOYER_ID" --mode reinstall gleaph-graph-index --args "(
    record { router_canister = principal \"$router_id\"; }
  )" >/dev/null

  log "Installing gleaph-graph-shard-0 (logical graph: $GRAPH_NAME)"
  icp_cmd canister install -e local -y --identity "$DEPLOYER_ID" --mode reinstall gleaph-graph-shard-0 --args "(
    record {
      logical_graph_name = opt \"$GRAPH_NAME\";
      router_canister = opt principal \"$router_id\";
      shard_id = opt ($SHARD_ID : nat32);
      index_canister = opt principal \"$index_id\";
    }
  )" >/dev/null

  if [[ "$skip_vector" != "1" ]]; then
    log "Installing gleaph-vector"
    icp_cmd canister install -e local -y --identity "$DEPLOYER_ID" --mode reinstall gleaph-vector --args "(
      record {
        router_canister = principal \"$router_id\";
        definition_map_seed = 7640891576956012809 : nat64;
        subject_map_seed = 13503953896175478587 : nat64;
      }
    )" >/dev/null
  fi

  log "Registering the $GRAPH_NAME graph (ADR 0056 register_graph, is_home=true)"
  call_ok "register_graph" gleaph-router register_graph "(
    record {
      graph_name = \"$GRAPH_NAME\";
      owner = principal \"$PRINCIPAL\";
      admins = vec {};
      is_home = true;
      shards = vec {
        record {
          shard_id = $SHARD_ID : nat32;
          graph_canister = principal \"$shard_id\";
          index_canister = principal \"$index_id\";
        }
      };
      requested_resources = vec {};
    }
  )"

  local url
  url="$(api_url)"

  # Migrations create the document_embedding definition targetless in dev mode
  # (ADR 0071); re-registering here would Conflict on the embedding-field uniqueness,
  # so only the target/dispatch/attach steps follow.
  local deployer_pem="$ICP_CLI_HOME/Library/Application Support/org.dfinity.icp-cli/identity/keys/$DEPLOYER_ID.pem"
  log "Applying migrations (graph type, graph, property indexes, vector index definition)"
  local gleaph_bin="$ROOT/target/debug/gleaph"
  if [[ ! -x "$gleaph_bin" ]]; then
    gleaph_bin="cargo-run"
  fi
  run_gleaph() {
    if [[ "$gleaph_bin" == "cargo-run" ]]; then
      (cd "$ROOT" && GLEAPH_NETWORK="$url" GLEAPH_FETCH_ROOT_KEY=true GLEAPH_CANISTER="$router_id" \
        cargo run -q -p gleaph-cli -- "$@")
    else
      GLEAPH_NETWORK="$url" GLEAPH_FETCH_ROOT_KEY=true GLEAPH_CANISTER="$router_id" \
        "$gleaph_bin" "$@"
    fi
  }
  run_gleaph migration apply --identity "$deployer_pem"

  # PUBLIC data-plane surface for the demo dataset: publication grants only the
  # prepared-query EXECUTE privilege (ADR 0074); executing visitors additionally need the
  # match/traverse/read rows these statements cover. Bounded to this graph.
  log "Granting the PUBLIC data-plane surface (match/read/traverse)"
  local grant_stmts=(
    "GRANT MATCH ON GRAPH $GRAPH_NAME NODES Concept TO PUBLIC"
    "GRANT MATCH ON GRAPH $GRAPH_NAME NODES Document TO PUBLIC"
    "GRANT MATCH ON GRAPH $GRAPH_NAME NODES Person TO PUBLIC"
    "GRANT MATCH ON GRAPH $GRAPH_NAME NODES Team TO PUBLIC"
    "GRANT READ ON GRAPH $GRAPH_NAME NODES Concept TO PUBLIC"
    "GRANT READ ON GRAPH $GRAPH_NAME NODES Document TO PUBLIC"
    "GRANT READ ON GRAPH $GRAPH_NAME NODES Person TO PUBLIC"
    "GRANT READ ON GRAPH $GRAPH_NAME NODES Team TO PUBLIC"
    "GRANT TRAVERSE ON GRAPH $GRAPH_NAME EDGES RELATED_TO TO PUBLIC"
    "GRANT TRAVERSE ON GRAPH $GRAPH_NAME EDGES CITES TO PUBLIC"
    "GRANT TRAVERSE ON GRAPH $GRAPH_NAME EDGES ABOUT TO PUBLIC"
    "GRANT TRAVERSE ON GRAPH $GRAPH_NAME EDGES AUTHORED_BY TO PUBLIC"
    "GRANT TRAVERSE ON GRAPH $GRAPH_NAME EDGES OWNS TO PUBLIC"
    "GRANT TRAVERSE ON GRAPH $GRAPH_NAME EDGES BELONGS_TO TO PUBLIC"
    "GRANT TRAVERSE ON GRAPH $GRAPH_NAME EDGES ROUTED_VIA TO PUBLIC"
  )
  local stmt
  for stmt in "${grant_stmts[@]}"; do
    # icp-cli parses the Router's embedded Candid interface (which trips its parser), so
    # encode arguments against a local minimal interface and pass the statement as the
    # standard three-argument tuple instead of a record.
    call_ok "$stmt" gleaph-router gql_mutate "(\"$stmt\", vec {}, \"public-grant-$stmt\")"
  done
  if [[ "$skip_vector" == "1" ]]; then
    cat <<BANNER

Knowledge platform bring-up complete (VECTOR DEFERRED via KNOWLEDGE_SKIP_VECTOR=1).
  Router:      $router_id
  Graph index: $index_id
  Shard 0:     $shard_id
  Gateway URL: $url

Scenario 3 additionally needs the vector attachment steps; rerun without
KNOWLEDGE_SKIP_VECTOR once the vector build is available.

export GLEAPH_NETWORK=$url
export GLEAPH_FETCH_ROOT_KEY=true
export GLEAPH_CANISTER=$router_id
BANNER
    return 0
  fi

  log "Setting the vector dispatch target"
  call_ok "set_vector_index_target" gleaph-router set_vector_index_target \
    '(record { logical_graph_name = "'"$GRAPH_NAME"'"; index_id = '"$INDEX_ID"' : nat32; target = principal "'"$vector_id"'" })'

  log "Enabling vector dispatch"
  call_ok "set_vector_dispatch_enabled" gleaph-router set_vector_dispatch_enabled '(true)'

  log "Attaching the vector shard"
  call_ok "attach_vector_shard" gleaph-router attach_vector_shard \
    '(record { logical_graph_name = "'"$GRAPH_NAME"'"; shard_id = '"$SHARD_ID"' : nat32; vector_canister = principal "'"$vector_id"'" })'

  cat <<BANNER

Knowledge platform bring-up complete.
  Router:      $router_id
  Graph index: $index_id
  Shard 0:     $shard_id
  Vector:      $vector_id
  Gateway URL: $url

Continue with the README quickstart CLI chain (load / embed ingest /
prepared apply / publish / run / codegen / write-env). Source the
exports below first — GLEAPH_CANISTER is the single Router-id SSOT for
both the CLI chain and \`pnpm write-env\`.

export GLEAPH_NETWORK=$url
export GLEAPH_FETCH_ROOT_KEY=true
export GLEAPH_CANISTER=$router_id
BANNER
}

main "$@"
