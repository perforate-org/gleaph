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

# Topology seam (ADR 0136, Decisions 1 and 11): the Graph shard reaches its store, lara-graph, only
# through `crates/graph/src/facade/topology`: no other file imports `lara_graph` (or the replaced
# `ic_stable_lara`), and graph-kernel names neither store.
seam_status=0
while read -r file; do
  case "$file" in
    crates/graph/src/facade/topology.rs | crates/graph/src/facade/topology/*) continue ;;
  esac
  echo "error: $file reaches the topology store outside facade::topology (ADR 0136, Decision 11)" >&2
  seam_status=1
done < <(grep -rlE --include='*.rs' '\b(lara_graph|ic_stable_lara)\b' crates/graph/src crates/graph-kernel/src | sort)
if grep -qE '^(lara-graph|ic-stable-lara)' crates/graph-kernel/Cargo.toml; then
  echo "error: crates/graph-kernel depends on a topology store (ADR 0136, Decision 11)" >&2
  seam_status=1
fi
if [[ $seam_status -eq 0 ]]; then
  echo "topology seam: ok"
fi

[[ $status -eq 0 && $seam_status -eq 0 ]]
