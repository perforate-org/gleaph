//! GQL value type with extension support.
//!
//! The [`Value`] enum covers all GQL standard value types plus an
//! [`Extension`](Value::Extension) slot for platform-specific types.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::str;

use crate::types::{Decimal, Int256, PathElement, Uint256};

/// Error returned when one [`Value`] cannot be encoded to, or decoded from,
/// the rewrite-side stable byte format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StableValueError {
    UnexpectedEof,
    InvalidTag(u8),
    InvalidUtf8,
    InvalidDecimal,
    InvalidInt256,
    InvalidUint256,
    InvalidExtensionType,
}

impl fmt::Display for StableValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "unexpected end of stable value bytes"),
            Self::InvalidTag(tag) => write!(f, "invalid stable value tag: {tag}"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 in stable value bytes"),
            Self::InvalidDecimal => write!(f, "invalid decimal in stable value bytes"),
            Self::InvalidInt256 => write!(f, "invalid i256 in stable value bytes"),
            Self::InvalidUint256 => write!(f, "invalid u256 in stable value bytes"),
            Self::InvalidExtensionType => write!(f, "extension values are not stable-encodable"),
        }
    }
}

impl std::error::Error for StableValueError {}

// ──── ExtensionValue trait ────

/// Trait for user-defined value types that can be plugged into [`Value::Extension`].
///
/// Implementors must be `Clone`-able (via `clone_box`), comparable, and displayable.
pub trait ExtensionValue: fmt::Debug + fmt::Display + Send + Sync {
    /// A short name identifying this extension type (e.g. `"Principal"`).
    fn type_name(&self) -> &str;

    /// Clone into a boxed trait object.
    fn clone_box(&self) -> Box<dyn ExtensionValue>;

    /// Equality comparison with another extension value.
    fn eq_ext(&self, other: &dyn ExtensionValue) -> bool;

    /// Ordering comparison with another extension value.
    fn cmp_ext(&self, other: &dyn ExtensionValue) -> Option<Ordering>;

    /// Downcast to concrete type.
    fn as_any(&self) -> &dyn Any;
}

// ──── Value enum ────

/// GQL runtime value, covering all standard types from GQL plus extensions.
pub enum Value {
    Null,
    Bool(bool),

    // signedBinaryExactNumericType
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    Int256(Int256),

    // unsignedBinaryExactNumericType
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Uint128(u128),
    Uint256(Uint256),

    // approximateNumericType
    Float16(half::f16),
    Float32(f32),
    Float64(f64),
    #[cfg(feature = "f128")]
    Float128(f128),
    #[cfg(feature = "f256")]
    Float256(f256::f256),

    // decimalExactNumericType
    Decimal(Decimal),

    // characterStringType / byteStringType
    Text(String),
    Bytes(Vec<u8>),

    // temporalInstantType
    /// Days since 1970-01-01.
    Date(i32),
    /// Nanoseconds since midnight (UTC time, no timezone).
    Time(u64),
    /// Nanoseconds since midnight (local time, no timezone).
    LocalTime(u64),
    /// (unix_seconds, subsec_nanos) — UTC datetime.
    DateTime(i64, u32),
    /// (unix_seconds, subsec_nanos) — local datetime (no timezone).
    LocalDateTime(i64, u32),
    /// (unix_seconds, subsec_nanos, tz_offset_seconds) — datetime with timezone.
    ZonedDateTime(i64, u32, i32),
    /// (nanos_since_midnight, tz_offset_seconds) — time with timezone.
    ZonedTime(u64, i32),

    // temporalDurationType
    /// (months, nanos) — ISO-8601 duration.
    Duration(i32, i64),

    // constructed types
    List(Vec<Value>),
    Path(Vec<PathElement>),
    Record(Vec<(String, Value)>),

    // extension slot
    Extension(Box<dyn ExtensionValue>),
}

// ──── Clone ────

impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Self::Null => Self::Null,
            Self::Bool(v) => Self::Bool(*v),
            Self::Int8(v) => Self::Int8(*v),
            Self::Int16(v) => Self::Int16(*v),
            Self::Int32(v) => Self::Int32(*v),
            Self::Int64(v) => Self::Int64(*v),
            Self::Int128(v) => Self::Int128(*v),
            Self::Int256(v) => Self::Int256(*v),
            Self::Uint8(v) => Self::Uint8(*v),
            Self::Uint16(v) => Self::Uint16(*v),
            Self::Uint32(v) => Self::Uint32(*v),
            Self::Uint64(v) => Self::Uint64(*v),
            Self::Uint128(v) => Self::Uint128(*v),
            Self::Uint256(v) => Self::Uint256(*v),
            Self::Float16(v) => Self::Float16(*v),
            Self::Float32(v) => Self::Float32(*v),
            Self::Float64(v) => Self::Float64(*v),
            #[cfg(feature = "f128")]
            Self::Float128(v) => Self::Float128(*v),
            #[cfg(feature = "f256")]
            Self::Float256(v) => Self::Float256(*v),
            Self::Decimal(v) => Self::Decimal(*v),
            Self::Text(v) => Self::Text(v.clone()),
            Self::Bytes(v) => Self::Bytes(v.clone()),
            Self::Date(v) => Self::Date(*v),
            Self::Time(v) => Self::Time(*v),
            Self::LocalTime(v) => Self::LocalTime(*v),
            Self::DateTime(s, n) => Self::DateTime(*s, *n),
            Self::LocalDateTime(s, n) => Self::LocalDateTime(*s, *n),
            Self::ZonedDateTime(s, n, tz) => Self::ZonedDateTime(*s, *n, *tz),
            Self::ZonedTime(n, tz) => Self::ZonedTime(*n, *tz),
            Self::Duration(m, n) => Self::Duration(*m, *n),
            Self::List(v) => Self::List(v.clone()),
            Self::Path(v) => Self::Path(v.clone()),
            Self::Record(v) => Self::Record(v.clone()),
            Self::Extension(v) => Self::Extension(v.clone_box()),
        }
    }
}

// ──── Debug ────

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "Null"),
            Self::Bool(v) => write!(f, "Bool({v})"),
            Self::Int8(v) => write!(f, "Int8({v})"),
            Self::Int16(v) => write!(f, "Int16({v})"),
            Self::Int32(v) => write!(f, "Int32({v})"),
            Self::Int64(v) => write!(f, "Int64({v})"),
            Self::Int128(v) => write!(f, "Int128({v})"),
            Self::Int256(v) => write!(f, "Int256({v})"),
            Self::Uint8(v) => write!(f, "Uint8({v})"),
            Self::Uint16(v) => write!(f, "Uint16({v})"),
            Self::Uint32(v) => write!(f, "Uint32({v})"),
            Self::Uint64(v) => write!(f, "Uint64({v})"),
            Self::Uint128(v) => write!(f, "Uint128({v})"),
            Self::Uint256(v) => write!(f, "Uint256({v})"),
            Self::Float16(v) => write!(f, "Float16({v})"),
            Self::Float32(v) => write!(f, "Float32({v})"),
            Self::Float64(v) => write!(f, "Float64({v})"),
            #[cfg(feature = "f128")]
            Self::Float128(v) => write!(f, "Float128({v:?})"),
            #[cfg(feature = "f256")]
            Self::Float256(v) => write!(f, "Float256({v})"),
            Self::Decimal(v) => write!(f, "Decimal({v})"),
            Self::Text(v) => write!(f, "Text({v:?})"),
            Self::Bytes(v) => write!(f, "Bytes(len={})", v.len()),
            Self::Date(v) => write!(f, "Date({v})"),
            Self::Time(v) => write!(f, "Time({v})"),
            Self::LocalTime(v) => write!(f, "LocalTime({v})"),
            Self::DateTime(s, n) => write!(f, "DateTime({s}, {n})"),
            Self::LocalDateTime(s, n) => write!(f, "LocalDateTime({s}, {n})"),
            Self::ZonedDateTime(s, n, tz) => write!(f, "ZonedDateTime({s}, {n}, {tz})"),
            Self::ZonedTime(n, tz) => write!(f, "ZonedTime({n}, {tz})"),
            Self::Duration(m, n) => write!(f, "Duration({m}, {n})"),
            Self::List(v) => write!(f, "List(len={})", v.len()),
            Self::Path(v) => write!(f, "Path(len={})", v.len()),
            Self::Record(v) => write!(f, "Record(len={})", v.len()),
            Self::Extension(v) => write!(f, "Extension({})", v.type_name()),
        }
    }
}

// ──── PartialEq ────

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int8(a), Self::Int8(b)) => a == b,
            (Self::Int16(a), Self::Int16(b)) => a == b,
            (Self::Int32(a), Self::Int32(b)) => a == b,
            (Self::Int64(a), Self::Int64(b)) => a == b,
            (Self::Int128(a), Self::Int128(b)) => a == b,
            (Self::Int256(a), Self::Int256(b)) => a == b,
            (Self::Uint8(a), Self::Uint8(b)) => a == b,
            (Self::Uint16(a), Self::Uint16(b)) => a == b,
            (Self::Uint32(a), Self::Uint32(b)) => a == b,
            (Self::Uint64(a), Self::Uint64(b)) => a == b,
            (Self::Uint128(a), Self::Uint128(b)) => a == b,
            (Self::Uint256(a), Self::Uint256(b)) => a == b,
            (Self::Float16(a), Self::Float16(b)) => a == b,
            (Self::Float32(a), Self::Float32(b)) => a == b,
            (Self::Float64(a), Self::Float64(b)) => a == b,
            #[cfg(feature = "f128")]
            (Self::Float128(a), Self::Float128(b)) => a == b,
            #[cfg(feature = "f256")]
            (Self::Float256(a), Self::Float256(b)) => a == b,
            (Self::Decimal(a), Self::Decimal(b)) => a == b,
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::Time(a), Self::Time(b)) => a == b,
            (Self::LocalTime(a), Self::LocalTime(b)) => a == b,
            (Self::DateTime(s1, n1), Self::DateTime(s2, n2)) => s1 == s2 && n1 == n2,
            (Self::LocalDateTime(s1, n1), Self::LocalDateTime(s2, n2)) => s1 == s2 && n1 == n2,
            (Self::ZonedDateTime(s1, n1, t1), Self::ZonedDateTime(s2, n2, t2)) => {
                s1 == s2 && n1 == n2 && t1 == t2
            }
            (Self::ZonedTime(n1, t1), Self::ZonedTime(n2, t2)) => n1 == n2 && t1 == t2,
            (Self::Duration(m1, n1), Self::Duration(m2, n2)) => m1 == m2 && n1 == n2,
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Path(a), Self::Path(b)) => a == b,
            (Self::Record(a), Self::Record(b)) => a == b,
            (Self::Extension(a), Self::Extension(b)) => a.eq_ext(b.as_ref()),
            _ => false,
        }
    }
}

// ──── Display ────

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "NULL"),
            Self::Bool(v) => write!(f, "{v}"),
            Self::Int8(v) => write!(f, "{v}"),
            Self::Int16(v) => write!(f, "{v}"),
            Self::Int32(v) => write!(f, "{v}"),
            Self::Int64(v) => write!(f, "{v}"),
            Self::Int128(v) => write!(f, "{v}"),
            Self::Int256(v) => write!(f, "{v}"),
            Self::Uint8(v) => write!(f, "{v}"),
            Self::Uint16(v) => write!(f, "{v}"),
            Self::Uint32(v) => write!(f, "{v}"),
            Self::Uint64(v) => write!(f, "{v}"),
            Self::Uint128(v) => write!(f, "{v}"),
            Self::Uint256(v) => write!(f, "{v}"),
            Self::Float16(v) => write!(f, "{v}"),
            Self::Float32(v) => write!(f, "{v}"),
            Self::Float64(v) => write!(f, "{v}"),
            #[cfg(feature = "f128")]
            Self::Float128(v) => write!(f, "{v:?}"),
            #[cfg(feature = "f256")]
            Self::Float256(v) => write!(f, "{v}"),
            Self::Decimal(v) => write!(f, "{v}"),
            Self::Text(v) => write!(f, "{v}"),
            Self::Bytes(v) => write!(f, "0x{}", hex_encode(v)),
            Self::Date(v) => write!(f, "{}", crate::temporal::format_date(*v)),
            Self::Time(v) | Self::LocalTime(v) => {
                write!(f, "{}", crate::temporal::format_time(*v))
            }
            Self::DateTime(s, n) | Self::LocalDateTime(s, n) => {
                write!(f, "{}", crate::temporal::format_datetime(*s, *n))
            }
            Self::ZonedDateTime(s, n, tz) => {
                write!(
                    f,
                    "{}{}",
                    crate::temporal::format_datetime(*s, *n),
                    format_tz_offset(*tz)
                )
            }
            Self::ZonedTime(n, tz) => {
                write!(
                    f,
                    "{}{}",
                    crate::temporal::format_time(*n),
                    format_tz_offset(*tz)
                )
            }
            Self::Duration(m, n) => write!(f, "{}", crate::temporal::format_duration(*m, *n)),
            Self::List(v) => {
                write!(f, "[")?;
                for (i, item) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Self::Path(v) => write!(f, "<path len={}>", v.len()),
            Self::Record(v) => {
                write!(f, "{{")?;
                for (i, (k, val)) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {val}")?;
                }
                write!(f, "}}")
            }
            Self::Extension(v) => write!(f, "{v}"),
        }
    }
}

fn format_tz_offset(offset_secs: i32) -> String {
    if offset_secs == 0 {
        return "Z".to_string();
    }
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs = offset_secs.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    format!("{sign}{h:02}:{m:02}")
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u32).to_le_bytes());
}

fn write_len_prefixed_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

struct StableCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> StableCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn peek_u8(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn read_u8(&mut self) -> Result<u8, StableValueError> {
        let byte = *self
            .bytes
            .get(self.offset)
            .ok_or(StableValueError::UnexpectedEof)?;
        self.offset += 1;
        Ok(byte)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], StableValueError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(StableValueError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(StableValueError::UnexpectedEof)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        self.offset = end;
        Ok(out)
    }

    fn read_len(&mut self) -> Result<usize, StableValueError> {
        Ok(u32::from_le_bytes(self.read_array()?) as usize)
    }

    fn read_len_prefixed_bytes(&mut self) -> Result<&'a [u8], StableValueError> {
        let len = self.read_len()?;
        let end = self
            .offset
            .checked_add(len)
            .ok_or(StableValueError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(StableValueError::UnexpectedEof)?;
        self.offset = end;
        Ok(slice)
    }

    fn read_string(&mut self) -> Result<String, StableValueError> {
        let bytes = self.read_len_prefixed_bytes()?;
        str::from_utf8(bytes)
            .map(|s| s.to_owned())
            .map_err(|_| StableValueError::InvalidUtf8)
    }
}

// ──── From impls ────

impl From<i8> for Value {
    fn from(v: i8) -> Self {
        Self::Int8(v)
    }
}
impl From<i16> for Value {
    fn from(v: i16) -> Self {
        Self::Int16(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Self::Int32(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int64(v)
    }
}
impl From<i128> for Value {
    fn from(v: i128) -> Self {
        Self::Int128(v)
    }
}
impl From<u8> for Value {
    fn from(v: u8) -> Self {
        Self::Uint8(v)
    }
}
impl From<u16> for Value {
    fn from(v: u16) -> Self {
        Self::Uint16(v)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Self::Uint32(v)
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Self::Uint64(v)
    }
}
impl From<u128> for Value {
    fn from(v: u128) -> Self {
        Self::Uint128(v)
    }
}
impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Self::Float32(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Float64(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Text(v.to_string())
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}
impl From<Decimal> for Value {
    fn from(d: Decimal) -> Self {
        Self::Decimal(d)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(inner) => inner.into(),
            None => Self::Null,
        }
    }
}

// ──── Value helper methods ────

impl Value {
    /// Encodes this value to the rewrite-side stable byte format.
    pub fn to_stable_bytes(&self) -> Result<Vec<u8>, StableValueError> {
        let mut out = Vec::new();
        self.encode_stable_into(&mut out)?;
        Ok(out)
    }

    /// Decodes one value from the rewrite-side stable byte format.
    pub fn from_stable_bytes(bytes: &[u8]) -> Result<Self, StableValueError> {
        let mut cursor = StableCursor::new(bytes);
        let value = Self::decode_stable_from(&mut cursor)?;
        if cursor.remaining() == 0 {
            Ok(value)
        } else {
            Err(StableValueError::InvalidTag(
                cursor.peek_u8().unwrap_or_default(),
            ))
        }
    }

    fn encode_stable_into(&self, out: &mut Vec<u8>) -> Result<(), StableValueError> {
        match self {
            Self::Null => out.push(0),
            Self::Bool(v) => {
                out.push(1);
                out.push(u8::from(*v));
            }
            Self::Int8(v) => {
                out.push(2);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Int16(v) => {
                out.push(3);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Int32(v) => {
                out.push(4);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Int64(v) => {
                out.push(5);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Int128(v) => {
                out.push(6);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Int256(v) => {
                out.push(7);
                write_len_prefixed_bytes(out, v.to_string().as_bytes());
            }
            Self::Uint8(v) => {
                out.push(8);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Uint16(v) => {
                out.push(9);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Uint32(v) => {
                out.push(10);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Uint64(v) => {
                out.push(11);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Uint128(v) => {
                out.push(12);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Uint256(v) => {
                out.push(13);
                write_len_prefixed_bytes(out, v.to_string().as_bytes());
            }
            Self::Float16(v) => {
                out.push(14);
                out.extend_from_slice(&v.to_bits().to_le_bytes());
            }
            Self::Float32(v) => {
                out.push(15);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Float64(v) => {
                out.push(16);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Decimal(v) => {
                out.push(17);
                write_len_prefixed_bytes(out, v.to_string().as_bytes());
            }
            Self::Text(v) => {
                out.push(18);
                write_len_prefixed_bytes(out, v.as_bytes());
            }
            Self::Bytes(v) => {
                out.push(19);
                write_len_prefixed_bytes(out, v);
            }
            Self::Date(v) => {
                out.push(20);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::Time(v) => {
                out.push(21);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::LocalTime(v) => {
                out.push(22);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Self::DateTime(s, n) => {
                out.push(23);
                out.extend_from_slice(&s.to_le_bytes());
                out.extend_from_slice(&n.to_le_bytes());
            }
            Self::LocalDateTime(s, n) => {
                out.push(24);
                out.extend_from_slice(&s.to_le_bytes());
                out.extend_from_slice(&n.to_le_bytes());
            }
            Self::ZonedDateTime(s, n, tz) => {
                out.push(25);
                out.extend_from_slice(&s.to_le_bytes());
                out.extend_from_slice(&n.to_le_bytes());
                out.extend_from_slice(&tz.to_le_bytes());
            }
            Self::ZonedTime(n, tz) => {
                out.push(26);
                out.extend_from_slice(&n.to_le_bytes());
                out.extend_from_slice(&tz.to_le_bytes());
            }
            Self::Duration(m, n) => {
                out.push(27);
                out.extend_from_slice(&m.to_le_bytes());
                out.extend_from_slice(&n.to_le_bytes());
            }
            Self::List(items) => {
                out.push(28);
                write_len(out, items.len());
                for item in items {
                    item.encode_stable_into(out)?;
                }
            }
            Self::Path(items) => {
                out.push(29);
                write_len(out, items.len());
                for item in items {
                    match item {
                        PathElement::Vertex(id) => {
                            out.push(0);
                            out.extend_from_slice(&id.to_le_bytes());
                        }
                        PathElement::Edge { src, dst, label } => {
                            out.push(1);
                            out.extend_from_slice(&src.to_le_bytes());
                            out.extend_from_slice(&dst.to_le_bytes());
                            match label {
                                Some(label) => {
                                    out.push(1);
                                    write_len_prefixed_bytes(out, label.as_bytes());
                                }
                                None => out.push(0),
                            }
                        }
                    }
                }
            }
            Self::Record(fields) => {
                out.push(30);
                write_len(out, fields.len());
                for (key, value) in fields {
                    write_len_prefixed_bytes(out, key.as_bytes());
                    value.encode_stable_into(out)?;
                }
            }
            Self::Extension(_) => return Err(StableValueError::InvalidExtensionType),
            #[cfg(feature = "f128")]
            Self::Float128(v) => {
                out.push(31);
                write_len_prefixed_bytes(out, format!("{v:?}").as_bytes());
            }
            #[cfg(feature = "f256")]
            Self::Float256(v) => {
                out.push(32);
                write_len_prefixed_bytes(out, v.to_string().as_bytes());
            }
        }
        Ok(())
    }

    fn decode_stable_from(cursor: &mut StableCursor<'_>) -> Result<Self, StableValueError> {
        match cursor.read_u8()? {
            0 => Ok(Self::Null),
            1 => Ok(Self::Bool(cursor.read_u8()? != 0)),
            2 => Ok(Self::Int8(i8::from_le_bytes([cursor.read_u8()?]))),
            3 => Ok(Self::Int16(i16::from_le_bytes(cursor.read_array()?))),
            4 => Ok(Self::Int32(i32::from_le_bytes(cursor.read_array()?))),
            5 => Ok(Self::Int64(i64::from_le_bytes(cursor.read_array()?))),
            6 => Ok(Self::Int128(i128::from_le_bytes(cursor.read_array()?))),
            7 => {
                let s = cursor.read_string()?;
                Ok(Self::Int256(Int256::parse(&s).ok_or(StableValueError::InvalidInt256)?))
            }
            8 => Ok(Self::Uint8(cursor.read_u8()?)),
            9 => Ok(Self::Uint16(u16::from_le_bytes(cursor.read_array()?))),
            10 => Ok(Self::Uint32(u32::from_le_bytes(cursor.read_array()?))),
            11 => Ok(Self::Uint64(u64::from_le_bytes(cursor.read_array()?))),
            12 => Ok(Self::Uint128(u128::from_le_bytes(cursor.read_array()?))),
            13 => {
                let s = cursor.read_string()?;
                Ok(Self::Uint256(Uint256::parse(&s).ok_or(StableValueError::InvalidUint256)?))
            }
            14 => Ok(Self::Float16(half::f16::from_bits(u16::from_le_bytes(cursor.read_array()?)))),
            15 => Ok(Self::Float32(f32::from_le_bytes(cursor.read_array()?))),
            16 => Ok(Self::Float64(f64::from_le_bytes(cursor.read_array()?))),
            17 => {
                let s = cursor.read_string()?;
                Ok(Self::Decimal(Decimal::parse(&s).ok_or(StableValueError::InvalidDecimal)?))
            }
            18 => Ok(Self::Text(cursor.read_string()?)),
            19 => Ok(Self::Bytes(cursor.read_len_prefixed_bytes()?.to_vec())),
            20 => Ok(Self::Date(i32::from_le_bytes(cursor.read_array()?))),
            21 => Ok(Self::Time(u64::from_le_bytes(cursor.read_array()?))),
            22 => Ok(Self::LocalTime(u64::from_le_bytes(cursor.read_array()?))),
            23 => Ok(Self::DateTime(
                i64::from_le_bytes(cursor.read_array()?),
                u32::from_le_bytes(cursor.read_array()?),
            )),
            24 => Ok(Self::LocalDateTime(
                i64::from_le_bytes(cursor.read_array()?),
                u32::from_le_bytes(cursor.read_array()?),
            )),
            25 => Ok(Self::ZonedDateTime(
                i64::from_le_bytes(cursor.read_array()?),
                u32::from_le_bytes(cursor.read_array()?),
                i32::from_le_bytes(cursor.read_array()?),
            )),
            26 => Ok(Self::ZonedTime(
                u64::from_le_bytes(cursor.read_array()?),
                i32::from_le_bytes(cursor.read_array()?),
            )),
            27 => Ok(Self::Duration(
                i32::from_le_bytes(cursor.read_array()?),
                i64::from_le_bytes(cursor.read_array()?),
            )),
            28 => {
                let len = cursor.read_len()?;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    items.push(Self::decode_stable_from(cursor)?);
                }
                Ok(Self::List(items))
            }
            29 => {
                let len = cursor.read_len()?;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    let tag = cursor.read_u8()?;
                    let item = match tag {
                        0 => PathElement::Vertex(u64::from_le_bytes(cursor.read_array()?)),
                        1 => {
                            let src = u64::from_le_bytes(cursor.read_array()?);
                            let dst = u64::from_le_bytes(cursor.read_array()?);
                            let label = if cursor.read_u8()? == 0 {
                                None
                            } else {
                                Some(cursor.read_string()?)
                            };
                            PathElement::Edge { src, dst, label }
                        }
                        other => return Err(StableValueError::InvalidTag(other)),
                    };
                    items.push(item);
                }
                Ok(Self::Path(items))
            }
            30 => {
                let len = cursor.read_len()?;
                let mut fields = Vec::with_capacity(len);
                for _ in 0..len {
                    let key = cursor.read_string()?;
                    let value = Self::decode_stable_from(cursor)?;
                    fields.push((key, value));
                }
                Ok(Self::Record(fields))
            }
            #[cfg(feature = "f128")]
            31 => {
                let s = cursor.read_string()?;
                let v = s.parse::<f128>().map_err(|_| StableValueError::InvalidTag(31))?;
                Ok(Self::Float128(v))
            }
            #[cfg(feature = "f256")]
            32 => {
                let s = cursor.read_string()?;
                let v = s.parse::<f256::f256>().map_err(|_| StableValueError::InvalidTag(32))?;
                Ok(Self::Float256(v))
            }
            tag => Err(StableValueError::InvalidTag(tag)),
        }
    }

    /// Extract value as i64 (works for Int8 through Int64; Int128/Int256 if in range).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int8(v) => Some(*v as i64),
            Self::Int16(v) => Some(*v as i64),
            Self::Int32(v) => Some(*v as i64),
            Self::Int64(v) => Some(*v),
            Self::Int128(v) => i64::try_from(*v).ok(),
            Self::Int256(v) => v.0.try_into().ok(),
            _ => None,
        }
    }

    /// Extract value as i128 (works for Int8 through Int128).
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Self::Int8(v) => Some(*v as i128),
            Self::Int16(v) => Some(*v as i128),
            Self::Int32(v) => Some(*v as i128),
            Self::Int64(v) => Some(*v as i128),
            Self::Int128(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract value as u128 (works for Uint8 through Uint128).
    pub fn as_u128(&self) -> Option<u128> {
        match self {
            Self::Uint8(v) => Some(*v as u128),
            Self::Uint16(v) => Some(*v as u128),
            Self::Uint32(v) => Some(*v as u128),
            Self::Uint64(v) => Some(*v as u128),
            Self::Uint128(v) => Some(*v),
            _ => None,
        }
    }

    /// Convert any numeric variant to f64.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int8(v) => Some(*v as f64),
            Self::Int16(v) => Some(*v as f64),
            Self::Int32(v) => Some(*v as f64),
            Self::Int64(v) => Some(*v as f64),
            Self::Int128(v) => Some(*v as f64),
            Self::Int256(v) => Some(v.0.as_f64()),
            Self::Uint8(v) => Some(*v as f64),
            Self::Uint16(v) => Some(*v as f64),
            Self::Uint32(v) => Some(*v as f64),
            Self::Uint64(v) => Some(*v as f64),
            Self::Uint128(v) => Some(*v as f64),
            Self::Uint256(v) => Some(v.0.as_f64()),
            Self::Float16(v) => Some(v.to_f64()),
            Self::Float32(v) => Some(*v as f64),
            Self::Float64(v) => Some(*v),
            #[cfg(feature = "f128")]
            Self::Float128(v) => Some(*v as f64),
            #[cfg(feature = "f256")]
            Self::Float256(v) => {
                // f256 → f64 via string roundtrip (no direct cast available)
                v.to_string().parse::<f64>().ok()
            }
            Self::Decimal(v) => v.to_f64(),
            _ => None,
        }
    }

    /// Returns true for Int8..Int256.
    pub fn is_signed_int(&self) -> bool {
        matches!(
            self,
            Self::Int8(_)
                | Self::Int16(_)
                | Self::Int32(_)
                | Self::Int64(_)
                | Self::Int128(_)
                | Self::Int256(_)
        )
    }

    /// Returns true for Uint8..Uint256.
    pub fn is_unsigned_int(&self) -> bool {
        matches!(
            self,
            Self::Uint8(_)
                | Self::Uint16(_)
                | Self::Uint32(_)
                | Self::Uint64(_)
                | Self::Uint128(_)
                | Self::Uint256(_)
        )
    }

    /// Returns true for any integer variant.
    pub fn is_any_int(&self) -> bool {
        self.is_signed_int() || self.is_unsigned_int()
    }

    /// Returns the bit width for integer variants (8, 16, 32, 64, 128, 256).
    pub fn int_width(&self) -> Option<u16> {
        match self {
            Self::Int8(_) | Self::Uint8(_) => Some(8),
            Self::Int16(_) | Self::Uint16(_) => Some(16),
            Self::Int32(_) | Self::Uint32(_) => Some(32),
            Self::Int64(_) | Self::Uint64(_) => Some(64),
            Self::Int128(_) | Self::Uint128(_) => Some(128),
            Self::Int256(_) | Self::Uint256(_) => Some(256),
            _ => None,
        }
    }

    /// Returns true for any float variant (Float16, Float32, Float64, Float128, Float256).
    pub fn is_float(&self) -> bool {
        matches!(self, Self::Float16(_) | Self::Float32(_) | Self::Float64(_))
            || {
                #[cfg(feature = "f128")]
                {
                    matches!(self, Self::Float128(_))
                }
                #[cfg(not(feature = "f128"))]
                false
            }
            || {
                #[cfg(feature = "f256")]
                {
                    matches!(self, Self::Float256(_))
                }
                #[cfg(not(feature = "f256"))]
                false
            }
    }

    /// Returns true for any numeric variant (integer, float, or decimal).
    pub fn is_numeric(&self) -> bool {
        self.is_any_int() || self.is_float() || matches!(self, Self::Decimal(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_clone_and_eq() {
        let v = Value::Int64(42);
        assert_eq!(v, v.clone());
    }

    #[test]
    fn value_from_impls() {
        assert_eq!(Value::from(42i32), Value::Int32(42));
        assert_eq!(Value::from("hello"), Value::Text("hello".into()));
        assert_eq!(Value::from(true), Value::Bool(true));
    }

    #[test]
    fn value_null_from_none() {
        let v: Value = None::<i64>.into();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn value_helpers() {
        assert_eq!(Value::Int8(42).as_i64(), Some(42));
        assert_eq!(Value::Uint64(100).as_u128(), Some(100));
        assert!(Value::Int32(1).is_signed_int());
        assert!(Value::Uint16(1).is_unsigned_int());
        assert!(Value::Float64(1.0).is_float());
        assert!(Value::Decimal(Decimal::from_i64(1)).is_numeric());
    }

    #[test]
    fn value_display() {
        assert_eq!(format!("{}", Value::Null), "NULL");
        assert_eq!(format!("{}", Value::Int64(42)), "42");
        assert_eq!(format!("{}", Value::Text("hello".into())), "hello");
        assert_eq!(format!("{}", Value::Bool(true)), "true");
    }

    #[test]
    fn value_debug() {
        assert_eq!(format!("{:?}", Value::Int64(42)), "Int64(42)");
        assert_eq!(format!("{:?}", Value::Text("hi".into())), "Text(\"hi\")");
    }

    #[test]
    fn as_i64_all_signed() {
        assert_eq!(Value::Int8(-1).as_i64(), Some(-1));
        assert_eq!(Value::Int16(1000).as_i64(), Some(1000));
        assert_eq!(Value::Int32(-100).as_i64(), Some(-100));
        assert_eq!(Value::Int64(i64::MAX).as_i64(), Some(i64::MAX));
        assert_eq!(Value::Int128(42).as_i64(), Some(42));
        assert!(Value::Int128(i128::MAX).as_i64().is_none());
        assert_eq!(Value::Text("x".into()).as_i64(), None);
    }

    #[test]
    fn as_i128_all() {
        assert_eq!(Value::Int8(-1).as_i128(), Some(-1));
        assert_eq!(Value::Int16(200).as_i128(), Some(200));
        assert_eq!(Value::Int32(300).as_i128(), Some(300));
        assert_eq!(Value::Int64(400).as_i128(), Some(400));
        assert_eq!(Value::Int128(i128::MAX).as_i128(), Some(i128::MAX));
        assert_eq!(Value::Float64(1.0).as_i128(), None);
    }

    #[test]
    fn as_u128_all() {
        assert_eq!(Value::Uint8(1).as_u128(), Some(1));
        assert_eq!(Value::Uint16(2).as_u128(), Some(2));
        assert_eq!(Value::Uint32(3).as_u128(), Some(3));
        assert_eq!(Value::Uint64(4).as_u128(), Some(4));
        assert_eq!(Value::Uint128(u128::MAX).as_u128(), Some(u128::MAX));
        assert_eq!(Value::Int64(1).as_u128(), None);
    }

    #[test]
    fn as_f64_integers() {
        assert_eq!(Value::Int8(1).as_f64(), Some(1.0));
        assert_eq!(Value::Int16(2).as_f64(), Some(2.0));
        assert_eq!(Value::Int32(3).as_f64(), Some(3.0));
        assert_eq!(Value::Int64(4).as_f64(), Some(4.0));
        assert_eq!(Value::Int128(5).as_f64(), Some(5.0));
        assert_eq!(Value::Uint8(6).as_f64(), Some(6.0));
        assert_eq!(Value::Uint16(7).as_f64(), Some(7.0));
        assert_eq!(Value::Uint32(8).as_f64(), Some(8.0));
        assert_eq!(Value::Uint64(9).as_f64(), Some(9.0));
        assert_eq!(Value::Uint128(10).as_f64(), Some(10.0));
    }

    #[test]
    fn as_f64_floats() {
        assert_eq!(Value::Float16(half::f16::from_f64(1.5)).as_f64(), Some(1.5));
        assert_eq!(Value::Float32(2.5).as_f64(), Some(2.5));
        assert_eq!(Value::Float64(3.5).as_f64(), Some(3.5));
        assert!(Value::Decimal(Decimal::from_i64(7)).as_f64().is_some());
        assert_eq!(Value::Null.as_f64(), None);
    }

    #[test]
    fn is_signed_int_all() {
        assert!(Value::Int8(0).is_signed_int());
        assert!(Value::Int16(0).is_signed_int());
        assert!(Value::Int64(0).is_signed_int());
        assert!(Value::Int128(0).is_signed_int());
        assert!(!Value::Uint8(0).is_signed_int());
        assert!(!Value::Float64(0.0).is_signed_int());
    }

    #[test]
    fn is_unsigned_int_all() {
        assert!(Value::Uint8(0).is_unsigned_int());
        assert!(Value::Uint32(0).is_unsigned_int());
        assert!(Value::Uint64(0).is_unsigned_int());
        assert!(Value::Uint128(0).is_unsigned_int());
        assert!(!Value::Int8(0).is_unsigned_int());
    }

    #[test]
    fn is_any_int_and_width() {
        assert!(Value::Int32(0).is_any_int());
        assert!(Value::Uint64(0).is_any_int());
        assert!(!Value::Float64(0.0).is_any_int());

        assert_eq!(Value::Int8(0).int_width(), Some(8));
        assert_eq!(Value::Uint16(0).int_width(), Some(16));
        assert_eq!(Value::Int32(0).int_width(), Some(32));
        assert_eq!(Value::Uint64(0).int_width(), Some(64));
        assert_eq!(Value::Int128(0).int_width(), Some(128));
        assert_eq!(Value::Float64(0.0).int_width(), None);
    }

    #[test]
    fn is_float_variants() {
        assert!(Value::Float16(half::f16::ZERO).is_float());
        assert!(Value::Float32(0.0).is_float());
        assert!(!Value::Int32(0).is_float());
    }

    #[test]
    fn is_numeric_all() {
        assert!(Value::Int8(0).is_numeric());
        assert!(Value::Uint64(0).is_numeric());
        assert!(Value::Float32(0.0).is_numeric());
        assert!(Value::Decimal(Decimal::from_i64(0)).is_numeric());
        assert!(!Value::Text("x".into()).is_numeric());
        assert!(!Value::Null.is_numeric());
    }

    #[test]
    fn from_impls_comprehensive() {
        assert_eq!(Value::from(1i8), Value::Int8(1));
        assert_eq!(Value::from(2i16), Value::Int16(2));
        assert_eq!(Value::from(3i64), Value::Int64(3));
        assert_eq!(Value::from(4i128), Value::Int128(4));
        assert_eq!(Value::from(5u8), Value::Uint8(5));
        assert_eq!(Value::from(6u16), Value::Uint16(6));
        assert_eq!(Value::from(7u32), Value::Uint32(7));
        assert_eq!(Value::from(8u64), Value::Uint64(8));
        assert_eq!(Value::from(9u128), Value::Uint128(9));
        assert_eq!(Value::from(1.0f32), Value::Float32(1.0));
        assert_eq!(Value::from(2.0f64), Value::Float64(2.0));
        assert_eq!(Value::from(vec![1u8, 2]), Value::Bytes(vec![1, 2]));
        assert_eq!(
            Value::from(Decimal::from_i64(1)),
            Value::Decimal(Decimal::from_i64(1))
        );
        let v: Value = Some(42i64).into();
        assert_eq!(v, Value::Int64(42));
    }

    #[test]
    fn display_more_variants() {
        assert_eq!(format!("{}", Value::Int8(1)), "1");
        assert_eq!(format!("{}", Value::Uint32(100)), "100");
        assert_eq!(format!("{}", Value::Float32(1.5)), "1.5");
        assert_eq!(format!("{}", Value::Bytes(vec![0xAB, 0xCD])), "0xabcd");
        assert_eq!(
            format!("{}", Value::List(vec![Value::Int64(1), Value::Int64(2)])),
            "[1, 2]"
        );
        assert_eq!(format!("{}", Value::List(vec![])), "[]");
        assert!(format!("{}", Value::Path(vec![])).contains("path"));
        assert_eq!(
            format!("{}", Value::Record(vec![("a".into(), Value::Int64(1))])),
            "{a: 1}"
        );
    }

    #[test]
    fn debug_more_variants() {
        assert_eq!(format!("{:?}", Value::Null), "Null");
        assert_eq!(format!("{:?}", Value::Bool(false)), "Bool(false)");
        assert_eq!(format!("{:?}", Value::Uint8(1)), "Uint8(1)");
        assert_eq!(format!("{:?}", Value::Float32(1.0)), "Float32(1)");
        assert!(format!("{:?}", Value::Bytes(vec![1, 2])).contains("Bytes"));
        assert!(format!("{:?}", Value::List(vec![])).contains("List"));
        assert!(format!("{:?}", Value::Record(vec![])).contains("Record"));
        assert!(format!("{:?}", Value::Date(0)).contains("Date"));
        assert!(format!("{:?}", Value::Duration(1, 2)).contains("Duration"));
    }

    #[test]
    fn clone_all_variants() {
        let values: Vec<Value> = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int8(1),
            Value::Int16(2),
            Value::Int32(3),
            Value::Int64(4),
            Value::Int128(5),
            Value::Uint8(1),
            Value::Uint16(2),
            Value::Uint32(3),
            Value::Uint64(4),
            Value::Uint128(5),
            Value::Float16(half::f16::from_f64(1.0)),
            Value::Float32(1.0),
            Value::Float64(1.0),
            Value::Decimal(Decimal::from_i64(1)),
            Value::Text("hi".into()),
            Value::Bytes(vec![1]),
            Value::Date(0),
            Value::Time(0),
            Value::LocalTime(0),
            Value::DateTime(0, 0),
            Value::LocalDateTime(0, 0),
            Value::ZonedDateTime(0, 0, 0),
            Value::ZonedTime(0, 0),
            Value::Duration(0, 0),
            Value::List(vec![]),
            Value::Path(vec![]),
            Value::Record(vec![]),
        ];
        for v in &values {
            assert_eq!(v, &v.clone());
        }
    }

    #[test]
    fn eq_different_types() {
        assert_ne!(Value::Int32(1), Value::Int64(1));
        assert_ne!(Value::Int32(1), Value::Null);
        assert_ne!(Value::Text("a".into()), Value::Int32(1));
    }

    #[test]
    fn display_temporal() {
        let _ = format!("{}", Value::Date(0));
        let _ = format!("{}", Value::Time(0));
        let _ = format!("{}", Value::LocalTime(0));
        let _ = format!("{}", Value::DateTime(0, 0));
        let _ = format!("{}", Value::LocalDateTime(0, 0));
        let _ = format!("{}", Value::ZonedDateTime(0, 0, 3600));
        let _ = format!("{}", Value::ZonedDateTime(0, 0, 0));
        let _ = format!("{}", Value::ZonedTime(0, -3600));
        let _ = format!("{}", Value::Duration(1, 1000000000));
    }

    // ---- Additional coverage tests ----

    #[test]
    fn display_temporal_values_verify() {
        assert_eq!(format!("{}", Value::Date(0)), "1970-01-01");
        assert_eq!(format!("{}", Value::Time(52_200_000_000_000)), "14:30:00");
        assert_eq!(
            format!("{}", Value::LocalTime(52_200_000_000_000)),
            "14:30:00"
        );
        assert_eq!(
            format!(
                "{}",
                Value::DateTime(19797 * 86400 + 14 * 3600 + 30 * 60, 0)
            ),
            "2024-03-15T14:30:00Z"
        );
        assert_eq!(
            format!(
                "{}",
                Value::LocalDateTime(19797 * 86400 + 14 * 3600 + 30 * 60, 0)
            ),
            "2024-03-15T14:30:00Z"
        );
        // ZonedDateTime with positive offset
        let zdt = format!("{}", Value::ZonedDateTime(0, 0, 9 * 3600));
        assert!(zdt.contains("+09:00"));
        // ZonedDateTime with zero offset uses Z
        let zdt_z = format!("{}", Value::ZonedDateTime(0, 0, 0));
        assert!(zdt_z.contains("Z"));
        // ZonedTime with negative offset
        let zt = format!("{}", Value::ZonedTime(52_200_000_000_000, -5 * 3600));
        assert!(zt.contains("-05:00"));
        // ZonedTime with zero offset
        let zt_z = format!("{}", Value::ZonedTime(0, 0));
        assert!(zt_z.contains("Z"));
        // Duration display
        assert_eq!(format!("{}", Value::Duration(0, 0)), "P0D");
        assert_eq!(format!("{}", Value::Duration(14, 0)), "P1Y2M");
    }

    #[test]
    fn display_bytes_empty() {
        assert_eq!(format!("{}", Value::Bytes(vec![])), "0x");
    }

    #[test]
    fn display_list_multiple_items() {
        let list = Value::List(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]);
        assert_eq!(format!("{}", list), "[1, 2, 3]");
    }

    #[test]
    fn display_record_multiple_fields() {
        let rec = Value::Record(vec![
            ("a".into(), Value::Int64(1)),
            ("b".into(), Value::Text("hello".into())),
        ]);
        assert_eq!(format!("{}", rec), "{a: 1, b: hello}");
    }

    #[test]
    fn display_record_empty() {
        assert_eq!(format!("{}", Value::Record(vec![])), "{}");
    }

    #[test]
    fn debug_all_temporal_variants() {
        assert!(format!("{:?}", Value::Time(0)).contains("Time"));
        assert!(format!("{:?}", Value::LocalTime(0)).contains("LocalTime"));
        assert!(format!("{:?}", Value::DateTime(0, 0)).contains("DateTime"));
        assert!(format!("{:?}", Value::LocalDateTime(0, 0)).contains("LocalDateTime"));
        assert!(format!("{:?}", Value::ZonedDateTime(0, 0, 0)).contains("ZonedDateTime"));
        assert!(format!("{:?}", Value::ZonedTime(0, 0)).contains("ZonedTime"));
        assert!(format!("{:?}", Value::Path(vec![])).contains("Path"));
    }

    #[test]
    fn debug_integer_variants() {
        assert_eq!(format!("{:?}", Value::Int8(1)), "Int8(1)");
        assert_eq!(format!("{:?}", Value::Int16(2)), "Int16(2)");
        assert_eq!(format!("{:?}", Value::Int32(3)), "Int32(3)");
        assert_eq!(format!("{:?}", Value::Int128(5)), "Int128(5)");
        assert_eq!(format!("{:?}", Value::Uint16(2)), "Uint16(2)");
        assert_eq!(format!("{:?}", Value::Uint32(3)), "Uint32(3)");
        assert_eq!(format!("{:?}", Value::Uint64(4)), "Uint64(4)");
        assert_eq!(format!("{:?}", Value::Uint128(5)), "Uint128(5)");
        assert!(format!("{:?}", Value::Float16(half::f16::from_f64(1.0))).contains("Float16"));
        assert!(format!("{:?}", Value::Decimal(Decimal::from_i64(1))).contains("Decimal"));
    }

    #[test]
    fn display_integer_variants() {
        assert_eq!(format!("{}", Value::Int16(100)), "100");
        assert_eq!(format!("{}", Value::Int128(999)), "999");
        assert_eq!(format!("{}", Value::Uint8(255)), "255");
        assert_eq!(format!("{}", Value::Uint16(1000)), "1000");
        assert_eq!(format!("{}", Value::Uint64(42)), "42");
        assert_eq!(format!("{}", Value::Uint128(99)), "99");
        assert_eq!(
            format!("{}", Value::Float16(half::f16::from_f64(1.5))),
            "1.5"
        );
        let _ = format!("{}", Value::Decimal(Decimal::from_i64(42)));
    }

    #[test]
    fn as_i64_int256() {
        let v = Value::Int256(crate::types::Int256::new(ethnum::I256::new(42)));
        assert_eq!(v.as_i64(), Some(42));
        // Int256 out of i64 range
        let big = Value::Int256(crate::types::Int256::new(ethnum::I256::MAX));
        assert!(big.as_i64().is_none());
    }

    #[test]
    fn as_f64_int256_uint256() {
        let v = Value::Int256(crate::types::Int256::new(ethnum::I256::new(100)));
        assert_eq!(v.as_f64(), Some(100.0));
        let u = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(200)));
        assert_eq!(u.as_f64(), Some(200.0));
    }

    #[test]
    fn int_width_256() {
        let v = Value::Int256(crate::types::Int256::new(ethnum::I256::new(0)));
        assert_eq!(v.int_width(), Some(256));
        let u = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(0)));
        assert_eq!(u.int_width(), Some(256));
        assert_eq!(Value::Uint128(0).int_width(), Some(128));
    }

    #[test]
    fn is_signed_int_256() {
        let v = Value::Int256(crate::types::Int256::new(ethnum::I256::new(0)));
        assert!(v.is_signed_int());
        assert!(v.is_any_int());
        assert!(v.is_numeric());
    }

    #[test]
    fn is_unsigned_int_256() {
        let u = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(0)));
        assert!(u.is_unsigned_int());
        assert!(u.is_any_int());
        assert!(u.is_numeric());
        assert!(!u.is_signed_int());
        assert!(Value::Uint16(0).is_unsigned_int());
    }

    #[test]
    fn eq_same_type_temporal() {
        assert_eq!(Value::Date(100), Value::Date(100));
        assert_ne!(Value::Date(100), Value::Date(200));
        assert_eq!(Value::Time(1000), Value::Time(1000));
        assert_ne!(Value::Time(1000), Value::Time(2000));
        assert_eq!(Value::LocalTime(1000), Value::LocalTime(1000));
        assert_ne!(Value::LocalTime(1000), Value::LocalTime(2000));
        assert_eq!(Value::DateTime(100, 0), Value::DateTime(100, 0));
        assert_ne!(Value::DateTime(100, 0), Value::DateTime(100, 1));
        assert_eq!(Value::LocalDateTime(100, 0), Value::LocalDateTime(100, 0));
        assert_ne!(Value::LocalDateTime(100, 0), Value::LocalDateTime(200, 0));
        assert_eq!(
            Value::ZonedDateTime(100, 0, 3600),
            Value::ZonedDateTime(100, 0, 3600)
        );
        assert_ne!(
            Value::ZonedDateTime(100, 0, 3600),
            Value::ZonedDateTime(100, 0, 7200)
        );
        assert_eq!(Value::ZonedTime(1000, 3600), Value::ZonedTime(1000, 3600));
        assert_ne!(Value::ZonedTime(1000, 3600), Value::ZonedTime(1000, 7200));
        assert_eq!(Value::Duration(1, 100), Value::Duration(1, 100));
        assert_ne!(Value::Duration(1, 100), Value::Duration(2, 100));
    }

    #[test]
    fn eq_same_type_constructed() {
        assert_eq!(
            Value::List(vec![Value::Int64(1)]),
            Value::List(vec![Value::Int64(1)])
        );
        assert_ne!(
            Value::List(vec![Value::Int64(1)]),
            Value::List(vec![Value::Int64(2)])
        );
        assert_eq!(
            Value::Record(vec![("a".into(), Value::Int64(1))]),
            Value::Record(vec![("a".into(), Value::Int64(1))])
        );
        assert_ne!(
            Value::Record(vec![("a".into(), Value::Int64(1))]),
            Value::Record(vec![("b".into(), Value::Int64(1))])
        );
        use crate::types::PathElement;
        assert_eq!(
            Value::Path(vec![PathElement::Vertex(1)]),
            Value::Path(vec![PathElement::Vertex(1)])
        );
        assert_ne!(
            Value::Path(vec![PathElement::Vertex(1)]),
            Value::Path(vec![PathElement::Vertex(2)])
        );
    }

    #[test]
    fn eq_float_variants() {
        assert_eq!(
            Value::Float16(half::f16::from_f64(1.0)),
            Value::Float16(half::f16::from_f64(1.0))
        );
        assert_ne!(
            Value::Float16(half::f16::from_f64(1.0)),
            Value::Float16(half::f16::from_f64(2.0))
        );
        assert_eq!(Value::Float32(1.0), Value::Float32(1.0));
        assert_ne!(Value::Float32(1.0), Value::Float32(2.0));
        assert_eq!(
            Value::Decimal(Decimal::from_i64(1)),
            Decimal::from_i64(1).into()
        );
    }

    #[test]
    fn eq_bytes_text() {
        assert_eq!(Value::Bytes(vec![1, 2]), Value::Bytes(vec![1, 2]));
        assert_ne!(Value::Bytes(vec![1, 2]), Value::Bytes(vec![3, 4]));
        assert_eq!(Value::Text("a".into()), Value::Text("a".into()));
        assert_ne!(Value::Text("a".into()), Value::Text("b".into()));
    }

    #[test]
    fn eq_256_variants() {
        let a = Value::Int256(crate::types::Int256::new(ethnum::I256::new(42)));
        let b = Value::Int256(crate::types::Int256::new(ethnum::I256::new(42)));
        let c = Value::Int256(crate::types::Int256::new(ethnum::I256::new(99)));
        assert_eq!(a, b);
        assert_ne!(a, c);

        let ua = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(42)));
        let ub = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(42)));
        let uc = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(99)));
        assert_eq!(ua, ub);
        assert_ne!(ua, uc);
    }

    #[test]
    fn display_256_variants() {
        let v = Value::Int256(crate::types::Int256::new(ethnum::I256::new(42)));
        assert_eq!(format!("{}", v), "42");
        assert!(format!("{:?}", v).contains("Int256"));

        let u = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(99)));
        assert_eq!(format!("{}", u), "99");
        assert!(format!("{:?}", u).contains("Uint256"));
    }

    #[test]
    fn clone_256_and_temporal() {
        let v = Value::Int256(crate::types::Int256::new(ethnum::I256::new(42)));
        assert_eq!(v, v.clone());
        let u = Value::Uint256(crate::types::Uint256::new(ethnum::U256::new(99)));
        assert_eq!(u, u.clone());
    }

    #[test]
    fn display_path_with_elements() {
        use crate::types::PathElement;
        let p = Value::Path(vec![
            PathElement::Vertex(1),
            PathElement::Edge {
                src: 1,
                dst: 2,
                label: Some("knows".into()),
            },
            PathElement::Vertex(2),
        ]);
        let s = format!("{}", p);
        assert!(s.contains("path"));
        assert!(s.contains("3"));
    }

    #[test]
    fn from_string_owned() {
        let s = String::from("hello");
        let v: Value = s.into();
        assert_eq!(v, Value::Text("hello".into()));
    }

    #[test]
    fn from_option_none_various_types() {
        let v: Value = None::<i32>.into();
        assert_eq!(v, Value::Null);
        let v: Value = None::<String>.into();
        assert_eq!(v, Value::Null);
        let v: Value = None::<bool>.into();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn from_option_some_various() {
        let v: Value = Some(42i32).into();
        assert_eq!(v, Value::Int32(42));
        let v: Value = Some(true).into();
        assert_eq!(v, Value::Bool(true));
        let v: Value = Some("hello").into();
        assert_eq!(v, Value::Text("hello".into()));
    }

    // Test ExtensionValue trait with a mock implementation
    #[derive(Debug, Clone)]
    struct MockExt(String);

    impl fmt::Display for MockExt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "MockExt({})", self.0)
        }
    }

    impl ExtensionValue for MockExt {
        fn type_name(&self) -> &str {
            "MockExt"
        }
        fn clone_box(&self) -> Box<dyn ExtensionValue> {
            Box::new(self.clone())
        }
        fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
            if let Some(o) = other.as_any().downcast_ref::<MockExt>() {
                self.0 == o.0
            } else {
                false
            }
        }
        fn cmp_ext(&self, other: &dyn ExtensionValue) -> Option<Ordering> {
            other
                .as_any()
                .downcast_ref::<MockExt>()
                .map(|o| self.0.cmp(&o.0))
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn extension_value_clone_eq_display_debug() {
        let ext = Value::Extension(Box::new(MockExt("hello".into())));
        let ext2 = ext.clone();
        assert_eq!(ext, ext2);

        let ext3 = Value::Extension(Box::new(MockExt("world".into())));
        assert_ne!(ext, ext3);

        assert_eq!(format!("{}", ext), "MockExt(hello)");
        assert!(format!("{:?}", ext).contains("Extension"));
        assert!(format!("{:?}", ext).contains("MockExt"));
    }

    #[test]
    fn extension_not_eq_different_types() {
        let ext = Value::Extension(Box::new(MockExt("hello".into())));
        assert_ne!(ext, Value::Null);
        assert_ne!(ext, Value::Int64(42));
    }

    #[test]
    fn stable_value_round_trips_nested_record() {
        let value = Value::Record(vec![
            ("uid".to_owned(), Value::Text("u1".to_owned())),
            ("weight".to_owned(), Value::Int64(5)),
            (
                "flags".to_owned(),
                Value::List(vec![Value::Bool(true), Value::Null, Value::Bytes(vec![1, 2])]),
            ),
        ]);
        let restored = Value::from_stable_bytes(&value.to_stable_bytes().expect("encode"))
            .expect("decode");
        assert_eq!(restored, value);
    }

    #[test]
    fn stable_value_rejects_extension_variant() {
        let value = Value::Extension(Box::new(MockExt("hello".into())));
        assert_eq!(
            value.to_stable_bytes().expect_err("extension should fail"),
            StableValueError::InvalidExtensionType
        );
    }
}
