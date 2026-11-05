#!/usr/bin/env bash
# Dev-identity execution matrix for all four scenario ops (current live state).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
export GLEAPH_NETWORK="${GLEAPH_NETWORK:-http://localhost:32781}"
export GLEAPH_FETCH_ROOT_KEY=true
export GLEAPH_CANISTER=4caro-hl777-77775-aaaba-cai
G="/Users/yota/dev/gleaph/target/debug/gleaph"
PEM="/Users/yota/.config/gleaph/identity/keys/dev.pem"

for op in variable-length-reach shortest-path team-readable-documents citation-reach; do
  echo "=== $op (dev) ==="
  "$G" prepared run "$op" --identity /Users/yota/.config/gleaph/identity/keys/dev.pem 2>&1 | tail -3
  echo "exit=$?"
done
