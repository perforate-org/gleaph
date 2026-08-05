//! GQL temporal row bindings backed by `jiff` (the default temporal crate).
//!
//! Serde and Candid forms mirror the wire representations exactly (`Date` as days since epoch,
//! times as nanoseconds since midnight, date-times as `{seconds, nanos}` records, durations as
//! `{months, nanos}` records), so generated rows decode without lossy conversion and canisters
//! return them directly over candid.

use candid::CandidType;
use candid::types::{Type, TypeInner};
use gleaph_gql::Value;
use gleaph_gql::temporal;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, CandidType)]
struct DateTimeWire {
    seconds: i64,
    nanos: u32,
}

#[derive(Deserialize, CandidType)]
struct LocalDateTimeWire {
    seconds: i64,
    nanos: u32,
}

#[derive(Deserialize, CandidType)]
struct ZonedDateTimeWire {
    seconds: i64,
    nanos: u32,
    offset_seconds: i32,
}

#[derive(Deserialize, CandidType)]
struct DurationWire {
    months: i32,
    nanos: i64,
}

/// GQL `Date` row binding backed by `jiff::civil::Date`.
///
/// Serde and Candid use the days-since-epoch wire form ([`crate::GqlWireValue::Date`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlDate(jiff::civil::Date);

impl GqlDate {
    /// Wrap a jiff civil date.
    pub const fn from_inner(date: jiff::civil::Date) -> Self {
        Self(date)
    }

    /// Return the wrapped jiff civil date.
    pub const fn into_inner(self) -> jiff::civil::Date {
        self.0
    }
}

impl From<jiff::civil::Date> for GqlDate {
    fn from(date: jiff::civil::Date) -> Self {
        Self(date)
    }
}

impl From<GqlDate> for jiff::civil::Date {
    fn from(value: GqlDate) -> Self {
        value.0
    }
}

impl Serialize for GqlDate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i32(temporal::jiff_date_to_days(self.0))
    }
}

impl<'de> Deserialize<'de> for GqlDate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let days = i32::deserialize(deserializer)?;
        temporal::date_to_jiff(days)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("invalid GQL Date days count"))
    }
}

impl CandidType for GqlDate {
    fn _ty() -> Type {
        TypeInner::Int32.into()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_int32(temporal::jiff_date_to_days(self.0))
    }
}

impl From<GqlDate> for Value {
    fn from(value: GqlDate) -> Value {
        Value::Date(temporal::jiff_date_to_days(value.0))
    }
}

/// GQL `Time` row binding backed by `jiff::civil::Time`.
///
/// Serde and Candid use the nanoseconds-since-midnight wire form
/// ([`crate::GqlWireValue::Time`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlTime(jiff::civil::Time);

impl GqlTime {
    /// Wrap a jiff civil time.
    pub const fn from_inner(time: jiff::civil::Time) -> Self {
        Self(time)
    }

    /// Return the wrapped jiff civil time.
    pub const fn into_inner(self) -> jiff::civil::Time {
        self.0
    }
}

impl From<jiff::civil::Time> for GqlTime {
    fn from(time: jiff::civil::Time) -> Self {
        Self(time)
    }
}

impl From<GqlTime> for jiff::civil::Time {
    fn from(value: GqlTime) -> Self {
        value.0
    }
}

impl Serialize for GqlTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(temporal::jiff_time_to_nanos(self.0))
    }
}

impl<'de> Deserialize<'de> for GqlTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let nanos = u64::deserialize(deserializer)?;
        temporal::time_to_jiff(nanos)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("invalid GQL Time nanos"))
    }
}

impl CandidType for GqlTime {
    fn _ty() -> Type {
        TypeInner::Nat64.into()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_nat64(temporal::jiff_time_to_nanos(self.0))
    }
}

impl From<GqlTime> for Value {
    fn from(value: GqlTime) -> Value {
        Value::Time(temporal::jiff_time_to_nanos(value.0))
    }
}

/// GQL `LocalTime` row binding backed by `jiff::civil::Time`.
///
/// Shares the wire form with [`GqlTime`] but projects to `Value::LocalTime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlLocalTime(jiff::civil::Time);

impl GqlLocalTime {
    /// Wrap a jiff civil time.
    pub const fn from_inner(time: jiff::civil::Time) -> Self {
        Self(time)
    }

    /// Return the wrapped jiff civil time.
    pub const fn into_inner(self) -> jiff::civil::Time {
        self.0
    }
}

impl From<jiff::civil::Time> for GqlLocalTime {
    fn from(time: jiff::civil::Time) -> Self {
        Self(time)
    }
}

impl From<GqlLocalTime> for jiff::civil::Time {
    fn from(value: GqlLocalTime) -> Self {
        value.0
    }
}

impl Serialize for GqlLocalTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(temporal::jiff_time_to_nanos(self.0))
    }
}

impl<'de> Deserialize<'de> for GqlLocalTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let nanos = u64::deserialize(deserializer)?;
        temporal::time_to_jiff(nanos)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("invalid GQL LocalTime nanos"))
    }
}

impl CandidType for GqlLocalTime {
    fn _ty() -> Type {
        TypeInner::Nat64.into()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_nat64(temporal::jiff_time_to_nanos(self.0))
    }
}

impl From<GqlLocalTime> for Value {
    fn from(value: GqlLocalTime) -> Value {
        Value::LocalTime(temporal::jiff_time_to_nanos(value.0))
    }
}

/// GQL `DateTime` row binding backed by `jiff::Timestamp`.
///
/// Serde and Candid use the `{seconds, nanos}` wire form ([`crate::GqlWireValue::DateTime`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlDateTime(jiff::Timestamp);

impl GqlDateTime {
    /// Wrap a jiff timestamp.
    pub const fn from_inner(timestamp: jiff::Timestamp) -> Self {
        Self(timestamp)
    }

    /// Return the wrapped jiff timestamp.
    pub const fn into_inner(self) -> jiff::Timestamp {
        self.0
    }
}

impl From<jiff::Timestamp> for GqlDateTime {
    fn from(timestamp: jiff::Timestamp) -> Self {
        Self(timestamp)
    }
}

impl From<GqlDateTime> for jiff::Timestamp {
    fn from(value: GqlDateTime) -> Self {
        value.0
    }
}

impl Serialize for GqlDateTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("GqlDateTime", 2)?;
        state.serialize_field("seconds", &self.0.as_second())?;
        state.serialize_field("nanos", &(self.0.subsec_nanosecond() as u32))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GqlDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DateTimeWire::deserialize(deserializer)?;
        jiff::Timestamp::new(wire.seconds, wire.nanos as i32)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl CandidType for GqlDateTime {
    fn _ty() -> Type {
        <DateTimeWire as CandidType>::ty()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        CandidType::idl_serialize(
            &DateTimeWire {
                seconds: self.0.as_second(),
                nanos: self.0.subsec_nanosecond() as u32,
            },
            serializer,
        )
    }
}

impl From<GqlDateTime> for Value {
    fn from(value: GqlDateTime) -> Value {
        Value::DateTime(value.0.as_second(), value.0.subsec_nanosecond() as u32)
    }
}

/// GQL `LocalDateTime` row binding backed by `jiff::civil::DateTime`.
///
/// Serde and Candid use the `{seconds, nanos}` wire form, interpreted as a civil date-time in
/// UTC ([`crate::GqlWireValue::LocalDateTime`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlLocalDateTime(jiff::civil::DateTime);

impl GqlLocalDateTime {
    /// Wrap a jiff civil date-time.
    pub const fn from_inner(datetime: jiff::civil::DateTime) -> Self {
        Self(datetime)
    }

    /// Return the wrapped jiff civil date-time.
    pub const fn into_inner(self) -> jiff::civil::DateTime {
        self.0
    }
}

impl From<jiff::civil::DateTime> for GqlLocalDateTime {
    fn from(datetime: jiff::civil::DateTime) -> Self {
        Self(datetime)
    }
}

impl From<GqlLocalDateTime> for jiff::civil::DateTime {
    fn from(value: GqlLocalDateTime) -> Self {
        value.0
    }
}

impl Serialize for GqlLocalDateTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let (seconds, nanos) = temporal::jiff_datetime_to_unix(self.0);
        let mut state = serializer.serialize_struct("GqlLocalDateTime", 2)?;
        state.serialize_field("seconds", &seconds)?;
        state.serialize_field("nanos", &nanos)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GqlLocalDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = LocalDateTimeWire::deserialize(deserializer)?;
        temporal::datetime_to_jiff(wire.seconds, wire.nanos)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("invalid GQL LocalDateTime"))
    }
}

impl CandidType for GqlLocalDateTime {
    fn _ty() -> Type {
        <LocalDateTimeWire as CandidType>::ty()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        let (seconds, nanos) = temporal::jiff_datetime_to_unix(self.0);
        CandidType::idl_serialize(&LocalDateTimeWire { seconds, nanos }, serializer)
    }
}

impl From<GqlLocalDateTime> for Value {
    fn from(value: GqlLocalDateTime) -> Value {
        let (seconds, nanos) = temporal::jiff_datetime_to_unix(value.0);
        Value::LocalDateTime(seconds, nanos)
    }
}

/// GQL `ZonedDateTime` row binding backed by `jiff::Zoned`.
///
/// Serde and Candid use the `{seconds, nanos, offset_seconds}` wire form
/// ([`crate::GqlWireValue::ZonedDateTime`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GqlZonedDateTime(jiff::Zoned);

impl GqlZonedDateTime {
    /// Wrap a jiff zoned date-time.
    pub fn from_inner(zoned: jiff::Zoned) -> Self {
        Self(zoned)
    }

    /// Return the wrapped jiff zoned date-time.
    pub fn into_inner(self) -> jiff::Zoned {
        self.0
    }
}

impl From<jiff::Zoned> for GqlZonedDateTime {
    fn from(zoned: jiff::Zoned) -> Self {
        Self(zoned)
    }
}

impl From<GqlZonedDateTime> for jiff::Zoned {
    fn from(value: GqlZonedDateTime) -> Self {
        value.0
    }
}

impl Serialize for GqlZonedDateTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let timestamp = self.0.timestamp();
        let mut state = serializer.serialize_struct("GqlZonedDateTime", 3)?;
        state.serialize_field("seconds", &timestamp.as_second())?;
        state.serialize_field("nanos", &(timestamp.subsec_nanosecond() as u32))?;
        state.serialize_field("offset_seconds", &self.0.offset().seconds())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GqlZonedDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ZonedDateTimeWire::deserialize(deserializer)?;
        let timestamp = jiff::Timestamp::new(wire.seconds, wire.nanos as i32)
            .map_err(serde::de::Error::custom)?;
        let time_zone = jiff::tz::Offset::from_seconds(wire.offset_seconds)
            .map_err(serde::de::Error::custom)?
            .to_time_zone();
        Ok(Self(jiff::Zoned::new(timestamp, time_zone)))
    }
}

impl CandidType for GqlZonedDateTime {
    fn _ty() -> Type {
        <ZonedDateTimeWire as CandidType>::ty()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        let timestamp = self.0.timestamp();
        CandidType::idl_serialize(
            &ZonedDateTimeWire {
                seconds: timestamp.as_second(),
                nanos: timestamp.subsec_nanosecond() as u32,
                offset_seconds: self.0.offset().seconds(),
            },
            serializer,
        )
    }
}

impl From<GqlZonedDateTime> for Value {
    fn from(value: GqlZonedDateTime) -> Value {
        let (seconds, nanos, offset_seconds) = temporal::jiff_zoned_to_value(&value.0);
        Value::ZonedDateTime(seconds, nanos, offset_seconds)
    }
}

/// GQL `Duration` row binding backed by `jiff::Span`.
///
/// Serde and Candid use the `{months, nanos}` wire form ([`crate::GqlWireValue::Duration`]).
#[derive(Clone, Copy, Debug)]
pub struct GqlDuration(jiff::Span);

impl PartialEq for GqlDuration {
    fn eq(&self, other: &Self) -> bool {
        self.0.fieldwise() == other.0.fieldwise()
    }
}

impl Eq for GqlDuration {}

impl GqlDuration {
    /// Wrap a jiff span.
    pub const fn from_inner(span: jiff::Span) -> Self {
        Self(span)
    }

    /// Return the wrapped jiff span.
    pub const fn into_inner(self) -> jiff::Span {
        self.0
    }
}

impl From<jiff::Span> for GqlDuration {
    fn from(span: jiff::Span) -> Self {
        Self(span)
    }
}

impl From<GqlDuration> for jiff::Span {
    fn from(value: GqlDuration) -> Self {
        value.0
    }
}

impl Serialize for GqlDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let (months, nanos) = temporal::jiff_span_to_duration(self.0);
        let mut state = serializer.serialize_struct("GqlDuration", 2)?;
        state.serialize_field("months", &months)?;
        state.serialize_field("nanos", &nanos)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GqlDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DurationWire::deserialize(deserializer)?;
        temporal::duration_to_jiff(wire.months, wire.nanos)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("invalid GQL Duration"))
    }
}

impl CandidType for GqlDuration {
    fn _ty() -> Type {
        <DurationWire as CandidType>::ty()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        let (months, nanos) = temporal::jiff_span_to_duration(self.0);
        CandidType::idl_serialize(&DurationWire { months, nanos }, serializer)
    }
}

impl From<GqlDuration> for Value {
    fn from(value: GqlDuration) -> Value {
        let (months, nanos) = temporal::jiff_span_to_duration(value.0);
        Value::Duration(months, nanos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoned(timestamp: jiff::Timestamp) -> jiff::Zoned {
        jiff::Zoned::new(
            timestamp,
            jiff::tz::Offset::from_seconds(9 * 3600)
                .unwrap()
                .to_time_zone(),
        )
    }

    #[test]
    fn jiff_bindings_round_trip_serde_wire_shapes() {
        let date = GqlDate::from(jiff::civil::Date::new(2024, 1, 15).unwrap());
        let json = serde_json::to_value(date).unwrap();
        assert_eq!(json, serde_json::json!(19737));
        assert_eq!(serde_json::from_value::<GqlDate>(json).unwrap(), date);

        let time = GqlTime::from(jiff::civil::Time::new(10, 30, 0, 0).unwrap());
        let json = serde_json::to_value(time).unwrap();
        assert_eq!(json, serde_json::json!(37_800_000_000_000_i64));
        assert_eq!(serde_json::from_value::<GqlTime>(json).unwrap(), time);

        let datetime = GqlDateTime::from(jiff::Timestamp::new(1_700_000_000, 123_000_000).unwrap());
        let json = serde_json::to_value(datetime).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "seconds": 1_700_000_000, "nanos": 123_000_000 })
        );
        assert_eq!(
            serde_json::from_value::<GqlDateTime>(json).unwrap(),
            datetime
        );

        let zoned = GqlZonedDateTime::from(zoned(jiff::Timestamp::new(1_700_000_000, 0).unwrap()));
        let json = serde_json::to_value(&zoned).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "seconds": 1_700_000_000,
                "nanos": 0,
                "offset_seconds": 9 * 3600
            })
        );
        assert_eq!(
            serde_json::from_value::<GqlZonedDateTime>(json).unwrap(),
            zoned
        );

        let duration = GqlDuration::from(jiff::Span::new().months(2).nanoseconds(5));
        let json = serde_json::to_value(duration).unwrap();
        assert_eq!(json, serde_json::json!({ "months": 2, "nanos": 5 }));
        assert_eq!(
            serde_json::from_value::<GqlDuration>(json).unwrap(),
            duration
        );
    }

    #[test]
    fn jiff_bindings_project_to_logical_values() {
        let datetime = GqlDateTime::from(jiff::Timestamp::new(1_700_000_000, 123_000_000).unwrap());
        assert_eq!(
            Value::from(datetime),
            Value::DateTime(1_700_000_000, 123_000_000)
        );

        let local = GqlLocalDateTime::from(jiff::civil::DateTime::from_parts(
            jiff::civil::Date::new(2024, 1, 15).unwrap(),
            jiff::civil::Time::new(10, 30, 0, 0).unwrap(),
        ));
        assert!(matches!(Value::from(local), Value::LocalDateTime(_, _)));

        let zoned = GqlZonedDateTime::from(zoned(jiff::Timestamp::new(1_700_000_000, 0).unwrap()));
        assert_eq!(
            Value::from(zoned),
            Value::ZonedDateTime(1_700_000_000, 0, 9 * 3600)
        );

        let duration = GqlDuration::from(jiff::Span::new().months(2).nanoseconds(5));
        assert_eq!(Value::from(duration), Value::Duration(2, 5));

        let date = GqlDate::from(jiff::civil::Date::new(2024, 1, 15).unwrap());
        assert!(matches!(Value::from(date), Value::Date(_)));

        let time = GqlTime::from(jiff::civil::Time::new(10, 30, 0, 0).unwrap());
        assert!(matches!(Value::from(time), Value::Time(_)));

        let local_time = GqlLocalTime::from(jiff::civil::Time::new(10, 30, 0, 0).unwrap());
        assert!(matches!(Value::from(local_time), Value::LocalTime(_)));
    }

    #[test]
    fn jiff_bindings_round_trip_candid() {
        let zoned = GqlZonedDateTime::from(zoned(jiff::Timestamp::new(1_700_000_000, 0).unwrap()));
        let bytes = candid::encode_one(zoned.clone()).unwrap();
        assert_eq!(
            candid::decode_one::<GqlZonedDateTime>(&bytes).unwrap(),
            zoned
        );

        let duration = GqlDuration::from(jiff::Span::new().months(2).nanoseconds(5));
        let bytes = candid::encode_one(duration).unwrap();
        assert_eq!(candid::decode_one::<GqlDuration>(&bytes).unwrap(), duration);

        let date = GqlDate::from(jiff::civil::Date::new(2024, 1, 15).unwrap());
        let bytes = candid::encode_one(date).unwrap();
        assert_eq!(candid::decode_one::<GqlDate>(&bytes).unwrap(), date);

        let time = GqlTime::from(jiff::civil::Time::new(10, 30, 0, 0).unwrap());
        let bytes = candid::encode_one(time).unwrap();
        assert_eq!(candid::decode_one::<GqlTime>(&bytes).unwrap(), time);
    }
}
