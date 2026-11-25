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

# Topology seam (ADR 0136, Decision 11): the Graph shard reaches its store, ic-stable-lara, only
# through `crates/graph/src/facade/topology`: no other file imports `ic_stable_lara` or takes the
# store itself (`topology::with_graph`, `with_graph_mut`, `StableGraph`). The exceptions are listed:
# graph-kernel's modules that name the store's vocabulary, and the facade files that exist for
# ic-stable-lara alone (its memory wiring, slab placement and allocator stats, the forward/reverse
# mirror repair, its substrate tests and fixtures), which the backend swap deletes or rewrites.
SEAM_EXCEPTIONS=(
  crates/graph-kernel/src/entry.rs
  crates/graph-kernel/src/entry/edge.rs
  crates/graph-kernel/src/entry/edge/id.rs
  crates/graph-kernel/src/entry/edge_order.rs
  crates/graph-kernel/src/entry/label.rs
  crates/graph-kernel/src/entry/vertex.rs
  crates/graph-kernel/src/entry/vertex_id.rs
  crates/graph/src/facade/stable/memory.rs
  crates/graph/src/facade/batch_placement.rs
  crates/graph/src/facade/batch_write.rs
  crates/graph/src/facade/derived_state/reverse_adjacency.rs
  crates/graph/src/facade/store/inline_update_fixture.rs
  crates/graph/src/facade/store/tests.rs
)
seam_status=0
while read -r file; do
  case "$file" in
    crates/graph/src/facade/topology.rs | crates/graph/src/facade/topology/*) continue ;;
  esac
  allowed=0
  for kept in "${SEAM_EXCEPTIONS[@]}"; do
    [[ "$file" == "$kept" ]] && allowed=1
  done
  if [[ $allowed -eq 0 ]]; then
    echo "error: $file reaches ic-stable-lara outside facade::topology (ADR 0136, Decision 11)" >&2
    seam_status=1
  fi
done < <(grep -rlE --include='*.rs' 'ic_stable_lara|topology::with_graph(_mut)?\b|topology::StableGraph' \
  crates/graph/src crates/graph-kernel/src | sort)
if [[ $seam_status -eq 0 ]]; then
  echo "topology seam: ok"
fi

[[ $status -eq 0 && $seam_status -eq 0 ]]
