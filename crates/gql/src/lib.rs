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
//! - `f128` -- Enables `Value::Float128` using `std::f128` (requires nightly).
//! - `f256` -- Enables `Value::Float256` using the `f256` crate.
//! - `ast-rkyv-no-span` -- Derives `rkyv::Archive` / `Serialize` / `Deserialize` for AST types and related
//!   values; source [`Span`](token::Span) fields are omitted from the archived form (`rkyv::with::Skip`).
//!   The workspace pins `rkyv` without the `bytecheck` default feature (validated `from_bytes` is unavailable;
//!   use `to_bytes` / `deserialize` / `from_bytes_unchecked` as in unit tests).
//!   rkyv-deserialized [`Value::Extension`](value::Value::Extension) wire bytes need a registered
//!   [`ExtensionBinaryDecode`](value::ExtensionBinaryDecode) — see [`try_install_global_rkyv_extension_binary_decode`].
//!
//! Internet Computer `Principal` as [`Value::Extension`](value::Value::Extension) lives in the
//! sibling crate **`gleaph-gql-ic`** (adds a `candid` dependency only there; **tag 34** short blob).

#![cfg_attr(feature = "f128", feature(f128))]
#![cfg_attr(feature = "ast-rkyv-no-span", feature(trivial_bounds))]

pub mod ast;
pub mod error;
pub mod extensions;
pub mod lexer;
/// Gleaph identifier length limits for properties, labels, and graph-type names.
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
pub mod value_join_hash;

pub use error::{GqlError, GqlResult};
pub use parser::ParseResult;
pub use value::{
    DenyExtensionBinaryDecode, ExtensionBinaryDecode, ExtensionSortableKey, ExtensionValue, Value,
    ValueBinaryError,
};
pub use value_join_hash::{hash_path_element_for_join, hash_value_for_join};

#[cfg(feature = "ast-rkyv-no-span")]
pub use rkyv_support::{
    GlobalRkyvExtensionDecodeAlreadyInstalled, RkyvExtensionDecodeScopeGuard,
    try_install_global_rkyv_extension_binary_decode,
};
