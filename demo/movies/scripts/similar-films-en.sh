#!/usr/bin/env bash
# English-title similar-films intent for the movie demo.
#
#   ROUTER=... ./scripts/similar-films-en.sh "Spirited Away"
set -euo pipefail
cd "$(dirname "$0")/.."

TITLE=${1:?usage: similar-films-en.sh "English film title"}
ROUTER=${ROUTER:-$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")}
NETWORK=${NETWORK:-$(icp network status local 2>/dev/null | awk -F': ' '/Api Url/{gsub(/\/$/,"",$2); print $2}')}
IDENTITY=${IDENTITY:-${GLEAPH_CONFIG_HOME:-$HOME/.config/gleaph}/identity/keys/dev.pem}
TITLE_JSON=$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$TITLE")
../../target/debug/gleaph prepared run films-similar-to-en --param "anchor=$TITLE_JSON" \
  --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key
