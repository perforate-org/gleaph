//! AST node definitions for the GQL parser (ISO/IEC 39075).
//!
//! This module defines all AST types covering the full GQL grammar, organized
//! by section. All types derive `Clone`, `Debug`, and `PartialEq`.

use crate::Value;
use crate::token::Span;
use crate::types::{EdgeDirection, LabelExpr};

// ════════════════════════════════════════════════════════════════════════════════
// §6 — Top-level program
// ════════════════════════════════════════════════════════════════════════════════

/// Top-level GQL program: an optional session activity followed by an optional
/// transaction activity.
#[derive(Clone, Debug, PartialEq)]
pub struct GqlProgram {
    pub span: Span,
    pub session_activity: Vec<SessionCommand>,
    pub transaction_activity: Option<TransactionActivity>,
}

/// A transaction activity contains an optional start-transaction command, a
/// statement block, and an optional commit/rollback.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionActivity {
    pub span: Span,
    pub start: Option<StartTransactionCommand>,
    pub body: Option<StatementBlock>,
    pub end: Option<TransactionEnd>,
}

/// A statement block: a primary statement optionally followed by NEXT-chained
/// statements (GQL `statementBlock`).
#[derive(Clone, Debug, PartialEq)]
pub struct StatementBlock {
    pub span: Span,
    pub first: Statement,
    pub next: Vec<NextStatement>,
}

impl StatementBlock {
    /// Iterate over all statements in the block (first + chained).
    pub fn iter_statements(&self) -> impl Iterator<Item = &Statement> {
        std::iter::once(&self.first).chain(self.next.iter().map(|n| &n.statement))
    }
}

/// A NEXT-chained statement with optional YIELD clause
/// (GQL `nextStatement`).
#[derive(Clone, Debug, PartialEq)]
pub struct NextStatement {
    pub span: Span,
    pub yield_items: Option<Vec<YieldItem>>,
    pub statement: Statement,
}

/// How to end a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionEnd {
    Commit,
    Rollback,
}

// ════════════════════════════════════════════════════════════════════════════════
// §7 — Session commands
// ════════════════════════════════════════════════════════════════════════════════

/// A session command (SET, RESET, or CLOSE).
#[derive(Clone, Debug, PartialEq)]
pub enum SessionCommand {
    Set(SessionSetCommand),
    Reset(SessionResetCommand),
    Close,
}

/// SESSION SET — set a session attribute.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionSetCommand {
    /// SESSION SET SCHEMA <catalog-qualified name>
    Schema(ObjectName),
    /// SESSION SET [PROPERTY] GRAPH <graph name>
    Graph {
        property_keyword: bool,
        name: ObjectName,
    },
    /// SESSION SET TIME ZONE <value>
    TimeZone(Box<Expr>),
    /// SESSION SET VALUE [IF NOT EXISTS] $name [TYPED|:: type] = <value>
    Parameter {
        if_not_exists: bool,
        name: String,
        typed_prefix: TypedPrefix,
        type_annotation: Option<BindingTypeAnnotation>,
        value: Box<Expr>,
    },
    /// SESSION SET [PROPERTY] GRAPH [IF NOT EXISTS] $name [TYPED|:: type] = <graph-expr>
    GraphParameter {
        property_keyword: bool,
        if_not_exists: bool,
        name: String,
        typed_prefix: TypedPrefix,
        type_annotation: Option<BindingTypeAnnotation>,
        value: Box<Expr>,
    },
    /// SESSION SET [BINDING] TABLE [IF NOT EXISTS] $name [TYPED|:: type] = <table-expr>
    BindingTableParameter {
        binding_keyword: bool,
        if_not_exists: bool,
        name: String,
        typed_prefix: TypedPrefix,
        type_annotation: Option<BindingTypeAnnotation>,
        value: Box<Expr>,
    },
}

/// SESSION RESET — reset a session attribute.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionResetCommand {
    /// SESSION RESET (no arguments — reset everything)
    All,
    /// RESET [ALL] PARAMETERS
    AllParameters {
        /// Whether `ALL` was explicitly specified.
        all_keyword: bool,
    },
    /// RESET [ALL] CHARACTERISTICS
    AllCharacteristics {
        /// Whether `ALL` was explicitly specified.
        all_keyword: bool,
    },
    /// RESET SCHEMA
    Schema,
    /// RESET [PROPERTY] GRAPH
    Graph { property_keyword: bool },
    /// RESET TIME ZONE
    TimeZone,
    /// RESET [PARAMETER] $name
    Parameter {
        parameter_keyword: bool,
        name: String,
    },
}

// ════════════════════════════════════════════════════════════════════════════════
// §8 — Transaction commands
// ════════════════════════════════════════════════════════════════════════════════

/// START TRANSACTION with optional transaction characteristics.
#[derive(Clone, Debug, PartialEq)]
pub struct StartTransactionCommand {
    pub span: Span,
    /// Transaction characteristics (may include multiple comma-separated modes).
    pub access_modes: Vec<TransactionAccessMode>,
}

/// Transaction access mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionAccessMode {
    ReadOnly,
    ReadWrite,
}

// ════════════════════════════════════════════════════════════════════════════════
// §9 — Statements (top-level enum)
// ════════════════════════════════════════════════════════════════════════════════

/// A fully-qualified (optionally schema-qualified) object name.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectName {
    pub parts: Vec<String>,
}

impl ObjectName {
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            parts: vec![name.into()],
        }
    }

    pub fn qualified(parts: Vec<String>) -> Self {
        Self { parts }
    }
}

/// All GQL statement types.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    // — DDL (§12) —
    CreateSchema(CreateSchemaStatement),
    DropSchema(DropSchemaStatement),
    CreateGraph(CreateGraphStatement),
    DropGraph(DropGraphStatement),
    CreateGraphType(CreateGraphTypeStatement),
    DropGraphType(DropGraphTypeStatement),

    // — DML (§13) —
    Insert(InsertStatement),
    Set(SetStatement),
    Remove(RemoveStatement),
    Delete(DeleteStatement),

    // — Query (§14) —
    /// A composite query expression, which also covers standalone CALL and
    /// inline procedure calls routed through the linear-query path.
    Query(Box<CompositeQueryExpr>),

    // — Session (§7) —
    Session(SessionCommand),
}

// ════════════════════════════════════════════════════════════════════════════════
// §12 — DDL statements
// ════════════════════════════════════════════════════════════════════════════════

/// CREATE SCHEMA [IF NOT EXISTS] <name>
#[derive(Clone, Debug, PartialEq)]
pub struct CreateSchemaStatement {
    pub span: Span,
    pub if_not_exists: bool,
    pub name: ObjectName,
}

/// DROP SCHEMA [IF EXISTS] <name>
#[derive(Clone, Debug, PartialEq)]
pub struct DropSchemaStatement {
    pub span: Span,
    pub if_exists: bool,
    pub name: ObjectName,
}

/// CREATE [PROPERTY] GRAPH [IF NOT EXISTS] [OR REPLACE] <name> ...
#[derive(Clone, Debug, PartialEq)]
pub struct CreateGraphStatement {
    pub span: Span,
    /// Whether the `PROPERTY` keyword was present (syntactic only — semantically equivalent).
    pub property_keyword: bool,
    pub or_replace: bool,
    pub if_not_exists: bool,
    pub name: ObjectName,
    /// The graph type, either inline or by reference.
    pub graph_type: Option<GraphTypeSpec>,
    /// AS COPY OF <source>
    pub copy_of: Option<ObjectName>,
}

/// Specifies a graph type — either inline or by referencing a named type.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphTypeSpec {
    /// ANY [PROPERTY] [GRAPH] — open graph type (accepts any schema).
    Any {
        property_keyword: bool,
        graph_keyword: bool,
    },
    /// LIKE <graph-expression> — type derived from another graph.
    Like(ObjectName),
    /// TYPED <graph-type-name> / :: <graph-type-name>
    Typed {
        name: ObjectName,
        typed_keyword: bool,
    },
    /// Inline definition { ... }
    Inline(GraphTypeDefinition),
}

/// DROP [PROPERTY] GRAPH [IF EXISTS] <name>
#[derive(Clone, Debug, PartialEq)]
pub struct DropGraphStatement {
    pub span: Span,
    pub property_keyword: bool,
    pub if_exists: bool,
    pub name: ObjectName,
}

/// CREATE [PROPERTY] GRAPH TYPE [IF NOT EXISTS] [OR REPLACE] <name> AS <definition>
#[derive(Clone, Debug, PartialEq)]
pub struct CreateGraphTypeStatement {
    pub span: Span,
    pub property_keyword: bool,
    pub or_replace: bool,
    pub if_not_exists: bool,
    pub name: ObjectName,
    pub definition: GraphTypeDefinition,
    /// Whether `AS` keyword was present before `COPY OF`.
    pub as_keyword: bool,
    /// [AS] COPY OF <source-type>
    pub copy_of: Option<ObjectName>,
}

/// DROP [PROPERTY] GRAPH TYPE [IF EXISTS] <name>
#[derive(Clone, Debug, PartialEq)]
pub struct DropGraphTypeStatement {
    pub span: Span,
    pub property_keyword: bool,
    pub if_exists: bool,
    pub name: ObjectName,
}

// ════════════════════════════════════════════════════════════════════════════════
// §13 — Data modification statements
// ════════════════════════════════════════════════════════════════════════════════

/// INSERT <insert-graph-pattern>
#[derive(Clone, Debug, PartialEq)]
pub struct InsertStatement {
    pub span: Span,
    pub graph_name: Option<ObjectName>,
    pub patterns: Vec<InsertPathPattern>,
}

/// An insert path pattern — a sequence of alternating node and edge patterns.
#[derive(Clone, Debug, PartialEq)]
pub struct InsertPathPattern {
    pub span: Span,
    pub elements: Vec<InsertElement>,
}

/// An element within an insert pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum InsertElement {
    Node(InsertNodePattern),
    Edge(InsertEdgePattern),
}

/// A node in an INSERT pattern: (var :Label {props})
#[derive(Clone, Debug, PartialEq)]
pub struct InsertNodePattern {
    pub span: Span,
    pub variable: Option<String>,
    pub is_or_colon: Option<IsOrColon>,
    pub labels: Vec<String>,
    pub properties: Vec<PropertySetting>,
}

/// An edge in an INSERT pattern with direction and label.
#[derive(Clone, Debug, PartialEq)]
pub struct InsertEdgePattern {
    pub span: Span,
    pub direction: EdgeDirection,
    pub variable: Option<String>,
    pub is_or_colon: Option<IsOrColon>,
    pub labels: Vec<String>,
    pub properties: Vec<PropertySetting>,
}

/// A property assignment: key = value.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertySetting {
    pub span: Span,
    pub name: String,
    pub value: Expr,
}

/// SET statement with a list of set items.
#[derive(Clone, Debug, PartialEq)]
pub struct SetStatement {
    pub span: Span,
    pub items: Vec<SetItem>,
}

/// A single SET clause item.
#[derive(Clone, Debug, PartialEq)]
pub enum SetItem {
    /// SET v.prop = expr
    Property {
        span: Span,
        variable: String,
        property: String,
        value: Expr,
    },
    /// SET v = expr  (replace all properties)
    AllProperties {
        span: Span,
        variable: String,
        value: Expr,
    },
    /// SET v :Label or SET v IS Label
    Label {
        span: Span,
        variable: String,
        label: String,
        is_or_colon: IsOrColon,
    },
}

/// REMOVE statement with a list of items.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoveStatement {
    pub span: Span,
    pub items: Vec<RemoveItem>,
}

/// A single REMOVE clause item.
#[derive(Clone, Debug, PartialEq)]
pub enum RemoveItem {
    /// REMOVE v.prop
    Property {
        span: Span,
        variable: String,
        property: String,
    },
    /// REMOVE v :Label or REMOVE v IS Label
    Label {
        span: Span,
        variable: String,
        label: String,
        is_or_colon: IsOrColon,
    },
}

/// DELETE [DETACH | NODETACH] <variable-list>
#[derive(Clone, Debug, PartialEq)]
pub struct DeleteStatement {
    pub span: Span,
    pub detach: DeleteDetach,
    /// Delete items — each is a value expression (typically a variable
    /// reference, but GQL §13.5 allows any `valueExpression`).
    pub items: Vec<Expr>,
}

/// Whether a DELETE is DETACH, NODETACH, or unspecified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteDetach {
    Detach,
    NoDetach,
    Unspecified,
}

// ════════════════════════════════════════════════════════════════════════════════
// §14 — Query statements
// ════════════════════════════════════════════════════════════════════════════════

/// A composite query expression: a linear query optionally combined with
/// UNION / EXCEPT / INTERSECT / OTHERWISE operators.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositeQueryExpr {
    pub span: Span,
    pub left: LinearQueryStatement,
    pub rest: Vec<(SetOp, LinearQueryStatement)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProcedureBindingKind {
    Graph,
    Table,
    Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProcedureBindingInitializer {
    Object(ObjectName),
    Expr(Expr),
    Query(Box<CompositeQueryExpr>),
}

/// How a type annotation was introduced syntactically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypedPrefix {
    /// `::` (double colon)
    DoubleColon,
    /// `TYPED` keyword
    Typed,
    /// Neither — the type appeared without an explicit prefix.
    None,
}

/// How a label was introduced syntactically (GQL `isOrColon`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsOrColon {
    /// `IS` keyword
    Is,
    /// `:` colon
    Colon,
}

/// Whether `PATH` (singular) or `PATHS` (plural) keyword was used (GQL `pathOrPaths`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathOrPaths {
    Path,
    Paths,
}

/// Whether `GROUP` (singular) or `GROUPS` (plural) keyword was used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupOrGroups {
    Group,
    Groups,
}

/// Original source keyword(s), preserved for formatting / round-tripping.
///
/// Always compares equal so that semantic `PartialEq` on the containing type
/// is unaffected by keyword spelling differences.
#[derive(Clone, Eq)]
pub struct Keyword(pub String);

impl Keyword {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl PartialEq for Keyword {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for Keyword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keyword({:?})", self.0)
    }
}

/// Type annotation for a binding variable definition (GQL
/// `optTypedGraphInitializer` / `optTypedBindingTableInitializer` /
/// `optTypedValueInitializer`).
///
/// Syntax: `[TYPED | ::] <type>`.
#[derive(Clone, Debug, PartialEq)]
pub enum BindingTypeAnnotation {
    /// `ANY [PROPERTY] [GRAPH] [NOT NULL]`
    AnyGraph {
        property_keyword: bool,
        graph_keyword: bool,
        not_null: bool,
    },
    /// `[PROPERTY] [GRAPH] <nestedGraphTypeSpecification> [NOT NULL]`
    ClosedGraph {
        property_keyword: bool,
        graph_keyword: bool,
        graph_type: ObjectName,
        not_null: bool,
    },
    /// `[BINDING] [TABLE] <fieldTypesSpecification> [NOT NULL]`
    BindingTable {
        binding_keyword: bool,
        table_keyword: bool,
        not_null: bool,
    },
    /// A value type such as `INT32`, `STRING`, etc.
    Value(ValueType),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcedureBindingDefinition {
    pub span: Span,
    pub kind: ProcedureBindingKind,
    pub variable: String,
    /// How the type annotation was introduced (`::`, `TYPED`, or none).
    pub typed_prefix: TypedPrefix,
    /// Optional type annotation between the variable name and the `=`.
    pub type_annotation: Option<BindingTypeAnnotation>,
    pub initializer: ProcedureBindingInitializer,
}

/// Set operation combining two query expressions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    /// UNION (bare — no quantifier)
    Union,
    /// UNION ALL
    UnionAll,
    /// UNION DISTINCT (explicit)
    UnionDistinct,
    /// EXCEPT (bare)
    Except,
    /// EXCEPT ALL
    ExceptAll,
    /// EXCEPT DISTINCT (explicit)
    ExceptDistinct,
    /// INTERSECT (bare)
    Intersect,
    /// INTERSECT ALL
    IntersectAll,
    /// INTERSECT DISTINCT (explicit)
    IntersectDistinct,
    /// OTHERWISE
    Otherwise,
}

/// A schema reference in an AT clause (GQL `schemaReference`).
#[derive(Clone, Debug, PartialEq)]
pub enum SchemaReference {
    /// HOME_SCHEMA or CURRENT_SCHEMA keyword.
    Current(String),
    /// Absolute catalog path: `/`, `/foo`, `/foo/bar`.
    Absolute(Vec<String>),
    /// Relative catalog path: `../foo/bar`.
    Relative(Vec<String>),
    /// Substituted parameter: `$$name`.
    Parameter(String),
}

/// A linear query: a sequence of simple query statements ending in a
/// result statement (RETURN/SELECT) or a nested query.
#[derive(Clone, Debug, PartialEq)]
pub struct LinearQueryStatement {
    pub span: Span,
    /// Optional AT schema clause (GQL `atSchemaClause`).
    pub at_schema: Option<SchemaReference>,
    /// Procedure-body prefix bindings declared before the first query clause.
    pub prefix_bindings: Vec<ProcedureBindingDefinition>,
    /// The sequence of simple query parts (MATCH, FILTER, LET, FOR, etc.).
    pub parts: Vec<SimpleQueryStatement>,
    /// The final result statement (RETURN or SELECT), if any.
    pub result: Option<ResultStatement>,
}

/// A simple (non-composite) query statement.
#[derive(Clone, Debug, PartialEq)]
pub enum SimpleQueryStatement {
    Match(MatchStatement),
    Filter(FilterStatement),
    Let(LetStatement),
    For(ForStatement),
    OrderBy(OrderByClause),
    Limit(LimitClause),
    Offset(OffsetClause),
    CallProcedure(CallProcedureStatement),
    InlineProcedureCall(InlineProcedureCall),
    /// A focused statement: `USE GRAPH <name>` scoping a subsequent statement.
    ///
    /// Models the GQL rules `focusedLinearQueryStatementPart`,
    /// `focusedLinearDataModifyingStatement`, and
    /// `focusedLinearDataModifyingStatementBody`.
    ///
    /// When `body` is `None`, the USE clause scopes the result statement
    /// that follows (e.g. `USE myGraph RETURN 1` — `focusedPrimitiveResultStatement`).
    Focused {
        graph: ObjectName,
        body: Option<Box<SimpleQueryStatement>>,
    },
    /// Inline data modification (INSERT, SET, REMOVE, DELETE) as a query step.
    Insert(InsertStatement),
    Set(SetStatement),
    Remove(RemoveStatement),
    Delete(DeleteStatement),
}

/// The result statement of a linear query.
#[derive(Clone, Debug, PartialEq)]
pub enum ResultStatement {
    Return(Box<ReturnStatement>),
    Select(Box<SelectStatement>),
    /// FINISH — terminates a linear query with no result bindings.
    Finish,
}

// ──── MATCH (§14.1) ────

/// MATCH [OPTIONAL] <graph_pattern> [ON <graph_name>]
#[derive(Clone, Debug, PartialEq)]
pub struct MatchStatement {
    pub span: Span,
    pub optional: bool,
    pub graph_name: Option<ObjectName>,
    pub pattern: GraphPattern,
    pub yield_items: Option<Vec<YieldItem>>,
}

// ──── FILTER (§14.2) ────

/// FILTER [WHERE] <condition>
#[derive(Clone, Debug, PartialEq)]
pub struct FilterStatement {
    pub span: Span,
    pub where_keyword: bool,
    pub condition: Expr,
}

// ──── LET (§14.3) ────

/// LET <bindings>
#[derive(Clone, Debug, PartialEq)]
pub struct LetStatement {
    pub span: Span,
    pub bindings: Vec<LetBinding>,
}

/// A single LET binding: variable = expression.
#[derive(Clone, Debug, PartialEq)]
pub struct LetBinding {
    pub span: Span,
    pub variable: String,
    pub value: Expr,
}

// ──── FOR (§14.4) ────

/// FOR <variable> IN <list-expression> [WITH ORDINALITY <ordinal-var>]
#[derive(Clone, Debug, PartialEq)]
pub struct ForStatement {
    pub span: Span,
    pub variable: String,
    pub list: Expr,
    /// `WITH ORDINALITY <var>` or `WITH OFFSET <var>`.
    pub ordinality: Option<ForOrdinality>,
}

/// Ordinality clause of a FOR statement.
#[derive(Clone, Debug, PartialEq)]
pub struct ForOrdinality {
    pub span: Span,
    /// Whether `OFFSET` was used instead of `ORDINALITY`.
    pub offset_keyword: bool,
    pub variable: String,
}

// ──── RETURN (§14.5) ────

/// RETURN statement.
#[derive(Clone, Debug, PartialEq)]
pub struct ReturnStatement {
    pub span: Span,
    pub set_quantifier: SetQuantifier,
    pub body: ReturnBody,
}

/// SELECT statement.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectStatement {
    pub span: Span,
    pub set_quantifier: SetQuantifier,
    pub source: Option<SelectSource>,
    pub body: SelectBody,
}

/// Set quantifier: DISTINCT, ALL, or none (GQL `setQuantifier`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetQuantifier {
    /// No keyword specified.
    None,
    /// `ALL` explicitly specified.
    All,
    /// `DISTINCT` explicitly specified.
    Distinct,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelectSource {
    GraphMatchList(Vec<SelectGraphMatch>),
    QuerySpecification(SelectQuerySpecification),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectGraphMatch {
    pub graph: ObjectName,
    pub match_statement: MatchStatement,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelectQuerySpecification {
    Nested(Box<CompositeQueryExpr>),
    GraphNested {
        graph: ObjectName,
        query: Box<CompositeQueryExpr>,
    },
}

/// The body of a RETURN statement.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ReturnBody {
    /// RETURN * — return all bindings.
    Star,
    /// RETURN NO BINDINGS — explicit empty (cypher extension, not in GQL).
    #[cfg(feature = "cypher")]
    NoBindings,
    /// RETURN <items>
    Items {
        items: Vec<ReturnItem>,
        group_by: Option<GroupByClause>,
        having: Option<Expr>,
        order_by: Option<OrderByClause>,
        limit: Option<LimitClause>,
        offset: Option<OffsetClause>,
    },
}

/// The body of a SELECT statement.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectBody {
    Star {
        group_by: Option<GroupByClause>,
        having: Option<Expr>,
        order_by: Option<OrderByClause>,
        limit: Option<LimitClause>,
        offset: Option<OffsetClause>,
    },
    Items {
        items: Vec<ReturnItem>,
        group_by: Option<GroupByClause>,
        having: Option<Expr>,
        order_by: Option<OrderByClause>,
        limit: Option<LimitClause>,
        offset: Option<OffsetClause>,
    },
}

/// A single return item: expression [AS alias].
#[derive(Clone, Debug, PartialEq)]
pub struct ReturnItem {
    pub span: Span,
    pub expr: Expr,
    pub alias: Option<String>,
}

// ──── ORDER BY / LIMIT / OFFSET / GROUP BY ────

/// ORDER BY <sort-items>
#[derive(Clone, Debug, PartialEq)]
pub struct OrderByClause {
    pub span: Span,
    pub items: Vec<SortItem>,
}

/// A single sort specification.
#[derive(Clone, Debug, PartialEq)]
pub struct SortItem {
    pub span: Span,
    pub expr: Expr,
    pub direction: Option<SortDirection>,
    pub null_order: Option<NullOrder>,
}

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDirection {
    /// `ASC`
    Asc,
    /// `ASCENDING`
    Ascending,
    /// `DESC`
    Desc,
    /// `DESCENDING`
    Descending,
}

/// Position of NULLs in ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NullOrder {
    First,
    Last,
}

/// LIMIT <count>
#[derive(Clone, Debug, PartialEq)]
pub struct LimitClause {
    pub span: Span,
    pub count: Expr,
}

/// OFFSET <count> or SKIP <count>
#[derive(Clone, Debug, PartialEq)]
pub struct OffsetClause {
    pub span: Span,
    /// Whether `SKIP` was used instead of `OFFSET`.
    pub skip_keyword: bool,
    pub count: Expr,
}

/// GROUP BY <items>
#[derive(Clone, Debug, PartialEq)]
pub struct GroupByClause {
    pub span: Span,
    pub items: Vec<Expr>,
}

// ════════════════════════════════════════════════════════════════════════════════
// §15 — Call procedure
// ════════════════════════════════════════════════════════════════════════════════

/// CALL <procedure-name> ( <args> ) [YIELD <items>]
/// or CALL { <inline-procedure-body> }
#[derive(Clone, Debug, PartialEq)]
pub struct CallProcedureStatement {
    pub span: Span,
    pub optional: bool,
    pub name: ObjectName,
    pub args: Vec<Expr>,
    pub yield_items: Option<Vec<YieldItem>>,
}

/// Inline procedure call: [OPTIONAL] CALL { <statements> }
///
/// When `use_graph` is `Some`, this models the GQL rule
/// `focusedNestedDataModifyingProcedureSpecification`:
/// `USE GRAPH <name> { <body> }`.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineProcedureCall {
    pub span: Span,
    pub optional: bool,
    pub use_graph: Option<ObjectName>,
    pub scope_vars: Vec<String>,
    pub body: Box<CompositeQueryExpr>,
}

/// A YIELD item: identifier [AS alias].
#[derive(Clone, Debug, PartialEq)]
pub struct YieldItem {
    pub span: Span,
    pub name: String,
    pub alias: Option<String>,
}

// ════════════════════════════════════════════════════════════════════════════════
// §16 — Graph pattern
// ════════════════════════════════════════════════════════════════════════════════

/// A graph pattern: [match-mode] <path-patterns> [KEEP <clause>] [WHERE <cond>]
#[derive(Clone, Debug, PartialEq)]
pub struct GraphPattern {
    pub span: Span,
    pub match_mode: Option<MatchMode>,
    pub paths: Vec<PathPattern>,
    pub keep: Option<KeepClause>,
    pub where_clause: Option<Expr>,
}

/// Match mode for a graph pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchMode {
    RepeatableElements {
        /// The exact keyword form used.
        keyword: MatchModeElementKeyword,
    },
    DifferentEdges {
        /// The exact keyword form used.
        keyword: MatchModeEdgeKeyword,
    },
}

/// How `REPEATABLE ELEMENT(S)` was spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchModeElementKeyword {
    /// `ELEMENT`
    Element,
    /// `ELEMENT BINDINGS`
    ElementBindings,
    /// `ELEMENTS`
    Elements,
}

/// How `DIFFERENT EDGE(S)/RELATIONSHIP(S)` was spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchModeEdgeKeyword {
    /// `EDGE`
    Edge,
    /// `EDGE BINDINGS`
    EdgeBindings,
    /// `EDGES`
    Edges,
    /// `RELATIONSHIP`
    Relationship,
    /// `RELATIONSHIP BINDINGS`
    RelationshipBindings,
    /// `RELATIONSHIPS`
    Relationships,
}

/// KEEP clause (GQL §16.4: `keepClause: KEEP pathPatternPrefix`).
///
/// Preserves a path pattern prefix for filtering. Per the spec, KEEP takes
/// a path pattern prefix (a path mode or search prefix), not a variable list.
#[derive(Clone, Debug, PartialEq)]
pub struct KeepClause {
    pub span: Span,
    pub prefix: PathPatternPrefix,
}

// ──── Path pattern (§16.3) ────

/// A path pattern: [<var> =] [<prefix>] <path-expr>
#[derive(Clone, Debug, PartialEq)]
pub struct PathPattern {
    pub span: Span,
    /// Optional path variable assigned with `=`.
    pub variable: Option<String>,
    /// Optional path prefix (mode or search).
    pub prefix: Option<PathPatternPrefix>,
    /// The path expression itself.
    pub expr: PathPatternExpr,
}

/// A path pattern prefix — either a path mode or a search prefix.
#[derive(Clone, Debug, PartialEq)]
pub enum PathPatternPrefix {
    Mode {
        mode: PathMode,
        path_keyword: Option<PathOrPaths>,
    },
    Search(SearchPrefix),
}

/// Path traversal mode (§16.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMode {
    Walk,
    Trail,
    Simple,
    Acyclic,
}

/// Search prefix for path patterns.
#[derive(Clone, Debug, PartialEq)]
pub enum SearchPrefix {
    /// ALL — all paths.
    All {
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
    },
    /// ANY [k] — any single path, or any k paths.
    Any {
        k: Option<u64>,
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
    },
    /// ALL SHORTEST — all shortest paths.
    AllShortest {
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
    },
    /// ANY SHORTEST — any one shortest path.
    AnyShortest {
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
    },
    /// SHORTEST <k> — up to k shortest paths.
    ShortestK {
        k: u64,
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
    },
    /// SHORTEST <k> GROUP — shortest paths grouped.
    ShortestKGroup {
        k: u64,
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
        group_keyword: GroupOrGroups,
    },
    /// COUNT PATHS — return the count of matching paths (cypher extension).
    #[cfg(feature = "cypher")]
    CountPaths {
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
    },
}

/// A path pattern expression (§16.5).
#[derive(Clone, Debug, PartialEq)]
pub enum PathPatternExpr {
    /// A single path term.
    Term(PathTerm),
    /// Multiset alternation: `p1 | p2`.
    MultisetAlternation(Vec<PathTerm>),
    /// Pattern union: `p1 |+| p2`.
    PatternUnion(Vec<PathTerm>),
}

/// A path term is a sequence of path factors.
#[derive(Clone, Debug, PartialEq)]
pub struct PathTerm {
    pub span: Span,
    pub factors: Vec<PathFactor>,
}

/// A path factor is a path primary with an optional quantifier.
#[derive(Clone, Debug, PartialEq)]
pub struct PathFactor {
    pub span: Span,
    pub primary: PathPrimary,
    pub quantifier: Option<PathQuantifier>,
}

/// A path primary — the atomic building block of a path pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum PathPrimary {
    /// A node pattern: `(var :Label {props} WHERE cond)`
    Node(NodePattern),
    /// An edge pattern: `-[var :Label {props} WHERE cond]->`
    Edge(EdgePattern),
    /// Parenthesized sub-path with optional variable, mode, and where.
    Parenthesized {
        variable: Option<String>,
        mode: Option<PathMode>,
        path_keyword: Option<PathOrPaths>,
        expr: Box<PathPatternExpr>,
        where_clause: Option<Box<Expr>>,
    },
    /// A simplified path pattern (e.g., `->`, `-[:KNOWS]->`, etc.).
    Simplified(SimplifiedPathPattern),
}

/// Path quantifier (§16.7).
#[derive(Clone, Debug, PartialEq)]
pub enum PathQuantifier {
    /// `*` — zero or more (equivalent to `{0,}`)
    Star,
    /// `+` — one or more (equivalent to `{1,}`)
    Plus,
    /// `?` — zero or one (equivalent to `{0,1}`)
    Optional,
    /// `{n}` — exactly n
    Fixed(u64),
    /// `{n,m}` — between n and m
    Range { lower: u64, upper: Option<u64> },
}

// ──── Node pattern (§16.8) ────

/// A node (vertex) pattern: `(var :Label {props} WHERE cond)`
#[derive(Clone, Debug, PartialEq)]
pub struct NodePattern {
    pub span: Span,
    pub variable: Option<String>,
    pub is_or_colon: Option<IsOrColon>,
    pub label: Option<LabelExpr>,
    pub properties: Vec<PropertySetting>,
    pub where_clause: Option<Box<Expr>>,
}

// ──── Edge pattern (§16.9) ────

/// An edge (relationship) pattern.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgePattern {
    pub span: Span,
    pub direction: EdgeDirection,
    pub variable: Option<String>,
    pub is_or_colon: Option<IsOrColon>,
    pub label: Option<LabelExpr>,
    pub properties: Vec<PropertySetting>,
    pub where_clause: Option<Box<Expr>>,
}

// ──── Simplified path pattern (§16.10) ────

/// A simplified path pattern using label expressions with directions.
#[derive(Clone, Debug, PartialEq)]
pub struct SimplifiedPathPattern {
    pub span: Span,
    pub elements: Vec<SimplifiedElement>,
}

/// An element in a simplified path pattern.
///
/// Represents one `opening_slash contents closing_slash` unit.
/// The `direction` comes from the slash pair; contents holds the full
/// simplified expression tree (which may itself contain quantifiers,
/// conjunction, union, etc.).
#[derive(Clone, Debug, PartialEq)]
pub struct SimplifiedElement {
    pub span: Span,
    pub direction: EdgeDirection,
    pub contents: SimplifiedContents,
}

/// The contents of a simplified path element (§16.10).
///
/// Models the full GQL `simplifiedContents` hierarchy:
///   simplifiedContents → union / multisetAlt of terms
///   simplifiedTerm → concatenation of factorLows
///   simplifiedFactorLow → conjunction (&) of factorHighs
///   simplifiedFactorHigh → tertiary with optional quantifier
///   simplifiedTertiary → direction override on secondary
///   simplifiedSecondary → optional negation on primary
///   simplifiedPrimary → labelName | (simplifiedContents)
#[derive(Clone, Debug, PartialEq)]
pub enum SimplifiedContents {
    /// A single label name or wildcard (`%`).
    Label(LabelExpr),
    /// Negation: `!primary`
    Negation(Box<SimplifiedContents>),
    /// Conjunction: `a & b`
    Conjunction(Box<SimplifiedContents>, Box<SimplifiedContents>),
    /// Union: `a | b`
    Union(Box<SimplifiedContents>, Box<SimplifiedContents>),
    /// Multiset alternation: `a |+| b`
    MultisetAlternation(Box<SimplifiedContents>, Box<SimplifiedContents>),
    /// Concatenation (juxtaposition of terms): `a b`
    Concatenation(Box<SimplifiedContents>, Box<SimplifiedContents>),
    /// Quantified: `a+`, `a{2,5}`, `a?`
    Quantified(Box<SimplifiedContents>, PathQuantifier),
    /// Direction override on a sub-expression (e.g., `<KNOWS`, `~LIKES`).
    DirectionOverride(EdgeDirection, Box<SimplifiedContents>),
    /// Parenthesized group: `(simplifiedContents)`
    Group(Box<SimplifiedContents>),
}

// ════════════════════════════════════════════════════════════════════════════════
// §18 — Graph type definitions
// ════════════════════════════════════════════════════════════════════════════════

/// A graph type definition containing node and edge type definitions.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphTypeDefinition {
    pub span: Span,
    pub elements: Vec<GraphTypeElement>,
}

/// An element of a graph type definition.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphTypeElement {
    Node(NodeTypeDef),
    Edge(EdgeTypeDef),
}

/// A key label set for node or edge type definitions (GQL
/// `nodeTypeKeyLabelSet` / `edgeTypeKeyLabelSet`).
///
/// Represents the `LABEL(S) label1 & label2 & ...` phrase within a graph
/// type element definition.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyLabelSet {
    pub span: Span,
    /// Whether `LABEL` (false) or `LABELS` (true) keyword was used.
    pub label_keyword_plural: bool,
    pub labels: Vec<String>,
}

/// A node type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTypeDef {
    pub span: Span,
    /// Which keyword was used: NODE or VERTEX (GQL `nodeSynonym`).
    pub keyword: Keyword,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub label_set: Option<KeyLabelSet>,
    pub properties: Vec<PropertyDef>,
}

/// An edge type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTypeDef {
    pub span: Span,
    /// Which keyword was used: EDGE or RELATIONSHIP (GQL `edgeSynonym`).
    pub keyword: Keyword,
    pub name: Option<String>,
    pub direction: EdgeDirection,
    pub source: EdgeEndpoint,
    pub destination: EdgeEndpoint,
    pub label_set: Option<KeyLabelSet>,
    pub properties: Vec<PropertyDef>,
}

/// An endpoint reference in an edge type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeEndpoint {
    pub span: Span,
    /// The node type name or label this endpoint connects to.
    pub label: Option<String>,
    /// The node type reference name.
    pub type_name: Option<String>,
}

/// A property definition within a node or edge type.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDef {
    pub span: Span,
    pub name: String,
    pub value_type: ValueType,
    pub not_null: bool,
    pub default_value: Option<Expr>,
}

// ──── Value types (§18.9) ────

/// GQL value type covering all types defined in GQL §18.9.
///
/// This enum represents type declarations, not runtime values. It is used
/// in DDL (property definitions, CAST targets, etc.).
///
/// Variants with keyword synonyms carry a [`Keyword`] field that preserves
/// the original source spelling for formatting / round-tripping.  `Keyword`
/// always compares equal, so semantic `PartialEq` is unaffected.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueType {
    // — Boolean —
    /// BOOL / BOOLEAN
    Bool { keyword: Keyword },

    // — Character string types —
    /// STRING [( max_length )]
    String {
        min_length: Option<u64>,
        max_length: Option<u64>,
    },
    /// CHAR [( length )] / CHARACTER [( length )]
    Char {
        keyword: Keyword,
        length: Option<u64>,
    },
    /// VARCHAR [( max_length )] / CHARACTER VARYING [( max_length )]
    Varchar {
        keyword: Keyword,
        max_length: Option<u64>,
    },

    // — Byte string types —
    /// BYTES [( max_length )]
    Bytes { max_length: Option<u64> },
    /// BINARY [( length )]
    Binary { length: Option<u64> },
    /// VARBINARY [( max_length )] / BINARY VARYING [( max_length )]
    Varbinary {
        keyword: Keyword,
        max_length: Option<u64>,
    },

    // — Exact numeric types (signed binary) —
    /// INT8 / INTEGER8 / TINYINT / SIGNED INTEGER8 / ...
    Int8 { keyword: Keyword },
    /// INT16 / INTEGER16 / SMALLINT / SMALL INTEGER / SIGNED ... / ...
    Int16 { keyword: Keyword },
    /// INT32 / INT / INTEGER / INT32 / INTEGER32 / SIGNED ... / ...
    Int32 { keyword: Keyword },
    /// INT64 / INTEGER64 / BIGINT / BIG INTEGER / SIGNED ... / ...
    Int64 { keyword: Keyword },
    /// INT(precision) / INTEGER(precision) / SIGNED INT(p) / ...
    IntPrecision { keyword: Keyword, precision: u64 },
    /// INT128 / INTEGER128 / SIGNED INTEGER128 / ...
    Int128 { keyword: Keyword },
    /// INT256 / INTEGER256 / SIGNED INTEGER256 / ...
    Int256 { keyword: Keyword },

    // — Exact numeric types (unsigned binary) —
    /// UINT8 / UNSIGNED INTEGER8 / ...
    Uint8 { keyword: Keyword },
    /// UINT16 / USMALLINT / UNSIGNED SMALLINT / ...
    Uint16 { keyword: Keyword },
    /// UINT32 / UINT / UNSIGNED INT / UNSIGNED INTEGER / ...
    Uint32 { keyword: Keyword },
    /// UINT64 / UBIGINT / UNSIGNED BIGINT / ...
    Uint64 { keyword: Keyword },
    /// UINT(precision) / UNSIGNED INT(p) / UNSIGNED INTEGER(p) / ...
    UintPrecision { keyword: Keyword, precision: u64 },
    /// UINT128 / UNSIGNED INTEGER128 / ...
    Uint128 { keyword: Keyword },
    /// UINT256 / UNSIGNED INTEGER256 / ...
    Uint256 { keyword: Keyword },

    // — Approximate numeric types —
    /// FLOAT16 / HALF
    Float16 { keyword: Keyword },
    /// FLOAT32 / FLOAT / REAL
    Float32 { keyword: Keyword },
    /// FLOAT64 / DOUBLE [PRECISION]
    Float64 { keyword: Keyword },
    /// FLOAT128
    Float128,
    /// FLOAT256
    Float256,
    /// FLOAT( precision [, scale ] )
    FloatPrecision { precision: u64, scale: Option<u64> },

    // — Decimal types —
    /// DECIMAL [( precision [, scale ] )] / DEC / NUMERIC
    Decimal {
        keyword: Keyword,
        precision: Option<u64>,
        scale: Option<u64>,
    },

    // — Date/Time types —
    /// DATE
    Date,
    /// TIME — UTC time without timezone info
    Time,
    /// LOCAL TIME / TIME WITHOUT TIME ZONE / LOCAL_TIME
    LocalTime { keyword: Keyword },
    /// ZONED TIME / TIME WITH TIME ZONE / ZONED_TIME
    ZonedTime { keyword: Keyword },
    /// DATETIME
    DateTime,
    /// LOCAL DATETIME / LOCAL TIMESTAMP / TIMESTAMP WITHOUT TIME ZONE /
    /// LOCAL_DATETIME / LOCAL_TIMESTAMP
    LocalDateTime { keyword: Keyword },
    /// ZONED DATETIME / TIMESTAMP WITH TIME ZONE / ZONED_DATETIME
    ZonedDateTime { keyword: Keyword },
    /// bare TIMESTAMP (no WITH/WITHOUT qualifier; those go through
    /// ZonedDateTime / LocalDateTime respectively)
    Timestamp,

    // — Duration types —
    /// DURATION — general duration
    Duration,
    /// DURATION YEAR TO MONTH
    DurationYearToMonth,
    /// DURATION DAY TO SECOND
    DurationDayToSecond,

    // — Constructed types —
    /// LIST / ARRAY ( element_type ) [( max_length )]
    List {
        keyword: Keyword,
        element_type: Box<ValueType>,
        max_length: Option<u64>,
    },
    /// PATH
    Path,
    /// [RECORD] { fields... }
    Record {
        record_keyword: bool,
        fields: Vec<RecordFieldType>,
    },

    // — Dynamic union types —
    /// ANY
    Any,
    /// ANY VALUE
    AnyValue,
    /// ANY PROPERTY VALUE
    AnyPropertyValue,
    /// NOTHING (the empty type — no value inhabits it)
    Nothing,
    /// NULL type
    Null,

    // — Reference types —
    /// GRAPH / PROPERTY GRAPH / ANY [PROPERTY] GRAPH
    GraphRef { keyword: Keyword },
    /// NODE / VERTEX / ANY NODE / ANY VERTEX (optionally typed)
    NodeRef {
        keyword: Keyword,
        label: Option<String>,
    },
    /// EDGE / RELATIONSHIP / ANY EDGE / ANY RELATIONSHIP (optionally typed)
    EdgeRef {
        keyword: Keyword,
        label: Option<String>,
    },
    /// BINDING TABLE reference, optionally with field type specification.
    BindingTableRef {
        fields: Option<Vec<RecordFieldType>>,
    },

    // — Closed dynamic union —
    /// Union of multiple value types
    ClosedDynamicUnion(Vec<ValueType>),

    // — NOT NULL wrapper —
    /// A value type with a NOT NULL constraint.
    NotNull(Box<ValueType>),
}

/// A field in a RECORD type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordFieldType {
    pub span: Span,
    pub name: String,
    /// How the type was introduced (`::`, `TYPED`, or none).
    pub typed_prefix: TypedPrefix,
    pub value_type: ValueType,
}

// ════════════════════════════════════════════════════════════════════════════════
// §19-20 — Expressions
// ════════════════════════════════════════════════════════════════════════════════

/// A GQL expression with source location.
///
/// Wraps an [`ExprKind`] variant with its [`Span`] in the source text.
/// `PartialEq` intentionally ignores the span so that semantic equality
/// comparisons (including tests) are unaffected by source positions.
#[derive(Clone, Debug)]
pub struct Expr {
    pub span: Span,
    pub kind: ExprKind,
}

impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl Expr {
    /// Convenience constructor with a dummy span (for tests and synthetic nodes).
    pub fn new(kind: ExprKind) -> Self {
        Self {
            span: Span::DUMMY,
            kind,
        }
    }
}

/// A GQL expression covering all expression forms from §19–§20.
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    // ── Parenthesized ──
    /// A parenthesized expression: `(expr)`
    Paren(Box<Expr>),

    // ── Literals & References ──
    /// A literal value (number, string, boolean, null, etc.).
    Literal(Value),
    /// A variable reference.
    Variable(String),
    /// A parameter reference (e.g., `$param` or `$$`).
    Parameter(String),

    // ── Property access ──
    /// Property access: `expr.property`
    PropertyAccess { expr: Box<Expr>, property: String },

    // ── Arithmetic (§20.3) ──
    /// Binary arithmetic operator.
    BinaryOp {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    /// Unary operator (negation, positive).
    UnaryOp { op: UnaryOp, expr: Box<Expr> },

    // ── Logical operators (§20.6) ──
    /// Logical AND.
    And(Box<Expr>, Box<Expr>),
    /// Logical OR.
    Or(Box<Expr>, Box<Expr>),
    /// Logical NOT.
    Not(Box<Expr>),
    /// Logical XOR.
    Xor(Box<Expr>, Box<Expr>),

    // ── Comparison (§20.7) ──
    /// Comparison operation.
    Compare {
        left: Box<Expr>,
        op: CmpOp,
        right: Box<Expr>,
    },

    // ── Null predicates (§20.8) ──
    /// IS NULL
    IsNull(Box<Expr>),
    /// IS NOT NULL
    IsNotNull(Box<Expr>),

    // ── IN predicate — sql-compat extension (not in GQL) ──
    /// expr IN (list-of-exprs)
    #[cfg(feature = "sql-compat")]
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },

    // ── String predicates (§20.10) ──
    /// String predicate: STARTS WITH, ENDS WITH, CONTAINS, LIKE, ILIKE.
    StringPredicate {
        expr: Box<Expr>,
        kind: StringPredicateKind,
        pattern: Box<Expr>,
        negated: bool,
    },

    // ── Normalization predicate (§20.11) ──
    /// IS [NOT] <normal-form> NORMALIZED
    IsNormalized {
        expr: Box<Expr>,
        form: NormalForm,
        negated: bool,
    },

    // ── Truth-value test (§20.12) ──
    /// IS [NOT] TRUE / FALSE / UNKNOWN
    IsTruth {
        expr: Box<Expr>,
        value: TruthValue,
        negated: bool,
    },

    // ── Graph predicates (§20.13) ──
    /// IS LABELED <label-expr>
    IsLabeled {
        expr: Box<Expr>,
        label: LabelExpr,
        negated: bool,
    },
    /// IS [NOT] SOURCE OF <edge-expr>
    IsSourceOf {
        node: Box<Expr>,
        edge: Box<Expr>,
        negated: bool,
    },
    /// IS [NOT] DESTINATION OF <edge-expr>
    IsDestOf {
        node: Box<Expr>,
        edge: Box<Expr>,
        negated: bool,
    },
    /// IS [NOT] TYPED <value-type>
    IsTyped {
        expr: Box<Expr>,
        target: ValueType,
        negated: bool,
    },
    /// IS DIRECTED
    IsDirected { expr: Box<Expr>, negated: bool },
    /// ALL_DIFFERENT( expr1, expr2, ... )
    AllDifferent(Vec<Expr>),
    /// SAME( expr1, expr2, ... )
    Same(Vec<Expr>),
    /// PROPERTY_EXISTS( expr, property )
    PropertyExists { expr: Box<Expr>, property: String },

    // ── Existential subquery (§20.14) ──
    /// EXISTS { <subquery> }
    ExistsSubquery(Box<CompositeQueryExpr>),
    /// EXISTS { <pattern> }
    ExistsPattern(Box<GraphPattern>),

    // ── Value subquery (§20.15) ──
    /// VALUE { <subquery> }
    ValueSubquery(Box<CompositeQueryExpr>),

    // ── Let expression ──
    /// LET <bindings> IN <expr>
    LetIn {
        bindings: Vec<LetBinding>,
        expr: Box<Expr>,
    },

    // ── String concatenation (§20.16) ──
    /// String concatenation: expr || expr
    Concat(Box<Expr>, Box<Expr>),

    // ── CAST (§20.17) ──
    /// CAST( expr AS value_type )
    Cast { expr: Box<Expr>, target: ValueType },

    // ── Function call (§20.18) ──
    /// A named function call: name( args... )
    FunctionCall {
        name: ObjectName,
        args: Vec<Expr>,
        distinct: bool,
    },

    // ── Aggregate functions (§20.19) ──
    /// An aggregate function call.
    Aggregate {
        func: AggregateFunc,
        expr: Option<Box<Expr>>,
        /// Second argument for binary set functions (PERCENTILE_CONT/DISC).
        expr2: Option<Box<Expr>>,
        distinct: bool,
        order_by: Option<OrderByClause>,
        filter: Option<Box<Expr>>,
    },

    // ── CASE expression (§20.20) ──
    /// Simple CASE: CASE expr WHEN val THEN result ... [ELSE default] END
    CaseSimple {
        operand: Box<Expr>,
        when_clauses: Vec<WhenClause>,
        else_clause: Option<Box<Expr>>,
    },
    /// Searched CASE: CASE WHEN cond THEN result ... [ELSE default] END
    CaseSearched {
        when_clauses: Vec<WhenClause>,
        else_clause: Option<Box<Expr>>,
    },

    // ── COALESCE / NULLIF (§20.21) ──
    /// COALESCE( expr, expr, ... )
    Coalesce(Vec<Expr>),
    /// NULLIF( expr, expr )
    NullIf(Box<Expr>, Box<Expr>),

    // ── Constructed values (§20.22) ──
    /// List literal: [ expr, expr, ... ]
    ListLiteral(Vec<Expr>),
    /// Keyworded list constructor: LIST[ expr, ... ] or ARRAY[ expr, ... ]
    ListConstructor { keyword: Keyword, items: Vec<Expr> },
    /// List element access: expr[index] (Cypher extension)
    #[cfg(feature = "cypher")]
    ListIndex { list: Box<Expr>, index: Box<Expr> },
    /// List slice: expr[from..to] (Cypher extension)
    #[cfg(feature = "cypher")]
    ListSlice {
        list: Box<Expr>,
        from: Option<Box<Expr>>,
        to: Option<Box<Expr>>,
    },
    /// Record literal: { key: value, ... }
    RecordLiteral(Vec<(String, Expr)>),
    /// Keyworded record constructor: RECORD { key: value, ... }
    RecordConstructor(Vec<(String, Expr)>),
    /// PATH constructor (construct a path from elements).
    PathConstructor { elements: Vec<Expr> },
    /// PATH_LENGTH( path-expr )
    PathLength(Box<Expr>),

    // ── Session/datetime functions (§20.23) ──
    /// SESSION_USER
    SessionUser,
    /// CURRENT_DATE
    CurrentDate,
    /// CURRENT_TIME
    CurrentTime,
    /// CURRENT_TIMESTAMP
    CurrentTimestamp,
    /// LOCAL_TIME
    CurrentLocalTime,
    /// LOCAL_TIMESTAMP / LOCAL_DATETIME
    CurrentLocalTimestamp,

    // ── Element ID (§20.24) ──
    /// ELEMENT_ID( expr )
    ElementId(Box<Expr>),

    // ── Datetime constructors (§20.25) ──
    /// DATE 'string' — §20.25 dateLiteral
    DateLiteral(Vec<Expr>),
    /// DATE( args... ) — §20.25 dateFunction
    DateFunction(Vec<Expr>),
    /// TIME 'string' — §20.25 timeLiteral (not a function; no parens form)
    TimeLiteral(Vec<Expr>),
    /// DATETIME 'string' — §20.25 datetimeLiteral
    DatetimeLiteral(Vec<Expr>),
    /// TIMESTAMP 'string' — §20.25 datetimeLiteral
    TimestampLiteral(Vec<Expr>),
    /// ZONED_TIME( args... ) — §20.25 timeFunction
    ZonedTimeFunction(Vec<Expr>),
    /// ZONED_DATETIME( args... ) — §20.25 datetimeFunction
    ZonedDatetimeFunction(Vec<Expr>),
    /// LOCAL_TIME( args... ) — §20.25 localtimeFunction
    LocalTimeFunction(Vec<Expr>),
    /// LOCAL_DATETIME( args... ) — §20.25 localdatetimeFunction
    LocalDatetimeFunction(Vec<Expr>),
    /// DURATION 'string' — §20.25 durationLiteral
    DurationLiteral(Vec<Expr>),
    /// DURATION( args... ) — §20.25 durationFunction
    DurationFunction(Vec<Expr>),
    /// DURATION_BETWEEN( expr, expr )
    DurationBetween {
        left: Box<Expr>,
        right: Box<Expr>,
        qualifier: Option<DurationQualifier>,
    },

    // ── String functions (§20.26) ──
    /// NORMALIZE( expr, form )
    Normalize { expr: Box<Expr>, form: NormalForm },
    /// TRIM( [spec] [char FROM] expr ) — string trim
    Trim {
        spec: Option<TrimSpec>,
        trim_char: Option<Box<Expr>>,
        expr: Box<Expr>,
    },
    /// TRIM( listExpr, numericExpr ) — list trim (trimListFunction)
    TrimList { list: Box<Expr>, count: Box<Expr> },
    /// UPPER( expr )
    Upper(Box<Expr>),
    /// LOWER( expr )
    Lower(Box<Expr>),
    /// LEFT( expr, n )
    Left(Box<Expr>, Box<Expr>),
    /// RIGHT( expr, n )
    Right(Box<Expr>, Box<Expr>),
    /// FOLD / BTRIM / LTRIM / RTRIM
    FoldString {
        kind: StringFoldKind,
        expr: Box<Expr>,
        chars: Option<Box<Expr>>,
    },

    // ── Length functions (§20.27) ──
    /// CHAR_LENGTH( expr ) / CHARACTER_LENGTH( expr )
    CharLength { keyword: Keyword, expr: Box<Expr> },
    /// BYTE_LENGTH( expr ) / OCTET_LENGTH( expr )
    ByteLength { keyword: Keyword, expr: Box<Expr> },
    /// CARDINALITY( expr ) / SIZE( expr )
    Cardinality { keyword: Keyword, expr: Box<Expr> },

    // ── Numeric functions (§20.28) ──
    /// ABS( expr )
    Abs(Box<Expr>),
    /// MOD( expr, expr )
    Mod(Box<Expr>, Box<Expr>),
    /// FLOOR( expr )
    Floor(Box<Expr>),
    /// CEIL / CEILING( expr )
    Ceil(Box<Expr>),
    /// SQRT( expr )
    Sqrt(Box<Expr>),
    /// EXP( expr )
    Exp(Box<Expr>),
    /// LN( expr )
    Ln(Box<Expr>),
    /// LOG( base, expr )
    Log(Box<Expr>, Box<Expr>),
    /// LOG10( expr )
    Log10(Box<Expr>),
    /// POWER( base, exponent )
    Power(Box<Expr>, Box<Expr>),
    /// SIN( expr )
    Sin(Box<Expr>),
    /// COS( expr )
    Cos(Box<Expr>),
    /// TAN( expr )
    Tan(Box<Expr>),
    /// ASIN( expr )
    Asin(Box<Expr>),
    /// ACOS( expr )
    Acos(Box<Expr>),
    /// ATAN( expr )
    Atan(Box<Expr>),
    // ── SQL-compat functions (not in GQL) ──
    /// ATAN2( y, x )
    #[cfg(feature = "sql-compat")]
    Atan2(Box<Expr>, Box<Expr>),
    /// SIGN( expr )
    #[cfg(feature = "sql-compat")]
    Sign(Box<Expr>),
    /// TRUNCATE / TRUNC( expr [, places] )
    #[cfg(feature = "sql-compat")]
    Truncate {
        expr: Box<Expr>,
        places: Option<Box<Expr>>,
    },
    /// ROUND( expr [, places] )
    #[cfg(feature = "sql-compat")]
    Round {
        expr: Box<Expr>,
        places: Option<Box<Expr>>,
    },
    /// DEGREES( expr )
    Degrees(Box<Expr>),
    /// RADIANS( expr )
    Radians(Box<Expr>),
    /// COT( expr )
    Cot(Box<Expr>),
    /// SINH( expr )
    Sinh(Box<Expr>),
    /// COSH( expr )
    Cosh(Box<Expr>),
    /// TANH( expr )
    Tanh(Box<Expr>),

    // ── Path/graph element functions (§20.29) ──
    /// ELEMENTS( path-expr ) — decompose a path into a list of elements.
    Elements(Box<Expr>),

    // ── Cypher-compat functions (not in GQL) ──
    /// NODES( path-expr ) — extract all nodes from a path.
    #[cfg(feature = "cypher")]
    Nodes(Box<Expr>),
    /// EDGES( path-expr ) — extract all edges from a path.
    #[cfg(feature = "cypher")]
    Edges(Box<Expr>),
    /// LABELS( node-or-edge-expr )
    #[cfg(feature = "cypher")]
    Labels(Box<Expr>),
    /// LABEL( node-or-edge-expr ) — single-label variant
    #[cfg(feature = "cypher")]
    Label(Box<Expr>),
    /// SOURCE( edge-expr )
    #[cfg(feature = "cypher")]
    Source(Box<Expr>),
    /// DESTINATION( edge-expr )
    #[cfg(feature = "cypher")]
    Destination(Box<Expr>),
}

// ──── Binary operators ──

/// Arithmetic binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
}

// ──── Unary operators ──

/// Unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// Unary negation `-`
    Neg,
    /// Unary positive `+`
    Pos,
}

// ──── Comparison operators ──

/// Comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

// ──── String predicate kinds ──

/// The kind of string predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringPredicateKind {
    /// STARTS WITH (cypher extension)
    #[cfg(feature = "cypher")]
    StartsWith,
    /// ENDS WITH (cypher extension)
    #[cfg(feature = "cypher")]
    EndsWith,
    #[cfg(feature = "cypher")]
    Contains,
    #[cfg(feature = "cypher")]
    ILike,
}

// ──── Truth values ──

/// Truth value for IS TRUE / IS FALSE / IS UNKNOWN tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruthValue {
    True,
    False,
    Unknown,
}

/// Qualifier on a duration expression such as `YEAR TO MONTH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationQualifier {
    YearToMonth,
    DayToSecond,
}

// ──── Normal form ──

/// Unicode normalization form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalForm {
    NFC,
    NFD,
    NFKC,
    NFKD,
}

// ──── Trim specification ──

/// Trim direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimSpec {
    Leading,
    Trailing,
    Both,
}

// ──── String fold kind ──

/// String fold/trim variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringFoldKind {
    BTrim,
    LTrim,
    RTrim,
}

// ──── Aggregate functions ──

/// Aggregate function identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    CountStar,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    StddevSamp,
    StddevPop,
    PercentileCont,
    PercentileDisc,
}

// ──── CASE when clause ──

/// A single WHEN clause in a CASE expression.
#[derive(Clone, Debug, PartialEq)]
pub struct WhenClause {
    pub span: Span,
    pub condition: Expr,
    pub result: Expr,
}

// ════════════════════════════════════════════════════════════════════════════════
// Display implementations for key operator types
// ════════════════════════════════════════════════════════════════════════════════

impl std::fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
        }
    }
}

impl std::fmt::Display for CmpOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eq => write!(f, "="),
            Self::Ne => write!(f, "<>"),
            Self::Lt => write!(f, "<"),
            Self::Le => write!(f, "<="),
            Self::Gt => write!(f, ">"),
            Self::Ge => write!(f, ">="),
        }
    }
}

impl std::fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Neg => write!(f, "-"),
            Self::Pos => write!(f, "+"),
        }
    }
}

impl std::fmt::Display for SetOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Union => write!(f, "UNION"),
            Self::UnionAll => write!(f, "UNION ALL"),
            Self::UnionDistinct => write!(f, "UNION DISTINCT"),
            Self::Except => write!(f, "EXCEPT"),
            Self::ExceptAll => write!(f, "EXCEPT ALL"),
            Self::ExceptDistinct => write!(f, "EXCEPT DISTINCT"),
            Self::Intersect => write!(f, "INTERSECT"),
            Self::IntersectAll => write!(f, "INTERSECT ALL"),
            Self::IntersectDistinct => write!(f, "INTERSECT DISTINCT"),
            Self::Otherwise => write!(f, "OTHERWISE"),
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Convenience constructors
// ════════════════════════════════════════════════════════════════════════════════

impl Expr {
    /// Create a literal integer expression.
    pub fn int(v: i64) -> Self {
        Self::new(ExprKind::Literal(Value::Int64(v)))
    }

    /// Create a literal string expression.
    pub fn string(s: impl Into<String>) -> Self {
        Self::new(ExprKind::Literal(Value::Text(s.into())))
    }

    /// Create a literal boolean expression.
    pub fn bool(b: bool) -> Self {
        Self::new(ExprKind::Literal(Value::Bool(b)))
    }

    /// Create a NULL literal expression.
    pub fn null() -> Self {
        Self::new(ExprKind::Literal(Value::Null))
    }

    /// Create a variable reference expression.
    pub fn var(name: impl Into<String>) -> Self {
        Self::new(ExprKind::Variable(name.into()))
    }
}

impl NodePattern {
    /// Create a bare node pattern with no variable, label, properties, or where.
    pub fn bare() -> Self {
        Self {
            span: Span::DUMMY,
            variable: None,
            is_or_colon: None,
            label: None,
            properties: vec![],
            where_clause: None,
        }
    }
}

impl GraphPattern {
    /// Create a graph pattern with a single path and no modifiers.
    pub fn simple(paths: Vec<PathPattern>) -> Self {
        Self {
            span: Span::DUMMY,
            match_mode: None,
            paths,
            keep: None,
            where_clause: None,
        }
    }
}

impl LinearQueryStatement {
    /// Create a linear query with no result statement.
    pub fn parts_only(parts: Vec<SimpleQueryStatement>) -> Self {
        Self {
            span: Span::DUMMY,
            at_schema: None,
            prefix_bindings: vec![],
            parts,
            result: None,
        }
    }
}

impl CompositeQueryExpr {
    /// Create a composite query from a single linear query (no set operations).
    pub fn single(linear: LinearQueryStatement) -> Self {
        Self {
            span: Span::DUMMY,
            left: linear,
            rest: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expr_convenience_constructors() {
        assert_eq!(
            Expr::int(42),
            Expr::new(ExprKind::Literal(Value::Int64(42)))
        );
        assert_eq!(
            Expr::string("hello"),
            Expr::new(ExprKind::Literal(Value::Text("hello".into())))
        );
        assert_eq!(
            Expr::bool(true),
            Expr::new(ExprKind::Literal(Value::Bool(true)))
        );
        assert_eq!(Expr::null(), Expr::new(ExprKind::Literal(Value::Null)));
        assert_eq!(Expr::var("x"), Expr::new(ExprKind::Variable("x".into())));
    }

    #[test]
    fn object_name_simple() {
        let name = ObjectName::simple("test");
        assert_eq!(name.parts, vec!["test".to_string()]);
    }

    #[test]
    fn object_name_qualified() {
        let name = ObjectName::qualified(vec!["catalog".into(), "schema".into(), "graph".into()]);
        assert_eq!(name.parts.len(), 3);
    }

    #[test]
    fn node_pattern_bare() {
        let n = NodePattern::bare();
        assert_eq!(n.variable, None);
        assert_eq!(n.label, None);
        assert!(n.properties.is_empty());
        assert!(n.where_clause.is_none());
    }

    #[test]
    fn composite_query_single() {
        let q = CompositeQueryExpr::single(LinearQueryStatement {
            span: Span::DUMMY,
            at_schema: None,
            prefix_bindings: vec![],
            parts: vec![],
            result: Some(ResultStatement::Return(Box::new(ReturnStatement {
                span: Span::DUMMY,
                set_quantifier: SetQuantifier::None,
                body: ReturnBody::Star,
            }))),
        });
        assert!(q.rest.is_empty());
    }

    #[test]
    fn value_type_not_null_wrapping() {
        let ty = ValueType::NotNull(Box::new(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }));
        match &ty {
            ValueType::NotNull(inner) => assert_eq!(
                **inner,
                ValueType::Int64 {
                    keyword: Keyword::new("INT64")
                }
            ),
            _ => panic!("Expected NotNull"),
        }
    }

    #[test]
    fn set_op_display() {
        assert_eq!(format!("{}", SetOp::Union), "UNION");
        assert_eq!(format!("{}", SetOp::UnionAll), "UNION ALL");
        assert_eq!(format!("{}", SetOp::Otherwise), "OTHERWISE");
    }

    #[test]
    fn binary_op_display() {
        assert_eq!(format!("{}", BinaryOp::Add), "+");
    }

    #[test]
    fn cmp_op_display() {
        assert_eq!(format!("{}", CmpOp::Ne), "<>");
        assert_eq!(format!("{}", CmpOp::Le), "<=");
    }

    #[test]
    fn path_quantifier_clone() {
        let q = PathQuantifier::Range {
            lower: 1,
            upper: Some(5),
        };
        assert_eq!(q.clone(), q);
    }

    #[test]
    fn edge_pattern_with_direction() {
        let e = EdgePattern {
            span: Span::DUMMY,
            direction: EdgeDirection::PointingRight,
            variable: Some("e".into()),
            is_or_colon: Some(IsOrColon::Colon),
            label: Some(LabelExpr::Name("KNOWS".into())),
            properties: vec![],
            where_clause: None,
        };
        assert_eq!(e.direction, EdgeDirection::PointingRight);
    }

    #[test]
    fn value_type_list_with_element_type() {
        let ty = ValueType::List {
            keyword: Keyword::new("LIST"),
            element_type: Box::new(ValueType::Int32 {
                keyword: Keyword::new("INT32"),
            }),
            max_length: Some(100),
        };
        match &ty {
            ValueType::List {
                element_type,
                max_length,
                ..
            } => {
                assert_eq!(
                    **element_type,
                    ValueType::Int32 {
                        keyword: Keyword::new("INT32")
                    }
                );
                assert_eq!(*max_length, Some(100));
            }
            _ => panic!("Expected List"),
        }
    }

    #[test]
    fn value_type_record() {
        let ty = ValueType::Record {
            record_keyword: true,
            fields: vec![
                RecordFieldType {
                    span: Span::DUMMY,
                    name: "name".into(),
                    typed_prefix: TypedPrefix::None,
                    value_type: ValueType::String {
                        min_length: None,
                        max_length: None,
                    },
                },
                RecordFieldType {
                    span: Span::DUMMY,
                    name: "age".into(),
                    typed_prefix: TypedPrefix::None,
                    value_type: ValueType::Int32 {
                        keyword: Keyword::new("INT32"),
                    },
                },
            ],
        };
        match &ty {
            ValueType::Record { fields, .. } => assert_eq!(fields.len(), 2),
            _ => panic!("Expected Record"),
        }
    }

    #[test]
    fn graph_pattern_simple() {
        let gp = GraphPattern::simple(vec![]);
        assert!(gp.match_mode.is_none());
        assert!(gp.paths.is_empty());
        assert!(gp.keep.is_none());
        assert!(gp.where_clause.is_none());
    }

    #[test]
    fn match_statement_optional() {
        let m = MatchStatement {
            span: Span::DUMMY,
            optional: true,
            graph_name: None,
            pattern: GraphPattern::simple(vec![]),
            yield_items: None,
        };
        assert!(m.optional);
    }

    #[test]
    fn delete_statement_detach() {
        let d = DeleteStatement {
            span: Span::DUMMY,
            detach: DeleteDetach::Detach,
            items: vec![Expr::new(ExprKind::Variable("n".into()))],
        };
        assert_eq!(d.detach, DeleteDetach::Detach);
        assert_eq!(
            d.items,
            vec![Expr::new(ExprKind::Variable("n".to_string()))]
        );
    }

    #[test]
    fn aggregate_func_copy() {
        let f = AggregateFunc::PercentileCont;
        let f2 = f;
        assert_eq!(f, f2);
    }

    #[test]
    fn normal_form_copy() {
        let nf = NormalForm::NFKD;
        let nf2 = nf;
        assert_eq!(nf, nf2);
    }

    #[test]
    fn trim_spec_copy() {
        let ts = TrimSpec::Both;
        let ts2 = ts;
        assert_eq!(ts, ts2);
    }

    // ── Display impl coverage ────────────────────────────────────────

    #[test]
    fn binary_op_display_all() {
        assert_eq!(format!("{}", BinaryOp::Add), "+");
        assert_eq!(format!("{}", BinaryOp::Sub), "-");
        assert_eq!(format!("{}", BinaryOp::Mul), "*");
        assert_eq!(format!("{}", BinaryOp::Div), "/");
    }

    #[test]
    fn cmp_op_display_all() {
        assert_eq!(format!("{}", CmpOp::Eq), "=");
        assert_eq!(format!("{}", CmpOp::Ne), "<>");
        assert_eq!(format!("{}", CmpOp::Lt), "<");
        assert_eq!(format!("{}", CmpOp::Le), "<=");
        assert_eq!(format!("{}", CmpOp::Gt), ">");
        assert_eq!(format!("{}", CmpOp::Ge), ">=");
    }

    #[test]
    fn unary_op_display_all() {
        assert_eq!(format!("{}", UnaryOp::Neg), "-");
        assert_eq!(format!("{}", UnaryOp::Pos), "+");
    }

    #[test]
    fn set_op_display_all() {
        assert_eq!(format!("{}", SetOp::Union), "UNION");
        assert_eq!(format!("{}", SetOp::UnionAll), "UNION ALL");
        assert_eq!(format!("{}", SetOp::UnionDistinct), "UNION DISTINCT");
        assert_eq!(format!("{}", SetOp::Except), "EXCEPT");
        assert_eq!(format!("{}", SetOp::ExceptAll), "EXCEPT ALL");
        assert_eq!(format!("{}", SetOp::ExceptDistinct), "EXCEPT DISTINCT");
        assert_eq!(format!("{}", SetOp::Intersect), "INTERSECT");
        assert_eq!(format!("{}", SetOp::IntersectAll), "INTERSECT ALL");
        assert_eq!(
            format!("{}", SetOp::IntersectDistinct),
            "INTERSECT DISTINCT"
        );
        assert_eq!(format!("{}", SetOp::Otherwise), "OTHERWISE");
    }

    // ── Keyword impls ────────────────────────────────────────────────

    #[test]
    fn keyword_equality_ignores_content() {
        let k1 = Keyword::new("INT");
        let k2 = Keyword::new("INTEGER");
        assert_eq!(k1, k2, "Keyword should always compare equal");
    }

    #[test]
    fn keyword_debug() {
        let k = Keyword::new("BIGINT");
        let dbg = format!("{:?}", k);
        assert!(dbg.contains("BIGINT"));
    }

    #[test]
    fn keyword_clone() {
        let k = Keyword::new("FLOAT");
        let k2 = k.clone();
        assert_eq!(k, k2);
        assert_eq!(k2.0, "FLOAT");
    }

    // ── StatementBlock::iter_statements ──────────────────────────────

    #[test]
    fn statement_block_iter_statements() {
        let block = StatementBlock {
            span: Span::DUMMY,
            first: Statement::Query(Box::new(CompositeQueryExpr::single(
                LinearQueryStatement::parts_only(vec![]),
            ))),
            next: vec![NextStatement {
                span: Span::DUMMY,
                yield_items: None,
                statement: Statement::Query(Box::new(CompositeQueryExpr::single(
                    LinearQueryStatement::parts_only(vec![]),
                ))),
            }],
        };
        let count = block.iter_statements().count();
        assert_eq!(count, 2, "expected 2 statements");
    }

    // ── Copy/Clone/PartialEq for small enums ────────────────────────

    #[test]
    fn transaction_end_copy() {
        let te = TransactionEnd::Commit;
        let te2 = te;
        assert_eq!(te, te2);
        assert_eq!(TransactionEnd::Rollback, TransactionEnd::Rollback);
    }

    #[test]
    fn transaction_access_mode_copy() {
        let m = TransactionAccessMode::ReadOnly;
        let m2 = m;
        assert_eq!(m, m2);
        assert_ne!(m, TransactionAccessMode::ReadWrite);
    }

    #[test]
    fn delete_detach_variants() {
        assert_eq!(DeleteDetach::Detach, DeleteDetach::Detach);
        assert_eq!(DeleteDetach::NoDetach, DeleteDetach::NoDetach);
        assert_eq!(DeleteDetach::Unspecified, DeleteDetach::Unspecified);
        assert_ne!(DeleteDetach::Detach, DeleteDetach::NoDetach);
    }

    #[test]
    fn set_quantifier_copy() {
        let q = SetQuantifier::Distinct;
        let q2 = q;
        assert_eq!(q, q2);
        assert_ne!(q, SetQuantifier::All);
        assert_ne!(q, SetQuantifier::None);
    }

    #[test]
    fn sort_direction_variants() {
        assert_eq!(SortDirection::Asc, SortDirection::Asc);
        assert_eq!(SortDirection::Desc, SortDirection::Desc);
        assert_eq!(SortDirection::Ascending, SortDirection::Ascending);
        assert_eq!(SortDirection::Descending, SortDirection::Descending);
        assert_ne!(SortDirection::Asc, SortDirection::Desc);
    }

    #[test]
    fn null_order_variants() {
        assert_eq!(NullOrder::First, NullOrder::First);
        assert_eq!(NullOrder::Last, NullOrder::Last);
        assert_ne!(NullOrder::First, NullOrder::Last);
    }

    #[test]
    fn truth_value_variants() {
        assert_eq!(TruthValue::True, TruthValue::True);
        assert_eq!(TruthValue::False, TruthValue::False);
        assert_eq!(TruthValue::Unknown, TruthValue::Unknown);
        assert_ne!(TruthValue::True, TruthValue::False);
    }

    #[test]
    fn normal_form_variants() {
        assert_eq!(NormalForm::NFC, NormalForm::NFC);
        assert_eq!(NormalForm::NFD, NormalForm::NFD);
        assert_eq!(NormalForm::NFKC, NormalForm::NFKC);
        assert_ne!(NormalForm::NFC, NormalForm::NFKD);
    }

    #[test]
    fn string_fold_kind_variants() {
        assert_eq!(StringFoldKind::BTrim, StringFoldKind::BTrim);
        assert_eq!(StringFoldKind::LTrim, StringFoldKind::LTrim);
        assert_eq!(StringFoldKind::RTrim, StringFoldKind::RTrim);
        assert_ne!(StringFoldKind::BTrim, StringFoldKind::LTrim);
    }

    #[test]
    fn aggregate_func_variants() {
        assert_eq!(AggregateFunc::Count, AggregateFunc::Count);
        assert_eq!(AggregateFunc::CountStar, AggregateFunc::CountStar);
        assert_eq!(AggregateFunc::Sum, AggregateFunc::Sum);
        assert_eq!(AggregateFunc::Avg, AggregateFunc::Avg);
        assert_eq!(AggregateFunc::Min, AggregateFunc::Min);
        assert_eq!(AggregateFunc::Max, AggregateFunc::Max);
        assert_eq!(AggregateFunc::Collect, AggregateFunc::Collect);
        assert_eq!(AggregateFunc::StddevSamp, AggregateFunc::StddevSamp);
        assert_eq!(AggregateFunc::StddevPop, AggregateFunc::StddevPop);
        assert_eq!(AggregateFunc::PercentileDisc, AggregateFunc::PercentileDisc);
    }

    #[test]
    fn trim_spec_variants() {
        assert_eq!(TrimSpec::Leading, TrimSpec::Leading);
        assert_eq!(TrimSpec::Trailing, TrimSpec::Trailing);
        assert_ne!(TrimSpec::Leading, TrimSpec::Trailing);
    }

    #[test]
    fn duration_qualifier_variants() {
        assert_eq!(
            DurationQualifier::YearToMonth,
            DurationQualifier::YearToMonth
        );
        assert_eq!(
            DurationQualifier::DayToSecond,
            DurationQualifier::DayToSecond
        );
        assert_ne!(
            DurationQualifier::YearToMonth,
            DurationQualifier::DayToSecond
        );
    }

    // ── IsOrColon / TypedPrefix ──────────────────────────────────────

    #[test]
    fn is_or_colon_variants() {
        assert_eq!(IsOrColon::Is, IsOrColon::Is);
        assert_eq!(IsOrColon::Colon, IsOrColon::Colon);
        assert_ne!(IsOrColon::Is, IsOrColon::Colon);
    }

    #[test]
    fn typed_prefix_variants() {
        assert_eq!(TypedPrefix::DoubleColon, TypedPrefix::DoubleColon);
        assert_eq!(TypedPrefix::Typed, TypedPrefix::Typed);
        assert_eq!(TypedPrefix::None, TypedPrefix::None);
        assert_ne!(TypedPrefix::DoubleColon, TypedPrefix::Typed);
    }

    // ── PathOrPaths / GroupOrGroups ──────────────────────────────────

    #[test]
    fn path_or_paths_variants() {
        assert_eq!(PathOrPaths::Path, PathOrPaths::Path);
        assert_eq!(PathOrPaths::Paths, PathOrPaths::Paths);
        assert_ne!(PathOrPaths::Path, PathOrPaths::Paths);
    }

    #[test]
    fn group_or_groups_variants() {
        assert_eq!(GroupOrGroups::Group, GroupOrGroups::Group);
        assert_eq!(GroupOrGroups::Groups, GroupOrGroups::Groups);
        assert_ne!(GroupOrGroups::Group, GroupOrGroups::Groups);
    }

    // ── MatchMode variants ──────────────────────────────────────────

    #[test]
    fn match_mode_element_keyword_variants() {
        assert_eq!(
            MatchModeElementKeyword::Element,
            MatchModeElementKeyword::Element
        );
        assert_eq!(
            MatchModeElementKeyword::ElementBindings,
            MatchModeElementKeyword::ElementBindings
        );
        assert_ne!(
            MatchModeElementKeyword::Element,
            MatchModeElementKeyword::Elements
        );
    }

    #[test]
    fn match_mode_edge_keyword_variants() {
        assert_eq!(MatchModeEdgeKeyword::Edge, MatchModeEdgeKeyword::Edge);
        assert_eq!(
            MatchModeEdgeKeyword::EdgeBindings,
            MatchModeEdgeKeyword::EdgeBindings
        );
        assert_eq!(
            MatchModeEdgeKeyword::Relationship,
            MatchModeEdgeKeyword::Relationship
        );
        assert_eq!(
            MatchModeEdgeKeyword::RelationshipBindings,
            MatchModeEdgeKeyword::RelationshipBindings
        );
        assert_ne!(
            MatchModeEdgeKeyword::Edge,
            MatchModeEdgeKeyword::Relationships
        );
    }

    // ── PathMode variants ───────────────────────────────────────────

    #[test]
    fn path_mode_variants() {
        assert_eq!(PathMode::Walk, PathMode::Walk);
        assert_eq!(PathMode::Trail, PathMode::Trail);
        assert_eq!(PathMode::Simple, PathMode::Simple);
        assert_eq!(PathMode::Acyclic, PathMode::Acyclic);
        assert_ne!(PathMode::Walk, PathMode::Trail);
    }

    // ── ValueType variants ──────────────────────────────────────────

    #[test]
    fn value_type_simple_variants() {
        assert_eq!(ValueType::Date, ValueType::Date);
        assert_eq!(ValueType::Time, ValueType::Time);
        assert_eq!(ValueType::DateTime, ValueType::DateTime);
        assert_eq!(ValueType::Timestamp, ValueType::Timestamp);
        assert_eq!(ValueType::Duration, ValueType::Duration);
        assert_eq!(
            ValueType::DurationYearToMonth,
            ValueType::DurationYearToMonth
        );
        assert_eq!(
            ValueType::DurationDayToSecond,
            ValueType::DurationDayToSecond
        );
        assert_eq!(ValueType::Path, ValueType::Path);
        assert_eq!(ValueType::Any, ValueType::Any);
        assert_eq!(ValueType::AnyValue, ValueType::AnyValue);
        assert_eq!(ValueType::AnyPropertyValue, ValueType::AnyPropertyValue);
        assert_eq!(ValueType::Nothing, ValueType::Nothing);
        assert_eq!(ValueType::Null, ValueType::Null);
        assert_eq!(ValueType::Float128, ValueType::Float128);
        assert_eq!(ValueType::Float256, ValueType::Float256);
    }

    #[test]
    fn value_type_string_with_lengths() {
        let ty = ValueType::String {
            min_length: Some(1),
            max_length: Some(255),
        };
        match &ty {
            ValueType::String {
                min_length,
                max_length,
            } => {
                assert_eq!(*min_length, Some(1));
                assert_eq!(*max_length, Some(255));
            }
            _ => panic!("Expected String"),
        }
    }

    #[test]
    fn value_type_closed_dynamic_union() {
        let ty = ValueType::ClosedDynamicUnion(vec![
            ValueType::Int32 {
                keyword: Keyword::new("INT"),
            },
            ValueType::String {
                min_length: None,
                max_length: None,
            },
        ]);
        match &ty {
            ValueType::ClosedDynamicUnion(types) => assert_eq!(types.len(), 2),
            _ => panic!("Expected ClosedDynamicUnion"),
        }
    }

    #[test]
    fn value_type_decimal() {
        let ty = ValueType::Decimal {
            keyword: Keyword::new("DECIMAL"),
            precision: Some(10),
            scale: Some(2),
        };
        match &ty {
            ValueType::Decimal {
                precision, scale, ..
            } => {
                assert_eq!(*precision, Some(10));
                assert_eq!(*scale, Some(2));
            }
            _ => panic!("Expected Decimal"),
        }
    }

    #[test]
    fn value_type_float_precision() {
        let ty = ValueType::FloatPrecision {
            precision: 32,
            scale: Some(8),
        };
        match &ty {
            ValueType::FloatPrecision { precision, scale } => {
                assert_eq!(*precision, 32);
                assert_eq!(*scale, Some(8));
            }
            _ => panic!("Expected FloatPrecision"),
        }
    }

    // ── SchemaReference ─────────────────────────────────────────────

    #[test]
    fn schema_reference_variants() {
        let sr = SchemaReference::Current("HOME_SCHEMA".into());
        assert_eq!(sr, SchemaReference::Current("HOME_SCHEMA".into()));

        let sr2 = SchemaReference::Absolute(vec!["catalog".into(), "schema".into()]);
        match &sr2 {
            SchemaReference::Absolute(parts) => assert_eq!(parts.len(), 2),
            _ => panic!("Expected Absolute"),
        }

        let sr3 = SchemaReference::Relative(vec!["..".into(), "other".into()]);
        match &sr3 {
            SchemaReference::Relative(parts) => assert_eq!(parts.len(), 2),
            _ => panic!("Expected Relative"),
        }

        let sr4 = SchemaReference::Parameter("myParam".into());
        match &sr4 {
            SchemaReference::Parameter(name) => assert_eq!(name, "myParam"),
            _ => panic!("Expected Parameter"),
        }
    }

    // ── ProcedureBindingKind ────────────────────────────────────────

    #[test]
    fn procedure_binding_kind_variants() {
        assert_eq!(ProcedureBindingKind::Graph, ProcedureBindingKind::Graph);
        assert_eq!(ProcedureBindingKind::Table, ProcedureBindingKind::Table);
        assert_eq!(ProcedureBindingKind::Value, ProcedureBindingKind::Value);
    }

    // ── StringPredicateKind ─────────────────────────────────────────

    #[cfg(feature = "cypher")]
    #[test]
    fn string_predicate_kind_variants() {
        assert_eq!(
            StringPredicateKind::StartsWith,
            StringPredicateKind::StartsWith
        );
        assert_eq!(StringPredicateKind::EndsWith, StringPredicateKind::EndsWith);
        assert_eq!(StringPredicateKind::Contains, StringPredicateKind::Contains);
        assert_eq!(StringPredicateKind::ILike, StringPredicateKind::ILike);
    }

    // ── LinearQueryStatement parts_only ─────────────────────────────

    #[test]
    fn linear_query_parts_only() {
        let lq = LinearQueryStatement::parts_only(vec![]);
        assert!(lq.at_schema.is_none());
        assert!(lq.prefix_bindings.is_empty());
        assert!(lq.parts.is_empty());
        assert!(lq.result.is_none());
    }
}
