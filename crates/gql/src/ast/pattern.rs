use crate::token::Span;
use crate::types::{EdgeDirection, LabelExpr};

use super::catalog::{ObjectName, PropertySetting};
use super::expr::Expr;
use super::query::{GroupOrGroups, IsOrColon, PathOrPaths};
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
