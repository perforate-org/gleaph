//! Core type representations for static type inference.

use crate::Value;
use crate::ast::{Keyword, ValueType};

/// Semantic metadata for a graph node type.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTypeInfo {
    /// Known label sets for this node.
    ///
    /// Each inner `Vec<String>` is an AND-conjunction of labels (e.g. `Person&Admin`).
    /// Multiple entries represent OR-alternatives from narrowing
    /// (e.g. `[["Person"], ["Company"]]` means `:Person OR :Company`).
    ///
    /// A single entry `[["Person"]]` is equivalent to the simple case.
    /// An empty outer vec means no label information.
    pub label_sets: Vec<Vec<String>>,
    /// Schema-known property types: `(name, value_type, required)`.
    pub properties: Vec<(String, ValueType, bool)>,
}

impl NodeTypeInfo {
    pub fn from_labels(labels: Vec<String>) -> Self {
        let label_sets = if labels.is_empty() {
            Vec::new()
        } else {
            vec![labels]
        };
        Self {
            label_sets,
            properties: Vec::new(),
        }
    }

    /// Convenience: return the first (or only) label set, or empty.
    pub fn primary_labels(&self) -> &[String] {
        self.label_sets.first().map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Check if there's any label information at all.
    pub fn has_labels(&self) -> bool {
        !self.label_sets.is_empty()
    }

    /// Flat list of all unique labels across all sets (for schema lookups).
    pub fn all_labels_flat(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ls in &self.label_sets {
            for l in ls {
                if !out.contains(l) {
                    out.push(l.clone());
                }
            }
        }
        out
    }
}

/// Semantic metadata for a graph edge type.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTypeInfo {
    /// Known edge label (if any).
    pub label: Option<String>,
    /// Schema-known endpoint constraints: `(from_labels, to_labels)` pairs.
    pub endpoints: Vec<(Vec<String>, Vec<String>)>,
    /// Schema-known property types: `(name, value_type, required)`.
    pub properties: Vec<(String, ValueType, bool)>,
}

impl EdgeTypeInfo {
    pub fn from_label(label: Option<String>) -> Self {
        Self {
            label,
            endpoints: Vec::new(),
            properties: Vec::new(),
        }
    }
}

/// Semantic metadata for a path type.
#[derive(Clone, Debug, PartialEq)]
pub struct PathTypeInfo {
    pub min_hops: Option<u32>,
    pub max_hops: Option<u32>,
}

impl PathTypeInfo {
    pub fn unbounded() -> Self {
        Self {
            min_hops: None,
            max_hops: None,
        }
    }
}

impl Default for PathTypeInfo {
    fn default() -> Self {
        Self::unbounded()
    }
}

/// Inferred type for a GQL expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    /// A concrete scalar type — reuses `ast::ValueType` directly.
    Scalar(ValueType),
    /// Union of possible types (from CASE/COALESCE).
    Union(Vec<Type>),
    /// List with known element type.
    TypedList(Box<Type>),
    /// Cannot be determined statically — suppresses all warnings.
    Unknown,
    /// Bottom type: expression can never produce a value.
    Never,
    /// Graph node with semantic metadata.
    Node(NodeTypeInfo),
    /// Graph edge with semantic metadata.
    Edge(EdgeTypeInfo),
    /// A path value with optional bounds.
    Path(PathTypeInfo),
    /// Record with known field types.
    Record(Vec<(String, Type)>),
    /// NOT NULL wrapper — the inner type is provably non-nullable.
    NonNull(Box<Type>),
}

// ── ValueType → Type conversion ──

impl Type {
    /// Convert an AST-level `ValueType` to an inferred `Type`.
    pub fn from_value_type(vt: &ValueType) -> Self {
        match vt {
            ValueType::NotNull(inner) => Type::NonNull(Box::new(Type::from_value_type(inner))),
            ValueType::List { element_type, .. } => {
                Type::TypedList(Box::new(Type::from_value_type(element_type)))
            }
            ValueType::Record { fields, .. } => {
                let typed = fields
                    .iter()
                    .map(|f| (f.name.clone(), Type::from_value_type(&f.value_type)))
                    .collect();
                Type::Record(typed)
            }
            ValueType::ClosedDynamicUnion(variants) => {
                let types = variants.iter().map(Type::from_value_type).collect();
                make_union(types)
            }
            ValueType::Any | ValueType::AnyValue | ValueType::AnyPropertyValue => Type::Unknown,
            ValueType::Nothing => Type::Never,
            ValueType::Path => Type::Path(PathTypeInfo::default()),
            other => Type::Scalar(other.clone()),
        }
    }
}

// ── Literal → Type inference ──

/// Infer the type of a literal value.
pub fn infer_literal(v: &Value) -> Type {
    match v {
        Value::Null => Type::Scalar(ValueType::Null),
        Value::Bool(_) => Type::Scalar(ValueType::Bool {
            keyword: Keyword::new("BOOL"),
        }),
        Value::Int8(_) => Type::Scalar(ValueType::Int8 {
            keyword: Keyword::new("INT8"),
        }),
        Value::Int16(_) => Type::Scalar(ValueType::Int16 {
            keyword: Keyword::new("INT16"),
        }),
        Value::Int32(_) => Type::Scalar(ValueType::Int32 {
            keyword: Keyword::new("INT32"),
        }),
        Value::Int64(_) => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        Value::Int128(_) => Type::Scalar(ValueType::Int128 {
            keyword: Keyword::new("INT128"),
        }),
        Value::Int256(_) => Type::Scalar(ValueType::Int256 {
            keyword: Keyword::new("INT256"),
        }),
        Value::Uint8(_) => Type::Scalar(ValueType::Uint8 {
            keyword: Keyword::new("UINT8"),
        }),
        Value::Uint16(_) => Type::Scalar(ValueType::Uint16 {
            keyword: Keyword::new("UINT16"),
        }),
        Value::Uint32(_) => Type::Scalar(ValueType::Uint32 {
            keyword: Keyword::new("UINT32"),
        }),
        Value::Uint64(_) => Type::Scalar(ValueType::Uint64 {
            keyword: Keyword::new("UINT64"),
        }),
        Value::Uint128(_) => Type::Scalar(ValueType::Uint128 {
            keyword: Keyword::new("UINT128"),
        }),
        Value::Uint256(_) => Type::Scalar(ValueType::Uint256 {
            keyword: Keyword::new("UINT256"),
        }),
        Value::Float16(_) => Type::Scalar(ValueType::Float16 {
            keyword: Keyword::new("FLOAT16"),
        }),
        Value::Float32(_) => Type::Scalar(ValueType::Float32 {
            keyword: Keyword::new("FLOAT32"),
        }),
        Value::Float64(_) => Type::Scalar(ValueType::Float64 {
            keyword: Keyword::new("FLOAT64"),
        }),
        #[cfg(feature = "f128")]
        Value::Float128(_) => Type::Scalar(ValueType::Float128),
        #[cfg(feature = "f256")]
        Value::Float256(_) => Type::Scalar(ValueType::Float256),
        Value::Decimal(_) => Type::Scalar(ValueType::Decimal {
            keyword: Keyword::new("DECIMAL"),
            precision: None,
            scale: None,
        }),
        Value::Text(_) => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        Value::Bytes(_) => Type::Scalar(ValueType::Bytes { max_length: None }),
        Value::Date(_) => Type::Scalar(ValueType::Date),
        Value::Time(_) => Type::Scalar(ValueType::Time),
        Value::LocalTime(_) => Type::Scalar(ValueType::LocalTime {
            keyword: Keyword::new("LOCAL TIME"),
        }),
        Value::DateTime(_, _) => Type::Scalar(ValueType::DateTime),
        Value::LocalDateTime(_, _) => Type::Scalar(ValueType::LocalDateTime {
            keyword: Keyword::new("LOCAL DATETIME"),
        }),
        Value::ZonedDateTime(_, _, _) => Type::Scalar(ValueType::ZonedDateTime {
            keyword: Keyword::new("ZONED DATETIME"),
        }),
        Value::ZonedTime(_, _) => Type::Scalar(ValueType::ZonedTime {
            keyword: Keyword::new("ZONED TIME"),
        }),
        Value::Duration(_, _) => Type::Scalar(ValueType::Duration),
        Value::List(_) => Type::TypedList(Box::new(Type::Unknown)),
        Value::Path(_) => Type::Path(PathTypeInfo::default()),
        Value::Record(_) => Type::Unknown,
        Value::Extension(_) => Type::Unknown,
    }
}

// ── Type helpers ──

pub fn is_unknown(t: &Type) -> bool {
    match t {
        Type::Unknown => true,
        Type::Union(variants) => variants.iter().any(is_unknown),
        _ => false,
    }
}

pub fn is_never(t: &Type) -> bool {
    matches!(t, Type::Never)
}

pub fn is_null(t: &Type) -> bool {
    matches!(t, Type::Scalar(ValueType::Null))
}

pub fn is_numeric(t: &Type) -> bool {
    match t {
        Type::Scalar(vt) => is_numeric_vt(vt),
        _ => false,
    }
}

pub fn scalar_type(t: &Type) -> Option<&ValueType> {
    match t {
        Type::Scalar(vt) => Some(vt),
        Type::NonNull(inner) => scalar_type(inner),
        _ => None,
    }
}

pub fn is_numeric_vt(vt: &ValueType) -> bool {
    is_integer_vt(vt) || is_float_vt(vt)
}

pub fn is_unsigned_vt(vt: &ValueType) -> bool {
    matches!(
        vt,
        ValueType::Uint8 { .. }
            | ValueType::Uint16 { .. }
            | ValueType::Uint32 { .. }
            | ValueType::Uint64 { .. }
            | ValueType::Uint128 { .. }
            | ValueType::Uint256 { .. }
            | ValueType::UintPrecision { .. }
    )
}

pub fn is_integer_vt(vt: &ValueType) -> bool {
    matches!(
        vt,
        ValueType::Int8 { .. }
            | ValueType::Int16 { .. }
            | ValueType::Int32 { .. }
            | ValueType::Int64 { .. }
            | ValueType::Int128 { .. }
            | ValueType::Int256 { .. }
            | ValueType::IntPrecision { .. }
            | ValueType::Uint8 { .. }
            | ValueType::Uint16 { .. }
            | ValueType::Uint32 { .. }
            | ValueType::Uint64 { .. }
            | ValueType::Uint128 { .. }
            | ValueType::Uint256 { .. }
            | ValueType::UintPrecision { .. }
    )
}

pub fn is_float_vt(vt: &ValueType) -> bool {
    matches!(
        vt,
        ValueType::Float16 { .. }
            | ValueType::Float32 { .. }
            | ValueType::Float64 { .. }
            | ValueType::Float128
            | ValueType::Float256
            | ValueType::FloatPrecision { .. }
            | ValueType::Decimal { .. }
    )
}

pub fn is_string_vt(vt: &ValueType) -> bool {
    matches!(
        vt,
        ValueType::String { .. } | ValueType::Char { .. } | ValueType::Varchar { .. }
    )
}

pub fn is_temporal_vt(vt: &ValueType) -> bool {
    matches!(
        vt,
        ValueType::Date
            | ValueType::Time
            | ValueType::LocalTime { .. }
            | ValueType::ZonedTime { .. }
            | ValueType::DateTime
            | ValueType::LocalDateTime { .. }
            | ValueType::ZonedDateTime { .. }
            | ValueType::Timestamp
    )
}

pub fn is_duration_vt(vt: &ValueType) -> bool {
    matches!(
        vt,
        ValueType::Duration | ValueType::DurationYearToMonth | ValueType::DurationDayToSecond
    )
}

/// Return the bit width of an integer `ValueType`.
fn int_width(vt: &ValueType) -> u16 {
    match vt {
        ValueType::Int8 { .. } | ValueType::Uint8 { .. } => 8,
        ValueType::Int16 { .. } | ValueType::Uint16 { .. } => 16,
        ValueType::Int32 { .. } | ValueType::Uint32 { .. } => 32,
        ValueType::Int64 { .. } | ValueType::Uint64 { .. } => 64,
        ValueType::Int128 { .. } | ValueType::Uint128 { .. } => 128,
        ValueType::Int256 { .. } | ValueType::Uint256 { .. } => 256,
        _ => 64,
    }
}

/// Given two integer ValueTypes, return the wider one.
pub fn wider_int_vt(a: &ValueType, b: &ValueType) -> ValueType {
    if int_width(a) >= int_width(b) {
        a.clone()
    } else {
        b.clone()
    }
}

/// Strip any NonNull wrapper.
pub fn strip_nonnull(t: Type) -> Type {
    match t {
        Type::NonNull(inner) => *inner,
        other => other,
    }
}

/// Ensure a type is wrapped in NonNull.
pub fn ensure_nonnull(t: Type) -> Type {
    match &t {
        Type::NonNull(_) | Type::Unknown | Type::Never => t,
        _ => Type::NonNull(Box::new(t)),
    }
}

/// Unwrap NonNull for comparison/inspection purposes (by reference).
pub fn unwrap_nonnull(t: &Type) -> &Type {
    match t {
        Type::NonNull(inner) => unwrap_nonnull(inner),
        other => other,
    }
}

/// Flatten a type into its constituent non-union variants.
pub fn flatten_union(t: &Type) -> Vec<&Type> {
    match t {
        Type::Union(variants) => variants.iter().collect(),
        other => vec![other],
    }
}

/// Build a union type, collapsing identical types and handling Never/Unknown.
pub fn make_union(types: Vec<Type>) -> Type {
    let types: Vec<Type> = types.into_iter().filter(|t| !is_never(t)).collect();
    if types.is_empty() {
        return Type::Never;
    }
    let first = &types[0];
    if types.iter().all(|t| t == first) {
        return first.clone();
    }
    if types.iter().any(is_unknown) {
        return Type::Unknown;
    }
    Type::Union(types)
}

/// Check if two types can be added together.
pub fn types_addable(a: &Type, b: &Type) -> bool {
    match (scalar_type(a), scalar_type(b)) {
        (Some(va), Some(vb)) if is_string_vt(va) && is_string_vt(vb) => true,
        (Some(va), Some(vb)) if is_numeric_vt(va) && is_numeric_vt(vb) => true,
        (Some(va), Some(vb)) if is_temporal_vt(va) && is_duration_vt(vb) => true,
        (Some(va), Some(vb)) if is_duration_vt(va) && is_temporal_vt(vb) => true,
        (Some(va), Some(vb)) if is_duration_vt(va) && is_duration_vt(vb) => true,
        _ => matches!((a, b), (Type::TypedList(_), Type::TypedList(_))),
    }
}

/// Check if two types support arithmetic (sub/mul/div/mod).
pub fn types_arithmetic(a: &Type, b: &Type, is_sub: bool) -> bool {
    match (scalar_type(a), scalar_type(b)) {
        (Some(va), Some(vb)) if is_numeric_vt(va) && is_numeric_vt(vb) => true,
        // Duration * Numeric, Duration / Numeric
        (Some(va), Some(vb)) if is_duration_vt(va) && is_numeric_vt(vb) => true,
        // Numeric * Duration (but NOT Numeric - Duration or Numeric / Duration)
        (Some(va), Some(vb)) if is_numeric_vt(va) && is_duration_vt(vb) => !is_sub,
        _ if is_sub => {
            matches!(
                (scalar_type(a), scalar_type(b)),
                (Some(va), Some(vb))
                    if (is_temporal_vt(va) && is_temporal_vt(vb))
                    || (is_temporal_vt(va) && is_duration_vt(vb))
                    || (is_duration_vt(va) && is_duration_vt(vb))
            )
        }
        _ => false,
    }
}

/// Check if two types can be compared.
pub fn types_comparable(a: &Type, b: &Type) -> bool {
    let a = unwrap_nonnull(a);
    let b = unwrap_nonnull(b);
    match (a, b) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Scalar(ValueType::Null), _) | (_, Type::Scalar(ValueType::Null)) => true,
        (Type::Scalar(va), Type::Scalar(vb)) => {
            // Same variant family, or both numeric.
            std::mem::discriminant(va) == std::mem::discriminant(vb)
                || (is_numeric_vt(va) && is_numeric_vt(vb))
        }
        (Type::Node(_), Type::Node(_)) | (Type::Edge(_), Type::Edge(_)) => true,
        _ => false,
    }
}

/// Coarse category check: are two types in the same broad category?
/// Node vs Node → true, Node vs Edge → false, String vs Int → false.
/// Used for variable redefinition: only warn when categories differ.
pub fn types_broadly_same_category(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Never, _) | (_, Type::Never) => true,
        (Type::Node(_), Type::Node(_)) => true,
        (Type::Edge(_), Type::Edge(_)) => true,
        (Type::Path(_), Type::Path(_)) => true,
        (Type::TypedList(_), Type::TypedList(_)) => true,
        (Type::Record(_), Type::Record(_)) => true,
        (Type::NonNull(inner), other) | (other, Type::NonNull(inner)) => {
            types_broadly_same_category(inner, other)
        }
        (Type::Scalar(va), Type::Scalar(vb)) => {
            std::mem::discriminant(va) == std::mem::discriminant(vb)
                || (is_numeric_vt(va) && is_numeric_vt(vb))
                || (is_string_vt(va) && is_string_vt(vb))
        }
        _ => false,
    }
}

/// Structural equality for types — used to detect variable redefinition.
/// Ignores keyword metadata on ValueType variants.
pub fn types_structurally_eq(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::Unknown, Type::Unknown) => true,
        (Type::Never, Type::Never) => true,
        (Type::Scalar(va), Type::Scalar(vb)) => {
            std::mem::discriminant(va) == std::mem::discriminant(vb)
        }
        (Type::NonNull(a), Type::NonNull(b)) => types_structurally_eq(a, b),
        (Type::TypedList(a), Type::TypedList(b)) => types_structurally_eq(a, b),
        (Type::Node(a), Type::Node(b)) => a.label_sets == b.label_sets,
        (Type::Edge(a), Type::Edge(b)) => a.label == b.label,
        (Type::Path(_), Type::Path(_)) => true,
        (Type::Record(a), Type::Record(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && types_structurally_eq(va, vb))
        }
        (Type::Union(a), Type::Union(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(a, b)| types_structurally_eq(a, b))
        }
        _ => false,
    }
}
