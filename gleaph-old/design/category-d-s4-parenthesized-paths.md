# Parenthesized Subpath Patterns (§16.7)

## Status: Implemented (Phases 1–4)

## Motivation

GQL §16.3 allows grouping path elements with parentheses and applying quantifiers:
```sql
MATCH (a)-[:KNOWS]->((x)-[:LIKES]->(y)){2,4}->(b)
```
This enables multi-hop subpatterns with bounded repetition.

## Current State

The parser currently flattens all chains into a linear `Vec<MatchChain>`.
Path quantifiers (`*`, `+`, `{n}`, `{n,m}`) apply to individual edges via `PathLength`.
There is no notion of grouped/nested patterns.

## Design

### AST Extension

```rust
// New variant in MatchChain or new wrapper type:
pub enum PatternElement {
    /// A simple edge-node hop.
    Chain(MatchChain),
    /// A grouped subpath with optional quantifier.
    SubPath {
        /// The sub-pattern: a start node + chains (reusing MatchClause).
        pattern: MatchClause,
        /// Repetition quantifier.
        quantifier: PathLength,
        /// Optional variable binding for the subpath.
        var: Option<String>,
    },
}

// MatchClause becomes:
pub struct MatchClause {
    pub start: NodePattern,
    pub elements: Vec<PatternElement>,  // was: chains: Vec<MatchChain>
}
```

### Alternative: Minimal Change

Keep `MatchChain` but add a `SubPath` variant to it:
```rust
pub enum MatchChain {
    Hop { edge: EdgePattern, node: NodePattern },
    SubPath {
        inner_chains: Vec<MatchChain>,
        quantifier: PathLength,
        var: Option<String>,
    },
}
```

**Recommendation**: Use the second (minimal change) approach to avoid breaking existing code.

### Parser Changes

In `parse_match_clause`, after parsing the start node:
1. If next token is `(` and this isn't a new match clause, parse as subpath group
2. Recursively parse inner chains until matching `)`
3. Parse optional quantifier `{n,m}` / `*` / `+`
4. Continue parsing outer chains

Maximum nesting depth guard (e.g., 4 levels) to prevent stack overflow.

### Executor Changes

In `extend_match`, handle `MatchChain::SubPath`:
1. For each quantifier repetition count `k` in `[min, max]`:
   - Apply the inner chains `k` times sequentially
   - Collect result bindings
2. Union all results

For `extend_var_len` BFS optimization:
- SubPaths cannot use the BFS fast path (too complex)
- Fall back to full enumeration with depth limit

### Path Mode Interaction

- `TRAIL`: Track visited edges across subpath repetitions
- `SIMPLE`: Track visited vertices across subpath repetitions
- `ACYCLIC`: Verify no vertex revisit including start

### Implementation Phases

1. **Phase 1**: AST + parser (parenthesized grouping, no quantifier yet)
2. **Phase 2**: Quantifier `{n}` (exact repetition) — simplest case
3. **Phase 3**: Quantifier `{n,m}` (range) — generates multiple paths
4. **Phase 4**: Interaction with path modes (Trail/Simple/Acyclic)

### Risks

- Parser complexity: recursive descent into pattern elements
- **Performance: exponential blowup with large quantifier ranges — O(fan_out^(hops×max))**
- Path mode interaction is subtle (edge/vertex tracking across repetitions)

### Computational Complexity Warning

Subpath expansion is O(fan_out^(hops × rep_max)). On an IC canister with 5B–40B instruction
limits, even moderate fan-out (10) with `{1,5}` can exhaust the budget. Users should:
- Keep quantifier ranges small (max ≤ 4)
- Use on sparse graphs or with label/property constraints that limit fan-out
- Prefer single-edge variable-length paths (`-[:E]*2..4->`) when the pattern is a single hop

### Test Plan (verified)

```sql
-- Basic grouping (no quantifier = exactly once)
MATCH (a:N)((x)-[:E]->(y))(b:N) RETURN a.idx, b.idx  -- 4 rows (=single hop)

-- Exact repetition
MATCH (a:N)((x)-[:E]->(y)){2}(b:N) RETURN a.idx, b.idx  -- 3 rows (2-hop paths)

-- Range repetition
MATCH (a:N)((x)-[:E]->(y)){1,2}(b:N) RETURN a.idx, b.idx  -- 7 rows (1+2 hop)
```

### Phase 4: Path mode interaction (TRAIL/SIMPLE/ACYCLIC) ✅

Implemented. When path_mode is non-Walk and the pattern contains SubPath elements,
an internal path variable (`__shortest_internal__`) accumulates all traversed edges/vertices
across subpath repetitions. `path_mode_allows()` is applied before stripping the internal
variable, so TRAIL (no repeated edges), SIMPLE (no repeated vertices), and ACYCLIC
constraints are correctly enforced across repetition boundaries. 2 tests.
