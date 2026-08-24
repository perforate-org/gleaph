#!/usr/bin/env bash
# One-shot discrimination probe: grant the probe privileges DIRECTLY to the dev principal
# (bypassing PUBLIC subject evaluation) and retry the dev run.
set -uo pipefail
DEMO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME="$DEMO/.icp/home"
export ICP_COREPACK_HOME="$DEMO/.icp/corepack-home"
export ICP_XDG_CACHE_HOME="$DEMO/.icp/xdg-cache"
export ICP_XDG_DATA_HOME="$DEMO/.icp/xdg-data"
export DO_NOT_TRACK=1
cd "$DEMO"

ROUTER="${GLEAPH_CANISTER:?set GLEAPH_CANISTER}"
DID=".icp/cache/router-full.did"
DEV_P="${1:-${DEV_PRINCIPAL:?pass dev principal as \$1}}"

i=0
for s in \
  "GRANT MATCH ON GRAPH knowledge NODES Concept TO PRINCIPAL '$DEV_P'" \
  "GRANT READ ON GRAPH knowledge NODES Concept { name } TO PRINCIPAL '$DEV_P'" \
  "GRANT TRAVERSE ON GRAPH knowledge NODES Concept TO PRINCIPAL '$DEV_P'"; do
  i=$((i + 1))
  out="$(icp canister call -e local --identity gleaph-demo-deployer --candid "$DID" "$ROUTER" gql_mutate "(\"$s\", vec {}, \"direct-grant-$i\")")"
  if [[ "$out" == *"Err"* ]]; then
    echo "direct grant $i REJECTED: $s"
    echo "$out" | head -3
    exit 1
  fi
  echo "direct grant $i ok"
done

GLEAPH_NETWORK="http://localhost:${GATEWAY_PORT:-32773}" GLEAPH_FETCH_ROOT_KEY=true \
  GLEAPH_CANISTER="$ROUTER" "$DEMO/../../target/debug/gleaph" prepared run probenoeid \
  --identity /Users/yota/.config/gleaph/identity/keys/dev.pem
echo "dev_run exit=$?"
