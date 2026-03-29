# Gleaph GQL Extensions

> **About this document**: Documents all Gleaph-specific extensions to the ISO/IEC 39075:2024 (GQL)
> standard. Extensions are features that go beyond or differ from the GQL specification, adopted
> from other languages (Cypher, SQL) or added for practical utility.
>
> **Policy**: All extensions must be documented here **before implementation**. Extensions must not
> conflict with GQL standard syntax. If a future GQL revision standardizes an extension, it should
> be moved to `gql-specification.md` and marked accordingly.

---

## Extension Levels

| Level                         | Description                                                                                                                                          | Policy               |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------- |
| **Level 1 — Cypher-compat**   | Defined in Neo4j Cypher, which GQL was designed with in mind. Eases migration and maximizes familiarity for the largest existing graph DB user base. | Implement freely     |
| **Level 2 — SQL-compat**      | Defined in SQL standard that GQL explicitly references (e.g. §20.9 references SQL aggregate functions).                                              | Implement freely     |
| **Level 3 — Gleaph-specific** | No precedent in Cypher, SQL, or GQL. Implement only when a concrete user need is identified.                                                         | Require user request |

---

## 1. Level 1 — Cypher-Compatible Extensions

> **Note**: Cypher-compatible list comprehension, list quantifiers (`any`/`all`/`none`/`single`),
> `reduce()`, and several scalar/aggregate/path functions have been **removed** to reduce
> future GQL standard conflict risk. See git history for the removed implementations.

### 1.1 MERGE (Upsert)

Cypher's `MERGE` provides match-or-create semantics not present in GQL.

| Syntax | Description | Status |
|--------|-------------|--------|
| `MERGE (n:Label {key: val})` | Match existing node or create if not found | ✅ Implemented |
| `ON CREATE SET n.prop = val` | Set properties only when creating | ✅ Implemented |
| `ON MATCH SET n.prop = val` | Set properties only when matching | ✅ Implemented |

**Note**: Edge MERGE patterns are not yet supported (node-only).

### 1.2 Infix String Predicates

Cypher-style infix string predicates. GQL defines `LIKE` (§19.8) but not these forms.

| Syntax | Description | Status |
|--------|-------------|--------|
| `expr STARTS WITH 'prefix'` | True if string starts with prefix | ✅ Implemented |
| `expr ENDS WITH 'suffix'` | True if string ends with suffix | ✅ Implemented |
| `expr CONTAINS 'substr'` | True if string contains substring | ✅ Implemented |
| `NOT` variants | Negated forms of all three | ✅ Implemented |

---

## 2. Level 2 — SQL-Compatible Extensions

GQL §20.9 states that aggregate functions follow SQL semantics. The following SQL-standard aggregates are included as natural extensions.

### 2.1 Aggregate Functions

| Function                   | Signature                      | SQL Reference  | Status         |
| -------------------------- | ------------------------------ | -------------- | -------------- |
| `PERCENTILE_CONT(expr, p)` | `(Number, Float[0,1]) → Float` | SQL:2003 §10.9 | ✅ Implemented |
| `PERCENTILE_DISC(expr, p)` | `(Number, Float[0,1]) → Float` | SQL:2003 §10.9 | ✅ Implemented |
| `STRING_AGG(expr, sep)`    | `(Any, String) → String`       | SQL:2016 §10.9 | ✅ Implemented |

### 2.2 String Predicates

GQL §20.23 defines `LIKE` and references SQL string predicates. `ILIKE` is a widely-adopted PostgreSQL extension.

| Syntax                   | Description                                            | Status         |
| ------------------------ | ------------------------------------------------------ | -------------- |
| `expr LIKE pattern`      | SQL wildcard match: `%` = any chars, `_` = single char | ✅ Implemented |
| `expr ILIKE pattern`     | Case-insensitive `LIKE` (PostgreSQL extension)         | ✅ Implemented |
| `expr NOT LIKE pattern`  | Negated `LIKE`                                         | ✅ Implemented |
| `expr NOT ILIKE pattern` | Negated `ILIKE`                                        | ✅ Implemented |

---

## 3. Level 3 — Gleaph-Specific Extensions

These have no direct precedent in Cypher, GQL, or SQL. Implemented only for demonstrated utility.

### 3.1 Numeric Literal Formats

GQL §21 defines only decimal integer and decimal float literals. Gleaph additionally accepts:

| Syntax                    | Example                  | Description                            | Status         |
| ------------------------- | ------------------------ | -------------------------------------- | -------------- |
| Hexadecimal integer       | `0xFF`, `0X1A`           | `0x`/`0X` prefix, digits `[0-9a-fA-F]` | ✅ Implemented |
| Octal integer             | `0o77`, `0O77`           | `0o`/`0O` prefix, digits `[0-7]`       | ✅ Implemented |
| Binary integer            | `0b1010`, `0B1010`       | `0b`/`0B` prefix, digits `[01]`        | ✅ Implemented |
| Scientific notation float | `1.5e3`, `2.7e-2`, `5e3` | `e`/`E` exponent suffix                | ✅ Implemented |

**Use case**: Debugging numeric property values stored as bit flags or hex IDs.
**Risk**: Dialect fragmentation — queries using these literals won't run on other GQL engines.

### 3.2 `caller()` Built-in Function

Returns the IC caller principal as a `Principal` value. Uses `ic_cdk::api::msg_caller()` on-chain; returns anonymous principal in native tests.

| Syntax | Description | Status |
|--------|-------------|--------|
| `caller()` | Returns the caller's `Principal` (no arguments) | ✅ Implemented |

**Use case**: Row-level access control — filter data by the calling principal without passing it as a parameter.

```gql
-- Filter documents owned by the caller
MATCH (d:Doc) WHERE d.owner = caller() RETURN d.title

-- Store caller as owner when creating
MATCH (d:Doc)-[:AUTHORED_BY]->(a) WHERE a.name = $name SET d.owner = caller()
```

**Implementation**: Thread-local injection via `set_caller()`/`clear_caller()` with a drop-guard in the IC bridge layer. The GQL engine (`crates/gql`) remains IC-agnostic.

### 3.3 `Value::Principal` Type

First-class `Principal` value type for representing IC principals in GQL expressions.

| Feature | Description | Status |
|---------|-------------|--------|
| Principal literal | Via `caller()` function or parameter binding | ✅ Implemented |
| CAST to TEXT | `CAST(caller() AS TEXT)` → principal text representation | ✅ Implemented |
| Comparison | `=`, `<>`, `<`, `>` ordering between principals | ✅ Implemented |
| Property storage | Stored/retrieved as property values (tag 13) | ✅ Implemented |

### 3.4 `AccessLevel::Execute`

Permission level that restricts a principal to only execute prepared statements. Cannot run direct queries or mutations.

| Level | Capabilities | Status |
|-------|-------------|--------|
| `Execute` | `execute_prepared`, `execute_prepared_mutation` only | ✅ Implemented |

**Use case**: Application principals that should only invoke pre-approved queries, not arbitrary GQL.

### 3.5 `PreparedStatementInfo` Metadata

The `prepare()` and `list_prepared()` endpoints return rich metadata about prepared statements.

| Field | Type | Description |
|-------|------|-------------|
| `name` | `String` | Statement name |
| `kind` | `Query \| Mutation` | Whether the statement is a read or write |
| `parameters` | `Vec<String>` | Parameter names (`$name` → `"name"`) |
| `columns` | `Vec<String>` | RETURN clause column names (empty for mutations) |
| `requires_caller` | `bool` | Whether the statement uses `caller()` |
| `source` | `String` | Original GQL source text |

---

## 4. Compatibility Reference

| Extension                        | Cypher | GQL            | SQL         | Notes                                  |
| -------------------------------- | ------ | -------------- | ----------- | -------------------------------------- |
| `MERGE` / `ON CREATE/MATCH SET`  | ✅     | ❌             | ❌          | Cypher upsert — no GQL equivalent      |
| `STARTS WITH` / `ENDS WITH`      | ✅     | ❌             | ❌          | Cypher string predicates               |
| `CONTAINS`                        | ✅     | ❌             | ❌          | Cypher string predicate                |
| `LIKE` / `ILIKE`                  | ❌     | ✅ §20.23      | ✅          | `ILIKE` is PostgreSQL extension of SQL |
| `PERCENTILE_CONT/DISC`            | ❌     | ✅ via SQL ref | ✅ SQL:2003 |                                        |
| `STRING_AGG`                      | ❌     | ✅ §20.9       | ✅ SQL:2016 |                                        |
| Hex/octal/binary literals         | ❌     | ❌             | ❌          | Gleaph-specific                        |
| Scientific notation               | ❌     | ❌             | ✅          | Standard in most languages             |
| `caller()` function               | ❌     | ❌             | ❌          | Gleaph-specific (IC caller principal)  |
| `Value::Principal` type           | ❌     | ❌             | ❌          | Gleaph-specific (IC principal type)    |
| `AccessLevel::Execute`            | ❌     | ❌             | ❌          | Gleaph-specific (prepared-stmt only)   |
| `PreparedStatementInfo` metadata  | ❌     | ❌             | ❌          | Gleaph-specific (rich prepare result)  |
