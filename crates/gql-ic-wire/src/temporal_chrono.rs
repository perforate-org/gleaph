//! GQL temporal row bindings backed by `chrono` (opt-in alternative to the default `jiff`
//! bindings).
//!
//! Serde and Candid forms mirror the wire representations exactly, matching the `jiff` bindings,
//! so generated rows decode identically under either temporal crate. `chrono::TimeDelta` cannot
//! represent calendar months, so `GqlDuration` serializes `months: 0` and rejects deserializing
//! a nonzero months component.

use candid::CandidType;
use candid::types::{Type, TypeInner};
use chrono::{Datelike, Timelike};
use gleaph_gql::Value;
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

/// Days from 0001-01-01 (the `chrono` era anchor) to 1970-01-01.
const UNIX_EPOCH_DAYS_FROM_CE: i32 = 719_163;

/// GQL `Date` row binding backed by `chrono::NaiveDate`.
///
/// Serde and Candid use the days-since-epoch wire form ([`crate::GqlWireValue::Date`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlDate(chrono::NaiveDate);

impl GqlDate {
    /// Wrap a chrono naive date.
    pub const fn from_inner(date: chrono::NaiveDate) -> Self {
        Self(date)
    }

    /// Return the wrapped chrono naive date.
    pub const fn into_inner(self) -> chrono::NaiveDate {
        self.0
    }
}

impl From<chrono::NaiveDate> for GqlDate {
    fn from(date: chrono::NaiveDate) -> Self {
        Self(date)
    }
}

impl From<GqlDate> for chrono::NaiveDate {
    fn from(value: GqlDate) -> Self {
        value.0
    }
}

impl Serialize for GqlDate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i32(self.0.num_days_from_ce() - UNIX_EPOCH_DAYS_FROM_CE)
    }
}

impl<'de> Deserialize<'de> for GqlDate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let days = i32::deserialize(deserializer)?;
        chrono::NaiveDate::from_num_days_from_ce_opt(days + UNIX_EPOCH_DAYS_FROM_CE)
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
        serializer.serialize_int32(self.0.num_days_from_ce() - UNIX_EPOCH_DAYS_FROM_CE)
    }
}

impl From<GqlDate> for Value {
    fn from(value: GqlDate) -> Value {
        Value::Date(value.0.num_days_from_ce() - UNIX_EPOCH_DAYS_FROM_CE)
    }
}

/// GQL `Time` row binding backed by `chrono::NaiveTime`.
///
/// Serde and Candid use the nanoseconds-since-midnight wire form
/// ([`crate::GqlWireValue::Time`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlTime(chrono::NaiveTime);

impl GqlTime {
    /// Wrap a chrono naive time.
    pub const fn from_inner(time: chrono::NaiveTime) -> Self {
        Self(time)
    }

    /// Return the wrapped chrono naive time.
    pub const fn into_inner(self) -> chrono::NaiveTime {
        self.0
    }
}

impl From<chrono::NaiveTime> for GqlTime {
    fn from(time: chrono::NaiveTime) -> Self {
        Self(time)
    }
}

impl From<GqlTime> for chrono::NaiveTime {
    fn from(value: GqlTime) -> Self {
        value.0
    }
}

fn time_to_nanos(time: chrono::NaiveTime) -> u64 {
    time.num_seconds_from_midnight() as u64 * 1_000_000_000 + time.nanosecond() as u64
}

impl Serialize for GqlTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(time_to_nanos(self.0))
    }
}

impl<'de> Deserialize<'de> for GqlTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let nanos = u64::deserialize(deserializer)?;
        let seconds = (nanos / 1_000_000_000) as u32;
        let subsec = (nanos % 1_000_000_000) as u32;
        chrono::NaiveTime::from_num_seconds_from_midnight_opt(seconds, subsec)
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
        serializer.serialize_nat64(time_to_nanos(self.0))
    }
}

impl From<GqlTime> for Value {
    fn from(value: GqlTime) -> Value {
        Value::Time(time_to_nanos(value.0))
    }
}

/// GQL `LocalTime` row binding backed by `chrono::NaiveTime`.
///
/// Shares the wire form with [`GqlTime`] but projects to `Value::LocalTime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlLocalTime(chrono::NaiveTime);

impl GqlLocalTime {
    /// Wrap a chrono naive time.
    pub const fn from_inner(time: chrono::NaiveTime) -> Self {
        Self(time)
    }

    /// Return the wrapped chrono naive time.
    pub const fn into_inner(self) -> chrono::NaiveTime {
        self.0
    }
}

impl From<chrono::NaiveTime> for GqlLocalTime {
    fn from(time: chrono::NaiveTime) -> Self {
        Self(time)
    }
}

impl From<GqlLocalTime> for chrono::NaiveTime {
    fn from(value: GqlLocalTime) -> Self {
        value.0
    }
}

impl Serialize for GqlLocalTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(time_to_nanos(self.0))
    }
}

impl<'de> Deserialize<'de> for GqlLocalTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let nanos = u64::deserialize(deserializer)?;
        let seconds = (nanos / 1_000_000_000) as u32;
        let subsec = (nanos % 1_000_000_000) as u32;
        chrono::NaiveTime::from_num_seconds_from_midnight_opt(seconds, subsec)
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
        serializer.serialize_nat64(time_to_nanos(self.0))
    }
}

impl From<GqlLocalTime> for Value {
    fn from(value: GqlLocalTime) -> Value {
        Value::LocalTime(time_to_nanos(value.0))
    }
}

/// GQL `DateTime` row binding backed by `chrono::DateTime<Utc>`.
///
/// Serde and Candid use the `{seconds, nanos}` wire form ([`crate::GqlWireValue::DateTime`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlDateTime(chrono::DateTime<chrono::Utc>);

impl GqlDateTime {
    /// Wrap a chrono UTC date-time.
    pub const fn from_inner(datetime: chrono::DateTime<chrono::Utc>) -> Self {
        Self(datetime)
    }

    /// Return the wrapped chrono UTC date-time.
    pub const fn into_inner(self) -> chrono::DateTime<chrono::Utc> {
        self.0
    }
}

impl From<chrono::DateTime<chrono::Utc>> for GqlDateTime {
    fn from(datetime: chrono::DateTime<chrono::Utc>) -> Self {
        Self(datetime)
    }
}

impl From<GqlDateTime> for chrono::DateTime<chrono::Utc> {
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
        state.serialize_field("seconds", &self.0.timestamp())?;
        state.serialize_field("nanos", &self.0.timestamp_subsec_nanos())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GqlDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DateTimeWire::deserialize(deserializer)?;
        chrono::DateTime::<chrono::Utc>::from_timestamp(wire.seconds, wire.nanos)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("invalid GQL DateTime"))
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
                seconds: self.0.timestamp(),
                nanos: self.0.timestamp_subsec_nanos(),
            },
            serializer,
        )
    }
}

impl From<GqlDateTime> for Value {
    fn from(value: GqlDateTime) -> Value {
        Value::DateTime(value.0.timestamp(), value.0.timestamp_subsec_nanos())
    }
}

/// GQL `LocalDateTime` row binding backed by `chrono::NaiveDateTime`.
///
/// Serde and Candid use the `{seconds, nanos}` wire form, interpreted as a civil date-time in
/// UTC ([`crate::GqlWireValue::LocalDateTime`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlLocalDateTime(chrono::NaiveDateTime);

impl GqlLocalDateTime {
    /// Wrap a chrono naive date-time.
    pub const fn from_inner(datetime: chrono::NaiveDateTime) -> Self {
        Self(datetime)
    }

    /// Return the wrapped chrono naive date-time.
    pub const fn into_inner(self) -> chrono::NaiveDateTime {
        self.0
    }
}

impl From<chrono::NaiveDateTime> for GqlLocalDateTime {
    fn from(datetime: chrono::NaiveDateTime) -> Self {
        Self(datetime)
    }
}

impl From<GqlLocalDateTime> for chrono::NaiveDateTime {
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
        let utc = self.0.and_utc();
        let mut state = serializer.serialize_struct("GqlLocalDateTime", 2)?;
        state.serialize_field("seconds", &utc.timestamp())?;
        state.serialize_field("nanos", &utc.timestamp_subsec_nanos())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GqlLocalDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = LocalDateTimeWire::deserialize(deserializer)?;
        chrono::DateTime::<chrono::Utc>::from_timestamp(wire.seconds, wire.nanos)
            .map(|utc| Self(utc.naive_utc()))
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
        let utc = self.0.and_utc();
        CandidType::idl_serialize(
            &LocalDateTimeWire {
                seconds: utc.timestamp(),
                nanos: utc.timestamp_subsec_nanos(),
            },
            serializer,
        )
    }
}

impl From<GqlLocalDateTime> for Value {
    fn from(value: GqlLocalDateTime) -> Value {
        let utc = value.0.and_utc();
        Value::LocalDateTime(utc.timestamp(), utc.timestamp_subsec_nanos())
    }
}

/// GQL `ZonedDateTime` row binding backed by `chrono::DateTime<FixedOffset>`.
///
/// Serde and Candid use the `{seconds, nanos, offset_seconds}` wire form
/// ([`crate::GqlWireValue::ZonedDateTime`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlZonedDateTime(chrono::DateTime<chrono::FixedOffset>);

impl GqlZonedDateTime {
    /// Wrap a chrono fixed-offset date-time.
    pub const fn from_inner(datetime: chrono::DateTime<chrono::FixedOffset>) -> Self {
        Self(datetime)
    }

    /// Return the wrapped chrono fixed-offset date-time.
    pub const fn into_inner(self) -> chrono::DateTime<chrono::FixedOffset> {
        self.0
    }
}

impl From<chrono::DateTime<chrono::FixedOffset>> for GqlZonedDateTime {
    fn from(datetime: chrono::DateTime<chrono::FixedOffset>) -> Self {
        Self(datetime)
    }
}

impl From<GqlZonedDateTime> for chrono::DateTime<chrono::FixedOffset> {
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
        let mut state = serializer.serialize_struct("GqlZonedDateTime", 3)?;
        state.serialize_field("seconds", &self.0.timestamp())?;
        state.serialize_field("nanos", &self.0.timestamp_subsec_nanos())?;
        state.serialize_field("offset_seconds", &self.0.offset().local_minus_utc())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GqlZonedDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ZonedDateTimeWire::deserialize(deserializer)?;
        let offset = chrono::FixedOffset::east_opt(wire.offset_seconds)
            .ok_or_else(|| serde::de::Error::custom("invalid GQL ZonedDateTime offset"))?;
        chrono::DateTime::<chrono::Utc>::from_timestamp(wire.seconds, wire.nanos)
            .map(|utc| Self(utc.with_timezone(&offset)))
            .ok_or_else(|| serde::de::Error::custom("invalid GQL ZonedDateTime"))
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
        CandidType::idl_serialize(
            &ZonedDateTimeWire {
                seconds: self.0.timestamp(),
                nanos: self.0.timestamp_subsec_nanos(),
                offset_seconds: self.0.offset().local_minus_utc(),
            },
            serializer,
        )
    }
}

impl From<GqlZonedDateTime> for Value {
    fn from(value: GqlZonedDateTime) -> Value {
        Value::ZonedDateTime(
            value.0.timestamp(),
            value.0.timestamp_subsec_nanos(),
            value.0.offset().local_minus_utc(),
        )
    }
}

/// GQL `Duration` row binding backed by `chrono::TimeDelta`.
///
/// Serde and Candid use the `{months, nanos}` wire form ([`crate::GqlWireValue::Duration`]).
/// `chrono::TimeDelta` cannot represent calendar months, so this binding always serializes
/// `months: 0` and rejects deserializing a nonzero months component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlDuration(chrono::TimeDelta);

impl GqlDuration {
    /// Wrap a chrono time delta.
    pub const fn from_inner(duration: chrono::TimeDelta) -> Self {
        Self(duration)
    }

    /// Return the wrapped chrono time delta.
    pub const fn into_inner(self) -> chrono::TimeDelta {
        self.0
    }
}

impl From<chrono::TimeDelta> for GqlDuration {
    fn from(duration: chrono::TimeDelta) -> Self {
        Self(duration)
    }
}

impl From<GqlDuration> for chrono::TimeDelta {
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
        let nanos = self
            .0
            .num_nanoseconds()
            .ok_or_else(|| serde::ser::Error::custom("GQL Duration out of chrono range"))?;
        let mut state = serializer.serialize_struct("GqlDuration", 2)?;
        state.serialize_field("months", &0)?;
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
        if wire.months != 0 {
            return Err(serde::de::Error::custom(
                "chrono cannot represent GQL Duration months",
            ));
        }
        Ok(Self(chrono::TimeDelta::nanoseconds(wire.nanos)))
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
        let nanos = self
            .0
            .num_nanoseconds()
            .ok_or_else(|| serde::ser::Error::custom("GQL Duration out of chrono range"))?;
        CandidType::idl_serialize(&DurationWire { months: 0, nanos }, serializer)
    }
}

impl From<GqlDuration> for Value {
    fn from(value: GqlDuration) -> Value {
        let nanos = value.0.num_nanoseconds().unwrap_or(0);
        Value::Duration(0, nanos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(seconds: i64, nanos: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, nanos).unwrap()
    }

    #[test]
    fn chrono_bindings_round_trip_serde_wire_shapes() {
        let date = GqlDate::from(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        let json = serde_json::to_value(date).unwrap();
        assert_eq!(json, serde_json::json!(19737));
        assert_eq!(serde_json::from_value::<GqlDate>(json).unwrap(), date);

        let time = GqlTime::from(chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap());
        let json = serde_json::to_value(time).unwrap();
        assert_eq!(json, serde_json::json!(37_800_000_000_000_i64));
        assert_eq!(serde_json::from_value::<GqlTime>(json).unwrap(), time);

        let datetime = GqlDateTime::from(utc(1_700_000_000, 123_000_000));
        let json = serde_json::to_value(datetime).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "seconds": 1_700_000_000, "nanos": 123_000_000 })
        );
        assert_eq!(
            serde_json::from_value::<GqlDateTime>(json).unwrap(),
            datetime
        );

        let offset = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
        let zoned = GqlZonedDateTime::from(utc(1_700_000_000, 0).with_timezone(&offset));
        let json = serde_json::to_value(zoned).unwrap();
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

        let duration = GqlDuration::from(chrono::TimeDelta::nanoseconds(5));
        let json = serde_json::to_value(duration).unwrap();
        assert_eq!(json, serde_json::json!({ "months": 0, "nanos": 5 }));
        assert_eq!(
            serde_json::from_value::<GqlDuration>(json).unwrap(),
            duration
        );
    }

    #[test]
    fn chrono_bindings_project_to_logical_values() {
        let datetime = GqlDateTime::from(utc(1_700_000_000, 123_000_000));
        assert_eq!(
            Value::from(datetime),
            Value::DateTime(1_700_000_000, 123_000_000)
        );

        let offset = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
        let zoned = GqlZonedDateTime::from(utc(1_700_000_000, 0).with_timezone(&offset));
        assert_eq!(
            Value::from(zoned),
            Value::ZonedDateTime(1_700_000_000, 0, 9 * 3600)
        );

        let duration = GqlDuration::from(chrono::TimeDelta::nanoseconds(5));
        assert_eq!(Value::from(duration), Value::Duration(0, 5));

        let date = GqlDate::from(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        assert!(matches!(Value::from(date), Value::Date(_)));

        let time = GqlTime::from(chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap());
        assert!(matches!(Value::from(time), Value::Time(_)));

        let local_time = GqlLocalTime::from(chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap());
        assert!(matches!(Value::from(local_time), Value::LocalTime(_)));
    }

    #[test]
    fn chrono_duration_rejects_months() {
        let error = serde_json::from_value::<GqlDuration>(serde_json::json!({
            "months": 2,
            "nanos": 5
        }))
        .unwrap_err();
        assert!(error.to_string().contains("months"));
    }

    #[test]
    fn chrono_bindings_round_trip_candid() {
        let offset = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
        let zoned = GqlZonedDateTime::from(utc(1_700_000_000, 0).with_timezone(&offset));
        let bytes = candid::encode_one(zoned).unwrap();
        assert_eq!(
            candid::decode_one::<GqlZonedDateTime>(&bytes).unwrap(),
            zoned
        );

        let duration = GqlDuration::from(chrono::TimeDelta::nanoseconds(5));
        let bytes = candid::encode_one(duration).unwrap();
        assert_eq!(candid::decode_one::<GqlDuration>(&bytes).unwrap(), duration);

        let date = GqlDate::from(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        let bytes = candid::encode_one(date).unwrap();
        assert_eq!(candid::decode_one::<GqlDate>(&bytes).unwrap(), date);

        let time = GqlTime::from(chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap());
        let bytes = candid::encode_one(time).unwrap();
        assert_eq!(candid::decode_one::<GqlTime>(&bytes).unwrap(), time);
    }
}
