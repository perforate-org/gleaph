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

#![cfg_attr(feature = "f128", feature(f128))]

use candid::{CandidType, Deserialize, Principal};
use serde::Serialize;
use std::fmt;

/// Logical GQL path element (opaque element ids, no Candid derives).
pub use gleaph_gql_value::types::PathElement as GqlPathElement;

/// Principal extension used by named GQL parameters (wire-compatible with the Router's
/// principal parameter decode).
pub use gleaph_gql_ic_wire::PrincipalValue;
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
        GqlValue::Extension(Box::new(PrincipalValue(self)))
    }
}

impl IntoGqlParam for () {
    fn into_gql_param(self) -> GqlValue {
        self.into()
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

#[cfg(feature = "i256")]
impl IntoGqlParam for ethnum::I256 {
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

#[cfg(feature = "u256")]
impl IntoGqlParam for ethnum::U256 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

#[cfg(feature = "f16")]
impl IntoGqlParam for half::f16 {
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

#[cfg(feature = "f128")]
impl IntoGqlParam for f128 {
    fn into_gql_param(self) -> GqlValue {
        self.into()
    }
}

#[cfg(feature = "f256")]
impl IntoGqlParam for f256::f256 {
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

#[cfg(feature = "decimal")]
impl IntoGqlParam for rust_decimal::Decimal {
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

// ──── Path elements ────

/// Opaque 8-byte vertex id in a GQL path value.
///
/// Mirrors `gleaph_graph_kernel::federation::encoded::EncodedVertexId`; the fixed 8-byte
/// length is part of the Router's path-value wire contract (ADR 0005).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Deserialize, Serialize,
)]
pub struct VertexPathElementId(pub [u8; 8]);

/// Opaque 12-byte edge id in a GQL path value.
///
/// Mirrors `gleaph_graph_kernel::federation::encoded::EncodedEdgeId`; the fixed 12-byte
/// length is part of the Router's path-value wire contract (ADR 0005).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Deserialize, Serialize,
)]
pub struct EdgePathElementId(pub [u8; 12]);

/// One vertex or edge in a GQL path value, with the fixed-length element id typed per kind.
///
/// The Candid shape is `variant { Vertex: vec nat8, Edge: vec nat8 }` — identical to the
/// Router's path element wire representation (ADR 0005). The distinct id types prevent
/// mixing a vertex id into an edge slot and vice versa, and enforce the byte lengths.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub enum PathElement {
    /// Vertex with its opaque 8-byte element id.
    Vertex(VertexPathElementId),
    /// Edge with its opaque 12-byte element id.
    Edge(EdgePathElementId),
}

impl PathElement {
    /// Convert into the logical GQL path element used inside `GqlValue::Path`.
    ///
    /// The conversion is infallible: the fixed-length binding ids round-trip losslessly
    /// into the opaque element ids of the general-purpose GQL value model.
    pub fn into_gql(self) -> GqlPathElement {
        match self {
            PathElement::Vertex(id) => GqlPathElement::Vertex(id.0.into()),
            PathElement::Edge(id) => GqlPathElement::Edge(id.0.into()),
        }
    }
}

/// Error returned when a logical GQL path element cannot be converted to the fixed-length
/// binding type because its id has the wrong byte length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathElementError {
    /// A vertex id was not 8 bytes.
    InvalidVertexLength { actual: usize },
    /// An edge id was not 12 bytes.
    InvalidEdgeLength { actual: usize },
}

impl fmt::Display for PathElementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVertexLength { actual } => write!(
                f,
                "invalid vertex path element id length: expected 8, got {actual}"
            ),
            Self::InvalidEdgeLength { actual } => write!(
                f,
                "invalid edge path element id length: expected 12, got {actual}"
            ),
        }
    }
}

impl std::error::Error for PathElementError {}

impl TryFrom<GqlPathElement> for PathElement {
    type Error = PathElementError;

    fn try_from(value: GqlPathElement) -> Result<Self, Self::Error> {
        match value {
            GqlPathElement::Vertex(id) => {
                let bytes: [u8; 8] = id
                    .as_ref()
                    .try_into()
                    .map_err(|_| PathElementError::InvalidVertexLength { actual: id.len() })?;
                Ok(PathElement::Vertex(VertexPathElementId(bytes)))
            }
            GqlPathElement::Edge(id) => {
                let bytes: [u8; 12] = id
                    .as_ref()
                    .try_into()
                    .map_err(|_| PathElementError::InvalidEdgeLength { actual: id.len() })?;
                Ok(PathElement::Edge(EdgePathElementId(bytes)))
            }
        }
    }
}

impl IntoGqlParam for Vec<PathElement> {
    fn into_gql_param(self) -> GqlValue {
        GqlValue::Path(self.into_iter().map(PathElement::into_gql).collect())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_element_ids_are_candid_blobs() {
        use candid::types::{Label, TypeInner};

        // The Router wire shape is `variant { Vertex: vec nat8, Edge: vec nat8 }`; the
        // newtype wrappers must inline to a plain blob, not a record wrapper.
        assert_eq!(VertexPathElementId::ty(), Vec::<u8>::ty());
        assert_eq!(EdgePathElementId::ty(), Vec::<u8>::ty());
        let TypeInner::Variant(fields) = &*PathElement::ty().0 else {
            panic!("path element must derive a candid variant");
        };
        assert_eq!(fields.len(), 2);
        for field in fields {
            assert_eq!(field.ty, Vec::<u8>::ty());
            assert!(matches!(
                field.id.as_ref(),
                Label::Named(name) if name == "Vertex" || name == "Edge"
            ));
        }
    }

    #[test]
    fn path_element_round_trips_candid() {
        let vertex = PathElement::Vertex(VertexPathElementId([1, 2, 3, 4, 5, 6, 7, 8]));
        let edge = PathElement::Edge(EdgePathElementId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]));
        for element in [vertex, edge] {
            let encoded = candid::encode_one(&element).expect("candid encode");
            assert_eq!(
                candid::decode_one::<PathElement>(&encoded).expect("candid decode"),
                element
            );
        }
    }

    #[test]
    fn path_element_serde_round_trip_keeps_kind() {
        let vertex = PathElement::Vertex(VertexPathElementId([1, 2, 3, 4, 5, 6, 7, 8]));
        let encoded = serde_json::to_string(&vertex).unwrap();
        assert_eq!(encoded, r#"{"Vertex":[1,2,3,4,5,6,7,8]}"#);
        assert_eq!(
            serde_json::from_str::<PathElement>(&encoded).unwrap(),
            vertex
        );

        let edge = PathElement::Edge(EdgePathElementId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]));
        let encoded = serde_json::to_string(&edge).unwrap();
        assert_eq!(encoded, r#"{"Edge":[1,2,3,4,5,6,7,8,9,10,11,12]}"#);
        assert_eq!(serde_json::from_str::<PathElement>(&encoded).unwrap(), edge);
    }

    #[test]
    fn path_element_rejects_wrong_length_ids() {
        assert!(serde_json::from_str::<PathElement>(r#"{"Vertex":[1,2,3]}"#).is_err());
        assert!(serde_json::from_str::<PathElement>(r#"{"Edge":[1,2,3]}"#).is_err());
        assert!(serde_json::from_str::<PathElement>(r#"{"Vertex":[1,2,3,4,5,6,7,8,9]}"#).is_err());
        assert!(
            serde_json::from_str::<PathElement>(r#"{"Edge":[1,2,3,4,5,6,7,8,9,10,11,12,13]}"#)
                .is_err()
        );
    }

    #[test]
    fn path_element_converts_to_logical_gql_element() {
        use gleaph_gql_value::types::PathElementId;

        let vertex = PathElement::Vertex(VertexPathElementId([9, 8, 7, 6, 5, 4, 3, 2]));
        let vertex_gql = vertex.clone().into_gql();
        assert_eq!(
            vertex_gql,
            GqlPathElement::Vertex(PathElementId::from([9, 8, 7, 6, 5, 4, 3, 2]))
        );
        assert_eq!(PathElement::try_from(vertex_gql), Ok(vertex));

        let edge = PathElement::Edge(EdgePathElementId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]));
        let edge_gql = edge.clone().into_gql();
        assert_eq!(
            edge_gql,
            GqlPathElement::Edge(PathElementId::from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]))
        );
        assert_eq!(PathElement::try_from(edge_gql), Ok(edge));

        // The logical-to-fixed direction validates the fixed byte lengths.
        assert_eq!(
            PathElement::try_from(GqlPathElement::Vertex(PathElementId::from([1, 2, 3]))),
            Err(PathElementError::InvalidVertexLength { actual: 3 })
        );
        assert_eq!(
            PathElement::try_from(GqlPathElement::Edge(PathElementId::from([1, 2, 3]))),
            Err(PathElementError::InvalidEdgeLength { actual: 3 })
        );
    }

    #[test]
    fn path_parameters_bind_into_gql_values() {
        let path = vec![
            PathElement::Vertex(VertexPathElementId([1, 2, 3, 4, 5, 6, 7, 8])),
            PathElement::Edge(EdgePathElementId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])),
        ];
        let query = gql!("MATCH (a)-[e]->(b) WHERE a.id IN $path RETURN a", path);
        assert!(matches!(query.params[0].1, GqlValue::Path(_)));
    }
}
