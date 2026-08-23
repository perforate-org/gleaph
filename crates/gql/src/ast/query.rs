use crate::token::Span;

use super::catalog::{DeleteStatement, InsertStatement, ObjectName, RemoveStatement, SetStatement};
use super::expr::Expr;
use super::graph_type::ValueType;
use super::pattern::GraphPattern;

// ════════════════════════════════════════════════════════════════════════════════
// §14 — Query statements
// ════════════════════════════════════════════════════════════════════════════════

/// A composite query expression: a linear query optionally combined with
/// UNION / EXCEPT / INTERSECT / OTHERWISE operators.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct CompositeQueryExpr {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub left: LinearQueryStatement,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub rest: Vec<(SetOp, LinearQueryStatement)>,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum ProcedureBindingKind {
    Graph,
    Table,
    Value,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum ProcedureBindingInitializer {
    Object(ObjectName),
    Expr(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Expr),
    Query(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<CompositeQueryExpr>),
}

/// How a type annotation was introduced syntactically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum IsOrColon {
    /// `IS` keyword
    Is,
    /// `:` colon
    Colon,
}

/// Whether `PATH` (singular) or `PATHS` (plural) keyword was used (GQL `pathOrPaths`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum PathOrPaths {
    Path,
    Paths,
}

/// Whether `GROUP` (singular) or `GROUPS` (plural) keyword was used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum GroupOrGroups {
    Group,
    Groups,
}

/// Original source keyword(s), preserved for formatting / round-tripping.
///
/// Always compares equal so that semantic `PartialEq` on the containing type
/// is unaffected by keyword spelling differences.
#[derive(Clone, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
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
    Value(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] ValueType),
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct ProcedureBindingDefinition {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub kind: ProcedureBindingKind,
    pub variable: String,
    /// How the type annotation was introduced (`::`, `TYPED`, or none).
    pub typed_prefix: TypedPrefix,
    /// Optional type annotation between the variable name and the `=`.
    pub type_annotation: Option<BindingTypeAnnotation>,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub initializer: ProcedureBindingInitializer,
}

/// Set operation combining two query expressions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct LinearQueryStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    /// Optional AT schema clause (GQL `atSchemaClause`).
    pub at_schema: Option<SchemaReference>,
    /// Procedure-body prefix bindings declared before the first query clause.
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub prefix_bindings: Vec<ProcedureBindingDefinition>,
    /// The sequence of simple query parts (MATCH, FILTER, LET, FOR, etc.).
    pub parts: Vec<SimpleQueryStatement>,
    /// The final result statement (RETURN or SELECT), if any.
    pub result: Option<ResultStatement>,
}

/// A simple (non-composite) query statement.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum SimpleQueryStatement {
    Match(MatchStatement),
    Filter(FilterStatement),
    Let(LetStatement),
    For(ForStatement),
    #[cfg(feature = "gleaph")]
    Search(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] SearchStatement),
    #[cfg(feature = "gleaph")]
    Grant(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] GrantStatement),
    #[cfg(feature = "gleaph")]
    Revoke(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] RevokeStatement),
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
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum ResultStatement {
    Return(Box<ReturnStatement>),
    Select(Box<SelectStatement>),
    /// FINISH — terminates a linear query with no result bindings.
    Finish,
}

// ──── MATCH (§14.1) ────

/// MATCH [OPTIONAL] <graph_pattern> [ON <graph_name>]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct MatchStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub optional: bool,
    pub graph_name: Option<ObjectName>,
    pub pattern: GraphPattern,
    pub yield_items: Option<Vec<YieldItem>>,
}

/// A `SEARCH` clause binding a graph variable to a search provider result.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct SearchStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub binding: String,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub provider: SearchProvider,
    pub output: SearchOutputBinding,
}

/// Search provider in a `SEARCH` clause.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum SearchProvider {
    VectorIndex(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] VectorSearchSpec),
}

#[cfg(feature = "gleaph")]
impl SearchProvider {
    pub fn query(&self) -> &Expr {
        match self {
            Self::VectorIndex(spec) => &spec.query,
        }
    }

    pub fn limit(&self) -> &Expr {
        match self {
            Self::VectorIndex(spec) => &spec.limit,
        }
    }

    pub fn filter(&self) -> Option<&Expr> {
        match self {
            Self::VectorIndex(spec) => spec.filter.as_ref(),
        }
    }
}

/// `VECTOR INDEX` search specification inside a `SEARCH` clause.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct VectorSearchSpec {
    pub index_name: ObjectName,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub query: Expr,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub limit: Expr,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub filter: Option<Expr>,
}

/// Output alias for a `SEARCH` clause: `SCORE AS alias` or `DISTANCE AS alias`.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct SearchOutputBinding {
    pub kind: SearchOutputKind,
    pub alias: String,
}

/// Whether a `SEARCH` output alias represents a score or a distance.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum SearchOutputKind {
    Score,
    Distance,
}

// ──── GRANT / REVOKE (Gleaph extension, ADR 0074 §5) ────

/// `GRANT <privilege> ON GRAPH <graph> <selector> TO <subject>` (ADR 0074 §5).
///
/// Gleaph-specific data-plane authorization statement. Parsed generically behind the
/// `gleaph` feature; binding of the `PRINCIPAL` literal to an identity is an
/// integration-layer concern (ADR 0034 dialect contract). Execution and schema
/// validation happen on the host's control path — the planner never sees these
/// statements.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct GrantStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub target: GrantTarget,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub subject: GrantSubjectLiteral,
}

/// Target of a `GRANT`/`REVOKE` statement (ADR 0074 §5).
///
/// One discriminated union so the two statement forms cannot be mixed at the type
/// level: a graph-scoped privilege is always bound to `(graph, resource-selector)`, and
/// the `EXECUTE` publication form is always bound to exactly one prepared query name.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum GrantTarget {
    /// `<privilege> ON GRAPH <graph> <resource-selector>` (ADR 0074 §2/§5).
    Graph {
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
        privilege: GrantPrivilege,
        graph: ObjectName,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
        resource: GrantResourceSelector,
    },
    /// `EXECUTE ON PREPARED QUERY <query-name>` — the caller-bounded publication
    /// form (ADR 0074 §1b). The bound graph resolves from the stored record on the
    /// Router; it is not part of the statement.
    PreparedQuery { name: String },
}

/// `REVOKE <privilege> ON GRAPH <graph> <selector> FROM <subject>` — the exact-key
/// inverse of [`GrantStatement`] (ADR 0074 §5).
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub struct RevokeStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub target: GrantTarget,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub subject: GrantSubjectLiteral,
}

/// The privilege operation of a `GRANT`/`REVOKE` statement ([ADR 0074] §2).
///
/// Property lists are part of the `Read` variant: a list lowers to per-property
/// `READ_PROPERTY` rows, so the AST cannot represent a property list attached to a
/// non-READ operation. Directional modifiers are part of the `Traverse` variant.
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum GrantPrivilege {
    Match,
    Traverse {
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
        direction: Option<GrantDirection>,
    },
    /// `READ`, optionally with a vertex property list lowering to `READ_PROPERTY`.
    Read {
        properties: Vec<String>,
    },
    Create,
    Update,
    Delete,
}
/// Directional modifier accessor.
#[cfg(feature = "gleaph")]
impl GrantPrivilege {
    /// Directional modifier of a `TRAVERSE` privilege, if any.
    pub fn direction(&self) -> Option<GrantDirection> {
        match self {
            GrantPrivilege::Traverse { direction } => *direction,
            _ => None,
        }
    }
}

/// Logical traversal direction (`OUTGOING = source → target`) of a `TRAVERSE`
/// privilege modifier ([ADR 0074] §2). Graph semantics, independent of physical
/// storage orientation.
#[cfg(feature = "gleaph")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum GrantDirection {
    Outgoing,
    Incoming,
}

/// Resource selector of a `GRANT`/`REVOKE` statement.
///
/// Both `NODES <label>` and `VERTICES <label>` parse into [`GrantResourceSelector::Vertex`]
/// (the canonical codebase term is vertex); the source keyword is not preserved.
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum GrantResourceSelector {
    Vertex { label: String },
    Edge { label: String },
}

/// Subject literal of a `GRANT ... TO` / `REVOKE ... FROM` clause ([ADR 0074] §5).
///
/// `Principal` carries the raw literal text; resolving it to an identity (and rejecting
/// the anonymous principal) is the integration layer's responsibility (ADR 0034).
#[cfg(feature = "gleaph")]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    rkyv(
        serialize_bounds(
            __S: rkyv::ser::Writer + rkyv::ser::Allocator,
            __S::Error: rkyv::rancor::Source,
        ),
        deserialize_bounds(__D::Error: rkyv::rancor::Source),
        bytecheck(bounds(
            __C: rkyv::validation::ArchiveContext,
            __C::Error: rkyv::rancor::Source,
        )),
    )
)]
pub enum GrantSubjectLiteral {
    Principal(String),
    Public,
}

// ──── FILTER (§14.2) ────

/// FILTER [WHERE] <condition>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct FilterStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub where_keyword: bool,
    pub condition: Expr,
}

// ──── LET (§14.3) ────

/// LET <bindings>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct LetStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub bindings: Vec<LetBinding>,
}

/// A single LET binding: variable = expression.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct LetBinding {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub variable: String,
    pub value: Expr,
}

// ──── FOR (§14.4) ────

/// FOR <variable> IN <list-expression> [WITH ORDINALITY <ordinal-var>]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct ForStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub variable: String,
    pub list: Expr,
    /// `WITH ORDINALITY <var>` or `WITH OFFSET <var>`.
    pub ordinality: Option<ForOrdinality>,
}

/// Ordinality clause of a FOR statement.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct ForOrdinality {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    /// Whether `OFFSET` was used instead of `ORDINALITY`.
    pub offset_keyword: bool,
    pub variable: String,
}

// ──── RETURN (§14.5) ────

/// RETURN statement.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct ReturnStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub set_quantifier: SetQuantifier,
    pub body: ReturnBody,
}

/// SELECT statement.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct SelectStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub set_quantifier: SetQuantifier,
    pub source: Option<SelectSource>,
    pub body: SelectBody,
}

/// Set quantifier: DISTINCT, ALL, or none (GQL `setQuantifier`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum SetQuantifier {
    /// No keyword specified.
    None,
    /// `ALL` explicitly specified.
    All,
    /// `DISTINCT` explicitly specified.
    Distinct,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum SelectSource {
    GraphMatchList(Vec<SelectGraphMatch>),
    QuerySpecification(SelectQuerySpecification),
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct SelectGraphMatch {
    pub graph: ObjectName,
    pub match_statement: MatchStatement,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum SelectQuerySpecification {
    Nested(Box<CompositeQueryExpr>),
    GraphNested {
        graph: ObjectName,
        query: Box<CompositeQueryExpr>,
    },
}

/// The body of a RETURN statement.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct ReturnItem {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub expr: Expr,
    pub alias: Option<String>,
}

// ──── ORDER BY / LIMIT / OFFSET / GROUP BY ────

/// ORDER BY <sort-items>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct OrderByClause {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub items: Vec<SortItem>,
}

/// A single sort specification.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct SortItem {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub expr: Expr,
    pub direction: Option<SortDirection>,
    pub null_order: Option<NullOrder>,
}

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum NullOrder {
    First,
    Last,
}

/// LIMIT <count>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct LimitClause {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub count: Expr,
}

/// OFFSET <count> or SKIP <count>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct OffsetClause {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    /// Whether `SKIP` was used instead of `OFFSET`.
    pub skip_keyword: bool,
    pub count: Expr,
}

/// GROUP BY <items>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct GroupByClause {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub items: Vec<Expr>,
}

// ════════════════════════════════════════════════════════════════════════════════
// §15 — Call procedure
// ════════════════════════════════════════════════════════════════════════════════

/// CALL <procedure-name> ( <args> ) [YIELD <items>]
/// or CALL { <inline-procedure-body> }
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct CallProcedureStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub optional: bool,
    pub name: ObjectName,
    pub args: Vec<Expr>,
    pub yield_items: Option<Vec<YieldItem>>,
}

/// Variable import scope for an inline procedure call.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum InlineProcedureScope {
    /// No scope clause was written; the full outer scope is visible.
    ImplicitAll,
    /// A scope clause was written; only these variables are visible.
    Explicit(Vec<String>),
}

/// Inline procedure call: [OPTIONAL] CALL { <statements> }
///
/// When `use_graph` is `Some`, this models the GQL rule
/// `focusedNestedDataModifyingProcedureSpecification`:
/// `USE GRAPH <name> { <body> }`.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct InlineProcedureCall {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub optional: bool,
    pub use_graph: Option<ObjectName>,
    pub scope: InlineProcedureScope,
    pub body: Box<CompositeQueryExpr>,
}

/// A YIELD item: identifier [AS alias].
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct YieldItem {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub name: String,
    pub alias: Option<String>,
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
