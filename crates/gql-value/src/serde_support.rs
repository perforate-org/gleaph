//! Canonical text (JSON / NDJSON) serde support for [`Value`], behind the `serde` feature.
//!
//! Every variant is an externally tagged single-key object, so a value always serializes to
//! one `{"Variant": payload}` pair:
//!
//! - scalars: `{"Text":"hi"}`, `{"Int64":42}`, `{"Bool":true}`, `{"Float64":1.5}`,
//!   `{"Null":null}`;
//! - integer payloads accept a JSON number or a decimal string; 128-bit and wider integers
//!   always serialize as decimal strings
//!   (`{"Int128":"170141183460469231731687303715884105727"}`);
//! - `Int256`, `Uint256`, `Decimal`, `Float128`, and `Float256` are decimal strings only;
//! - temporals are named objects: `{"DateTime":{"seconds":N,"nanos":M}}`,
//!   `{"ZonedDateTime":{"seconds":N,"nanos":M,"offset_seconds":O}}`,
//!   `{"Duration":{"months":M,"nanos":N}}`, `{"Date":12345}`, `{"Time":N}`;
//! - `Bytes` is a base64 string (`{"Bytes":"AQID"}`);
//! - `Record` is an object whose field order is preserved; duplicate keys are rejected;
//! - `List` and `Path` are arrays; path elements are `{"Vertex":"id"}` / `{"Edge":"id"}`;
//! - `Extension` values are rejected in both directions (fail-closed).

use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use base64::Engine as _;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use crate::Value;
use crate::types::{Decimal, Int256, PathElement, PathElementId, Uint256};

// ──── helpers ────

fn finite_ser<S: Serializer>(name: &str, is_finite: bool) -> Result<(), S::Error> {
    if is_finite {
        Ok(())
    } else {
        Err(serde::ser::Error::custom(format!("{name} must be finite")))
    }
}

fn finite_de<E: de::Error>(name: &str, is_finite: bool) -> Result<(), E> {
    if is_finite {
        Ok(())
    } else {
        Err(de::Error::custom(format!("{name} must be finite")))
    }
}

fn parse_decimal<T, E>(text: &str) -> Result<T, E>
where
    T: FromStr,
    T::Err: fmt::Display,
    E: de::Error,
{
    T::from_str(text).map_err(|_| de::Error::custom(format!("invalid integer {text:?}")))
}

/// Integer payload that may appear as a JSON number or a decimal string.
///
/// The canonical text form accepts both so that hand-written JSON and tooling that cannot
/// represent integers as JSON numbers (e.g. JS `BigInt`) can produce the same artifact.
fn de_int_flex<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: FromStr + Copy,
    T::Err: fmt::Display,
    D: Deserializer<'de>,
{
    struct Flex<T>(PhantomData<T>);

    impl<'de, T> Visitor<'de> for Flex<T>
    where
        T: FromStr + Copy,
        T::Err: fmt::Display,
    {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "an integer or a decimal string")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_decimal(&value.to_string())
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_decimal(&value.to_string())
        }

        fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_decimal(&value.to_string())
        }

        fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_decimal(&value.to_string())
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_decimal(value)
        }
    }

    deserializer.deserialize_any(Flex::<T>(PhantomData))
}

struct FlexSeed<T>(PhantomData<T>);

impl<'de, T> serde::de::DeserializeSeed<'de> for FlexSeed<T>
where
    T: FromStr + Copy,
    T::Err: fmt::Display,
{
    type Value = T;

    fn deserialize<D>(self, deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
    {
        de_int_flex(deserializer)
    }
}

/// Integer payload serialized as a plain JSON number but accepted as a number or string.
#[derive(Clone, Copy)]
struct I64Flex(i64);

impl Serialize for I64Flex {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for I64Flex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        de_int_flex(deserializer).map(I64Flex)
    }
}

/// Unsigned counterpart of [`I64Flex`].
#[derive(Clone, Copy)]
struct U64Flex(u64);

impl Serialize for U64Flex {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for U64Flex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        de_int_flex(deserializer).map(U64Flex)
    }
}

/// Payload parsed from a required decimal string.
fn next_parsed<'de, T, A>(map: &mut A, parse: fn(&str) -> Option<T>) -> Result<T, A::Error>
where
    A: MapAccess<'de>,
{
    let text = map.next_value::<String>()?;
    parse(&text).ok_or_else(|| de::Error::custom(format!("invalid decimal value {text:?}")))
}

fn next_f16<'de, A>(map: &mut A) -> Result<half::f16, A::Error>
where
    A: MapAccess<'de>,
{
    let value = map.next_value::<f64>()?;
    let narrowed = half::f16::from_f64(value);
    finite_de::<A::Error>("Float16", narrowed.is_finite())?;
    Ok(narrowed)
}

#[cfg(feature = "f128")]
fn next_float128<'de, A>(map: &mut A) -> Result<f128, A::Error>
where
    A: MapAccess<'de>,
{
    let text = map.next_value::<String>()?;
    // `std::f128` has no `FromStr`/`Display` on this toolchain; parse exactly through `f256`
    // and narrow through `f64`, the same lossy path the crate uses elsewhere.
    let widened: f256::f256 = text
        .parse()
        .map_err(|_| de::Error::custom(format!("invalid f128 value {text:?}")))?;
    let narrowed: f64 = widened
        .to_string()
        .parse()
        .map_err(|_| de::Error::custom(format!("invalid f128 value {text:?}")))?;
    let value = narrowed as f128;
    finite_de::<A::Error>("Float128", value.is_finite())?;
    Ok(value)
}

#[cfg(feature = "f256")]
fn next_float256<'de, A>(map: &mut A) -> Result<f256::f256, A::Error>
where
    A: MapAccess<'de>,
{
    let text = map.next_value::<String>()?;
    let value: f256::f256 = text
        .parse()
        .map_err(|_| de::Error::custom(format!("invalid f256 value {text:?}")))?;
    finite_de::<A::Error>("Float256", value.is_finite())?;
    Ok(value)
}

fn next_base64<'de, A>(map: &mut A) -> Result<Vec<u8>, A::Error>
where
    A: MapAccess<'de>,
{
    let text = map.next_value::<String>()?;
    base64::engine::general_purpose::STANDARD
        .decode(text.as_bytes())
        .map_err(|_| de::Error::custom("Bytes must be a valid base64 string"))
}

// ──── temporal payload shapes ────

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DateTimePayload {
    seconds: I64Flex,
    nanos: u32,
}

impl DateTimePayload {
    fn into_value(self) -> (i64, u32) {
        (self.seconds.0, self.nanos)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalDateTimePayload {
    seconds: I64Flex,
    nanos: u32,
}

impl LocalDateTimePayload {
    fn into_value(self) -> (i64, u32) {
        (self.seconds.0, self.nanos)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ZonedDateTimePayload {
    seconds: I64Flex,
    nanos: u32,
    offset_seconds: i32,
}

impl ZonedDateTimePayload {
    fn into_value(self) -> (i64, u32, i32) {
        (self.seconds.0, self.nanos, self.offset_seconds)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ZonedTimePayload {
    nanos: U64Flex,
    offset_seconds: i32,
}

impl ZonedTimePayload {
    fn into_value(self) -> (u64, i32) {
        (self.nanos.0, self.offset_seconds)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurationPayload {
    months: i32,
    nanos: I64Flex,
}

impl DurationPayload {
    fn into_value(self) -> (i32, i64) {
        (self.months, self.nanos.0)
    }
}

// ──── Record ────

/// Serialization helper that emits fields in vector order (the binary wire contract).
struct RecordRef<'a>(&'a [(String, Value)]);

impl Serialize for RecordRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

/// Deserialization helper that preserves document field order and rejects duplicate names.
struct RecordOwned(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for RecordOwned {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RecordOwnedVisitor)
    }
}

struct RecordOwnedVisitor;

impl<'de> Visitor<'de> for RecordOwnedVisitor {
    type Value = RecordOwned;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a record object with unique field names")
    }

    fn visit_map<A>(self, mut map: A) -> Result<RecordOwned, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = Vec::new();
        let mut seen = HashSet::new();
        while let Some(name) = map.next_key::<String>()? {
            if !seen.insert(name.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate record field {name:?}"
                )));
            }
            fields.push((name, map.next_value::<Value>()?));
        }
        Ok(RecordOwned(fields))
    }
}

// ──── PathElement ────

impl Serialize for PathElement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (variant, id) = match self {
            PathElement::Vertex(id) => ("Vertex", id),
            PathElement::Edge(id) => ("Edge", id),
        };
        let text = String::from_utf8(id.to_vec())
            .map_err(|_| serde::ser::Error::custom("path element id is not valid UTF-8"))?;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(variant, &text)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for PathElement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PathElementVisitor;

        impl<'de> Visitor<'de> for PathElementVisitor {
            type Value = PathElement;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a path element like {{\"Vertex\":\"id\"}}")
            }

            fn visit_map<A>(self, mut map: A) -> Result<PathElement, A::Error>
            where
                A: MapAccess<'de>,
            {
                let variant = map.next_key::<String>()?.ok_or_else(|| {
                    de::Error::custom("path element requires exactly one variant key")
                })?;
                let id = PathElementId::from(map.next_value::<String>()?.into_bytes());
                if map.next_key::<IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "path element has more than one variant key",
                    ));
                }
                match variant.as_str() {
                    "Vertex" => Ok(PathElement::Vertex(id)),
                    "Edge" => Ok(PathElement::Edge(id)),
                    other => Err(de::Error::custom(format!(
                        "unknown path element variant {other:?}"
                    ))),
                }
            }
        }

        deserializer.deserialize_map(PathElementVisitor)
    }
}

// ──── Value ────

impl Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Value::Null => map.serialize_entry("Null", &())?,
            Value::Bool(value) => map.serialize_entry("Bool", value)?,
            Value::Int8(value) => map.serialize_entry("Int8", value)?,
            Value::Int16(value) => map.serialize_entry("Int16", value)?,
            Value::Int32(value) => map.serialize_entry("Int32", value)?,
            Value::Int64(value) => map.serialize_entry("Int64", value)?,
            Value::Int128(value) => map.serialize_entry("Int128", &value.to_string())?,
            Value::Int256(value) => map.serialize_entry("Int256", &value.to_string())?,
            Value::Uint8(value) => map.serialize_entry("Uint8", value)?,
            Value::Uint16(value) => map.serialize_entry("Uint16", value)?,
            Value::Uint32(value) => map.serialize_entry("Uint32", value)?,
            Value::Uint64(value) => map.serialize_entry("Uint64", value)?,
            Value::Uint128(value) => map.serialize_entry("Uint128", &value.to_string())?,
            Value::Uint256(value) => map.serialize_entry("Uint256", &value.to_string())?,
            Value::Float16(value) => {
                finite_ser::<S>("Float16", value.is_finite())?;
                map.serialize_entry("Float16", &value.to_f64())?
            }
            Value::Float32(value) => {
                finite_ser::<S>("Float32", value.is_finite())?;
                map.serialize_entry("Float32", value)?
            }
            Value::Float64(value) => {
                finite_ser::<S>("Float64", value.is_finite())?;
                map.serialize_entry("Float64", value)?
            }
            #[cfg(feature = "f128")]
            Value::Float128(value) => {
                finite_ser::<S>("Float128", value.is_finite())?;
                // `std::f128` has no `Display`; print through `f64` (lossy, like the crate's
                // own f128 narrowing paths).
                map.serialize_entry("Float128", &(*value as f64).to_string())?
            }
            #[cfg(feature = "f256")]
            Value::Float256(value) => {
                finite_ser::<S>("Float256", value.is_finite())?;
                map.serialize_entry("Float256", &value.to_string())?
            }
            Value::Decimal(value) => map.serialize_entry("Decimal", &value.0.to_string())?,
            Value::Text(value) => map.serialize_entry("Text", value)?,
            Value::Bytes(value) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(value);
                map.serialize_entry("Bytes", &encoded)?
            }
            Value::Date(value) => map.serialize_entry("Date", value)?,
            Value::Time(value) => map.serialize_entry("Time", value)?,
            Value::LocalTime(value) => map.serialize_entry("LocalTime", value)?,
            Value::DateTime(seconds, nanos) => map.serialize_entry(
                "DateTime",
                &DateTimePayload {
                    seconds: I64Flex(*seconds),
                    nanos: *nanos,
                },
            )?,
            Value::LocalDateTime(seconds, nanos) => map.serialize_entry(
                "LocalDateTime",
                &LocalDateTimePayload {
                    seconds: I64Flex(*seconds),
                    nanos: *nanos,
                },
            )?,
            Value::ZonedDateTime(seconds, nanos, offset_seconds) => map.serialize_entry(
                "ZonedDateTime",
                &ZonedDateTimePayload {
                    seconds: I64Flex(*seconds),
                    nanos: *nanos,
                    offset_seconds: *offset_seconds,
                },
            )?,
            Value::ZonedTime(nanos, offset_seconds) => map.serialize_entry(
                "ZonedTime",
                &ZonedTimePayload {
                    nanos: U64Flex(*nanos),
                    offset_seconds: *offset_seconds,
                },
            )?,
            Value::Duration(months, nanos) => map.serialize_entry(
                "Duration",
                &DurationPayload {
                    months: *months,
                    nanos: I64Flex(*nanos),
                },
            )?,
            Value::List(items) => map.serialize_entry("List", items)?,
            Value::Path(items) => map.serialize_entry("Path", items)?,
            Value::Record(fields) => map.serialize_entry("Record", &RecordRef(fields))?,
            Value::Extension(_) => {
                return Err(serde::ser::Error::custom(
                    "extension values cannot be serialized to the canonical text form",
                ));
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Value;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a canonical GQL value object like {{\"Text\": \"...\"}}")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let variant = map.next_key::<String>()?.ok_or_else(|| {
                    de::Error::custom("canonical GQL value requires exactly one variant key")
                })?;
                let value = match variant.as_str() {
                    "Null" => {
                        map.next_value::<()>()?;
                        Value::Null
                    }
                    "Bool" => Value::Bool(map.next_value::<bool>()?),
                    "Int8" => Value::Int8(map.next_value_seed(FlexSeed::<i8>(PhantomData))?),
                    "Int16" => Value::Int16(map.next_value_seed(FlexSeed::<i16>(PhantomData))?),
                    "Int32" => Value::Int32(map.next_value_seed(FlexSeed::<i32>(PhantomData))?),
                    "Int64" => Value::Int64(map.next_value_seed(FlexSeed::<i64>(PhantomData))?),
                    "Int128" => Value::Int128(map.next_value_seed(FlexSeed::<i128>(PhantomData))?),
                    "Int256" => Value::Int256(next_parsed(&mut map, Int256::parse)?),
                    "Uint8" => Value::Uint8(map.next_value_seed(FlexSeed::<u8>(PhantomData))?),
                    "Uint16" => Value::Uint16(map.next_value_seed(FlexSeed::<u16>(PhantomData))?),
                    "Uint32" => Value::Uint32(map.next_value_seed(FlexSeed::<u32>(PhantomData))?),
                    "Uint64" => Value::Uint64(map.next_value_seed(FlexSeed::<u64>(PhantomData))?),
                    "Uint128" => {
                        Value::Uint128(map.next_value_seed(FlexSeed::<u128>(PhantomData))?)
                    }
                    "Uint256" => Value::Uint256(next_parsed(&mut map, Uint256::parse)?),
                    "Float16" => Value::Float16(next_f16(&mut map)?),
                    "Float32" => Value::Float32(map.next_value::<f32>()?),
                    "Float64" => Value::Float64(map.next_value::<f64>()?),
                    #[cfg(feature = "f128")]
                    "Float128" => Value::Float128(next_float128(&mut map)?),
                    #[cfg(feature = "f256")]
                    "Float256" => Value::Float256(next_float256(&mut map)?),
                    "Decimal" => Value::Decimal(next_parsed(&mut map, Decimal::parse)?),
                    "Text" => Value::Text(map.next_value::<String>()?),
                    "Bytes" => Value::Bytes(next_base64(&mut map)?),
                    "Date" => Value::Date(map.next_value_seed(FlexSeed::<i32>(PhantomData))?),
                    "Time" => Value::Time(map.next_value_seed(FlexSeed::<u64>(PhantomData))?),
                    "LocalTime" => {
                        Value::LocalTime(map.next_value_seed(FlexSeed::<u64>(PhantomData))?)
                    }
                    "DateTime" => {
                        let (seconds, nanos) = map.next_value::<DateTimePayload>()?.into_value();
                        Value::DateTime(seconds, nanos)
                    }
                    "LocalDateTime" => {
                        let (seconds, nanos) =
                            map.next_value::<LocalDateTimePayload>()?.into_value();
                        Value::LocalDateTime(seconds, nanos)
                    }
                    "ZonedDateTime" => {
                        let (seconds, nanos, offset_seconds) =
                            map.next_value::<ZonedDateTimePayload>()?.into_value();
                        Value::ZonedDateTime(seconds, nanos, offset_seconds)
                    }
                    "ZonedTime" => {
                        let (nanos, offset_seconds) =
                            map.next_value::<ZonedTimePayload>()?.into_value();
                        Value::ZonedTime(nanos, offset_seconds)
                    }
                    "Duration" => {
                        let (months, nanos) = map.next_value::<DurationPayload>()?.into_value();
                        Value::Duration(months, nanos)
                    }
                    "List" => Value::List(map.next_value::<Vec<Value>>()?),
                    "Path" => Value::Path(map.next_value::<Vec<PathElement>>()?),
                    "Record" => Value::Record(map.next_value::<RecordOwned>()?.0),
                    other => {
                        return Err(de::Error::custom(format!(
                            "unknown canonical GQL value variant {other:?}"
                        )));
                    }
                };
                if map.next_key::<IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "canonical GQL value has more than one variant key",
                    ));
                }
                Ok(value)
            }
        }

        deserializer.deserialize_map(ValueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(value: &Value) -> String {
        serde_json::to_string(value).expect("serialization should succeed")
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("deserialization should succeed")
    }

    #[test]
    fn scalars_use_external_tag_objects() {
        assert_eq!(json(&Value::Null), r#"{"Null":null}"#);
        assert_eq!(json(&Value::Bool(true)), r#"{"Bool":true}"#);
        assert_eq!(json(&Value::Int64(42)), r#"{"Int64":42}"#);
        assert_eq!(json(&Value::Uint64(42)), r#"{"Uint64":42}"#);
        assert_eq!(json(&Value::Float64(1.5)), r#"{"Float64":1.5}"#);
        assert_eq!(json(&Value::Text("hi".into())), r#"{"Text":"hi"}"#);
        assert_eq!(
            json(&Value::Int256(Int256::parse("42").unwrap())),
            r#"{"Int256":"42"}"#
        );
        assert_eq!(
            json(&Value::Int128(i128::MAX)),
            r#"{"Int128":"170141183460469231731687303715884105727"}"#
        );
    }

    #[test]
    fn round_trips_every_variant() {
        let cases = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int8(i8::MIN),
            Value::Int16(-1234),
            Value::Int32(-1),
            Value::Int64(i64::MIN),
            Value::Int128(i128::MIN),
            Value::Int256(Int256::parse(
                "-57896044618658097711785492504343953926634992332820282019728792003956564819968",
            )
            .unwrap()),
            Value::Uint8(u8::MAX),
            Value::Uint16(u16::MAX),
            Value::Uint32(u32::MAX),
            Value::Uint64(u64::MAX),
            Value::Uint128(u128::MAX),
            Value::Uint256(Uint256::parse(
                "115792089237316195423570985008687907853269984665640564039457584007913129639935",
            )
            .unwrap()),
            Value::Float16(half::f16::from_f64(1.5)),
            Value::Float32(0.5),
            Value::Float64(-2.25),
            Value::Decimal(Decimal::parse("123.4500").unwrap()),
            Value::Text("hello".into()),
            Value::Bytes(vec![0x01, 0x02, 0x03]),
            Value::Date(12345),
            Value::Time(1),
            Value::LocalTime(2),
            Value::DateTime(1_700_000_000, 5),
            Value::LocalDateTime(1_700_000_000, 6),
            Value::ZonedDateTime(1_700_000_000, 7, 3600),
            Value::ZonedTime(8, -3600),
            Value::Duration(2, 1_000_000),
            Value::List(vec![Value::Text("a".into()), Value::Null]),
            Value::Path(vec![
                PathElement::Vertex(PathElementId::from(b"v1".to_vec())),
                PathElement::Edge(PathElementId::from(b"e1".to_vec())),
            ]),
            Value::Record(vec![
                ("k".into(), Value::Int64(1)),
                ("k2".into(), Value::Bool(false)),
            ]),
        ];
        let cases = {
            let mut all = cases;
            #[cfg(feature = "f128")]
            all.push(Value::Float128(1.5f128));
            #[cfg(feature = "f256")]
            all.push(Value::Float256(f256::f256::from(1.5f64)));
            all
        };
        for value in cases {
            let text = json(&value);
            assert_eq!(parse(&text), value, "round trip failed for {text}");
        }
    }

    #[test]
    fn integers_accept_number_and_string_forms() {
        assert_eq!(parse(r#"{"Int64":42}"#), Value::Int64(42));
        assert_eq!(parse(r#"{"Int64":"42"}"#), Value::Int64(42));
        assert_eq!(
            parse(r#"{"Uint64":"18446744073709551615"}"#),
            Value::Uint64(u64::MAX)
        );
        assert_eq!(
            parse(r#"{"Int128":"170141183460469231731687303715884105727"}"#),
            Value::Int128(i128::MAX)
        );
        assert_eq!(parse(r#"{"Int8":"-128"}"#), Value::Int8(i8::MIN));
        assert!(serde_json::from_str::<Value>(r#"{"Int8":"300"}"#).is_err());
        assert!(serde_json::from_str::<Value>(r#"{"Int256":42}"#).is_err());
    }

    #[test]
    fn temporals_use_named_objects() {
        assert_eq!(
            json(&Value::DateTime(1_700_000_000, 5)),
            r#"{"DateTime":{"seconds":1700000000,"nanos":5}}"#
        );
        assert_eq!(
            json(&Value::ZonedDateTime(1_700_000_000, 7, 3600)),
            r#"{"ZonedDateTime":{"seconds":1700000000,"nanos":7,"offset_seconds":3600}}"#
        );
        assert_eq!(
            json(&Value::ZonedTime(8, -3600)),
            r#"{"ZonedTime":{"nanos":8,"offset_seconds":-3600}}"#
        );
        assert_eq!(
            json(&Value::Duration(2, 1_000_000)),
            r#"{"Duration":{"months":2,"nanos":1000000}}"#
        );
        assert_eq!(json(&Value::Date(12345)), r#"{"Date":12345}"#);
        assert_eq!(
            parse(r#"{"DateTime":{"seconds":"1700000000","nanos":5}}"#),
            Value::DateTime(1_700_000_000, 5)
        );
        assert!(serde_json::from_str::<Value>(r#"{"DateTime":{"seconds":1}}"#).is_err());
        assert!(
            serde_json::from_str::<Value>(r#"{"DateTime":{"seconds":1,"nanos":2,"extra":3}}"#)
                .is_err()
        );
    }

    #[test]
    fn bytes_use_base64() {
        assert_eq!(
            json(&Value::Bytes(vec![0x01, 0x02, 0x03])),
            r#"{"Bytes":"AQID"}"#
        );
        assert_eq!(
            parse(r#"{"Bytes":"AQID"}"#),
            Value::Bytes(vec![0x01, 0x02, 0x03])
        );
        assert!(serde_json::from_str::<Value>(r#"{"Bytes":"!!!"}"#).is_err());
    }

    #[test]
    fn record_preserves_order_and_rejects_duplicates() {
        let record = Value::Record(vec![
            ("b".into(), Value::Int64(1)),
            ("a".into(), Value::Int64(2)),
        ]);
        assert_eq!(
            json(&record),
            r#"{"Record":{"b":{"Int64":1},"a":{"Int64":2}}}"#
        );
        assert_eq!(
            parse(r#"{"Record":{"z":{"Int64":1},"m":{"Int64":2}}}"#),
            Value::Record(vec![
                ("z".into(), Value::Int64(1)),
                ("m".into(), Value::Int64(2)),
            ])
        );
        assert!(
            serde_json::from_str::<Value>(r#"{"Record":{"a":{"Int64":1},"a":{"Int64":2}}}"#)
                .is_err()
        );
    }

    #[test]
    fn lists_and_paths_use_arrays() {
        assert_eq!(
            json(&Value::List(vec![Value::Int64(1)])),
            r#"{"List":[{"Int64":1}]}"#
        );
        let path = Value::Path(vec![
            PathElement::Vertex(PathElementId::from(b"v1".to_vec())),
            PathElement::Edge(PathElementId::from(b"e1".to_vec())),
        ]);
        assert_eq!(json(&path), r#"{"Path":[{"Vertex":"v1"},{"Edge":"e1"}]}"#);
        assert_eq!(parse(r#"{"Path":[{"Vertex":"v1"},{"Edge":"e1"}]}"#), path);
    }

    #[test]
    fn rejects_unknown_variants_extra_keys_and_empty_objects() {
        assert!(serde_json::from_str::<Value>(r#"{"Bogus":1}"#).is_err());
        assert!(serde_json::from_str::<Value>(r#"{"Text":"a","Int64":1}"#).is_err());
        assert!(serde_json::from_str::<Value>(r#"{}"#).is_err());
        assert!(serde_json::from_str::<Value>(r#"[1,2]"#).is_err());
    }

    #[test]
    fn rejects_non_finite_floats() {
        assert!(serde_json::to_string(&Value::Float64(f64::NAN)).is_err());
        assert!(serde_json::to_string(&Value::Float32(f32::INFINITY)).is_err());
        assert!(serde_json::to_string(&Value::Float16(half::f16::INFINITY)).is_err());
        #[cfg(feature = "f256")]
        assert!(serde_json::from_str::<Value>(r#"{"Float256":"NaN"}"#).is_err());
        #[cfg(feature = "f128")]
        assert!(serde_json::from_str::<Value>(r#"{"Float128":"inf"}"#).is_err());
    }

    #[test]
    fn rejects_extension_values_in_both_directions() {
        #[derive(Debug)]
        struct TestExtension;

        impl fmt::Display for TestExtension {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "TestExtension")
            }
        }

        impl crate::ExtensionValue for TestExtension {
            fn type_name(&self) -> &str {
                "Test"
            }
            fn clone_box(&self) -> Box<dyn crate::ExtensionValue> {
                Box::new(TestExtension)
            }
            fn eq_ext(&self, other: &dyn crate::ExtensionValue) -> bool {
                other.as_any().is::<TestExtension>()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let extension = Value::Extension(Box::new(TestExtension));
        let error = serde_json::to_string(&extension).expect_err("extensions must not serialize");
        assert!(error.to_string().contains("extension"));
        assert!(
            serde_json::from_str::<Value>(r#"{"Extension":{"Principal":"aaaaa-aa"}}"#).is_err()
        );
    }
}
