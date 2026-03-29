# GQL Standard Deviations & Conformance Plan

> **About this document**: Catalogs all known deviations of the Gleaph GQL implementation
> from ISO/IEC 39075:2024 (GQL). Each deviation is classified by severity and accompanied
> by a recommended action.
>
> **Context**: Gleaph is **not yet in production**. There are no deployed tenants and no
> existing queries to break. This means we can make breaking changes freely — backward
> compatibility is not a constraint. Where the GQL standard conflicts with our current
> syntax, we should align with the standard now, before any users depend on the current
> behavior.

---

## Severity Levels

| Severity | Meaning |
|----------|---------|
| **High** | Syntactic incompatibility — a valid GQL query would fail, or a Gleaph query produces non-standard AST |
| **Medium** | Semantic or structural difference — the feature works but the surface syntax diverges from the standard |
| **Low** | Gleaph-specific extension with no GQL counterpart — not a conflict, just non-portable |

---

## High Severity

### H1. ~~`CREATE` for data mutations~~ (RESOLVED)

| | |
|---|---|
| **Before** | `CREATE (:User {name: "Alice"})` |
| **GQL standard** | `INSERT (:User {name: "Alice"})` |
| **Status** | **Fixed.** `INSERT` is now the only accepted keyword for data mutations. `CREATE (` returns a parse error directing users to `INSERT`. DDL statements (`CREATE GRAPH`, `CREATE GRAPH TYPE`, `CREATE SCHEMA`) are unaffected. |

### H2. ~~Undirected edge syntax `~`~~ (NOT SUPPORTED)

| | |
|---|---|
| **GQL standard** | `~[e:KNOWS]~` for undirected edges |
| **Gleaph** | Not supported. Gleaph is a directed-only graph database. All `~` based syntax (`~[e:L]~`, `~[e:L]~>`, `<~[e:L]~`, `~/L/~`, etc.) is rejected with a parse error. |
| **Bidirectional matching** | Use `-[e:L]-` or `-/L/-` to match directed edges in either direction. |

### H3. ~~`SKIP` keyword~~ (RESOLVED)

| | |
|---|---|
| **Before** | Both `SKIP n` and `OFFSET n` accepted |
| **GQL standard** | `OFFSET n` only. |
| **Status** | **Fixed.** `SKIP` removed from parser and reserved keywords. Only `OFFSET` is accepted. |

### H4. ~~Simplified edge syntax `-/L/->`~~ (RESOLVED — restored)

| | |
|---|---|
| **GQL standard** | §16.12 defines abbreviated edge patterns: `-/L/->`, `<-/L/-`, `<-/L/->`, `-/L/-` |
| **Status** | **Fixed.** All four GQL simplified edge forms are now supported. Previously removed by mistake — the syntax is part of the GQL standard. |

---

## Medium Severity

### M1. ~~`OPTIONAL MATCH` vs `OPTIONAL { ... }`~~ (RESOLVED)

| | |
|---|---|
| **Before** | Only `OPTIONAL MATCH (n:User)-[:KNOWS]->(m) RETURN n, m` |
| **GQL standard** | `OPTIONAL { MATCH ... }`, `OPTIONAL ( MATCH ... )`, or `OPTIONAL MATCH ...` |
| **Status** | **Fixed.** All three forms are now accepted: `OPTIONAL MATCH ...` (Cypher/GQL), `OPTIONAL { MATCH ... }` (GQL block), and `OPTIONAL ( MATCH ... )` (GQL paren). |

### M2. ~~`WITH` clause (Cypher piping)~~ (RESOLVED)

| | |
|---|---|
| **Before** | Only Cypher-style `MATCH (n) WITH n.name AS name MATCH (m) RETURN m` |
| **GQL standard** | `MATCH (n) RETURN n.name AS name NEXT MATCH (m) RETURN m` using `NEXT` for linear composition. |
| **Status** | **Fixed.** `NEXT` is supported as a compound operator (like `UNION`/`EXCEPT`) with optional `YIELD` clause. Both `WITH` piping and `NEXT` pipeline forms are accepted. See `design/with-to-next-migration.md`. |

### M3. ~~String literal quoting convention~~ (RESOLVED)

| | |
|---|---|
| **Before** | Both `"str"` and `'str'` accepted as string literals |
| **GQL standard** | `'str'` for string literals (single quotes). `"ident"` for delimited identifiers (double quotes). |
| **Status** | **Fixed.** `'str'` is now the only string literal syntax. `"ident"` produces `Token::QuotedIdent` (same as backtick-quoted identifiers), following GQL/SQL convention. |

### M4. `DETACH DELETE` syntax

| | |
|---|---|
| **Current** | `MATCH (n:User) DETACH DELETE n` |
| **GQL standard** | `DETACH DELETE` exists in GQL (§13.3). Syntax is compatible. |
| **Status** | **Conformant.** No action needed. |

### M5. `MERGE` / `ON CREATE SET` / `ON MATCH SET`

| | |
|---|---|
| **Current** | Cypher-style `MERGE (n:User {id: 1}) ON CREATE SET n.name = "A" ON MATCH SET n.updated = true` |
| **GQL standard** | GQL does not define `MERGE`. The standard uses `INSERT ... ON CONFLICT` or similar patterns are left to implementation. |
| **Impact** | `MERGE` is a practical upsert mechanism widely expected by graph DB users. |
| **Action** | Keep as a Gleaph extension. Document in `gleaph-extensions.md`. |

### M6. `STARTS WITH` / `ENDS WITH` / `CONTAINS` infix predicates

| | |
|---|---|
| **Current** | `n.name STARTS WITH "Al"` |
| **GQL standard** | GQL defines `LIKE` (§19.8) for pattern matching. These infix predicates are Cypher-derived. |
| **Action** | Keep as extensions (widely expected by Cypher users). Document in `gleaph-extensions.md`. |

---

## Low Severity (Extensions — No GQL Conflict)

### L1. Hex / Octal / Binary integer literals

`0xFF`, `0o77`, `0b1010` — GQL defines only decimal integers. No conflict since these tokens cannot be confused with standard syntax. Keep as extension.

### L2. `ILIKE`

Case-insensitive `LIKE`. PostgreSQL origin. GQL defines `LIKE` but not `ILIKE`. Keep as extension.

### L3. `FOR ... IN ... RETURN`

Row expansion over lists. No GQL counterpart. Keep as Gleaph-specific extension.

### L4. `FINISH` statement

Execute mutations without returning a result set. No GQL counterpart. Keep as extension.

### L5. `FILTER` statement

`MATCH ... FILTER condition` — streaming row filter. No GQL counterpart. Keep as extension.

### L6. `LET` statement

`MATCH (n) LET score = n.a + n.b RETURN n, score` — inline computed bindings. No GQL counterpart. Keep as extension.

### L7. `VALUE { subquery }` / `LET x = e IN body END`

Value-level subquery and inline bindings. Gleaph-specific. Keep as extension.

### L8. Inline `WHERE` in patterns

`(n:User WHERE n.age > 25)` — GQL has a similar concept in element pattern predicates (§16.7). Likely conformant; verify against grammar.

### L9. Query parameters `$param`

Named parameters via `$paramName`. GQL defines parameter references (§21.3). Likely conformant.

### L10. `CALL (vars) { body }`

Inline subquery with outer-scope variable seeding. Gleaph-specific. Keep as extension.

---

## Action Summary

Since Gleaph is pre-production with zero deployed users, **all breaking changes can be made immediately** without migration paths, deprecation periods, or backward-compatible aliases.

### Immediate (simple parser changes) — ALL DONE

| Item | Effort | Status |
|------|--------|--------|
| H1. CREATE → INSERT | Trivial | ✅ Done |
| H2. Undirected `~` rejected (directed-only DB) | Small | ✅ Done |
| H3. Remove `SKIP` | Trivial | ✅ Done |
| H4. Restore `-/L/->` simplified edge syntax (GQL §16.12) | Trivial | ✅ Done |

### Short-term (moderate parser + test changes) — ALL DONE

| Item | Effort | Status |
|------|--------|--------|
| M3. String quoting (`'str'` only, `"ident"` for identifiers) | Medium | ✅ Done |

### Short-term (moderate parser + test changes) — ALL DONE

| Item | Effort | Status |
|------|--------|--------|
| M1. `OPTIONAL { ... }` block syntax | Medium — parser addition | ✅ Done |

### Already implemented (discovered during investigation)

| Item | Status |
|------|--------|
| M2. `NEXT` linear composition | ✅ Already implemented as `SetOp::Next` compound operator |

### No action needed

| Item | Reason |
|------|--------|
| M4. DETACH DELETE | Already GQL-conformant |
| M5. MERGE | Keep as extension (document) |
| M6. STARTS WITH / ENDS WITH / CONTAINS | Keep as extension (document) |
| L1–L10 | Extensions with no GQL conflict |

---

## References

- ISO/IEC 39075:2024 — Information technology — Database languages — GQL
- `design/gql-specification.md` — Gleaph's section-by-section GQL spec reference
- `design/gleaph-extensions.md` — Registry of all Gleaph-specific extensions
- `reference/grammar/GQL.g4` — ANTLR4 grammar for GQL standard
