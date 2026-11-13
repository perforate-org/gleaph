#!/usr/bin/env bash
# Similar-films intent for the movie demo.
#
# The client sends the anchor film's QID. A preceding MATCH binds the anchor; the Router resolves
# its stored embedding from the vector canister and searches the same index, so no client-side
# model or embedding asset is needed and the anchor cannot drift from the indexed vector space.
#
#   ROUTER=... ./scripts/similar-films.sh "千と千尋の神隠し"
set -euo pipefail
cd "$(dirname "$0")/.."

TITLE=${1:?usage: similar-films.sh "film title"}
ROUTER=${ROUTER:-$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")}
NETWORK=${NETWORK:-$(icp network status local 2>/dev/null | awk -F': ' '/Api Url/{gsub(/\/$/,"",$2); print $2}')}
IDENTITY=${IDENTITY:-${GLEAPH_CONFIG_HOME:-$HOME/.config/gleaph}/identity/keys/dev.pem}
TITLE_JSON=$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$TITLE")
QID=$(../../target/debug/gleaph gql \
  --query 'MATCH (f:Film {title: $title}) RETURN f.qid AS qid' \
  --param "title=$TITLE_JSON" \
  --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key \
  | awk 'NR == 2 { print $1 }')
if [ -z "$QID" ]; then
  echo "film title not found: $TITLE" >&2
  exit 1
fi
QID_JSON=$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$QID")
../../target/debug/gleaph prepared run films-similar-to --param "anchor_qid=$QID_JSON" \
  --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key
