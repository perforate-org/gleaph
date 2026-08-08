#!/usr/bin/env bash
set -euo pipefail

# Full local bootstrap for a Gleaph demo: deploys the Gleaph platform canisters
# (Router/Index/Graph/Vector) from the repository-root icp.yaml, registers the
# demo graph and vector wiring, then runs the demo's own application flow
# (<demo>/scripts/deploy-local.sh) on top of it.
#
# Reusable across demos: set GLEAPH_DEMO_DIR (default demo/social) to the demo
# whose scripts/deploy-local.sh should run and GLEAPH_DEMO_GRAPH_NAME (default
# social) to the logical graph it seeds. Each demo directory owns its application
# flow (migrations, seeds, prepared queries, typed client, asset canister); this
# script owns only the platform half. The demo flow calls this script itself when
# the platform is missing (GLEAPH_DEMO_SKIP_AUTO_BOOTSTRAP=1 opts out).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORIGINAL_HOME="${HOME:-}"

DEMO_DIR="${GLEAPH_DEMO_DIR:-demo/social}"
GRAPH_NAME="${GLEAPH_DEMO_GRAPH_NAME:-social}"
SHARD_ID="${GLEAPH_DEMO_SHARD_ID:-0}"
INSTALL_MODE="${GLEAPH_DEMO_INSTALL_MODE:-auto}"
VECTOR_INDEX_ID="${GLEAPH_DEMO_VECTOR_INDEX_ID:-1}"
EMBEDDING_NAME="${GLEAPH_DEMO_EMBEDDING_NAME:-post_vec}"
EMBEDDING_DIMS="${GLEAPH_DEMO_EMBEDDING_DIMS:-8}"

# icp-cli's --debug flag couples two behaviors: it hides the managed recipe's
# progress renderer (which can terminate early in non-interactive shells) and
# reports the full build output on failure, but it also enables DEBUG tracing
# that dumps recipe template bodies. Keep the build-attach behavior by default
# and filter the DEBUG/TRACE noise out of stderr; GLEAPH_DEMO_ICP_DEBUG=1
# restores the raw debug output for troubleshooting.
ICP_DEBUG="${GLEAPH_DEMO_ICP_DEBUG:-0}"

# Six canisters are created and each is topped up with 100T below. Keep the
# local deployer funded for the whole bootstrap rather than relying on the
# currently selected icp identity's balance.
LOCAL_DEPLOYER_CYCLES=1000000000000000
LOCAL_DEPLOYER_CYCLES_LABEL=1_000t

ICP_CLI_HOME="${ICP_CLI_HOME:-$ROOT/.icp/home}"
ICP_COREPACK_HOME="${ICP_COREPACK_HOME:-$ROOT/.icp/corepack-home}"
ICP_XDG_CACHE_HOME="${ICP_XDG_CACHE_HOME:-$ROOT/.icp/xdg-cache}"
ICP_XDG_DATA_HOME="${ICP_XDG_DATA_HOME:-$ROOT/.icp/xdg-data}"
RUSTUP_HOME="${RUSTUP_HOME:-$ORIGINAL_HOME/.rustup}"
CARGO_HOME="${CARGO_HOME:-$ORIGINAL_HOME/.cargo}"

# The deployer identity must match the graph owner/admin registered for the demo graph.
ICP_IDENTITY_NAME="${ICP_IDENTITY_NAME:-gleaph-demo-deployer}"
DEPLOYER_PEM="${DEPLOYER_PEM:-$ICP_CLI_HOME/Library/Application Support/org.dfinity.icp-cli/identity/keys/$ICP_IDENTITY_NAME.pem}"

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

# Filters icp-cli debug-layer stderr: drops TRACE/DEBUG lines and any
# continuation lines they span (a DEBUG event can carry a multi-line message,
# e.g. the recipe template dump), keeping INFO/WARN/ERROR and unformatted
# output such as panics. ANSI level colors are stripped before matching so the
# filter also works when stderr is a terminal.
icp_debug_filter() {
  awk '
    BEGIN { esc = sprintf("%c", 27) }
    {
      line = $0
      clean = line
      gsub(esc "\\[[0-9;]*m", "", clean)
      if (clean ~ /^(TRACE|DEBUG) /) { skipping = 1; next }
      if (clean ~ /^(INFO|WARN|ERROR) /) { skipping = 0; print line; next }
      if (skipping) { next }
      print line
    }
  ' >&2
}

build_instrumented_router_wasm() {
  local wasm="${GLEAPH_DEMO_ROUTER_WASM:-$ROOT/.icp/cache/artifacts/gleaph-router-batch-instr.wasm}"
  local did="${wasm%.wasm}.did"
  log "Building Router with batch-instr-log explicitly (icp recipe feature forwarding is not relied upon)"
  env \
    HOME="$ICP_CLI_HOME" \
    RUSTUP_HOME="$RUSTUP_HOME" \
    CARGO_HOME="$CARGO_HOME" \
    cargo build -p gleaph-router --features batch-instr-log \
      --target wasm64-unknown-unknown -Z build-std=core,alloc,std,panic_abort --release >&2
  local raw_wasm="$ROOT/target/wasm64-unknown-unknown/release/gleaph_router.wasm"
  # The composite `admin_graph_batch_instr_log` query is cfg-gated on the feature, so
  # its name string is only embedded in the wasm when batch-instr-log was compiled in.
  if ! grep -a -F -q 'admin_graph_batch_instr_log' "$raw_wasm"; then
    log "ERROR: Router release was built without batch-instr-log"
    exit 1
  fi
  candid-extractor "$raw_wasm" > "$did"
  ic-wasm "$raw_wasm" -o "$wasm" metadata candid:service -f "$did" -v public
  log "Instrumented Router artifact ready: $wasm"
  printf '%s\n' "$wasm"
}

ensure_local_network() {
  log "Checking local IC network"
  # `icp network status` trusts the cached descriptor, which can point at a stopped
  # container (per-project managed networks are cached independently and restarting
  # from another project leaves stale ports behind). Verify liveness over HTTP.
  local api_url
  api_url="$(icp_cmd network status local --json 2>/dev/null | node -e '
const fs = require("node:fs");
const raw = fs.readFileSync(0, "utf8");
const status = JSON.parse(raw);
const u = status.api_url || status.gateway_url || "";
process.stdout.write(u.replace(/\/+$/, ""));
' || true)"
  if [[ -n "$api_url" ]] && curl -fsS --max-time 2 -o /dev/null "$api_url/api/v2/status" 2>/dev/null; then
    log "Local IC network is already running ($api_url)"
    return
  fi
  if [[ "${GLEAPH_DEMO_SKIP_NETWORK_START:-0}" == "1" ]]; then
    log "Local IC network is not reachable at ${api_url:-<none>} and GLEAPH_DEMO_SKIP_NETWORK_START=1 was set"
    log "Start it first with: icp network start local -d"
    exit 1
  fi
  log "Starting local IC network"
  # `network start` alone refuses with "already running" when the cached descriptor is
  # stale (container stopped/removed); stop first, tolerating an already-gone network.
  icp_cmd network stop local >/dev/null 2>&1 || true
  icp_cmd network start local -d
}

ensure_canister() {
  local name="$1"
  local id
  local identity="${ICP_DEPLOYER_IDENTITY:-}"

  if [[ -z "$identity" ]]; then
    log "ERROR: ICP_DEPLOYER_IDENTITY is required before creating canisters"
    exit 1
  fi

  log "Resolving canister id for $name"
  if id="$(icp_cmd canister status -e local -i "$name" 2>/dev/null | head -n 1)" && [[ -n "$id" ]]; then
    log "Using existing $name canister $id"
  else
    log "Creating $name canister"
    icp_cmd canister create -e local --identity "$identity" --quiet \
      --reserved-cycles-limit 100t "$name" >/dev/null
    id="$(icp_cmd canister status -e local -i "$name" 2>/dev/null | head -n 1)"
    log "Created $name canister $id"
  fi

  # Local replica canisters can drain their cycle pool across repeated deploys.
  # Ensure each canister can afford inter-canister update-call reservations (~42B per call).
  icp_cmd canister top-up -e local --identity "$identity" --amount 100t "$name" >/dev/null \
    || log "WARN: could not top-up $name"

  printf '%s\n' "$id"
}

ensure_deployer_cycles() {
  local identity="$1"
  local principal="$2"
  local balance_text balance

  balance_text="$(icp_cmd cycles balance -e local --identity "$identity" --quiet)"
  balance="${balance_text%% *}"
  balance="${balance//_/}"
  if [[ "$balance" =~ ^[0-9]+$ ]] && (( balance >= LOCAL_DEPLOYER_CYCLES )); then
    log "Deployer identity '$identity' has sufficient cycles ($balance_text)"
    return
  fi

  log "Funding deployer identity '$identity' with $LOCAL_DEPLOYER_CYCLES_LABEL local cycles"
  if ! icp_cmd cycles transfer "$LOCAL_DEPLOYER_CYCLES_LABEL" "$principal" \
      -e local --identity anonymous >/dev/null; then
    log "ERROR: could not transfer local fabricated cycles from anonymous to '$identity'"
    log "       Check that the local network is running and that the anonymous identity is funded"
    exit 1
  fi
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

setup_vector_index() {
  local vector_id="$1"

  log "Registering vector index $EMBEDDING_NAME with target $vector_id"
  icp_call_expect_ok "Register post_vec vector index" "" -e local gleaph-router admin_register_vector_index \
    '(record { logical_graph_name = "'"$GRAPH_NAME"'"; embedding_name = "'"$EMBEDDING_NAME"'"; index_id = '"$VECTOR_INDEX_ID"' : nat32; dims = '"$EMBEDDING_DIMS"' : nat16; metric = opt variant { L2Squared }; target = opt principal "'"$vector_id"'"; if_not_exists = true })'

  log "Activating vector dispatch"
  icp_call_expect_ok "Activate vector dispatch" "" -e local gleaph-router set_vector_dispatch_enabled \
    '(true)'

  log "Attaching vector index shard"
  icp_call_expect_ok "Attach vector index shard" "" -e local gleaph-router attach_vector_shard \
    '(record { logical_graph_name = "'"$GRAPH_NAME"'"; shard_id = '"$SHARD_ID"' : nat32; vector_canister = principal "'"$vector_id"'" })'
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
  ensure_deployer_cycles "$deployer_id" "$admin"

  log "Building all canisters"
  # The managed recipe's progress renderer can terminate early in non-interactive shells;
  # --debug keeps the build operation attached and reports the actual completion result.
  # Its DEBUG-level tracing (recipe template dumps) is filtered out of stderr by default;
  # set GLEAPH_DEMO_ICP_DEBUG=1 to see it.
  if [[ "$ICP_DEBUG" == "1" ]]; then
    icp_cmd build --debug \
      gleaph-graph-index gleaph-graph-shard-0 gleaph-vector
  else
    icp_cmd build --debug \
      gleaph-graph-index gleaph-graph-shard-0 gleaph-vector 2>&1 | icp_debug_filter
  fi
  local router_wasm
  router_wasm="$(build_instrumented_router_wasm)"

  local router_id index_id graph_id vector_id
  router_id="$(ensure_canister gleaph-router)"
  index_id="$(ensure_canister gleaph-graph-index)"
  graph_id="$(ensure_canister gleaph-graph-shard-0)"
  vector_id="$(ensure_canister gleaph-vector)"
  # The demo's asset canister belongs to its own icp.yaml project; the demo flow
  # creates/deploys it later, so its principal is not resolved from the root project.

  log "Installing gleaph-router"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" --wasm "$router_wasm" gleaph-router --args "(
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

  # Install the graph shard before registering it in the Router. Shard
  # registration attaches the shard to the index canister, which requires the
  # graph canister to be running and able to answer inter-canister calls.

  log "Installing gleaph-graph-shard-0"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-graph-shard-0 --args "(
    record {
      logical_graph_name = opt \"$GRAPH_NAME\";
      router_canister = opt principal \"$router_id\";
      shard_id = opt ($SHARD_ID : nat32);
      index_canister = opt principal \"$index_id\";
    }
  )"

  log "Registering demo graph in Router"
  # ADR 0056 dev-mode API: `register_graph` registers the graph and its shards in one
  # intent (admin_register_graph/admin_register_shard were consolidated into it).
  icp_call_expect_ok "Registering demo graph in Router" "Conflict" -e local gleaph-router register_graph "(
    record {
      graph_name = \"$GRAPH_NAME\";
      owner = principal \"$admin\";
      admins = vec {};
      is_home = false;
      shards = vec {
        record {
          shard_id = $SHARD_ID : nat32;
          graph_canister = principal \"$graph_id\";
          index_canister = principal \"$index_id\";
        }
      };
      requested_resources = vec {};
    }
  )"
  log "Installing gleaph-vector"
  icp_cmd canister install -e local -y --mode "$INSTALL_MODE" gleaph-vector --args "(
    record {
      router_canister = principal \"$router_id\";
    }
  )"

  setup_vector_index "$vector_id"

  log "Running demo application flow ($DEMO_DIR)"
  # GLEAPH_DEMO_FROM_BOOTSTRAP stops the demo flow from re-delegating to this script
  # if the platform somehow still looks unreachable after the bootstrap.
  GLEAPH_CANISTER="$router_id" GLEAPH_DEMO_FROM_BOOTSTRAP=1 bash "$ROOT/$DEMO_DIR/scripts/deploy-local.sh"

  local gateway
  log "Resolving local gateway URL"
  gateway="$(local_gateway_url)"

  printf '\nGleaph platform local deployment is ready.\n'
  printf '  Router:        %s\n' "$router_id"
  printf '  Graph index:   %s\n' "$index_id"
  printf '  Graph shard 0: %s\n' "$graph_id"
  printf '  Vector index:  %s\n' "$vector_id"
  if [[ -n "$gateway" ]]; then
    printf '  Gateway URL:   %s\n' "$gateway"
  fi
  printf '\nThe demo application (%s) printed its own ready banner above.\n' "$DEMO_DIR"
}

main "$@"
