//! Named GQL parameters and the [`gql!`] query builder.
//!
//! General-purpose, dependency-light helpers for building dynamic GQL queries with named
//! parameters. The Gleaph canister SDK (`gleaph-cdk`) re-exports these, and any other crate can
//! use them directly: bind Rust values to `$name` placeholders in GQL query text without
//! depending on canister runtime crates.
//!
//! - [`IntoGqlParam`] converts a Rust value into [`GqlValue`]. It is implemented for every
//!   standard type that converts into `GqlValue` via `From` (primitives, `String`, `&str`,
//!   `Vec<u8>`, `Option<T>`, `Decimal`) plus `candid::Principal`. Custom types implement the
//!   trait directly.
//! - [`gql!`] builds a [`GqlQuery`] from query text and named parameter bindings.
//! - [`encode_gql_params`] encodes the ordered parameters as a compact-binary record blob.
//!
//! # Why not a blanket `T: Into<GqlValue>` implementation?
//!
//! A blanket `impl<T: Into<GqlValue>> IntoGqlParam for T` cannot coexist with
//! `impl IntoGqlParam for candid::Principal`: the orphan rule allows the `candid` crate to add
//! `From<candid::Principal> for gleaph_gql_value::Value` in a future version, so the two impls
//! are treated as potentially overlapping (E0119). The explicit per-type impls mirror the
//! `From` conversions `gleaph-gql-value` provides, and keep the `Principal` special case
//! conflict-free.

use candid::Principal;
use gleaph_gql_value::types::Decimal;

/// Principal extension used by named GQL parameters (wire-compatible with the Router's
/// principal parameter decode).
pub use gleaph_gql_ic_wire::GqlPrincipal;
/// Logical GQL value shared by dynamic GQL, prepared operations, and procedures.
pub use gleaph_gql_value::Value as GqlValue;
/// Error returned when a logical GQL value cannot be compact-binary encoded.
pub use gleaph_gql_value::ValueBinaryError;

/// Named GQL parameters: ordered `(name, value)` pairs.
///
/// GQL record order is retained because the compact wire representation preserves field order.
pub type GqlParams = Vec<(String, GqlValue)>;

/// A dynamic GQL query with named parameters, built by the [`gql!`] macro.
#[derive(Clone, Debug, PartialEq)]
pub struct GqlQuery {
    /// GQL query text with `$name` placeholders.
    pub query: String,
    /// Ordered named parameters; names match the `$name` placeholders without the `$`.
    pub params: GqlParams,
}

impl From<&str> for GqlQuery {
    fn from(query: &str) -> Self {
        Self {
            query: query.to_string(),
            params: Vec::new(),
        }
    }
}

impl From<String> for GqlQuery {
    fn from(query: String) -> Self {
        Self {
            query,
            params: Vec::new(),
        }
    }
}

/// Types that can be bound as a named GQL parameter.
///
/// Implemented for the standard [`GqlValue`] conversions (mirroring the `From` impls in
/// `gleaph-gql-value`) and for `candid::Principal`. Custom types — for example generated
/// binding parameter structs — implement this trait directly.
pub trait IntoGqlParam {
    /// Convert this value into a logical GQL value.
    fn into_gql_param(self) -> GqlValue;
}

impl IntoGqlParam for GqlValue {
    fn into_gql_param(self) -> GqlValue {
        self
    }
}

impl IntoGqlParam for Principal {
    fn into_gql_param(self) -> GqlValue {
        GqlValue::Extension(Box::new(GqlPrincipal::from_inner(self)))
    }
}

impl IntoGqlParam for bool {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for i8 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for i16 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for i32 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for i64 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for i128 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for u8 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for u16 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for u32 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for u64 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for u128 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for f32 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for f64 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for String {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for &str {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for Vec<u8> {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl IntoGqlParam for Decimal {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

impl<T: Into<GqlValue>> IntoGqlParam for Option<T> {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

/// Convert any GQL-parameter-compatible value into a logical GQL value.
pub fn gql_param_value<T: IntoGqlParam>(value: T) -> GqlValue {
    value.into_gql_param()
}

/// Encode ordered named GQL parameters as a compact-binary top-level `Value::Record` blob.
pub fn encode_gql_params(params: GqlParams) -> Result<Vec<u8>, ValueBinaryError> {
    GqlValue::Record(params).to_binary_bytes()
}

/// Build a dynamic GQL query with named parameters.
///
/// Two binding forms are supported; keep them homogeneous within one invocation:
///
/// - Explicit: `gql!("MATCH (n:Person {id: $id}) RETURN n.name", id = 42u64)` binds the
///   placeholder `$id` from any expression.
/// - Inferred: `gql!("MATCH (n {owner: $owner}) RETURN n", owner)` binds `$owner` from the
///   identifier `owner`; its spelling becomes the parameter name.
///
/// Values must implement [`IntoGqlParam`]: the standard `Into<GqlValue>` types, `candid::Principal`,
/// or a custom impl.
///
/// ```
/// use gleaph_gql_params::gql;
///
/// let query = gql!("MATCH (n:Person {id: $id}) RETURN n.name", id = 42u64);
/// assert_eq!(query.params[0].0, "id");
/// assert!(matches!(
///     query.params[0].1,
///     gleaph_gql_params::GqlValue::Uint64(42)
/// ));
/// ```
#[macro_export]
macro_rules! gql {
    ($query:literal $(, $name:ident = $value:expr)* $(,)?) => {{
        $crate::GqlQuery {
            query: $query.to_string(),
            params: vec![
                $((
                    stringify!($name).to_string(),
                    $crate::gql_param_value($value),
                )),*
            ],
        }
    }};
    ($query:literal $(, $name:ident)* $(,)?) => {{
        $crate::GqlQuery {
            query: $query.to_string(),
            params: vec![
                $((
                    stringify!($name).to_string(),
                    $crate::gql_param_value($name),
                )),*
            ],
        }
    }};
}
