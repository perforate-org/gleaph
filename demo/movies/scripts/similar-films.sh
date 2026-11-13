#!/usr/bin/env bash
# Similar-films intent for the movie demo.
#
# The canister only executes a vector search. This client-side intent resolves the anchor film to
# its stored synopsis embedding and binds that 384-d little-endian f32 vector to the shared
# `films-by-embedding` operation.
#
#   ROUTER=... ./scripts/similar-films.sh "千と千尋の神隠し"
set -euo pipefail
cd "$(dirname "$0")/.."

TITLE=${1:?usage: similar-films.sh "film title"}
ROUTER=${ROUTER:-$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")}
NETWORK=${NETWORK:-$(icp network status local 2>/dev/null | awk -F': ' '/Api Url/{gsub(/\/$/,"",$2); print $2}')}
IDENTITY=${IDENTITY:-${GLEAPH_CONFIG_HOME:-$HOME/.config/gleaph}/identity/keys/dev.pem}
VECTOR=$(../../target/venv/bin/python scripts/embed-film.py "$TITLE")
../../target/debug/gleaph prepared run films-by-embedding --param-bytes "embedding=$VECTOR" \
  --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key
