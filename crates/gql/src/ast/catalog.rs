use crate::token::Span;
use crate::types::EdgeDirection;

use super::expr::Expr;
use super::graph_type::GraphTypeDefinition;
use super::program::SessionCommand;
use super::query::{CompositeQueryExpr, IsOrColon};

// ════════════════════════════════════════════════════════════════════════════════
// §9 — Statements (top-level enum)
// ════════════════════════════════════════════════════════════════════════════════

/// A fully-qualified (optionally schema-qualified) object name.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct CreateSchemaStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub if_not_exists: bool,
    pub name: ObjectName,
}

/// DROP SCHEMA [IF EXISTS] <name>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct DropSchemaStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub if_exists: bool,
    pub name: ObjectName,
}

/// CREATE [PROPERTY] GRAPH [IF NOT EXISTS] [OR REPLACE] <name> ...
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct CreateGraphStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct DropGraphStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub property_keyword: bool,
    pub if_exists: bool,
    pub name: ObjectName,
}

/// CREATE [PROPERTY] GRAPH TYPE [IF NOT EXISTS] [OR REPLACE] <name> AS <definition>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct CreateGraphTypeStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct DropGraphTypeStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct InsertStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub graph_name: Option<ObjectName>,
    pub patterns: Vec<InsertPathPattern>,
}

/// An insert path pattern — a sequence of alternating node and edge patterns.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct InsertPathPattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub elements: Vec<InsertElement>,
}

/// An element within an insert pattern.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum InsertElement {
    Node(InsertNodePattern),
    Edge(InsertEdgePattern),
}

/// A node in an INSERT pattern: (var :Label {props})
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct InsertNodePattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub variable: Option<String>,
    pub is_or_colon: Option<IsOrColon>,
    pub labels: Vec<String>,
    pub properties: Vec<PropertySetting>,
}

/// An edge in an INSERT pattern with direction and label.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct InsertEdgePattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub direction: EdgeDirection,
    pub variable: Option<String>,
    pub is_or_colon: Option<IsOrColon>,
    pub labels: Vec<String>,
    pub properties: Vec<PropertySetting>,
}

/// A property assignment: key = value.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct PropertySetting {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub name: String,
    pub value: Expr,
}

/// SET statement with a list of set items.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct SetStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub items: Vec<SetItem>,
}

/// A single SET clause item.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum SetItem {
    /// SET v.prop = expr
    Property {
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
        span: Span,
        variable: String,
        property: String,
        value: Expr,
    },
    /// SET v = expr  (replace all properties)
    AllProperties {
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
        span: Span,
        variable: String,
        value: Expr,
    },
    /// SET v :Label or SET v IS Label
    Label {
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
        span: Span,
        variable: String,
        label: String,
        is_or_colon: IsOrColon,
    },
}

/// REMOVE statement with a list of items.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct RemoveStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub items: Vec<RemoveItem>,
}

/// A single REMOVE clause item.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum RemoveItem {
    /// REMOVE v.prop
    Property {
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
        span: Span,
        variable: String,
        property: String,
    },
    /// REMOVE v :Label or REMOVE v IS Label
    Label {
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
        span: Span,
        variable: String,
        label: String,
        is_or_colon: IsOrColon,
    },
}

/// DELETE [DETACH | NODETACH] <variable-list>
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct DeleteStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub detach: DeleteDetach,
    /// Delete items — each is a value expression (typically a variable
    /// reference, but GQL §13.5 allows any `valueExpression`).
    pub items: Vec<Expr>,
}

/// Whether a DELETE is DETACH, NODETACH, or unspecified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum DeleteDetach {
    Detach,
    NoDetach,
    Unspecified,
}

