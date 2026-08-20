//! Router-facing wire types, the query response envelope, read freshness contracts, and the
//! typed-binding helpers used by `gleaph-codegen` output.
//!
//! The Router data-plane wire contract is owned by `gleaph-router-wire`; this module re-exports
//! it so `gleaph-cdk` consumers keep a single `gleaph_cdk::` import surface. The cdk-specific
//! temporal row bindings are layered on top here because they depend on the cdk's
//! `temporal-jiff` / `temporal-chrono` feature selection.

/// Router data-plane wire contract (response envelope, read modes, mutation tokens,
/// `RouterError`, bulk-load family, row-decode helpers).
pub use gleaph_router_wire::types::*;

/// Binary128 row binding carrying the canonical little-endian wire form.
#[cfg(feature = "nightly-f128")]
pub use gleaph_router_wire::types::Float128;
/// Binary256 row binding carrying the canonical little-endian wire form.
#[cfg(feature = "f256")]
pub use gleaph_router_wire::types::Float256;
/// Decimal row binding used by generated canister bindings.
#[cfg(feature = "decimal")]
pub use gleaph_router_wire::types::GqlDecimal;
/// Half-precision float row binding used by generated canister bindings.
#[cfg(feature = "f16")]
pub use gleaph_router_wire::types::GqlFloat16;
/// Signed 256-bit integer row binding used by generated canister bindings.
#[cfg(feature = "i256")]
pub use gleaph_router_wire::types::GqlInt256;
/// Unsigned 256-bit integer row binding used by generated canister bindings.
#[cfg(feature = "u256")]
pub use gleaph_router_wire::types::GqlUint256;

/// GQL `Date` row binding used by generated canister bindings.
///
/// Backed by `jiff::civil::Date` (default `temporal-jiff` feature) or `chrono::NaiveDate`
/// (`temporal-chrono`). Serde and Candid use the days-since-epoch wire form.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlDate;
/// GQL `DateTime` row binding used by generated canister bindings.
///
/// Backed by `jiff::Timestamp` or `chrono::DateTime<Utc>`; serde and Candid use the
/// `{seconds, nanos}` wire form.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlDateTime;
/// GQL `Duration` row binding used by generated canister bindings.
///
/// Backed by `jiff::Span` (faithful, includes months) or `chrono::TimeDelta` (months are not
/// representable and are serialized as zero); serde and Candid use the `{months, nanos}` wire
/// form.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlDuration;
/// GQL `LocalDateTime` row binding used by generated canister bindings.
///
/// Backed by `jiff::civil::DateTime` or `chrono::NaiveDateTime`, interpreted as a civil
/// date-time in UTC; serde and Candid use the `{seconds, nanos}` wire form.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlLocalDateTime;
/// GQL `LocalTime` row binding used by generated canister bindings.
///
/// Same representation as [`GqlTime`] but projects to `Value::LocalTime`.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlLocalTime;
/// GQL `Time` row binding used by generated canister bindings.
///
/// Backed by `jiff::civil::Time` or `chrono::NaiveTime`; serde and Candid use nanoseconds since
/// midnight.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlTime;
/// GQL `ZonedDateTime` row binding used by generated canister bindings.
///
/// Backed by `jiff::Zoned` or `chrono::DateTime<FixedOffset>`; serde and Candid use the
/// `{seconds, nanos, offset_seconds}` wire form.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlZonedDateTime;
/// GQL `ZonedTime` row binding used by generated canister bindings.
///
/// Time of day with a fixed UTC offset; neither `jiff` nor `chrono` models this, so the binding
/// keeps the `{nanos, offset_seconds}` wire record form.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlZonedTime;

/// Prepared-operation wire types shared with the Router.
pub use gleaph_prepared_api::{PreparedManifest, PreparedOperation, PreparedSortSpec};
