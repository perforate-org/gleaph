use crate::token::Span;
use crate::types::EdgeDirection;

use super::catalog::ObjectName;
use super::expr::Expr;
use super::query::{Keyword, TypedPrefix};

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
