#!/usr/bin/env bash
# Similar-films intent for the movie demo.
#
# The client sends only the anchor film's ELEMENT_ID. The Router asks the vector canister for the
# anchor's stored embedding and searches the same index, so no client-side model or embedding
# asset is needed and the anchor cannot drift from the indexed vector space.
#
#   ROUTER=... ./scripts/similar-films.sh "千と千尋の神隠し"
set -euo pipefail
cd "$(dirname "$0")/.."

TITLE=${1:?usage: similar-films.sh "film title"}
ROUTER=${ROUTER:-$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")}
NETWORK=${NETWORK:-$(icp network status local 2>/dev/null | awk -F': ' '/Api Url/{gsub(/\/$/,"",$2); print $2}')}
IDENTITY=${IDENTITY:-${GLEAPH_CONFIG_HOME:-$HOME/.config/gleaph}/identity/keys/dev.pem}
TITLE_JSON=$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$TITLE")
ID_ROW=$(../../target/debug/gleaph gql \
  --query 'MATCH (f:Film {title: $title}) RETURN ELEMENT_ID(f) AS id' \
  --param "title=$TITLE_JSON" \
  --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key \
  | awk 'NR == 2 { print $1 }')
if [ -z "$ID_ROW" ]; then
  echo "film title not found: $TITLE" >&2
  exit 1
fi
ANCHOR=$(printf '%s' "$ID_ROW" | python3 -c 'import base64,json,sys; print(base64.b64encode(bytes(json.load(sys.stdin))).decode())')
../../target/debug/gleaph prepared run films-similar-to --param-bytes "anchor=$ANCHOR" \
  --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key
