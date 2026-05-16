//! Core type definitions for the GQL crate.
//!
//! These are GQL-centric types, independent of any specific platform (IC, etc.).

use std::{fmt, ops::Deref};

// ──── 256-bit integer wrappers ────

fn u256_decimal_digit_count(n: ethnum::U256) -> u64 {
    let ten = ethnum::U256::from(10u8);
    let zero = ethnum::U256::ZERO;
    let mut n = n;
    let mut digits = 0;
    while n > zero {
        digits += 1;
        n /= ten;
    }
    digits
}

/// 256-bit signed integer wrapping [`ethnum::I256`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Int256(pub ethnum::I256);

impl Int256 {
    pub fn new(v: ethnum::I256) -> Self {
        Self(v)
    }

    pub fn is_zero(self) -> bool {
        self.0 == ethnum::I256::ZERO
    }

    pub fn unsigned_decimal_digit_count(self) -> u64 {
        if self.is_zero() {
            return 1;
        }
        // unsigned_abs() is panic-safe on I256::MIN (magnitude 2^255).
        u256_decimal_digit_count(self.0.unsigned_abs())
    }

    pub fn parse(s: &str) -> Option<Self> {
        s.parse::<ethnum::I256>().ok().map(Int256)
    }
}

impl std::str::FromStr for Int256 {
    type Err = <ethnum::I256 as std::str::FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<ethnum::I256>().map(Int256)
    }
}

impl fmt::Display for Int256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 256-bit unsigned integer wrapping [`ethnum::U256`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uint256(pub ethnum::U256);

impl Uint256 {
    pub fn new(v: ethnum::U256) -> Self {
        Self(v)
    }

    pub fn is_zero(self) -> bool {
        self.0 == ethnum::U256::ZERO
    }

    pub fn unsigned_decimal_digit_count(self) -> u64 {
        if self.is_zero() {
            return 1;
        }
        u256_decimal_digit_count(self.0)
    }

    pub fn parse(s: &str) -> Option<Self> {
        s.parse::<ethnum::U256>().ok().map(Uint256)
    }
}

impl std::str::FromStr for Uint256 {
    type Err = <ethnum::U256 as std::str::FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<ethnum::U256>().map(Uint256)
    }
}

impl fmt::Display for Uint256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ──── Decimal wrapper ────

/// Fixed-point decimal wrapping [`rust_decimal::Decimal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Decimal(pub rust_decimal::Decimal);

impl Decimal {
    pub fn new(d: rust_decimal::Decimal) -> Self {
        Self(d)
    }

    pub fn parse(s: &str) -> Option<Self> {
        rust_decimal::Decimal::from_str_exact(s).ok().map(Decimal)
    }

    pub fn to_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.0.to_f64()
    }

    pub fn from_i64(v: i64) -> Self {
        Self(rust_decimal::Decimal::from(v))
    }

    pub fn from_u64(v: u64) -> Self {
        Self(rust_decimal::Decimal::from(v))
    }

    pub fn from_i128(v: i128) -> Self {
        Self(rust_decimal::Decimal::from(v))
    }

    pub fn from_u128(v: u128) -> Self {
        Self(rust_decimal::Decimal::from(v))
    }

    pub fn from_f64(v: f64) -> Option<Self> {
        use rust_decimal::prelude::FromPrimitive;
        if !v.is_finite() {
            return None;
        }
        rust_decimal::Decimal::from_f64(v).map(Decimal)
    }

    pub fn trunc_to_i128(self) -> Option<i128> {
        use rust_decimal::prelude::ToPrimitive;
        self.0.trunc().to_i128()
    }

    pub fn trunc_to_u128(self) -> Option<u128> {
        use rust_decimal::prelude::ToPrimitive;
        let truncated = self.0.trunc();
        if truncated.is_sign_negative() {
            return None;
        }
        truncated.to_u128()
    }

    pub fn normalize(&self) -> Self {
        Self(self.0.normalize())
    }

    pub fn round_to_scale(self, scale: u32) -> Self {
        Self(self.0.round_dp(scale))
    }

    /// Whether this value fits `DECIMAL(precision, scale)` / `FLOAT(precision, scale)` digit rules.
    pub fn fits_sql_precision_scale(self, precision: u64, scale: u64) -> bool {
        if precision == 0 {
            return self.0.is_zero();
        }
        if scale > precision {
            return false;
        }
        let rounded = self.0.round_dp(scale as u32);
        if rounded.scale() > scale as u32 {
            return false;
        }
        if sql_mantissa_digit_count(&rounded) as u64 > precision {
            return false;
        }
        sql_integer_mantissa_digit_count(&rounded) as u64 <= precision - scale
    }
}

fn sql_mantissa_digit_count(decimal: &rust_decimal::Decimal) -> usize {
    if decimal.is_zero() {
        return 1;
    }
    let mantissa = decimal.mantissa().unsigned_abs();
    (mantissa.ilog10() as usize) + 1
}

fn sql_integer_mantissa_digit_count(decimal: &rust_decimal::Decimal) -> usize {
    let truncated = decimal.trunc().abs();
    if truncated.is_zero() {
        return 0;
    }
    sql_mantissa_digit_count(&truncated)
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ──── Label expression ────

/// A label expression used in node and edge patterns.
///
/// Supports AND (`&`), OR (`|`), NOT (`!`), wildcard (`%`), and plain names.
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
pub enum LabelExpr {
    /// A single label name, e.g. `:Person`
    Name(String),
    /// Wildcard `%` -- matches any entity that has at least one label.
    Wildcard,
    /// AND expression `A&B` -- entity must have both labels.
    And(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<LabelExpr>,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<LabelExpr>,
    ),
    /// OR expression `A|B` -- entity must have at least one of the labels.
    Or(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<LabelExpr>,
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<LabelExpr>,
    ),
    /// NOT expression `!A` -- entity must not have the label.
    Not(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Box<LabelExpr>),
}

/// Evaluate a label expression against a single edge label string.
///
/// An edge has exactly one label (or none). For `And`, both sides must accept
/// that same label. `Wildcard` accepts any `Some(_)` label.
pub fn matches_edge_label(expr: &LabelExpr, edge_label: Option<&str>) -> bool {
    match expr {
        LabelExpr::Name(name) => edge_label.is_some_and(|l| l == name),
        LabelExpr::Wildcard => edge_label.is_some(),
        LabelExpr::And(a, b) => {
            matches_edge_label(a, edge_label) && matches_edge_label(b, edge_label)
        }
        LabelExpr::Or(a, b) => {
            matches_edge_label(a, edge_label) || matches_edge_label(b, edge_label)
        }
        LabelExpr::Not(e) => !matches_edge_label(e, edge_label),
    }
}

// ──── Entity type ────

/// Specifies whether a pattern element refers to a vertex or an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum EntityType {
    Vertex,
    Edge,
}

// ──── Path element ────

/// Opaque runtime-owned identifier for an element in a path result.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct PathElementId(Box<[u8]>);

impl PathElementId {
    pub fn into_vec(self) -> Vec<u8> {
        self.0.into_vec()
    }
}

impl From<Vec<u8>> for PathElementId {
    fn from(value: Vec<u8>) -> Self {
        Self(value.into_boxed_slice())
    }
}

impl From<Box<[u8]>> for PathElementId {
    fn from(value: Box<[u8]>) -> Self {
        Self(value)
    }
}

impl<const N: usize> From<[u8; N]> for PathElementId {
    fn from(value: [u8; N]) -> Self {
        Self(Box::from(value))
    }
}

impl AsRef<[u8]> for PathElementId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Deref for PathElementId {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// An element along a path result: alternating vertices and edges.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum PathElement {
    Vertex(PathElementId),
    Edge(PathElementId),
}

// ──── Edge direction ────

/// All seven edge directions defined in GQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum EdgeDirection {
    /// `->` or `~>`
    PointingRight,
    /// `<-` or `<~`
    PointingLeft,
    /// `<->` or `<~>`
    LeftOrRight,
    /// `-` or `~`
    Undirected,
    /// `<~[…]~` — left or undirected
    LeftOrUndirected,
    /// `~[…]~>` — undirected or right
    UndirectedOrRight,
    /// `-[…]-`, `-` — any direction
    AnyDirection,
}

// ──── Narrow helpers ────

/// Narrow an i128 to the smallest signed integer of the given width.
/// Returns `None` on overflow.
pub fn narrow_signed(v: i128, width: u16) -> Option<crate::Value> {
    use crate::Value;
    match width {
        8 => i8::try_from(v).ok().map(Value::Int8),
        16 => i16::try_from(v).ok().map(Value::Int16),
        32 => i32::try_from(v).ok().map(Value::Int32),
        64 => i64::try_from(v).ok().map(Value::Int64),
        128 => Some(Value::Int128(v)),
        256 => Some(Value::Int256(Int256::new(ethnum::I256::from(v)))),
        _ => None,
    }
}

/// Narrow a u128 to the smallest unsigned integer of the given width.
/// Returns `None` on overflow.
pub fn narrow_unsigned(v: u128, width: u16) -> Option<crate::Value> {
    use crate::Value;
    match width {
        8 => u8::try_from(v).ok().map(Value::Uint8),
        16 => u16::try_from(v).ok().map(Value::Uint16),
        32 => u32::try_from(v).ok().map(Value::Uint32),
        64 => u64::try_from(v).ok().map(Value::Uint64),
        128 => Some(Value::Uint128(v)),
        256 => Some(Value::Uint256(Uint256::new(ethnum::U256::from(v)))),
        _ => None,
    }
}

// ──── rkyv remote defs (`ast-rkyv-no-span` feature) ────

#[cfg(feature = "ast-rkyv-no-span")]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(remote = Int256)]
pub(crate) struct Int256Def(#[rkyv(getter = int256_le_bytes)] [u8; 32]);

#[cfg(feature = "ast-rkyv-no-span")]
fn int256_le_bytes(i: &Int256) -> [u8; 32] {
    i.0.to_le_bytes()
}

#[cfg(feature = "ast-rkyv-no-span")]
impl From<Int256Def> for Int256 {
    fn from(Int256Def(bytes): Int256Def) -> Self {
        Int256(ethnum::I256::from_le_bytes(bytes))
    }
}

#[cfg(feature = "ast-rkyv-no-span")]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(remote = Uint256)]
pub(crate) struct Uint256Def(#[rkyv(getter = uint256_le_bytes)] [u8; 32]);

#[cfg(feature = "ast-rkyv-no-span")]
fn uint256_le_bytes(i: &Uint256) -> [u8; 32] {
    i.0.to_le_bytes()
}

#[cfg(feature = "ast-rkyv-no-span")]
impl From<Uint256Def> for Uint256 {
    fn from(Uint256Def(bytes): Uint256Def) -> Self {
        Uint256(ethnum::U256::from_le_bytes(bytes))
    }
}

#[cfg(feature = "ast-rkyv-no-span")]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(remote = Decimal)]
pub(crate) struct DecimalDef(#[rkyv(getter = decimal_le_bytes)] [u8; 16]);

#[cfg(feature = "ast-rkyv-no-span")]
fn decimal_le_bytes(d: &Decimal) -> [u8; 16] {
    d.0.serialize()
}

#[cfg(feature = "ast-rkyv-no-span")]
impl From<DecimalDef> for Decimal {
    fn from(DecimalDef(bytes): DecimalDef) -> Self {
        Decimal(rust_decimal::Decimal::deserialize(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_expr_matching() {
        let expr = LabelExpr::Or(
            Box::new(LabelExpr::Name("KNOWS".into())),
            Box::new(LabelExpr::Name("LIKES".into())),
        );
        assert!(matches_edge_label(&expr, Some("KNOWS")));
        assert!(matches_edge_label(&expr, Some("LIKES")));
        assert!(!matches_edge_label(&expr, Some("HATES")));
        assert!(!matches_edge_label(&expr, None));
    }

    #[test]
    fn label_wildcard() {
        assert!(matches_edge_label(&LabelExpr::Wildcard, Some("ANY")));
        assert!(!matches_edge_label(&LabelExpr::Wildcard, None));
    }

    #[test]
    fn label_not() {
        let expr = LabelExpr::Not(Box::new(LabelExpr::Name("BAD".into())));
        assert!(matches_edge_label(&expr, Some("GOOD")));
        assert!(!matches_edge_label(&expr, Some("BAD")));
    }

    #[test]
    fn int256_display() {
        let v = Int256::new(ethnum::I256::from(42));
        assert_eq!(v.to_string(), "42");
    }

    #[test]
    fn uint256_display() {
        let v = Uint256::new(ethnum::U256::from(100u128));
        assert_eq!(v.to_string(), "100");
    }

    #[test]
    fn decimal_roundtrip() {
        let d = Decimal::parse("123.456").unwrap();
        assert_eq!(d.to_string(), "123.456");
    }

    #[test]
    fn int256_parse_valid() {
        let v = Int256::parse("12345678901234567890").unwrap();
        assert_eq!(v.to_string(), "12345678901234567890");
    }
    #[test]
    fn int256_parse_negative() {
        let v = Int256::parse("-999").unwrap();
        assert_eq!(v.0, ethnum::I256::from(-999));
    }
    #[test]
    fn int256_parse_invalid() {
        assert!(Int256::parse("not_a_number").is_none());
    }
    #[test]
    fn int256_from_str() {
        let v: Int256 = "42".parse().unwrap();
        assert_eq!(v.0, ethnum::I256::from(42));
    }
    #[test]
    fn uint256_parse_valid() {
        let v = Uint256::parse("99999999999999999999").unwrap();
        assert_eq!(v.to_string(), "99999999999999999999");
    }
    #[test]
    fn uint256_parse_invalid() {
        assert!(Uint256::parse("abc").is_none());
    }
    #[test]
    fn uint256_from_str() {
        let v: Uint256 = "100".parse().unwrap();
        assert_eq!(v.0, ethnum::U256::from(100u128));
    }
    #[test]
    fn decimal_new_and_display() {
        let d = Decimal::new(rust_decimal::Decimal::new(314, 2));
        assert_eq!(d.to_string(), "3.14");
    }
    #[test]
    fn decimal_to_f64() {
        let d = Decimal::parse("2.5").unwrap();
        assert_eq!(d.to_f64(), Some(2.5));
    }
    #[test]
    fn decimal_from_i64() {
        assert_eq!(Decimal::from_i64(42).to_string(), "42");
    }
    #[test]
    fn decimal_from_u64() {
        assert_eq!(Decimal::from_u64(100).to_string(), "100");
    }
    #[test]
    fn decimal_from_i128() {
        assert_eq!(Decimal::from_i128(-1).to_string(), "-1");
    }
    #[test]
    fn decimal_from_u128() {
        assert_eq!(Decimal::from_u128(999).to_string(), "999");
    }
    #[test]
    fn decimal_normalize() {
        let d = Decimal::parse("1.2000").unwrap();
        assert_eq!(d.normalize().to_string(), "1.2");
    }
    #[test]
    fn decimal_parse_invalid() {
        assert!(Decimal::parse("not_decimal").is_none());
    }

    #[test]
    fn decimal_from_f64_rejects_non_finite() {
        assert!(Decimal::from_f64(f64::NAN).is_none());
        assert!(Decimal::from_f64(f64::INFINITY).is_none());
    }

    #[test]
    fn decimal_from_f64_round_trips_simple_values() {
        let d = Decimal::from_f64(2.5).expect("finite float");
        assert_eq!(d.to_f64(), Some(2.5));
    }

    #[test]
    fn decimal_fits_sql_precision_scale() {
        let d = Decimal::parse("12.34").expect("decimal");
        assert!(d.fits_sql_precision_scale(4, 2));
        assert!(d.fits_sql_precision_scale(4, 1));
        assert!(!d.fits_sql_precision_scale(3, 2));
    }

    #[test]
    fn decimal_round_to_scale() {
        let d = Decimal::parse("12.345").expect("decimal");
        assert_eq!(d.round_to_scale(2).to_string(), "12.34");
    }

    #[test]
    fn decimal_fits_sql_precision_scale_rejects_large_integer_part() {
        let d = Decimal::from_f64(999.9).expect("decimal");
        assert!(!d.fits_sql_precision_scale(4, 2));
        let d = Decimal::from_f64(99.99).expect("decimal");
        assert!(d.fits_sql_precision_scale(4, 2));
    }

    #[test]
    fn int256_unsigned_decimal_digit_count() {
        let v = Int256::parse("1000000000000000").expect("int256");
        assert_eq!(v.unsigned_decimal_digit_count(), 16);
    }

    #[test]
    fn int256_wrong_literal_is_not_min() {
        let wrong = Int256::parse("-170141183460469231731687303715884105728");
        assert_ne!(wrong, Some(Int256::new(ethnum::I256::MIN)));
        assert_eq!(wrong.map(|v| v.unsigned_decimal_digit_count()), Some(39));
    }

    #[test]
    fn int256_min_unsigned_decimal_digit_count_does_not_panic() {
        let v = Int256::new(ethnum::I256::MIN);
        assert_eq!(v.unsigned_decimal_digit_count(), 77);
    }

    #[test]
    fn int256_negative_unsigned_decimal_digit_count() {
        let v = Int256::new(ethnum::I256::from(-42));
        assert_eq!(v.unsigned_decimal_digit_count(), 2);
    }

    #[test]
    fn int256_is_zero() {
        assert!(Int256::new(ethnum::I256::ZERO).is_zero());
        assert!(!Int256::new(ethnum::I256::from(1)).is_zero());
    }

    #[test]
    fn narrow_signed_all_widths() {
        use crate::Value;
        assert!(matches!(narrow_signed(42, 8), Some(Value::Int8(42))));
        assert!(matches!(narrow_signed(1000, 16), Some(Value::Int16(1000))));
        assert!(matches!(
            narrow_signed(100000, 32),
            Some(Value::Int32(100000))
        ));
        assert!(matches!(narrow_signed(1, 64), Some(Value::Int64(1))));
        assert!(matches!(narrow_signed(1, 128), Some(Value::Int128(1))));
        assert!(matches!(narrow_signed(1, 256), Some(Value::Int256(_))));
        assert!(narrow_signed(1, 7).is_none());
    }
    #[test]
    fn narrow_signed_overflow() {
        assert!(narrow_signed(200, 8).is_none());
        assert!(narrow_signed(40000, 16).is_none());
    }
    #[test]
    fn narrow_unsigned_all_widths() {
        use crate::Value;
        assert!(matches!(narrow_unsigned(42, 8), Some(Value::Uint8(42))));
        assert!(matches!(
            narrow_unsigned(1000, 16),
            Some(Value::Uint16(1000))
        ));
        assert!(matches!(
            narrow_unsigned(100000, 32),
            Some(Value::Uint32(100000))
        ));
        assert!(matches!(narrow_unsigned(1, 64), Some(Value::Uint64(1))));
        assert!(matches!(narrow_unsigned(1, 128), Some(Value::Uint128(1))));
        assert!(matches!(narrow_unsigned(1, 256), Some(Value::Uint256(_))));
        assert!(narrow_unsigned(1, 7).is_none());
    }
    #[test]
    fn narrow_unsigned_overflow() {
        assert!(narrow_unsigned(300, 8).is_none());
        assert!(narrow_unsigned(70000, 16).is_none());
    }
    #[test]
    fn label_and_expression() {
        let expr = LabelExpr::And(
            Box::new(LabelExpr::Name("A".into())),
            Box::new(LabelExpr::Name("A".into())),
        );
        assert!(matches_edge_label(&expr, Some("A")));
        assert!(!matches_edge_label(&expr, Some("B")));
    }
}
