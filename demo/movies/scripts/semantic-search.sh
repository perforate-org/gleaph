#!/usr/bin/env bash
# Semantic topic search for the movie demo.
#
# The canister does not run an embedding model: the model named by the embedding contract encodes the topic
# on the client, and the 384-d little-endian f32 vector is bound to `films-by-embedding` as
# Value::Bytes. This is the semantic-topic intent. Similar films use `films-similar-to` with an
# anchor film title and do not need a client-side model or embedding asset.
#
#   ROUTER=... ./scripts/semantic-search.sh "films about time travel"
#
# Optional: run `scripts/embed-query.py --serve --port 8099` once and point another client at it.
set -euo pipefail
cd "$(dirname "$0")/.."

TEXT=${1:?usage: semantic-search.sh "topic text"}
ROUTER=${ROUTER:-$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")}
NETWORK=${NETWORK:-$(icp network status local 2>/dev/null | awk -F': ' '/Api Url/{gsub(/\/$/,"",$2); print $2}')}
IDENTITY=${IDENTITY:-${GLEAPH_CONFIG_HOME:-$HOME/.config/gleaph}/identity/keys/dev.pem}
VECTOR=$(../../target/venv/bin/python scripts/embed-query.py "$TEXT" 2>/dev/null)
../../target/debug/gleaph prepared run films-by-embedding --param-bytes "embedding=$VECTOR" \
  --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key
