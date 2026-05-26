use super::ExtensionValue;
use crate::types::{Decimal, Int256, PathElement, Uint256};

// ──── Value enum ────

/// GQL runtime value, covering all standard types from GQL plus extensions.
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
pub enum Value {
    Null,
    Bool(bool),

    // signedBinaryExactNumericType
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    Int256(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = crate::types::Int256Def))] Int256),

    // unsignedBinaryExactNumericType
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Uint128(u128),
    Uint256(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = crate::types::Uint256Def))] Uint256,
    ),

    // approximateNumericType
    Float16(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = crate::rkyv_support::F16Def))]
        half::f16,
    ),
    Float32(f32),
    Float64(f64),
    #[cfg(feature = "f128")]
    Float128(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = crate::rkyv_support::F128Def))] f128,
    ),
    #[cfg(feature = "f256")]
    Float256(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = crate::rkyv_support::F256Def))]
        f256::f256,
    ),

    // decimalExactNumericType
    Decimal(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = crate::types::DecimalDef))] Decimal,
    ),

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
    List(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Vec<Value>),
    Path(Vec<PathElement>),
    Record(#[cfg_attr(feature = "ast-rkyv-no-span", rkyv(omit_bounds))] Vec<(String, Value)>),

    // extension slot
    Extension(
        #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = crate::rkyv_support::ExtensionBinaryWire))]
         Box<dyn ExtensionValue>,
    ),
}
