#!/usr/bin/env bash
set -euo pipefail

# Dependency-direction gate for the portable GQL crates (AGENTS.md, "gql and gql-planner").
#
# Each crate in PORTABLE must reach, through normal and build edges with every feature enabled,
#   - no workspace-local crate outside PORTABLE, and
#   - no Internet Computer crate (FORBIDDEN_EXTERNAL).
# The check is transitive: a forbidden crate pulled in through an allowed-looking dependency fails
# just like a direct one.
#
# gleaph-gql-planner is not listed: it depends on gleaph-graph-kernel, which reaches candid,
# ic-stable-structures and ic-cdk. Add it here once that edge is gone.

PORTABLE=(
  gleaph-gql
  gleaph-gql-keywords
  gleaph-gql-macros
  gleaph-gql-value
)
FORBIDDEN_EXTERNAL='^(ic[-_][a-z0-9_-]*|candid[a-z0-9_-]*|canbench[a-z0-9_-]*|pocket-ic)$'

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

is_portable() {
  local name="$1" p
  for p in "${PORTABLE[@]}"; do
    [[ "$p" == "$name" ]] && return 0
  done
  return 1
}

status=0
for crate in "${PORTABLE[@]}"; do
  # `{p}` prints "name vX.Y.Z" and appends " (/abs/path)" for path dependencies, which is how a
  # workspace-local crate is told apart from a registry one. Proc-macro crates also carry a
  # "(proc-macro)" marker, so match on the path form rather than on any trailing text.
  while read -r name _version rest; do
    [[ -z "$name" || "$name" == "$crate" ]] && continue
    if [[ "$rest" == *"(/"* ]] && ! is_portable "$name"; then
      echo "error: $crate depends on workspace crate $name, which is outside the portable GQL set" >&2
      status=1
    elif [[ "$name" =~ $FORBIDDEN_EXTERNAL ]]; then
      echo "error: $crate depends on Internet Computer crate $name" >&2
      status=1
    fi
  done < <(cargo tree -p "$crate" -e normal,build --all-features --prefix none --format '{p}' | awk '!seen[$1]++')
done

if [[ $status -eq 0 ]]; then
  echo "dependency direction: ok (${PORTABLE[*]})"
fi
exit $status
