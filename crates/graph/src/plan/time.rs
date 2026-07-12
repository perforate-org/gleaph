//! Current-date/time helpers shared between query and mutation expression evaluators.
//!
//! On the IC these are sourced from `ic_cdk::api::time()`; outside canister code (unit
//! tests, host builds) they fall back to the host system clock so the same paths can be
//! exercised without a running canister. `jiff` provides the calendar/zone-aware current
//! time on hosts; the IC path uses a fixed UTC offset because system zoneinfo is
//! unavailable in the canister environment.

use gleaph_gql::Value;
use jiff::{Zoned, civil};
#[cfg(not(target_family = "wasm"))]
use std::time::{SystemTime, UNIX_EPOCH};

fn ic_now_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }
}

fn split_ic_now_ns() -> (i64, u32) {
    let ns = ic_now_ns();
    let seconds = (ns / 1_000_000_000) as i64;
    let nanos = (ns % 1_000_000_000) as u32;
    (seconds, nanos)
}

pub(crate) fn current_datetime_value() -> Value {
    let (seconds, nanos) = split_ic_now_ns();
    Value::DateTime(seconds, nanos)
}

pub(crate) fn current_local_datetime_value() -> Value {
    let (seconds, nanos) = split_ic_now_ns();
    Value::LocalDateTime(seconds, nanos)
}

pub(crate) fn current_date_value() -> Value {
    let (seconds, _) = split_ic_now_ns();
    let days = (seconds / 86_400) as i32;
    Value::Date(days)
}

pub(crate) fn current_time_value() -> Value {
    #[cfg(target_family = "wasm")]
    {
        let (seconds, nanos) = split_ic_now_ns();
        let nanos_since_midnight = ((seconds % 86_400) as u64) * 1_000_000_000 + nanos as u64;
        Value::ZonedTime(nanos_since_midnight, 0)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let zdt = Zoned::now();
        Value::ZonedTime(jiff_time_to_nanos(zdt.time()), zdt.offset().seconds())
    }
}

fn jiff_time_to_nanos(time: civil::Time) -> u64 {
    ((time.hour() as u64 * 3_600 + time.minute() as u64 * 60 + time.second() as u64)
        * 1_000_000_000)
        + time.subsec_nanosecond() as u64
}

pub(crate) fn current_local_time_value() -> Value {
    let (seconds, nanos) = split_ic_now_ns();
    let nanos_since_midnight = ((seconds % 86_400) as u64) * 1_000_000_000 + nanos as u64;
    Value::LocalTime(nanos_since_midnight)
}
