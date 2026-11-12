# 0117. Parameter value domains for schema-conditioned routing

Date: 2026-11-13
Status: accepted (2026-11-13). Implemented: wire v7 domain entries with aliases, Router-owned
per-graph domain catalog, canister-side domain completion, CLI `gleaph route domain --aliases`.
Evidence: `demo/movies/eval/REAL-QUERIES.md` (Japanese genre gate 1/12 -> 12/12; English genre
gate 12/12 with `domains/genre-aliases.json`; canonical slots inspected through
`admin_general_route_raw`, and the CLI `ask --execute` path prints the canonical genre and 12 rows),
unit tests in
`crates/gleaph-routing-canister/src/domains.rs`, router validation tests, wire tests, and the CLI
tests.
Last revised: 2026-11-13
Anchor timestamp: 2026-11-13 01:20:00 UTC +0900

## Context

ADR 0114 makes routing schema/catalog-conditioned: the Router builds a `GraphRoutingContext` from
the graph type plus the prepared-operation cards, and the inference canister selects an operation
and extracts typed slot values. The extraction itself is a fixed set of rules over semantic roles
(`decade`, `category`, `team`, `anchor`), each with English and Japanese surface patterns and a
small lexicon.

That rule set cannot cover a value the graph knows and the rule author did not: `demo/movies`
asks for `genre`, whose values are Wikidata labels such as `SF映画`, `ホラー映画`, `アクション映画`.
The extractor only matches ASCII tokens before `映画`, so `ホラー` and `SF` (as a prefix) are lost;
the operation then reports an incomplete slot and the route abstains. The alternatives are equally
bad: teaching the extractor Japanese genre words puts one graph's vocabulary into a shared model,
and a per-language regex turns the next graph (cities, genes, products) into another rule.

Every such value is a property of the **graph instance**, not of the language or the domain. The
general mechanism for "this parameter takes one of these values" already exists in function-calling
schemas as an `enum` constraint; schema-guided extraction tasks carry the same enum labels in the
schema so the extractor can conform to them. Gleaph's routing context is that schema.

## Decision

1. **A parameter may declare a value domain**: an ordered, bounded set of entries that the parameter
   accepts. Each entry has a canonical graph value and may declare alias surfaces. The domain is
   graph-scoped routing-catalog data, owned by the graph tenant or Router controller under the ADR
   0115 authority predicate. It is not part of the prepared query; the same prepared operation can
   have different domains on two graphs.
2. **The Router attaches domains to the routing context** it builds, and the context fingerprint
   covers them, so a route is always answered against exactly the catalog the caller was shown.
3. **Domain completion is one post-extraction step in the inference canister.** After the model's
   rule extraction, for each parameter with a domain:
   - a slot that already matched a canonical value or alias is canonicalized to the canonical value;
   - a missing slot is filled by the longest **exact or prefix** match of a canonical value or alias
     in the question text, where matching normalizes case and collapses whitespace (no
     language-specific rules and no kana folding; the canonical value is always returned, so
     `ホラー` or `horror` → `ホラー映画`);
   - an ambiguous match (two canonical values share the winning surface/prefix) is refused rather
     than guessed. An alias never reaches execution; only the canonical graph value does.
   Required-slot completeness and the abstain decision are recomputed after completion, so a
   completed slot can rescue an operation and an ambiguous one cannot.
4. **Bounds**: at most 16 parameters per graph, 64 values per parameter, 16 aliases per value,
   128 bytes per canonical value or alias, 8 KiB per graph counting both; writes outside the bounds
   are rejected. A parameter whose real value set exceeds the bound must not declare a domain (the
   client offers a picker instead) - a silently truncated domain would make extraction worse, not
   better.
5. **Values are graph data, materialized by tooling**: `gleaph route domain --from-label L
   --property P --limit N [--aliases PATH]` runs a bounded `DISTINCT` query, attaches aliases from a
   JSON map keyed by canonical value, and stores the result. No domain vocabulary and no language
   rule lives in generic code; aliases are graph routing data supplied by the graph owner.

## Non-goals

- A Router-side title/entity lexicon: the context is a query payload, and thousands of values would
  exceed it. Large domains stay client-side (a picker or a shipped index).
- Per-language or per-domain regexes in the extractor.
- Neural retraining.

## Consequences

- `WIRE_VERSION` becomes 7 (`GeneralParameter.domain` entries carry `value` and `aliases`), so
  Router and inference canister must be deployed together; the release flow already installs both.
  This is a fresh-install change; pre-production has no compatibility branch.
- Domain completion makes a value's *surface* irrelevant and its *canonical form* authoritative,
  which also removes the "question text vs stored label" drift (for example `SF` vs `SF映画`).
- The completion rule is deliberately conservative: an unknown surface stays missing and the route
  still abstains. Domains raise coverage, not recall of out-of-scope questions.
- Limitation: materialization from a multi-shard graph samples the shard that answers the query; a
  graph whose values differ per shard must declare its domain explicitly.
- Limitation: an alias is only a routing surface. It must canonicalize to a value that actually
  exists in the graph; the domain tooling checks that aliases name materialized canonical values,
  but does not prove that the canonical value is semantically the requested genre/topic.
