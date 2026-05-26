use crate::Value;
use crate::token::Span;
use crate::types::{EdgeDirection, LabelExpr};

use super::catalog::{ObjectName, PropertySetting};
use super::query::{
    BindingTypeAnnotation, CompositeQueryExpr, GroupOrGroups, IsOrColon, Keyword, LetBinding,
    LinearQueryStatement, OrderByClause, PathOrPaths, SetOp, SimpleQueryStatement, TypedPrefix,
};

// ════════════════════════════════════════════════════════════════════════════════
// §16 — Graph pattern
// ════════════════════════════════════════════════════════════════════════════════

/// A graph pattern: [match-mode] <path-patterns> [KEEP <clause>] [WHERE <cond>]
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
pub struct GraphPattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub match_mode: Option<MatchMode>,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub paths: Vec<PathPattern>,
    pub keep: Option<KeepClause>,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub where_clause: Option<Expr>,
}

/// Match mode for a graph pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct KeepClause {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub prefix: PathPatternPrefix,
}

// ──── Path pattern (§16.3) ────

/// Vendor extension clause attached to a path pattern: `<name> BY <expr>`.
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
pub struct PathPatternExtension {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub name: ObjectName,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub expr: Expr,
}

/// A path pattern: [<var> =] [<prefix>] <path-expr> [<extension>]*
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
pub struct PathPattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    /// Optional path variable assigned with `=`.
    pub variable: Option<String>,
    /// Optional path prefix (mode or search).
    pub prefix: Option<PathPatternPrefix>,
    /// The path expression itself.
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub expr: PathPatternExpr,
    /// Optional vendor extension clauses following the path expression.
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub extensions: Vec<PathPatternExtension>,
}

/// A path pattern prefix — either a path mode or a search prefix.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum PathPatternPrefix {
    Mode {
        mode: PathMode,
        path_keyword: Option<PathOrPaths>,
    },
    Search(SearchPrefix),
}

/// Path traversal mode (§16.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum PathMode {
    Walk,
    Trail,
    Simple,
    Acyclic,
}

/// Search prefix for path patterns.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct PathTerm {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub factors: Vec<PathFactor>,
}

/// A path factor is a path primary with an optional quantifier.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct PathFactor {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub primary: PathPrimary,
    pub quantifier: Option<PathQuantifier>,
}

/// A path primary — the atomic building block of a path pattern.
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
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
        expr: Box<PathPatternExpr>,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
        where_clause: Option<Box<Expr>>,
    },
    /// A simplified path pattern (e.g., `->`, `-[:KNOWS]->`, etc.).
    Simplified(SimplifiedPathPattern),
}

/// Path quantifier (§16.7).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct NodePattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct EdgePattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
pub struct SimplifiedPathPattern {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub elements: Vec<SimplifiedElement>,
}

/// An element in a simplified path pattern.
///
/// Represents one `opening_slash contents closing_slash` unit.
/// The `direction` comes from the slash pair; contents holds the full
/// simplified expression tree (which may itself contain quantifiers,
/// conjunction, union, etc.).
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
pub struct SimplifiedElement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub direction: EdgeDirection,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
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
pub enum SimplifiedContents {
    /// A single label name or wildcard (`%`).
    Label(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] LabelExpr),
    /// Negation: `!primary`
    Negation(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>),
    /// Conjunction: `a & b`
    Conjunction(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
    ),
    /// Union: `a | b`
    Union(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
    ),
    /// Multiset alternation: `a |+| b`
    MultisetAlternation(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
    ),
    /// Concatenation (juxtaposition of terms): `a b`
    Concatenation(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
    ),
    /// Quantified: `a+`, `a{2,5}`, `a?`
    Quantified(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
        PathQuantifier,
    ),
    /// Direction override on a sub-expression (e.g., `<KNOWS`, `~LIKES`).
    DirectionOverride(
        EdgeDirection,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>,
    ),
    /// Parenthesized group: `(simplifiedContents)`
    Group(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<SimplifiedContents>),
}

// ════════════════════════════════════════════════════════════════════════════════
// §18 — Graph type definitions
// ════════════════════════════════════════════════════════════════════════════════

/// A graph type definition containing node and edge type definitions.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct GraphTypeDefinition {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub elements: Vec<GraphTypeElement>,
}

/// An element of a graph type definition.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct KeyLabelSet {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    /// Whether `LABEL` (false) or `LABELS` (true) keyword was used.
    pub label_keyword_plural: bool,
    pub labels: Vec<String>,
}

/// A node type definition.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct NodeTypeDef {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct EdgeTypeDef {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct EdgeEndpoint {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    /// The node type name or label this endpoint connects to.
    pub label: Option<String>,
    /// The node type reference name.
    pub type_name: Option<String>,
}

/// A property definition within a node or edge type.
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
pub struct PropertyDef {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub name: String,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
    pub value_type: ValueType,
    pub not_null: bool,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
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
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
        element_type: Box<ValueType>,
        max_length: Option<u64>,
    },
    /// PATH
    Path,
    /// [RECORD] { fields... }
    Record {
        record_keyword: bool,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
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
    ClosedDynamicUnion(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Vec<ValueType>),

    // — Host extension type —
    /// Host-defined scalar/type name accepted by parser and resolved by host runtime.
    ExtensionType { name: ObjectName },

    // — NOT NULL wrapper —
    /// A value type with a NOT NULL constraint.
    NotNull(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<ValueType>),
}

/// A field in a RECORD type definition.
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
pub struct RecordFieldType {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub name: String,
    /// How the type was introduced (`::`, `TYPED`, or none).
    pub typed_prefix: TypedPrefix,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
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
pub struct Expr {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
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
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))]
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
    ExistsSubquery(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<CompositeQueryExpr>,
    ),
    /// EXISTS { <pattern> }
    ExistsPattern(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<GraphPattern>),

    // ── Value subquery (§20.15) ──
    /// VALUE { <subquery> }
    ValueSubquery(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<CompositeQueryExpr>,
    ),

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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
}

// ──── Unary operators ──

/// Unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum UnaryOp {
    /// Unary negation `-`
    Neg,
    /// Unary positive `+`
    Pos,
}

// ──── Comparison operators ──

/// Comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    all(feature = "ast-rkyv-no-span", feature = "cypher"),
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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

/// When `cypher` is disabled, [`StringPredicateKind`] is an empty enum; rkyv cannot derive
/// `Archive` for it, so we archive it as `()` and reject deserialization (the AST variant is
/// unreachable at runtime).
#[cfg(all(feature = "ast-rkyv-no-span", not(feature = "cypher")))]
impl rkyv::Archive for StringPredicateKind {
    type Archived = ();
    type Resolver = ();

    fn resolve(&self, _: Self::Resolver, _: rkyv::Place<Self::Archived>) {
        match *self {}
    }
}

#[cfg(all(feature = "ast-rkyv-no-span", not(feature = "cypher")))]
impl<S: rkyv::rancor::Fallible + ?Sized> rkyv::Serialize<S> for StringPredicateKind {
    fn serialize(&self, _: &mut S) -> Result<Self::Resolver, S::Error> {
        match *self {}
    }
}

#[cfg(all(feature = "ast-rkyv-no-span", not(feature = "cypher")))]
#[derive(Debug)]
struct StringPredicateKindDeserializeError;

#[cfg(all(feature = "ast-rkyv-no-span", not(feature = "cypher")))]
impl std::fmt::Display for StringPredicateKindDeserializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot deserialize StringPredicateKind without cypher feature"
        )
    }
}

#[cfg(all(feature = "ast-rkyv-no-span", not(feature = "cypher")))]
impl std::error::Error for StringPredicateKindDeserializeError {}

#[cfg(all(feature = "ast-rkyv-no-span", not(feature = "cypher")))]
impl<D: rkyv::rancor::Fallible + ?Sized> rkyv::Deserialize<StringPredicateKind, D> for ()
where
    D::Error: rkyv::rancor::Source,
{
    fn deserialize(&self, _: &mut D) -> Result<StringPredicateKind, D::Error> {
        rkyv::rancor::fail!(StringPredicateKindDeserializeError);
    }
}

// ──── Truth values ──

/// Truth value for IS TRUE / IS FALSE / IS UNKNOWN tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum TruthValue {
    True,
    False,
    Unknown,
}

/// Qualifier on a duration expression such as `YEAR TO MONTH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum DurationQualifier {
    YearToMonth,
    DayToSecond,
}

// ──── Normal form ──

/// Unicode normalization form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum NormalForm {
    NFC,
    NFD,
    NFKC,
    NFKD,
}

// ──── Trim specification ──

/// Trim direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum TrimSpec {
    Leading,
    Trailing,
    Both,
}

// ──── String fold kind ──

/// String fold/trim variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum StringFoldKind {
    BTrim,
    LTrim,
    RTrim,
}

// ──── Aggregate functions ──

/// Aggregate function identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
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
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct WhenClause {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
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
