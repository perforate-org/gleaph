use crate::Value;
use crate::token::Span;
use crate::types::LabelExpr;

use super::catalog::ObjectName;
use super::graph_type::ValueType;
use super::pattern::{GraphPattern, NodePattern, PathPattern};
use super::query::{CompositeQueryExpr, Keyword, LetBinding, OrderByClause};

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
    /// TIME( args... ) — alias for LOCAL_TIME(...)
    TimeFunction(Vec<Expr>),
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
