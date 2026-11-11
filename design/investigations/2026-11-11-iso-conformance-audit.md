# ISO/IEC 39075 cross-check of the parser, planner and their contracts

Date: 2026-11-11 23:20 UTC+09:00
Author: Gleaph agent
Status: first pass; findings recorded, fixes listed at the end
Plans: 0398–0406 (the `SELECT ... FROM` and match-mode work this audit grew out of)

## Goal

Compare the text of ISO/IEC 39075:2024 with what this repository actually accepts, plans and renders,
and with the claims its design documents make, then record every disagreement instead of leaving it
implicit in whichever module happens to be loose.

## Disposition update (2026-11-11, same day, after the fixes)

F3/F4/F9 were closed by introducing `GraphTypeSource { CopyOf, Like, Nested }` (parser, formatter,
graph-catalog, router catalog paths); F1/F2/F5/F8 turned out to be owned by the **validate** layer
(`gleaph_gql::validate`), which rejects them before any plan or catalog write — the parser stays
permissive and the ledger's parse axis therefore cannot express them. Two findings were withdrawn:

- **F10 is not a dead field.** The probe ran without the `cypher` feature; with it, `INSERT INTO` is a
  documented dialect extension (`parser/statement.rs`, `#[cfg(feature = "cypher")]`) and the field is
  live. The audit's claim was feature-dependent and is retracted.
- **F6 stays open** (`MATCH ... YIELD <name> AS <alias>`), because fix requires splitting 16.3's
  graph-pattern yield (names only, or `NO BINDINGS`) from 16.14's yield clause (aliases allowed), which
  `CALL`/`NEXT` keep using. That split also unblocks F7.

F6 and F7 were then closed by plan 0408: 16.3's `<graph pattern yield clause>` (element/path variable
names, or `NO BINDINGS`) is now a separate AST type from 16.14's `<yield clause>` (items with `AS`
aliases), which `CALL` and `NEXT` keep using. The parser therefore rejects `MATCH ... YIELD n AS m` and
accepts `MATCH ... YIELD NO BINDINGS`, and the validate layer enforces 16.3 rule 12 a (declared names
only, no repeats) on the new type. No remaining slate.

## Method

- The standard's text was obtained from a public mirror of ISO/IEC 39075:2024 (a licensed copy; the
  mirror is not authoritative) and extracted with `pdftotext -layout` (628 pages). Clause numbers,
  rule numbers and short quotes below refer to that text; the sweep was clause-by-clause, formats and
  Syntax Rules, for the clauses this repository implements (12, 13, 14.12, 15.1, 16.3, 16.4, 18.2–18.4,
  20.x, 21.3).
- Behaviour was measured, not inferred: every "repo behaviour" line is a parser/planner probe run
  against the current tree, not a reading of the code. The probe inputs are reproducible by hand.
- Contracts were read from `design/gql/syntax-support.md`, `design/adr/0105`–`0108`, and the crate
  documentation they point at.

## Findings

Classification: **over-accept** = ISO-invalid input the parser accepts; **under-accept** = ISO-valid
input the parser rejects; **conflation** = two ISO constructs stored in one AST shape; **contract
drift** = a design document says something the code or the standard contradicts.

| # | ISO clause and rule | Repo behaviour | Class | Disposition |
| --- | --- | --- | --- | --- |
| F1 | 12.4 Format: `{ <open graph type> \| <of graph type> }` is mandatory | `CREATE GRAPH g` parses, plans an empty-plan catalog path, and renders unchanged | over-accept | tighten the parser to require the type spec; the planner ledger's "catalog statement takes an empty plan" rows are unaffected once the type is present |
| F2 | 12.4/12.6 Format: `{ ... IF NOT EXISTS \| OR REPLACE ... }` are alternatives | `CREATE OR REPLACE GRAPH IF NOT EXISTS g ANY` parses | over-accept | reject the combination in the parser with a diagnostic naming the grammar alternatives |
| F3 | 12.6 Format: `<graph type source>` is mandatory | `CREATE GRAPH TYPE t` parses and renders unchanged | over-accept | same as F1 |
| F4 | 12.6 Format: `<graph type source> ::= [AS] <copy of graph type> \| <graph type like graph> \| [AS] <nested graph type specification>` — `LIKE` and `COPY OF` are different sources | `CREATE GRAPH TYPE t LIKE g` stores `copy_of` with `as_keyword = false` and renders `CREATE GRAPH TYPE t COPY OF g`. `graphTypeLikeGraph` derives the type from a graph expression's constraining type; `<copy of graph type>` copies an existing graph type | conflation | split the AST into a `GraphTypeSource` enum; the formatter must render `LIKE` back and must not rewrite one into the other |
| F5 | 13.3 Format: `<set all properties item> ::= <binding variable reference> = { [ <property key value pair list> ] }` — the right side is a property map, not an expression | `SET n = 1` parses and renders | over-accept | restrict the parser to a `{ ... }` map (the AST already keeps the map as an expression; the map node is what should be required) |
| F6 | 16.3 Format: `<graph pattern yield item> ::= <element variable reference> \| <path variable reference>` — no alias | `MATCH (n) YIELD n AS m` parses and renders | over-accept | drop the alias from the yield parser (the AST's `YieldItem.alias` then also has to go, or lose its input path) |
| F7 | 16.3 Format: `<graph pattern yield item list> ::= items \| NO BINDINGS` | `MATCH (n) YIELD NO BINDINGS` is rejected | under-accept | add `NO BINDINGS` to the yield parser and to `MATCH` planning |
| F8 | 16.3 Syntax Rule 2: every yield item is a variable in the scope of the graph pattern | `MATCH (n) YIELD undeclared` parses | over-accept | reject a yield item the pattern does not declare (a name check at parse or validate time) |
| F9 | 12.4 Format: `<of graph type>` allows `[ [ PROPERTY ] GRAPH ] <nested graph type specification>`, i.e. the inner `PROPERTY GRAPH` is an optional spelling | `CREATE GRAPH g PROPERTY GRAPH { NODE Person }` renders without the inner `PROPERTY GRAPH` | spelling loss | acceptable: the inner keywords are cosmetic; recorded here so it is not mistaken for a meaning change |
| F10 | 13.2 Format: `INSERT <insert graph pattern>` — no `INTO <graph>` clause | the AST carries `InsertStatement.graph_name` and the formatter renders `INSERT INTO <graph> ...`, but no parser path sets the field (`INSERT INTO g (:A)` is rejected) | dead field | delete the field and the formatter branch, or drop the formatter branch and keep the field only when a parser path needs it |
| F11 | 14.12 Format lists `<select statement body>` with `<where clause>` after it | implemented in plan 0404; the graph-match source keeps its `WHERE` inside the match, which is rule 9 b's lifting case | conform | — |
| F12 | 15.1 Syntax Rule 9: an inline call's incoming and outgoing tables are name-disjoint; the scope is its incoming table | implemented in plan 0403 (seeded `ImplicitAll`/`Explicit` scopes; the `SELECT` rewrite returns only new names) | conform | — |
| F13 | 16.4 Syntax Rule 6: `KEEP` supplies the default prefix of a prefix-less path pattern | implemented in plan 0406 by distributing the prefix before lowering | conform | — |
| F14 | 4.11.9: the implicit match mode is implementation-defined (ID086) | declared as `REPEATABLE ELEMENTS` and pinned by plan 0406; `DIFFERENT EDGES` and `TRAIL`/`SIMPLE`/`ACYCLIC` stay fail-closed because they need executor state | conform (declared) | — |

Also confirmed rejected where the standard has no such production: `INSERT INTO g ...` (13.2),
`NODE TYPE`/`NODE :`/`IMPLIES`/`=>` graph-type elements (18.2–18.4, already tracked as parse gaps),
and `MATCH` outside a linear query.

## Contract drift found and fixed in this change

- `design/gql/syntax-support.md` listed `CREATE GRAPH TYPE t LIKE g` → `COPY OF g` as a "deliberate
  canonicalization". It is not a spelling choice: F4 shows the two are different ISO sources, so the
  row now names the AST gap instead.
- The same document had no place for ISO-invalid input the parser tolerates. It gained an
  "Over-accepted spellings" table with F1, F2, F3, F5, F6, F8, and F7 joined the parse-gap table.

## Fix slates

Ordered so each is independently landable and measurable:

1. **Parser tightenings (F1, F2, F3, F5, F6, F8).** Each narrowing turns a currently accepted
   ISO-invalid input into a rejection; the conformance ledger needs one `Parse::Err` row per input so
   the tightened boundary is pinned, and any test that used the permissive spelling moves to the
   valid one. F6 also removes `YieldItem.alias`, which the formatter and `MATCH YIELD` planning read.
2. **`YIELD NO BINDINGS` (F7).** Parser plus `MATCH` planning: the yield list becomes
   `Items | NoBindings`, matching the shape `RETURN` already has.
3. **Graph-type source split (F4).** `CreateGraphTypeStatement` gains
   `GraphTypeSource::{Like(ObjectName), CopyOf { as_keyword, name }, Nested { as_keyword, definition }}`;
   the parser stops storing `LIKE` in `copy_of` and the formatter renders each source back. This is
   the only finding that changes an AST shape, so it is also the one that touches the most call sites
   (`crates/router` catalog paths, `crates/cli` if it reads the field).
4. **Dead `INSERT INTO` field (F10).** Delete `InsertStatement.graph_name` and the formatter branch; no
   parser path produces it, and ISO has no such clause.

Until 1–4 land, the status document's "over-accepted spellings" table is the record, and none of these
findings block the planner: they are acceptance-boundary questions, not planning ones.

## Method limits

- The audit covers the clauses this repository implements. Clauses with no implementation at all
  (catalog object lifecycle beyond create/drop, transactions beyond the envelope, sessions,
  authorization, procedures, and the whole of Clause 22's general rules) were not swept.
- Behaviour was measured on the parser/planner as they stand; a finding may be a deliberately recorded
  gap (F7, F9) or a defect (F4, F5, F6, F8) — the table says which.
- The standard text came from a mirror of a licensed copy. Anything that turns on wording rather than
  structure (Notes, implementation-defined markers) was not used as a conformance claim.
