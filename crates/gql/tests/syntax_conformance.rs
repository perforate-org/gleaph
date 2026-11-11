//! ISO/IEC 39075 syntax conformance ledger (§6–§21).
//!
//! This is the machine-checked source of truth for **what the parser accepts**, referenced by
//! [`design/gql/syntax-support.md`](../../design/gql/syntax-support.md). Each case records the
//! expected outcome on two axes:
//!
//! * `parse` — does `gleaph_gql::parser::parse` accept the input?
//! * `format` — when the `format` feature is on, does `format_query` accept the parsed program, and
//!   if it refuses, with which [`FormatError::Unsupported`] reason?
//!
//! A case whose `parse` expectation is `Err` is a **known gap** tracked for a later slice: the input
//! is grammar-valid ISO GQL that the parser does not accept yet. Flipping one of those to `Ok` is
//! the completion signal for the slice that implements it, and the test then proves it.
//!
//! Plan-time rejections (constructs that parse but whose semantics the planner refuses) are pinned in
//! `gleaph-gql-planner`'s ledger, not here, because that layer owns them. This file is deliberately
//! independent of `design/` at build and test time.

use gleaph_gql::parser;

/// Expected parser outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parse {
    Ok,
    Err,
}

/// Expected formatter outcome (only asserted when the `format` feature is enabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// `format_query` succeeds and the output is stable under a second format pass.
    Ok,
    /// `format_query` fails with `FormatError::Unsupported(<reason>)`.
    Unsupported(&'static str),
    /// The parse axis expects an error, so there is nothing to format.
    NotParsed,
}

/// Feature gate: the case is only asserted when the feature is enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feature {
    Any,
    Gleaph,
    Cypher,
    SqlCompat,
}

impl Feature {
    fn is_active(self) -> bool {
        match self {
            Self::Any => true,
            Self::Gleaph => cfg!(feature = "gleaph"),
            Self::Cypher => cfg!(feature = "cypher"),
            Self::SqlCompat => cfg!(feature = "sql-compat"),
        }
    }
}

struct Case {
    /// Stable construct id, shared with the planner ledger entries and the status document.
    id: &'static str,
    /// Grammar section (`"6"`, `"14.4"`, …). Used by the family-coverage test.
    section: &'static str,
    input: &'static str,
    parse: Parse,
    /// Only read by the format-axis test, which is compiled under the `format` feature.
    #[cfg_attr(not(feature = "format"), allow(dead_code))]
    format: Format,
    feature: Feature,
}

/// The ledger. Grouped by grammar section; keep the section order of the standard.
const CASES: &[Case] = &[
    // ── §6 GQL program ────────────────────────────────────────────────────────────────────────
    Case {
        id: "s06-bare-query",
        section: "6",
        input: "MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s06-empty-program",
        section: "6",
        input: "",
        parse: Parse::Ok,
        format: Format::Unsupported("empty program"),
        feature: Feature::Any,
    },
    Case {
        id: "s06-session-then-query",
        section: "6",
        input: "SESSION SET GRAPH g MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s06-trailing-session-close",
        section: "6",
        input: "MATCH (n) RETURN n SESSION CLOSE",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §7 Session commands ───────────────────────────────────────────────────────────────────
    Case {
        id: "s07.1-session-set-schema-absolute",
        section: "7.1",
        input: "SESSION SET SCHEMA /a/b",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-schema-current",
        section: "7.1",
        input: "SESSION SET SCHEMA CURRENT_SCHEMA",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-schema-relative",
        section: "7.1",
        input: "SESSION SET SCHEMA ../a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-schema-parameter",
        section: "7.1",
        input: "SESSION SET SCHEMA $$p",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-graph",
        section: "7.1",
        input: "SESSION SET GRAPH g",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-property-graph",
        section: "7.1",
        input: "SESSION SET PROPERTY GRAPH g",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-time-zone",
        section: "7.1",
        input: "SESSION SET TIME ZONE 'UTC'",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-value-parameter",
        section: "7.1",
        input: "SESSION SET VALUE $x = 1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-graph-parameter",
        section: "7.1",
        input: "SESSION SET GRAPH $g = 1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.1-session-set-table-parameter",
        section: "7.1",
        input: "SESSION SET TABLE $t = 1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.2-session-reset-all",
        section: "7.2",
        input: "SESSION RESET",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.2-session-reset-schema",
        section: "7.2",
        input: "SESSION RESET SCHEMA",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.2-session-reset-all-parameters",
        section: "7.2",
        input: "SESSION RESET ALL PARAMETERS",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.2-session-reset-time-zone",
        section: "7.2",
        input: "SESSION RESET TIME ZONE",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.2-session-reset-parameter",
        section: "7.2",
        input: "SESSION RESET PARAMETER $x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s07.3-session-close",
        section: "7.3",
        input: "SESSION CLOSE",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §8 Transactions ───────────────────────────────────────────────────────────────────────
    Case {
        id: "s08.1-start-transaction-body",
        section: "8.1",
        input: "START TRANSACTION READ ONLY MATCH (n) RETURN n COMMIT",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s08.1-start-read-write",
        section: "8.1",
        input: "START TRANSACTION READ WRITE",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s08.1-start-read-only",
        section: "8.1",
        input: "START TRANSACTION READ ONLY",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s08.3-rollback",
        section: "8.3",
        input: "ROLLBACK",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s08.4-commit",
        section: "8.4",
        input: "COMMIT",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §9 Procedure specifications ───────────────────────────────────────────────────────────
    Case {
        id: "s09.1-call-inline",
        section: "9.1",
        input: "CALL { MATCH (n) RETURN n }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s09.1-call-inline-next-chain",
        section: "9.1",
        input: "CALL { MATCH (n) NEXT RETURN n }",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s09.2-at-schema-prefix",
        section: "9.2",
        input: "AT /a MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s09.2-value-variable-definition",
        section: "9.2",
        input: "VALUE x = 1 MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §10–§11 Variable definitions and references ──────────────────────────────────────────
    Case {
        id: "s10.1-graph-variable-definition",
        section: "10.1",
        input: "GRAPH g = h MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s10.2-binding-table-variable-definition",
        section: "10.2",
        input: "TABLE t = b MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s11.3-variable-object-expression",
        section: "11.3",
        input: "SESSION SET GRAPH VARIABLE $x",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    // ── §12 Catalog-modifying statements ─────────────────────────────────────────────────────
    Case {
        id: "s12.2-create-schema",
        section: "12.2",
        input: "CREATE SCHEMA /a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.3-drop-schema",
        section: "12.3",
        input: "DROP SCHEMA /a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.4-create-property-graph",
        section: "12.4",
        input: "CREATE PROPERTY GRAPH g ANY",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.4-create-graph-any",
        section: "12.4",
        input: "CREATE GRAPH g ANY",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.4-create-graph-inline-type",
        section: "12.4",
        input: "CREATE GRAPH g { NODE Person }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.4-create-graph-property-graph-inline",
        section: "12.4",
        input: "CREATE GRAPH g PROPERTY GRAPH { NODE Person }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.4-create-graph-copy-of",
        section: "12.4",
        input: "CREATE GRAPH g AS COPY OF h",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.5-drop-graph",
        section: "12.5",
        input: "DROP GRAPH g",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.6-create-graph-type-inline",
        section: "12.6",
        input: "CREATE GRAPH TYPE t { NODE Person }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.6-graph-type-like",
        section: "12.6",
        input: "CREATE GRAPH TYPE t LIKE g",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // 12.6: `LIKE` and `COPY OF` are different sources; the AST keeps them apart.
    Case {
        id: "s12.6-graph-type-copy-of",
        section: "12.6",
        input: "CREATE GRAPH TYPE t COPY OF g",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // 12.6: `<graph type source>` is mandatory, so a bare CREATE GRAPH TYPE is a parse error.
    Case {
        id: "s12.6-create-graph-type-without-source",
        section: "12.6",
        input: "CREATE GRAPH TYPE t",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    // 12.4: the graph type is mandatory, but that is a validate-layer rule, so the parse axis accepts.
    Case {
        id: "s12.4-create-graph-without-type",
        section: "12.4",
        input: "CREATE GRAPH g",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.6-graph-type-as-nested",
        section: "12.6",
        input: "CREATE GRAPH TYPE t AS { NODE Person }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s12.7-drop-graph-type",
        section: "12.7",
        input: "DROP GRAPH TYPE t",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §13 Data-modifying statements ────────────────────────────────────────────────────────
    Case {
        id: "s13.2-insert-bare-pattern",
        section: "13.2",
        input: "INSERT (:A)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s13.3-set-ambient",
        section: "13.3",
        input: "SET n.x = 1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s13.4-remove-ambient",
        section: "13.4",
        input: "REMOVE n:Label",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s13.5-delete-ambient",
        section: "13.5",
        input: "DELETE n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s13.1-dml-first-result-statement",
        section: "13.1",
        input: "INSERT (:A) RETURN 1",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s13.1-juxtaposed-dml",
        section: "13.1",
        input: "INSERT (:A) INSERT (:B)",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s13.1-select-result-after-data-modification",
        section: "13.1",
        input: "MATCH (n) INSERT (:A) SELECT n",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s13.1-bare-block-statement",
        section: "13.1",
        input: "{ MATCH (n) RETURN n }",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    // ── §14 Query statements ─────────────────────────────────────────────────────────────────
    Case {
        id: "s14.2-union",
        section: "14.2",
        input: "MATCH (a) RETURN a UNION MATCH (b) RETURN b",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.2-except-all",
        section: "14.2",
        input: "MATCH (a) RETURN a EXCEPT ALL MATCH (b) RETURN b",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.2-intersect",
        section: "14.2",
        input: "MATCH (a) RETURN a INTERSECT MATCH (b) RETURN b",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.2-otherwise",
        section: "14.2",
        input: "MATCH (a) RETURN a OTHERWISE MATCH (b) RETURN b",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.3-next-yield",
        section: "14.3",
        input: "MATCH (n) RETURN n NEXT YIELD n MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.4-optional-match",
        section: "14.4",
        input: "MATCH (n) OPTIONAL MATCH (n)-[e]->(m) RETURN m",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.4-optional-block-operand",
        section: "14.4",
        input: "MATCH (n) OPTIONAL { MATCH (n)-[e]->(m) } RETURN m",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s14.4-optional-parenthesized-operand",
        section: "14.4",
        input: "MATCH (n) OPTIONAL ( MATCH (n)-[e]->(m) ) RETURN m",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s14.6-filter-where",
        section: "14.6",
        input: "MATCH (n) FILTER WHERE n.x IS NULL RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.7-let",
        section: "14.7",
        input: "MATCH (n) LET x = n.x RETURN x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.7-let-value",
        section: "14.7",
        input: "MATCH (n) LET VALUE x = 1 RETURN x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.8-for",
        section: "14.8",
        input: "MATCH (n) FOR x IN [1, 2] RETURN x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.8-for-offset",
        section: "14.8",
        input: "MATCH (n) FOR x IN [1, 2] WITH OFFSET i RETURN x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s10.2-binding-table-query-initializer",
        section: "10.2",
        input: "TABLE t = { MATCH (n) RETURN n } MATCH (m) RETURN m",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.8-for-ordinality",
        section: "14.8",
        input: "MATCH (n) FOR x IN [1, 2] WITH ORDINALITY i RETURN x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.9-standalone-order-by",
        section: "14.9",
        input: "MATCH (n) ORDER BY n.x RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.9-standalone-limit",
        section: "14.9",
        input: "MATCH (n) LIMIT 5 RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.10-finish",
        section: "14.10",
        input: "MATCH (n) FINISH",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.11-return-star",
        section: "14.11",
        input: "MATCH (n) RETURN *",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.11-return-star-with-order-by",
        section: "14.11",
        input: "MATCH (n) RETURN * ORDER BY n.name",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s14.11-return-distinct",
        section: "14.11",
        input: "MATCH (n) RETURN DISTINCT n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.11-return-group-by",
        section: "14.11",
        input: "MATCH (n) RETURN n.x, COUNT(*) GROUP BY n.x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-without-from",
        section: "14.12",
        input: "SELECT 1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-from",
        section: "14.12",
        input: "SELECT * FROM { MATCH (n) RETURN n }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-from-graph-match",
        section: "14.12",
        input: "SELECT n.x FROM g MATCH (n)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-from-two-graphs",
        section: "14.12",
        input: "SELECT * FROM g1 MATCH (n), g2 MATCH (m)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-after-match",
        section: "14.12",
        input: "MATCH (n) SELECT n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-from-match-mode",
        section: "14.12",
        input: "SELECT * FROM g MATCH REPEATABLE ELEMENTS (n)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-from-yield",
        section: "14.12",
        input: "SELECT * FROM g MATCH (n) YIELD n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-from-multi-path",
        section: "14.12",
        input: "SELECT * FROM g1 MATCH (a), (b), g2 MATCH (m)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s14.12-select-where",
        section: "14.12",
        input: "SELECT * FROM { MATCH (n) RETURN n } WHERE true",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §15 Procedure calls ──────────────────────────────────────────────────────────────────
    Case {
        id: "s15.2-inline-procedure-call",
        section: "15.2",
        input: "CALL { RETURN 1 }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s15.2-inline-procedure-scope",
        section: "15.2",
        input: "CALL (a, b) { MATCH (n) RETURN n }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.2-use-graph-nested-body",
        section: "16.2",
        input: "USE g { MATCH (n) RETURN n }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s15.3-named-procedure-call",
        section: "15.3",
        input: "CALL db.labels()",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s15.3-named-procedure-call-yield",
        section: "15.3",
        input: "CALL db.labels() YIELD lbl RETURN lbl",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §16 Graph patterns ───────────────────────────────────────────────────────────────────
    Case {
        id: "s16.2-use-graph",
        section: "16.2",
        input: "USE g MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.2-use-graph-relative-reference",
        section: "16.2",
        input: "USE ../s/g MATCH (n) RETURN n",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s16.3-graph-pattern-yield",
        section: "16.3",
        input: "MATCH (n) YIELD n RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.4-match-mode-repeatable-elements",
        section: "16.4",
        input: "MATCH REPEATABLE ELEMENTS (a)-[e]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.4-match-mode-different-edges",
        section: "16.4",
        input: "MATCH DIFFERENT EDGES (a)-[e]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.4-keep-clause",
        section: "16.4",
        input: "MATCH (a)-[e]->(b) KEEP WALK RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-path-mode-qualified-shortest",
        section: "16.6",
        input: "MATCH ANY SHORTEST WALK (a)-[e]->{1,2}(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.3-path-variable",
        section: "16.3",
        input: "MATCH p = WALK (a)-[e]->(b) RETURN p",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.3-path-extension",
        section: "16.3",
        input: "MATCH ANY SHORTEST (a)-[e:L]->{1,5}(b) VENDOR_COST BY f(e) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.7-pattern-union",
        section: "16.7",
        input: "MATCH (a)-[e]->(b) | (a)-[f]->(c) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.7-multiset-alternation",
        section: "16.7",
        input: "MATCH (a)-[e]->(b) |+| (a)-[f]->(c) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-path-mode-walk",
        section: "16.6",
        input: "MATCH WALK (a)-[e]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-all-shortest",
        section: "16.6",
        input: "MATCH ALL SHORTEST (a)-[e]->{1,2}(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-any-shortest",
        section: "16.6",
        input: "MATCH ANY SHORTEST (a)-[e]->{1,2}(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-shortest-k",
        section: "16.6",
        input: "MATCH SHORTEST 2 (a)-[e]->{1,2}(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-shortest-k-group",
        section: "16.6",
        input: "MATCH SHORTEST 2 PATHS GROUP (a)-[e:REL]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-all-paths",
        section: "16.6",
        input: "MATCH ALL PATHS (a)-[e]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-any-paths",
        section: "16.6",
        input: "MATCH ANY (a)-[e]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-any-paths-parameter",
        section: "16.6",
        input: "MATCH ANY $k (a)-[e]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-shortest-k-parameter",
        section: "16.6",
        input: "MATCH SHORTEST $k (a)-[e]->{1,2}(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.6-count-paths",
        section: "16.6",
        input: "MATCH COUNT PATHS (a)-[e]->(b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Cypher,
    },
    Case {
        id: "s16.7-variable-length-quantifier",
        section: "16.7",
        input: "MATCH (a)-[:L]->{1,3}(b) RETURN b",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.8-label-expression-union",
        section: "16.8",
        input: "MATCH (n:A|B) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.8-label-expression-negation",
        section: "16.8",
        input: "MATCH (n:!A) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.8-label-expression-nested-negation",
        section: "16.8",
        input: "MATCH (n:!!A) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.11-quantified-subpath",
        section: "16.11",
        input: "MATCH (a)((u)-[e:L]->(v)){1,2}(c) RETURN c",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.13-where-clause",
        section: "16.13",
        input: "MATCH (n) WHERE n.x IS NULL RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s16.17-order-by-nulls-first",
        section: "16.17",
        input: "MATCH (n) RETURN n ORDER BY n.x ASC NULLS FIRST",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §17 References ──────────────────────────────────────────────────────────────────────
    Case {
        id: "s17.1-schema-reference-relative",
        section: "17.1",
        input: "AT ../a MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s17.2-home-graph-reference",
        section: "17.2",
        input: "USE HOME_GRAPH MATCH (n) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s17.5-procedure-reference",
        section: "17.5",
        input: "CALL GLEAPH.FINALIZE_BULK_INGEST()",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §18 Graph types ──────────────────────────────────────────────────────────────────────
    Case {
        id: "s18.1-nested-graph-type-pattern",
        section: "18.1",
        input: "CREATE GRAPH TYPE t { (n:Person)-[:R]->(m:Person) }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.2-node-type-phrase-labels",
        section: "18.2",
        input: "CREATE GRAPH TYPE t { NODE Person LABELS Person }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.2-node-type-phrase-vertex-synonym",
        section: "18.2",
        input: "CREATE GRAPH TYPE t { VERTEX Person }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.2-node-type-phrase-type-keyword",
        section: "18.2",
        input: "CREATE GRAPH TYPE t { NODE TYPE Person }",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s18.2-node-type-key-label-set-implies",
        section: "18.2",
        input: "CREATE GRAPH TYPE t { NODE Person IMPLIES }",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s18.3-edge-type-arrow-endpoints",
        section: "18.3",
        input: "CREATE GRAPH TYPE t { DIRECTED EDGE R CONNECTING (A -> B) }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.3-edge-type-to-endpoints",
        section: "18.3",
        input: "CREATE GRAPH TYPE t { DIRECTED EDGE R CONNECTING (A TO B) }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.4-pattern-label-set-ampersand",
        section: "18.4",
        input: "CREATE GRAPH TYPE t { (:A&B) }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.4-edge-endpoint-label-set",
        section: "18.4",
        input: "CREATE GRAPH TYPE t { (:A)-[:R]->(:B&C) }",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s18.6-property-type-plain",
        section: "18.6",
        input: "CREATE GRAPH TYPE t { NODE Person { age INT64 } }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.6-property-type-typed",
        section: "18.6",
        input: "CREATE GRAPH TYPE t { NODE Person { age :: INT64 } }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.8-binding-table-type",
        section: "18.8",
        input: "SESSION SET VALUE $t :: TABLE { f INT64 } = 1",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s18.9-any-value-type",
        section: "18.9",
        input: "SESSION SET VALUE $x :: ANY VALUE = 1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.9-any-node-reference-type",
        section: "18.9",
        input: "SESSION SET VALUE $x :: ANY NODE = 1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s18.9-closed-reference-type",
        section: "18.9",
        input: "SESSION SET VALUE $x :: (:Person) = 1",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s18.9-property-value-type",
        section: "18.9",
        input: "SESSION SET VALUE $x :: PROPERTY VALUE = 1",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    // ── §19 Predicates ──────────────────────────────────────────────────────────────────────
    Case {
        id: "s19.3-comparison-predicate",
        section: "19.3",
        input: "MATCH (n) WHERE n.x = 1 RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.4-exists-predicate",
        section: "19.4",
        input: "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.5-null-predicate",
        section: "19.5",
        input: "MATCH (n) WHERE n.x IS NULL RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.6-value-type-predicate",
        section: "19.6",
        input: "MATCH (n) WHERE n.x IS TYPED INT64 RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.6-value-type-predicate-double-colon",
        section: "19.6",
        input: "MATCH (n) WHERE n.x IS :: INT64 RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.7-normalized-predicate",
        section: "19.7",
        input: "MATCH (n) WHERE n.x IS NFC NORMALIZED RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.8-directed-predicate",
        section: "19.8",
        input: "MATCH (n) WHERE n IS DIRECTED RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.9-labeled-predicate",
        section: "19.9",
        input: "MATCH (n) WHERE n IS LABELED Person RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.10-source-predicate",
        section: "19.10",
        input: "MATCH (n)-[e]->(m) WHERE n IS SOURCE OF e RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.11-all-different-predicate",
        section: "19.11",
        input: "MATCH (a), (b) WHERE ALL_DIFFERENT(a, b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.12-same-predicate",
        section: "19.12",
        input: "MATCH (a), (b) WHERE SAME(a, b) RETURN a",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s19.13-property-exists-predicate",
        section: "19.13",
        input: "MATCH (n) WHERE PROPERTY_EXISTS(n, x) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §20 Expressions ─────────────────────────────────────────────────────────────────────
    Case {
        id: "s20.1-arithmetic-expression",
        section: "20.1",
        input: "RETURN 1 + 2 * 3",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.1-graph-value-expression",
        section: "20.1",
        input: "RETURN GRAPH $g",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s20.1-table-value-expression",
        section: "20.1",
        input: "RETURN TABLE { MATCH (n) RETURN n }",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s20.7-simple-case",
        section: "20.7",
        input: "MATCH (n) RETURN CASE n.x WHEN 1 THEN 'a' ELSE 'b' END",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.7-simple-case-comparison-operand",
        section: "20.7",
        input: "MATCH (n) RETURN CASE n.x WHEN = 1 THEN 'a' END",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s20.7-coalesce",
        section: "20.7",
        input: "RETURN COALESCE(1, 2)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.8-cast",
        section: "20.8",
        input: "RETURN CAST(1 AS INT64)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.9-count-star",
        section: "20.9",
        input: "MATCH (n) RETURN COUNT(*)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.9-aggregate-all-quantifier",
        section: "20.9",
        input: "MATCH (n) RETURN COUNT(ALL n.x)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.9-percentile",
        section: "20.9",
        input: "MATCH (n) RETURN PERCENTILE_CONT(n.x, 0.5)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.16-string-concat",
        section: "20.16",
        input: "RETURN 'a' || 'b'",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.17-cast-length-bounded-string",
        section: "20.17",
        input: "RETURN CAST('a' AS STRING(1, 5))",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.22-list-index",
        section: "20.22",
        input: "RETURN [1, 2][0]",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Cypher,
    },
    Case {
        id: "s20.22-list-slice",
        section: "20.22",
        input: "RETURN [1, 2, 3][0..2]",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Cypher,
    },
    Case {
        id: "s20.23-session-user",
        section: "20.23",
        input: "RETURN SESSION_USER",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.25-date-literal",
        section: "20.25",
        input: "RETURN DATE '2024-01-01'",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.26-upper-function",
        section: "20.26",
        input: "RETURN UPPER('a')",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.28-abs-function",
        section: "20.28",
        input: "RETURN ABS(-1)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.10-element-id-function",
        section: "20.10",
        input: "MATCH (n) RETURN ELEMENT_ID(n)",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.14-path-constructor",
        section: "20.14",
        input: "MATCH (a)-[e]->(b) RETURN PATH [a, e, b]",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.17-list-constructor",
        section: "20.17",
        input: "RETURN [1, 2, 3]",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s20.18-record-constructor",
        section: "20.18",
        input: "RETURN { a: 1 }",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    // ── §21 Names, literals, tokens ─────────────────────────────────────────────────────────
    Case {
        id: "s21.2-single-quoted-string",
        section: "21.2",
        input: "RETURN 'abc'",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s21.2-double-quoted-string-literal",
        section: "21.2",
        input: "RETURN DATE \"2024-01-01\"",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    Case {
        id: "s21.2-byte-string-literal",
        section: "21.2",
        input: "RETURN X'00FF'",
        parse: Parse::Ok,
        format: Format::Unsupported("literal"),
        feature: Feature::Any,
    },
    Case {
        id: "s21.2-leading-dot-number",
        section: "21.2",
        input: "RETURN .5",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s21.2-underscore-number",
        section: "21.2",
        input: "RETURN 1_000",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s21.3-general-parameter",
        section: "21.3",
        input: "RETURN $x",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s21.3-digit-parameter-name",
        section: "21.3",
        input: "RETURN $1",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s21.3-quoted-identifier",
        section: "21.3",
        input: "MATCH (n:`a b`) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Any,
    },
    Case {
        id: "s21.4-non-ascii-whitespace",
        section: "21.4",
        input: "MATCH\u{a0}(n) RETURN n",
        parse: Parse::Err,
        format: Format::NotParsed,
        feature: Feature::Any,
    },
    // ── Gleaph dialect (gate-checked, documented in design/gql/extension-syntax.md) ───────────
    Case {
        id: "x-sqlcompat-in-list",
        section: "20.1",
        input: "MATCH (n) WHERE n.x IN (1, 2) RETURN n",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::SqlCompat,
    },
    Case {
        id: "x-gleaph-search",
        section: "16.4",
        input: "MATCH (d:Document) SEARCH d IN (VECTOR INDEX idx FOR $q LIMIT 10) SCORE AS s RETURN d, s",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Gleaph,
    },
    Case {
        id: "x-gleaph-grant",
        section: "12.8",
        input: "GRANT MATCH ON GRAPH g NODES Person TO PUBLIC",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Gleaph,
    },
    Case {
        id: "x-gleaph-grant-all-vertex-labels",
        section: "12.8",
        input: "GRANT MATCH ON GRAPH g NODES * TO PUBLIC",
        parse: Parse::Ok,
        format: Format::Ok,
        feature: Feature::Gleaph,
    },
];

/// Grammar sections the ledger must cover, so a missing family is a visible failure rather than an
/// omission nobody notices.
const REQUIRED_SECTIONS: &[&str] = &[
    "6", "7.1", "7.2", "7.3", "8.1", "8.3", "8.4", "9.1", "9.2", "10.1", "10.2", "11.3", "12.2",
    "12.3", "12.4", "12.5", "12.6", "12.7", "13.1", "13.2", "13.3", "13.4", "13.5", "14.2", "14.3",
    "14.4", "14.6", "14.7", "14.8", "14.9", "14.10", "14.11", "14.12", "15.2", "15.3", "16.2",
    "16.3", "16.4", "16.6", "16.7", "16.8", "16.11", "16.13", "16.17", "17.1", "17.2", "17.5",
    "18.1", "18.2", "18.3", "18.4", "18.6", "18.8", "18.9", "19.3", "19.4", "19.5", "19.6", "19.7",
    "19.8", "19.9", "19.10", "19.11", "19.12", "19.13", "20.1", "20.7", "20.8", "20.9", "20.10",
    "20.14", "20.17", "20.18", "20.23", "20.25", "20.26", "20.28", "21.2", "21.3", "21.4",
];

fn active_cases() -> impl Iterator<Item = &'static Case> {
    CASES.iter().filter(|c| c.feature.is_active())
}

#[test]
fn ledger_ids_are_unique() {
    let mut seen = std::collections::BTreeSet::new();
    for case in CASES {
        assert!(seen.insert(case.id), "duplicate ledger id: {}", case.id);
    }
}

#[test]
fn ledger_covers_every_required_section() {
    let covered: std::collections::BTreeSet<&str> = CASES.iter().map(|c| c.section).collect();
    let missing: Vec<&&str> = REQUIRED_SECTIONS
        .iter()
        .filter(|section| !covered.contains(**section))
        .collect();
    assert!(
        missing.is_empty(),
        "ledger has no case for grammar section(s): {missing:?}"
    );
}

#[test]
fn parse_axis_matches_the_ledger() {
    let mut failures = Vec::new();
    for case in active_cases() {
        let actual = match parser::parse(case.input) {
            Ok(_) => Parse::Ok,
            Err(_) => Parse::Err,
        };
        if actual != case.parse {
            failures.push(format!(
                "{} (§{}): expected {:?}, got {:?} for input {:?}",
                case.id, case.section, case.parse, actual, case.input
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "parse axis drifted from the ledger ({} case(s)):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[cfg(feature = "format")]
#[test]
fn format_axis_matches_the_ledger() {
    use gleaph_gql::{FormatError, FormatOptions, format_query};

    let options = FormatOptions::default();
    let mut failures = Vec::new();
    for case in active_cases() {
        if case.parse == Parse::Err {
            continue;
        }
        let actual = match format_query(case.input, &options) {
            Ok(formatted) => Ok(formatted),
            Err(FormatError::Unsupported(reason)) => Err(reason),
            Err(other) => {
                failures.push(format!(
                    "{}: formatter returned a non-Unsupported error: {other}",
                    case.id
                ));
                continue;
            }
        };
        let matches = match (case.format, &actual) {
            (Format::Ok, Ok(formatted)) => {
                // A formatter that accepts a program must be stable on its own output.
                match format_query(formatted, &options) {
                    Ok(again) if &again == formatted => true,
                    Ok(_) => {
                        failures.push(format!("{}: formatter output is not stable", case.id));
                        false
                    }
                    Err(other) => {
                        failures.push(format!(
                            "{}: formatter cannot re-parse its own output: {other}",
                            case.id
                        ));
                        false
                    }
                }
            }
            (Format::Unsupported(expected), Err(actual)) => expected == actual,
            _ => false,
        };
        if !matches {
            failures.push(format!(
                "{}: expected {:?}, got {:?} for input {:?}",
                case.id, case.format, actual, case.input
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "format axis drifted from the ledger ({} case(s)):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
