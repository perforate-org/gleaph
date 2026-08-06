#!/usr/bin/env bash
set -euo pipefail

# Deploys the social-demo application on top of an already-deployed Gleaph platform.
#
# This is the application flow a Gleaph user runs; it deliberately excludes the
# platform deployment (Router/Index/Graph/Vector canisters and the `social` graph
# registration), which is a manual prerequisite. See demo/social/README.md.
# `scripts/deploy-demo-local.sh` runs the platform bootstrap and then this script
# (GLEAPH_DEMO_DIR=demo/social by default). When the platform is not reachable
# (network down or Router missing), this script automatically delegates to the full
# bootstrap first; set GLEAPH_DEMO_SKIP_AUTO_BOOTSTRAP=1 to fail with guidance
# instead. A non-default GLEAPH_DEMO_INSTALL_MODE (e.g. reinstall) always delegates
# too, so the platform canisters are (re)installed even when the platform looks ready.
#
# Steps:
#   0. pnpm build:config       - regenerate seeds/ and src/data/*.generated.*
#   1. gleaph migration apply   - schema: graph type, typed graph, property indexes
#   2. gleaph load              - seed vertices and edges
#   3. gleaph prepared apply    - register the six scenario prepared queries
#   4. gleaph codegen           - regenerate src/generated.ts from the Router manifest
#   5. writes .env.local        - so the frontend build bakes in the Router id
#   6. vite build               - frontend bundle (SDK dist prebuilt if missing)
#   7. icp deploy               - asset canister (demo/social/icp.yaml)
#
# Env knobs:
#   GLEAPH_DEMO_SKIP_VITE_ENV=1      - do not touch .env.local (CI)
#   GLEAPH_DEMO_FORCE_VITE_IC_HOST=1 - also overwrite VITE_IC_HOST in .env.local
#   GLEAPH_DEMO_VERIFY_QUERY=1       - run `gleaph prepared status` after registering
#   GLEAPH_DEMO_INSTALL_MODE=<mode>  - delegate to the full bootstrap with this install
#                                      mode (auto/reinstall/upgrade) for the platform
#   GLEAPH_DEMO_SKIP_AUTO_BOOTSTRAP=1 - fail instead of running the full bootstrap
#   GLEAPH_DEMO_FROM_BOOTSTRAP=1     - internal: set by the full bootstrap to avoid
#                                      re-delegating when the platform is still down

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DEMO="$ROOT/demo/social"
GRAPH_NAME="${GLEAPH_DEMO_GRAPH_NAME:-social}"
ORIGINAL_HOME="${HOME:-}"

ICP_CLI_HOME="${ICP_CLI_HOME:-$ROOT/.icp/home}"
ICP_COREPACK_HOME="${ICP_COREPACK_HOME:-$ROOT/.icp/corepack-home}"
ICP_XDG_CACHE_HOME="${ICP_XDG_CACHE_HOME:-$ROOT/.icp/xdg-cache}"
ICP_XDG_DATA_HOME="${ICP_XDG_DATA_HOME:-$ROOT/.icp/xdg-data}"
RUSTUP_HOME="${RUSTUP_HOME:-$ORIGINAL_HOME/.rustup}"
CARGO_HOME="${CARGO_HOME:-$ORIGINAL_HOME/.cargo}"

# The deployer identity must match the graph owner/admin registered for the demo
# graph (the same identity the platform deploy used).
ICP_IDENTITY_NAME="${ICP_IDENTITY_NAME:-gleaph-demo-deployer}"
DEPLOYER_PEM="${DEPLOYER_PEM:-$ICP_CLI_HOME/Library/Application Support/org.dfinity.icp-cli/identity/keys/$ICP_IDENTITY_NAME.pem}"

# The demo's asset canister belongs to this project's own managed local network, which seeds its
# ledger independently of the repository-root platform network. Keep the local deployer funded for
# the canister create (mirrors the bootstrap's constants).
LOCAL_DEPLOYER_CYCLES=1000000000000000
LOCAL_DEPLOYER_CYCLES_LABEL=1_000t

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

resolve_router_id() {
  if [[ -n "${GLEAPH_CANISTER:-}" ]]; then
    printf '%s\n' "$GLEAPH_CANISTER"
    return
  fi
  # The Router lives in the repository-root icp.yaml project.
  (cd "$ROOT" && icp_cmd canister status -e local -i gleaph-router | head -n 1)
}

gleaph_cmd() {
  # The icp-cli managed network listens on a dynamic port, so every CLI call must
  # target the real replica URL (see local_api_url) instead of the `local` name.
  local env_args=()
  if [[ -n "${LOCAL_API_URL:-}" ]]; then
    env_args=(GLEAPH_NETWORK="$LOCAL_API_URL" GLEAPH_FETCH_ROOT_KEY=true)
  fi
  if command -v gleaph >/dev/null 2>&1; then
    env "${env_args[@]}" gleaph "$@"
  else
    env "${env_args[@]}" cargo run -q -p gleaph-cli --manifest-path "$ROOT/Cargo.toml" -- "$@"
  fi
}

# Funds the demo project's local deployer identity before the asset-canister create. The demo
# icp.yaml project owns a separate managed local network whose ledger is seeded independently of
# the repository-root platform network, so the bootstrap's funding there never reaches this ledger.
ensure_deployer_cycles() {
  local principal
  principal="$(icp_cmd identity principal --identity "$ICP_IDENTITY_NAME" | head -n 1)"
  principal="${principal//[$'\r\n ']/}"
  if [[ -z "$principal" || "$principal" == "2vxsx-fae" ]]; then
    log "ERROR: deployer identity '$ICP_IDENTITY_NAME' resolved to an empty/anonymous principal"
    exit 1
  fi

  local balance_text balance
  balance_text="$(icp_cmd cycles balance -e local --identity "$ICP_IDENTITY_NAME" --quiet)"
  balance="${balance_text%% *}"
  balance="${balance//_/}"
  if [[ "$balance" =~ ^[0-9]+$ ]] && (( balance >= LOCAL_DEPLOYER_CYCLES )); then
    log "Deployer identity '$ICP_IDENTITY_NAME' has sufficient cycles ($balance_text)"
    return
  fi

  log "Funding deployer identity '$ICP_IDENTITY_NAME' with $LOCAL_DEPLOYER_CYCLES_LABEL local cycles"
  if ! icp_cmd cycles transfer "$LOCAL_DEPLOYER_CYCLES_LABEL" "$principal" \
      -e local --identity anonymous >/dev/null; then
    log "ERROR: could not transfer local fabricated cycles from anonymous to '$ICP_IDENTITY_NAME'"
    log "       Check that this project's local network is running and that the anonymous identity is funded"
    exit 1
  fi
}

# Resolves the icp-cli managed local replica URL (http://localhost:<port>) from the
# repository-root project, where the platform (Router) is deployed. icp-cli runs one
# managed "local" instance per icp.yaml project, so resolving from this demo project
# would return the demo's own (empty) replica. The Gleaph CLI's `local` name hardcodes
# http://localhost:8000, which is not where any managed network listens, so every
# `gleaph` call must pass the real URL.
local_api_url() {
  (cd "$ROOT" && icp_cmd network status local --json | node -e '
const fs = require("node:fs");
const raw = fs.readFileSync(0, "utf8");
const status = JSON.parse(raw);
const u = status.api_url || status.gateway_url || "";
process.stdout.write(u.replace(/\/+$/, ""));
')
}

# Writes .env.local so the frontend build bakes in the Router id and the local
# replica URL. GLEAPH_DEMO_SKIP_VITE_ENV=1 disables the write (CI);
# GLEAPH_DEMO_FORCE_VITE_IC_HOST=1 also overwrites VITE_IC_HOST. Existing files
# are preserved line-by-line (only the managed lines are replaced), and the write
# is skipped when the replica does not answer /api/v2/status within 1s so a stale
# or half-up URL never poisons .env.local.
write_vite_env() {
  local router_id="$1"
  local env_path="$DEMO/.env.local"

  if [[ "${GLEAPH_DEMO_SKIP_VITE_ENV:-0}" == "1" ]]; then
    log "Skipping Vite .env.local write (GLEAPH_DEMO_SKIP_VITE_ENV=1)"
    return 0
  fi

  local api_url
  api_url="$(local_api_url)"
  if [[ -z "$api_url" ]]; then
    log "WARN: could not resolve local api_url; .env.local not written"
    return 0
  fi

  # 4xx/5xx still counts as "listening"; only connection-refused / timeout triggers
  # the no-op path.
  if ! curl -fsS --max-time 1 -o /dev/null "$api_url/api/v2/status" 2>/dev/null; then
    log "WARN: $api_url/api/v2/status did not respond within 1s; .env.local not written"
    return 0
  fi

  if [[ ! -f "$env_path" ]]; then
    cat > "$env_path" <<EOF
VITE_GLEAPH_ROUTER_CANISTER_ID=$router_id
VITE_IC_HOST=$api_url
VITE_FETCH_ROOT_KEY=true
EOF
    log "Wrote $env_path (router=$router_id, host=$api_url)"
    return 0
  fi

  # Replace the managed lines in place, preserving every other line verbatim;
  # append at the end when a managed line is absent.
  local tmp_path
  tmp_path="$(mktemp "${TMPDIR:-/tmp}/gleaph-vite-env.XXXXXX")"
  local force_host="${GLEAPH_DEMO_FORCE_VITE_IC_HOST:-0}"
  awk \
      -v new_id="$router_id" \
      -v new_host="$api_url" \
      -v force_host="$force_host" '
    /^[[:space:]]*#/ { print; next }
    /^[[:space:]]*VITE_GLEAPH_ROUTER_CANISTER_ID[[:space:]]*=/ {
      print "VITE_GLEAPH_ROUTER_CANISTER_ID=" new_id
      found_id = 1
      next
    }
    /^[[:space:]]*VITE_IC_HOST[[:space:]]*=/ {
      if (force_host == "1") print "VITE_IC_HOST=" new_host
      else print
      found_host = 1
      next
    }
    { print }
    END {
      if (found_id == 0) print "VITE_GLEAPH_ROUTER_CANISTER_ID=" new_id
      if (found_host == 0 && force_host == "1") print "VITE_IC_HOST=" new_host
    }
  ' "$env_path" > "$tmp_path"
  mv "$tmp_path" "$env_path"

  if [[ "$force_host" == "1" ]]; then
    log "Updated VITE_GLEAPH_ROUTER_CANISTER_ID and VITE_IC_HOST in $env_path (router=$router_id, host=$api_url, force=true)"
  else
    log "Updated VITE_GLEAPH_ROUTER_CANISTER_ID in $env_path (router=$router_id)"
  fi
}

# Delegates to the full bootstrap when the platform is missing (fresh network, stale
# Router mapping, or an unreachable replica). Opt out with GLEAPH_DEMO_SKIP_AUTO_BOOTSTRAP=1;
# GLEAPH_DEMO_FROM_BOOTSTRAP (set by the bootstrap itself) prevents re-delegation loops.
delegate_to_bootstrap() {
  local reason="$1"
  if [[ "${GLEAPH_DEMO_SKIP_AUTO_BOOTSTRAP:-0}" == "1" || -n "${GLEAPH_DEMO_FROM_BOOTSTRAP:-}" ]]; then
    log "ERROR: $reason"
    log "       Run the full bootstrap: $ROOT/scripts/deploy-demo-local.sh"
    exit 1
  fi
  log "Delegating to the full bootstrap ($reason)"
  exec env GLEAPH_DEMO_FROM_BOOTSTRAP=1 GLEAPH_DEMO_DIR="${DEMO#"$ROOT/"}" \
    bash "$ROOT/scripts/deploy-demo-local.sh"
}

main() {
  # A non-default install mode is a platform-level intent: reinstall wipes the Router
  # and graph-shard state, and upgrade reinstalls the platform wasm. Only the full
  # bootstrap owns the platform canister lifecycle, so hand off even when the platform
  # already looks ready. `auto` (the default) keeps the application-only fast path.
  # GLEAPH_DEMO_FROM_BOOTSTRAP skips the handoff so the bootstrap's own app-flow pass
  # cannot re-delegate into a loop.
  if [[ -z "${GLEAPH_DEMO_FROM_BOOTSTRAP:-}" \
    && -n "${GLEAPH_DEMO_INSTALL_MODE:-}" \
    && "${GLEAPH_DEMO_INSTALL_MODE}" != "auto" ]]; then
    delegate_to_bootstrap "GLEAPH_DEMO_INSTALL_MODE=${GLEAPH_DEMO_INSTALL_MODE} requests a platform-level (re)install"
  fi

  log "Resolving Router canister principal"
  local router_id
  # A fresh network has no Router mapping yet; that is a missing-platform signal too, so
  # do not let the lookup failure abort before the auto-bootstrap delegation can run.
  router_id="$(resolve_router_id 2>/dev/null | sed 's/[[:space:]]//g' || true)"
  if [[ -z "$router_id" || "$router_id" == "2vxsx-fae" ]]; then
    delegate_to_bootstrap "no Router canister mapping (platform not deployed)"
  fi
  if [[ ! -f "$DEPLOYER_PEM" ]]; then
    log "ERROR: deployer PEM not found at $DEPLOYER_PEM"
    log "       Create it with: icp identity new --storage plaintext $ICP_IDENTITY_NAME"
    exit 1
  fi

  LOCAL_API_URL="$(local_api_url)"
  if [[ -z "$LOCAL_API_URL" ]]; then
    log "ERROR: could not resolve the local replica URL (icp network status local)"
    exit 1
  fi
  # Platform readiness: the repository-root replica must answer and the demo graph must
  # be registered. `list_graphs` also proves the Router exists (a missing canister rejects
  # the query), so the management-canister status update call is unnecessary; `icp network
  # status` trusts a cached descriptor, and the Router id lookup by name reads the local
  # mapping without querying — verify both for real so a partially deployed platform
  # re-bootstraps instead of failing mid-flow at migration apply.
  local platform_ready=0
  if curl -fsS --max-time 2 -o /dev/null "$LOCAL_API_URL/api/v2/status" 2>/dev/null \
    && (cd "$ROOT" && icp_cmd canister call -e local gleaph-router list_graphs '()' 2>/dev/null | grep -Fq "\"$GRAPH_NAME\""); then
    platform_ready=1
  fi
  if [[ "$platform_ready" == "0" ]]; then
    delegate_to_bootstrap "platform not ready at $LOCAL_API_URL (Router $router_id or graph $($GRAPH_NAME) missing)"
  fi
  log "Local replica: $LOCAL_API_URL"

  cd "$DEMO"

  log "Building seed artifacts and generated sources"
  pnpm run build:config

  log "Generating missing avatar assets (default 140 are committed)"
  # The default user set's 140 avatar SVGs are committed under public/avatars/,
  # so this is a no-op unless a changed user scale leaves some missing (then the
  # missing ones are fetched from DiceBear; network failure degrades to the
  # initial-letter chip instead of failing the build).
  pnpm run generate:avatars

  log "Applying schema migrations (type, typed graph, indexes)"
  gleaph_cmd migration apply --canister "$router_id" --identity "$DEPLOYER_PEM"

  log "Loading seed graph"
  gleaph_cmd load seeds/vertices.jsonl seeds/edges.jsonl --canister "$router_id" --identity "$DEPLOYER_PEM"

  log "Registering prepared queries"
  gleaph_cmd prepared apply --canister "$router_id" --identity "$DEPLOYER_PEM"

  log "Regenerating typed client from the Router manifest"
  gleaph_cmd codegen --canister "$router_id" --identity "$DEPLOYER_PEM"

  log "Writing Vite .env.local (router=$router_id)"
  write_vite_env "$router_id"

  log "Building frontend (Router id baked in at build time)"
  if [[ ! -f "$ROOT/sdk/client/js/dist/index.mjs" ]]; then
    log "Building @gleaph/sdk (dist is gitignored)"
    (cd "$ROOT" && pnpm sdk:build)
  fi
  (cd "$DEMO" && pnpm run build)

  if [[ "${GLEAPH_DEMO_VERIFY_QUERY:-0}" == "1" ]]; then
    log "Verifying prepared queries are registered (gleaph prepared status)"
    gleaph_cmd prepared status --canister "$router_id" --identity "$DEPLOYER_PEM"
  else
    log "Skipping prepared-query verification; set GLEAPH_DEMO_VERIFY_QUERY=1 to enable it"
  fi

  log "Deploying asset canister"
  if ! icp_cmd canister status -e local -i social-demo >/dev/null 2>&1; then
    log "Creating asset canister social-demo"
    ensure_deployer_cycles
    icp_cmd canister create -e local --identity "$ICP_IDENTITY_NAME" --quiet social-demo >/dev/null
  fi
  icp_cmd deploy -e local -y social-demo

  printf '\nSocial demo is ready.\n'
  printf '  Router: %s\n' "$router_id"
  printf '  Run the frontend with: pnpm -C demo/social dev\n'
}

main "$@"
