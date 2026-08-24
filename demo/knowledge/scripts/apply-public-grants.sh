#!/usr/bin/env bash
# Apply the PUBLIC data-plane grant surface to the knowledge demo Router.
# Usage: ./apply-public-grants.sh   (run from anywhere; envs set below)
set -euo pipefail

DEMO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME="$DEMO/.icp/home"
export ICP_COREPACK_HOME="$DEMO/.icp/corepack-home"
export ICP_XDG_CACHE_HOME="$DEMO/.icp/xdg-cache"
export ICP_XDG_DATA_HOME="$DEMO/.icp/xdg-data"
export DO_NOT_TRACK=1
cd "$DEMO"

ROUTER="${GLEAPH_CANISTER:?set GLEAPH_CANISTER to the Router principal}"
# Full extracted Router interface: decodes Router Err variants truthfully (the embedded
# interface that icp-cli fetches from the network trips its own parser).
DID="${GLEAPH_ROUTER_DID:-$DEMO/.icp/cache/router-full.did}"
[[ -f "$DID" ]] || { echo "missing $DID; run bootstrap.sh (it extracts it) or set GLEAPH_ROUTER_DID"; exit 1; }
i=0
while IFS= read -r stmt; do
  i=$((i + 1))
  out="$(icp canister call -e local --identity gleaph-demo-deployer \
    --candid "$DID" "$ROUTER" gql_mutate \
    "(\"$stmt\", vec {}, \"public-grant-$i\")")"
  if [[ "$out" == *"Err"* ]]; then
    printf 'grant %02d REJECTED: %s\n       %s\n' "$i" "$stmt" "$out"
    exit 1
  fi
  printf 'grant %02d ok: %s\n' "$i" "$stmt"
done <<'STATEMENTS'
GRANT MATCH ON GRAPH knowledge NODES Person TO PUBLIC
GRANT MATCH ON GRAPH knowledge NODES Team TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge NODES Concept TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge NODES Document TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge NODES Person TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge NODES Team TO PUBLIC
GRANT READ ON GRAPH knowledge NODES Concept { name, definition } TO PUBLIC
GRANT READ ON GRAPH knowledge NODES Document { title, body, is_public } TO PUBLIC
GRANT READ ON GRAPH knowledge NODES Person { name, role } TO PUBLIC
GRANT READ ON GRAPH knowledge NODES Team { name } TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge EDGES RELATED_TO TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge EDGES CITES TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge EDGES ABOUT TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge EDGES AUTHORED_BY TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge EDGES OWNS TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge EDGES BELONGS_TO TO PUBLIC
GRANT TRAVERSE ON GRAPH knowledge EDGES ROUTED_VIA TO PUBLIC
STATEMENTS
printf 'all grants applied\n'
