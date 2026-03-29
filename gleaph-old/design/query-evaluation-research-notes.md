# Query Evaluation Research Notes

## Status

- Draft research note
- Scope: broad query-evaluation strategies for avoiding avoidable full scans and intermediate-result blowups
- Audience: planner/executor work in `gleaph`

## Goal

We need a general strategy for making a wider class of graph and relational-style queries efficient, not just isolated fast paths.

The core principle is:

> Do not read or materialize more than the query shape fundamentally requires.

In practice, this breaks down into four recurring tactics:

1. Shrink the candidate set before traversal or join
2. Avoid constructing large intermediate rows
3. Return ranked results early when only top-k is needed
4. Reuse maintained state when the same aggregate is queried repeatedly

There is no single research result that eliminates full scans for all exact ad-hoc queries. The literature instead provides a toolbox whose value depends on query shape.

## Main Takeaways

### 1. Full scans are sometimes information-theoretically necessary

For exact ad-hoc queries without precomputed state, some workloads inherently require reading all relevant input edges or tuples. The goal is not "never scan", but rather:

- avoid scanning irrelevant regions
- avoid repeated rescans of the same facts
- avoid exploding intermediate states
- stop early when ranking semantics permit it

This is especially important for graph workloads where traversal and aggregation can otherwise degenerate into repeated neighborhood rescans.

### 2. The most useful broad families are not all the same

The most relevant research families for `gleaph` are:

- semijoin reduction and acyclic join processing
- worst-case optimal multiway joins
- factorized query processing and partial aggregation
- top-k / ranked enumeration
- incremental view maintenance
- adaptive runtime switching between binary and factorized/WCOJ execution

### 3. `gleaph` should treat query shapes differently

A single "generic executor" will leave too much performance on the table. A more realistic architecture is:

- planner identifies a query family
- executor chooses a specialized execution mode for that family
- generic row materialization remains the fallback, not the default for everything

## Research Map

## 1. Semijoin Reduction and Acyclic Joins

### Why it matters

For tree-shaped or path-shaped queries, much of the useless work can be removed before the main join/traversal phase. This is the oldest and still one of the most practical ways to avoid broad scans.

### Key idea

Push constraints across edges first, so that later stages only touch candidates that can actually contribute to answers.

### Best fit in `gleaph`

- acyclic multi-hop `MATCH` patterns
- path queries with selective labels or property predicates
- cases where current execution expands one hop at a time with weak early pruning

### Notes

Yannakakis-style semijoin reduction is the classical reference point for acyclic joins. Dynamic extensions are relevant if we later want maintained structures rather than one-shot execution.

### References

- [Algorithms for Acyclic Database Schemes (Yannakakis, VLDB 1981)](https://www.sigmod.org/publications/dblp/db/conf/vldb/Yannakakis81.html)

## 2. Worst-Case Optimal Joins

### Why it matters

Binary join pipelines often create huge intermediates for cyclic patterns such as triangles, diamonds, and many-to-many graph motifs. Worst-case optimal joins (WCOJ) avoid that class of blow-up by processing multiway joins more directly.

### Key idea

Evaluate a conjunctive pattern as a coordinated intersection problem instead of as a sequence of pairwise joins.

### Best fit in `gleaph`

- cyclic graph patterns
- many-to-many joins
- future graph motif queries beyond simple 1-hop aggregations

### Where it is not the answer

- simple 1-hop grouped counts
- highly selective single-anchor traversals where the main win comes from pruning or aggregation pushdown rather than multiway join shape

### References

- [Leapfrog Triejoin: A Worst-Case Optimal Join Algorithm](https://arxiv.org/abs/1210.0481)
- [Juggling Functions Inside a Database](https://sigmodrecord.org/2017/05/11/juggling-functions-inside-a-database-2/)
- [Free Join: Unifying Worst-Case Optimal and Traditional Joins](https://doi.org/10.1145/3589295)
- [Join Processing for Graph Patterns: An Old Dog with New Tricks](https://dl.acm.org/doi/10.1145/2764947.2764948)

## 3. FAQ / InsideOut

### Why it matters

Many `MATCH + aggregate` queries are not "join first, aggregate later" problems. They are variable-elimination problems where the right elimination order determines whether the engine scans and materializes too much.

### Key idea

Represent joins, aggregations, and related computations as a Functional Aggregate Query (FAQ), then use variable elimination and dynamic programming to minimize work.

### Best fit in `gleaph`

- multi-hop patterns with aggregation
- cases where the current executor expands rows eagerly and only later groups them
- future unification of traversal planning and aggregate planning

### Implication

The recent `endpoint.prop, COUNT(*)` optimization is a small, hand-written instance of this broader idea: aggregate as early as possible, and do not materialize rows that will be immediately collapsed.

### References

- [Juggling Functions Inside a Database](https://sigmodrecord.org/2017/05/11/juggling-functions-inside-a-database-2/)

## 4. Factorized Query Processing and Partial Aggregation

### Why it matters

This is one of the most directly applicable families for `gleaph`.

### Key idea

Store and process repeated structure once instead of re-emitting it as flat rows. Aggregate while traversing, not after generating a large row stream.

### Best fit in `gleaph`

- grouped counts and sums over 1-hop and 2-hop traversals
- many-to-many joins where repeated endpoint bindings dominate row counts
- graph workloads where endpoint identities repeat heavily across edges

### Important consequence

This family generalizes the ad-hoc fast paths we already added. Instead of one function per benchmark, we should think in terms of a small number of factorized execution families:

- endpoint-grouped count
- endpoint-grouped weighted sum
- partial aggregation over 2-hop expansions
- factorized join outputs for many-to-many patterns

### References

- [Aggregation and Ordering in Factorised Databases](https://www.cs.ox.ac.uk/dan.olteanu/papers/bkoz-vldb13-with-response.pdf)
- [Factorized Databases summary and publication links](https://www.cs.ox.ac.uk/dan.olteanu/publications.html~)

## 5. Top-k and Ranked Enumeration

### Why it matters

If the query only wants the best `k` answers, evaluating all answers exactly and sorting them afterward is often wasted work.

### Key idea

Interleave joining/traversal with ranking, and stop as soon as enough evidence exists that the current top-k cannot be beaten.

### Best fit in `gleaph`

- `ORDER BY ... LIMIT k`
- top influencers / recommendations / ranked neighborhood analytics
- graph algorithms that return only a small frontier of best candidates

### Important nuance

Top-k research helps only when the ranking function permits meaningful upper bounds or incremental ordering. For arbitrary ranking expressions this is harder.

### References

- [Top-K Aggregation Queries over Large Networks](https://research.ibm.com/publications/top-k-aggregation-queries-over-large-networks)
- [Optimal Join Algorithms Meet Top-k](https://pmc.ncbi.nlm.nih.gov/articles/PMC7872590/)
- [Ranked Enumeration for Database Queries](https://sigmodrecord.org/issues-sigmod-record-september-2024/)

## 6. Incremental View Maintenance

### Why it matters

If the same expensive aggregate is queried repeatedly, the best way to avoid full scans is often not a better query plan, but maintaining the answer incrementally during updates.

### Key idea

Pay more at write time so that read time avoids recomputation.

### Best fit in `gleaph`

- in-degree / out-degree summaries
- repeated top influencer / recommendation summary queries
- repeated graph analytics over slowly changing graphs

### Practical interpretation

For `gleaph`, full general-purpose IVM is a long-term project. Narrow maintained summaries are the practical starting point:

- maintained degree counts
- maintained label-scoped degree counts
- optionally maintained top-k heaps for selected workloads

### References

- [DBToaster: higher-order delta processing for dynamic, frequently fresh views](https://link.springer.com/article/10.1007/s00778-013-0348-4)
- [Incremental View Maintenance with Triple Lock Factorization Benefits (F-IVM)](https://www.cs.ox.ac.uk/dan.olteanu/papers/no-sigmod18.pdf)

## 7. Adaptive Runtime Choice

### Why it matters

No one physical strategy wins for every query instance. Some shapes are better as binary joins; others are better as factorized or WCOJ execution. Choosing statically from weak statistics is often not enough.

### Key idea

Gather cheap runtime evidence, then switch execution style when the observed shape suggests a different operator family will win.

### Best fit in `gleaph`

- cyclic joins where WCOJ sometimes wins big and sometimes loses
- workloads where stored statistics are weak or stale
- embedded execution where low planning overhead matters

### References

- [Adaptive Factorization Using Linear-Chained Hash Tables](https://www.vldb.org/cidrdb/2025/adaptive-factorization-using-linear-chained-hash-tables.html)

## 8. Graph-System Specific Guidance

### Why it matters

Graph DBMSs face recurring patterns that relational systems do not always optimize well by default:

- many-to-many neighborhood joins
- recursive path expansion
- compact adjacency access patterns
- interaction between factorization and graph-specific storage

### Relevant system papers

- [KÙZU Graph Database Management System](https://vldb.org/cidrdb/2023/kuzu-graph-database-management-system.html)
- [DuckPGQ: Efficient Property Graph Queries in an analytical RDBMS](https://vldb.org/cidrdb/2023/duckpgq-efficient-property-graph-queries-in-an-analytical-rdbms.html)

Both are useful because they explicitly discuss the tension between:

- factorization quality
- scan locality
- graph-specific join operators
- recursive/path functionality

## Recommended Design Principles for `gleaph`

## 1. Make query-family detection explicit

The planner should identify at least these families:

- anchored selective scan
- acyclic path/tree join
- cyclic join / motif
- endpoint-grouped aggregate
- ranked/top-k aggregate
- repeated maintained aggregate

The generic row executor should be fallback behavior.

## 2. Push aggregation into traversal

If a query can be represented as:

- expand one edge or one hop
- update a small per-group accumulator

then do that directly. Do not produce rows only to re-group them.

## 3. Treat top-k as a first-class physical property

`ORDER BY ... LIMIT k` should influence planning early, not only after row production. It changes whether full enumeration is necessary.

## 4. Separate exact ad-hoc optimization from maintained-state optimization

These are different design paths:

- exact ad-hoc: pruning, factorization, WCOJ, top-k early stop
- repeated workload: maintained summaries, IVM-style updates

Do not force one mechanism to solve both.

## 5. Prefer narrow execution families over one giant "smart executor"

Broadly useful specialization families are easier to reason about than a single monolithic optimizer. Examples:

- endpoint-grouped count
- endpoint-grouped sum
- 2-hop partial aggregate
- acyclic semijoin-pruned path execution
- cyclic WCOJ execution

## Suggested Near-Term Roadmap

## Phase A: broaden factorized aggregate execution

Target:

- `endpoint.prop, COUNT(*)`
- `endpoint.prop, SUM(weight)`
- single-hop and selected two-hop variants

Why:

- highest confidence continuation of the work already paying off

## Phase B: semijoin-style pruning for acyclic multi-hop queries

Target:

- tree/path `MATCH` with selective node or edge predicates

Why:

- avoids unnecessary frontier expansion without requiring full WCOJ infrastructure

## Phase C: top-k aware execution

Target:

- ranked aggregates and ranked neighbor exploration

Why:

- many recommendation/influencer queries only need top results

## Phase D: cyclic-pattern operator family

Target:

- triangle-like and many-to-many graph motifs

Why:

- current binary/traversal execution will eventually hit intermediate-result blowups

## Phase E: maintained summaries for repeated analytics

Target:

- degree counts
- label-scoped degrees
- top-k summaries for selected high-frequency queries

Why:

- best way to avoid recomputation when workloads repeat

## Non-Goals and Cautions

- WCOJ is not a universal replacement for all joins
- top-k algorithms are only useful when ranking semantics permit pruning
- IVM can shift too much cost to writes if applied indiscriminately
- factorization can hurt if it is used blindly on shapes where flat execution is cheaper
- adaptive switching is useful only if runtime signals are cheap to collect

## Bottom Line

For `gleaph`, the most promising broad strategy is not a single algorithm. It is a layered execution model:

1. prune aggressively for acyclic/path queries
2. factorize and aggregate early for grouped traversals
3. use top-k aware execution when ranking semantics permit it
4. introduce WCOJ for cyclic many-to-many patterns
5. maintain narrow summaries for repeated analytics
6. add adaptive runtime switching where static planning is too coarse

That stack is consistent with the literature and with the benchmark evidence we have already seen locally.

## References

- [Algorithms for Acyclic Database Schemes (Yannakakis, VLDB 1981)](https://www.sigmod.org/publications/dblp/db/conf/vldb/Yannakakis81.html)
- [Leapfrog Triejoin: A Worst-Case Optimal Join Algorithm](https://arxiv.org/abs/1210.0481)
- [Juggling Functions Inside a Database](https://sigmodrecord.org/2017/05/11/juggling-functions-inside-a-database-2/)
- [Aggregation and Ordering in Factorised Databases](https://www.cs.ox.ac.uk/dan.olteanu/papers/bkoz-vldb13-with-response.pdf)
- [Top-K Aggregation Queries over Large Networks](https://research.ibm.com/publications/top-k-aggregation-queries-over-large-networks)
- [Optimal Join Algorithms Meet Top-k](https://pmc.ncbi.nlm.nih.gov/articles/PMC7872590/)
- [Ranked Enumeration for Database Queries](https://sigmodrecord.org/issues-sigmod-record-september-2024/)
- [DBToaster: higher-order delta processing for dynamic, frequently fresh views](https://link.springer.com/article/10.1007/s00778-013-0348-4)
- [Incremental View Maintenance with Triple Lock Factorization Benefits (F-IVM)](https://www.cs.ox.ac.uk/dan.olteanu/papers/no-sigmod18.pdf)
- [Adaptive Factorization Using Linear-Chained Hash Tables](https://www.vldb.org/cidrdb/2025/adaptive-factorization-using-linear-chained-hash-tables.html)
- [KÙZU Graph Database Management System](https://vldb.org/cidrdb/2023/kuzu-graph-database-management-system.html)
- [DuckPGQ: Efficient Property Graph Queries in an analytical RDBMS](https://vldb.org/cidrdb/2023/duckpgq-efficient-property-graph-queries-in-an-analytical-rdbms.html)
