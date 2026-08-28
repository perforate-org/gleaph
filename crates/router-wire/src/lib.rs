//! Shared Router-facing wire types for the Gleaph client and canister SDKs.
//!
//! This crate owns the Router data-plane wire contract: the query/mutation response envelope,
//! read freshness contracts, mutation tokens, the Router error type, and the durable bulk-load
//! command family. Both `gleaph-sdk` (Rust application client) and `gleaph-cdk` (Rust canister
//! helpers) depend on this crate so the wire contract has a single source of truth.

#![warn(missing_docs)]
#![cfg_attr(feature = "nightly-f128", feature(f128))]

pub mod rows;
pub mod types;

#[cfg(feature = "nightly-f128")]
pub use types::Float128;
#[cfg(feature = "decimal")]
pub use types::GqlDecimal;
#[cfg(feature = "f16")]
pub use types::GqlFloat16;
#[cfg(feature = "i256")]
pub use types::GqlInt256;
#[cfg(feature = "u256")]
pub use types::GqlUint256;
pub use types::{
    AtomicInsertPropertyV1, AtomicInsertReceiptV1, AtomicInsertVertexV1, BulkLoadChunkReceiptV1,
    BulkLoadChunkV1, BulkLoadCommand, BulkLoadEdgeV1, BulkLoadEndpointV1,
    BulkLoadPropertyEndpointV1, BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage,
    EdgePathElementId, FromGqlRow, GqlQueryResult, GqlRecord, GqlRow, GqlValue, GqlWireDecodeError,
    GqlWireRow, GqlWireRows, GqlWireValue, GqlZonedTime, MutationLifecyclePhase, MutationToken,
    MutationTokenShard, PathElement, PrincipalValue, ReadMode, RouterError,
    VectorActivationBlockReason, VertexPathElementId, gql_principal_from_value,
    gql_record_to_json_map, gql_value_to_json, gql_wire_value_to_json, take_gql_row_field,
};
#[cfg(feature = "f256")]
pub use types::{Float256, GqlFloat256};
