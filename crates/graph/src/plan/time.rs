//! Current-date/time helpers shared between query and mutation expression evaluators.
//!
//! On the IC these are sourced from `ic_cdk::api::time()`; outside canister code (unit
//! tests, host builds) they fall back to the host system clock so the same paths can be
//! exercised without a running canister.

use gleaph_gql::Value;
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
    let (seconds, nanos) = split_ic_now_ns();
    let nanos_since_midnight = ((seconds % 86_400) as u64) * 1_000_000_000 + nanos as u64;
    Value::Time(nanos_since_midnight)
}

pub(crate) fn current_local_time_value() -> Value {
    let (seconds, nanos) = split_ic_now_ns();
    let nanos_since_midnight = ((seconds % 86_400) as u64) * 1_000_000_000 + nanos as u64;
    Value::LocalTime(nanos_since_midnight)
}
