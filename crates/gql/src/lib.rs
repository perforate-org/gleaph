//! GQL parser crate -- ISO/IEC 39075 (GQL) compliant graph query language parser.
//!
//! This crate provides:
//! - A [`Value`] type covering all GQL standard scalar and constructed types,
//!   with an [`ExtensionValue`](value::ExtensionValue) trait for platform-specific extensions.
//! - An ISO-8601 temporal parser/formatter ([`temporal`]).
//! - Core types ([`types`]) including [`LabelExpr`], [`Int256`], [`Uint256`], [`Decimal`].
//! - Value comparison ([`value_cmp::compare_values`]) supporting cross-width integers.
//!
//! # Feature flags
//!
//! - `cypher` -- Enables Cypher-compatible syntax extensions (e.g. relationship patterns, property access).
//! - `sql-compat` -- Enables SQL-compatible syntax extensions (e.g. SQL-style expressions and operators).
//! - `f128` -- Enables `Value::Float128` using `std::f128` (requires nightly).
//! - `f256` -- Enables `Value::Float256` using the `f256` crate.
//!
//! Internet Computer `Principal` as [`Value::Extension`](value::Value::Extension) lives in the
//! sibling crate **`gleaph-gql-ic`** (adds a `candid` dependency only there; **tag 34** short blob).

#![cfg_attr(feature = "f128", feature(f128))]

pub mod ast;
pub mod error;
pub mod extensions;
pub mod lexer;
pub mod parser;
pub mod temporal;
pub mod token;
pub mod type_check;
pub mod types;
pub mod validate;
pub mod value;
pub mod value_cmp;

pub use error::{GqlError, GqlResult};
pub use parser::ParseResult;
pub use value::{
    DenyExtensionBinaryDecode, ExtensionBinaryDecode, ExtensionValue, Value, ValueBinaryError,
};
