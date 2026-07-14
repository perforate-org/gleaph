//! GQL parser crate -- ISO/IEC 39075 (GQL) compliant graph query language parser.
//!
//! This crate provides:
//! - A [`Value`] type covering all GQL standard scalar and constructed types,
//!   with an [`ExtensionValue`](value::ExtensionValue) trait for platform-specific extensions.
//! - An ISO-8601 temporal parser/formatter ([`temporal`]).
//! - Core types ([`types`]) including [`LabelExpr`], [`Int256`], [`Uint256`], [`Decimal`].
//! - Value comparison ([`value_cmp::compare_values`]) supporting cross-width integers.
//! - [`ExtensionValue::hash_join_key`](value::ExtensionValue::hash_join_key) for join-bucket / DISTINCT-style hashing (not `std::hash::Hash` on [`Value`]).
//!
//! # Feature flags
//!
//! - `cypher` -- Enables Cypher-compatible syntax extensions (e.g. relationship patterns, property access).
//! - `sql-compat` -- Enables SQL-compatible syntax extensions (e.g. SQL-style expressions and operators).
//! - `gleaph` -- Enables Gleaph-specific GQL syntax extensions such as `SEARCH ... IN (VECTOR INDEX ...)`.
//!   This feature is **non-default**; standard-GQL consumers should not enable it.
//! - `f128` -- Enables `Value::Float128` using `std::f128` (requires nightly).
//! - `f256` -- Enables `Value::Float256` using the `f256` crate.
//! - `ast-rkyv-no-span` -- Derives `rkyv::Archive` / `Serialize` / `Deserialize` for AST types and related
//!   values; source [`Span`](token::Span) fields are omitted from the archived form (`rkyv::with::Skip`).
//!   Use [`rkyv_from_aligned_bytes`] when the buffer is already root-aligned (e.g. fresh
//!   `rkyv::to_bytes` output or bytes copied into [`AlignedVec`](rkyv::util::AlignedVec) at a
//!   storage boundary). Use [`rkyv_from_wire_bytes`] for wire-format subslices and any buffer
//!   whose alignment is not guaranteed (copies on wasm32 when needed; native fast path when
//!   aligned).
//!   rkyv-deserialized [`Value::Extension`](value::Value::Extension) wire bytes need a registered
//!   [`ExtensionBinaryDecode`](value::ExtensionBinaryDecode) — see [`try_install_global_rkyv_extension_binary_decode`].

#![cfg_attr(feature = "f128", feature(f128))]
#![cfg_attr(feature = "ast-rkyv-no-span", feature(trivial_bounds))]

extern crate self as gleaph_gql;

#[doc(inline)]
pub use gleaph_gql_macros::define_gql_extension as gql_extension;

pub mod ast;
pub mod error;
pub mod extensions;
pub mod lexer;
/// Identifier length limits for properties, labels, and graph-type names.
pub mod name_limits;
pub mod numeric_ops;
pub mod numeric_order;
pub mod parser;
/// Static classification of programs for authorization (data vs catalog modification).
pub mod program_modification;
#[cfg(feature = "ast-rkyv-no-span")]
pub(crate) mod rkyv_support;
pub mod temporal;
pub mod token;
pub mod type_check;
pub mod types;
pub mod validate;
pub mod value;
pub mod value_cmp;
pub mod value_index_key;
pub mod value_join_hash;
pub mod vendor_extension;

pub use error::{GqlError, GqlResult};
pub use parser::ParseResult;
pub use value::{
    DenyExtensionBinaryDecode, ExtensionBinaryDecode, ExtensionSortableKey, ExtensionValue, Value,
    ValueBinaryError,
};
pub use value_index_key::{
    ValueIndexKeyError, index_key_bytes_to_value, numeric_range_bounds, value_to_index_key_bytes,
};
pub use value_join_hash::{hash_path_element_for_join, hash_value_for_join};

#[cfg(feature = "ast-rkyv-no-span")]
pub use rkyv_support::{
    GlobalRkyvExtensionDecodeAlreadyInstalled, RKYV_WIRE_ALIGN, RkyvExtensionDecodeScopeGuard,
    rkyv_from_aligned_bytes, rkyv_from_wire_bytes, try_install_global_rkyv_extension_binary_decode,
};
