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
../../target/venv/bin/python scripts/embedding_contract.py --check
gleaph prepared apply
gleaph grants apply
gleaph text-index --graph movies --name film_search

# Natural-language routing (ADR 0111/0117): enable routing, then declare the genre domain from the
# graph's own labels with English aliases so both kana and English genre surfaces canonicalize to
# the graph labels before execution.
URL=$(icp network status local | awk -F': ' '/Api Url/{gsub(/\/$/,"",$2); print $2}')
ROUTER=$(python3 -c "import json;print(json.load(open('.gleaph/cache/account/local.router.json')))")
IDENTITY="${GLEAPH_CONFIG_HOME:-$HOME/.config/gleaph}/identity/keys/dev.pem"
mkdir -p browser
python3 - "$ROUTER" "$URL" <<'PYCONFIG'
import json
import sys

router, host = sys.argv[1], sys.argv[2]
with open("browser/config.json", "w", encoding="utf-8") as file:
    json.dump({"canisterId": router, "host": host, "fetchRootKey": True}, file, indent=2)
    file.write("\n")
PYCONFIG
gleaph config enable-ask --graph movies --canister "$ROUTER" --network "$URL" --fetch-root-key \
  --identity "$IDENTITY"
gleaph config parameter-values --graph movies --canister "$ROUTER" --network "$URL" --fetch-root-key \
  --identity "$IDENTITY" --param genre --from-label Genre --property name --limit 64 \
  --aliases domains/genre-aliases.json

gleaph ask --eval eval/routing.jsonl
gleaph ask --eval eval/similar-2026-11-13.jsonl --graph movies --canister "$ROUTER" \
  --network "$URL" --fetch-root-key --identity "$IDENTITY"
node scripts/eval-similarity-gate.mjs --gate --output /tmp/gleaph-similarity-gate.json
gleaph ask --eval eval/genre-2026-11-13.jsonl --graph movies --canister "$ROUTER" \
  --network "$URL" --fetch-root-key --identity "$IDENTITY"
python3 scripts/check-genre-aliases.py
gleaph ask --eval eval/english-genre-2026-11-13.jsonl --graph movies --canister "$ROUTER" \
  --network "$URL" --fetch-root-key --identity "$IDENTITY"
gleaph ask --eval eval/english-topic-2026-11-13.jsonl --graph movies --canister "$ROUTER" \
  --network "$URL" --fetch-root-key --identity "$IDENTITY"
