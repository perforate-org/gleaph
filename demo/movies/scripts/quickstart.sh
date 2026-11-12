#!/usr/bin/env bash
# Bring the movie demo up on a local network and load its data.
# Prerequisites: a built `cargo build -p gleaph-cli`, wasm32-unknown-unknown, Docker/OrbStack.
set -euo pipefail
cd "$(dirname "$0")/.."

gleaph identity new dev && gleaph login
gleaph network start
gleaph migration apply
gleaph load seeds/vertices.jsonl seeds/edges.jsonl --graph movies
if [ ! -f seeds/embeddings.jsonl ]; then
  echo "seeds/embeddings.jsonl is missing; regenerate it with" >&2
  echo "  ../../target/venv/bin/python scripts/gen-embeddings.py" >&2
  exit 1
fi
gleaph embed ingest --vertices seeds/vertices.jsonl --embeddings seeds/embeddings.jsonl --graph movies
gleaph prepared apply
gleaph grants apply
gleaph text-index --graph movies --name film_synopsis

# Natural-language routing (ADR 0111/0117): enable routing, then declare the genre domain from the
# graph's own labels so Japanese genre questions resolve to the canonical value.
URL=$(icp network status local | awk -F': ' '/Api Url/{gsub(/\/$/,"",$2); print $2}')
ROUTER=$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")
IDENTITY="${GLEAPH_CONFIG_HOME:-$HOME/.config/gleaph}/identity/keys/dev.pem"
gleaph route inference --graph movies --canister "$ROUTER" --network "$URL" --fetch-root-key \
  --identity "$IDENTITY" --enable
gleaph route domain --graph movies --canister "$ROUTER" --network "$URL" --fetch-root-key \
  --identity "$IDENTITY" --param genre --from-label Genre --property name --limit 64

gleaph ask --eval eval/routing.jsonl
gleaph ask --eval eval/genre-2026-11-13.jsonl --graph movies --canister "$ROUTER" \
  --network "$URL" --fetch-root-key --identity "$IDENTITY"

# Optional: model routing (ADR 0111) and the genre value domain (ADR 0117). Both need the Router
# principal of the deployment; the project keeps it in its local cache.
ROUTER=$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")
IDENTITY="$HOME/.config/gleaph/identity/keys/dev.pem"
gleaph route inference --graph movies --canister "$ROUTER" --network local --identity "$IDENTITY" --enable
# Materialize the domain from the graph itself: a prepared query's genre parameter accepts the
# labels that exist on Genre nodes.
VALUES=$(gleaph gql --query 'MATCH (g:Genre) RETURN DISTINCT g.name AS value ORDER BY value' \
  --canister "$ROUTER" --network local --fetch-root-key \
  | python3 -c 'import sys
for line in sys.stdin:
    word = line.split()
    if word and word[0] not in {"value", "rows"} and not word[0].isdigit():
        print(word[0])' | paste -sd, -)
gleaph route domain --graph movies --canister "$ROUTER" --network local --identity "$IDENTITY" \
  --param genre --set-values "$VALUES"
gleaph ask --eval eval/genre-2026-11-13.jsonl --graph movies --canister "$ROUTER" --network local \
  --identity "$IDENTITY" --fetch-root-key
