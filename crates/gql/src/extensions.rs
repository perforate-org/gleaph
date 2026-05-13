//! Helpers for implementing and registering host extension values.
//!
//! Typical extension setup has two steps:
//! 1. Implement the concrete value with [`impl_extension_value`].
//! 2. Register binary decode paths with [`declare_extension_types`].
//!
//! `impl_extension_value!` generates:
//! - An [`crate::value::ExtensionValue`] implementation for a concrete type
//! - Type-safe downcast handling for equality and ordering
//! - Optional binary, short-blob, compact-kind, hash, and sortable-index hooks
//!
//! `declare_extension_types!` is for the decode/registration half. It generates:
//! - A decoder struct implementing [`crate::value::ExtensionBinaryDecode`]
//! - A `EXTENSION_TYPE_NAMES` constant for host-side allowlist registration
//! - A `for_each_extension_type` helper
//!
//! # Example: value implementation
//!
//! ```ignore
//! use std::borrow::Cow;
//! use gleaph_gql::extensions::impl_extension_value;
//! use gleaph_gql::ExtensionValue;
//!
//! #[derive(Clone, Debug)]
//! struct SessionTokenValue(Vec<u8>);
//!
//! impl std::fmt::Display for SessionTokenValue {
//!     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//!         write!(f, "SessionToken")
//!     }
//! }
//!
//! impl_extension_value! {
//!     impl ExtensionValue for SessionTokenValue {
//!         type_name: "auth.SessionToken";
//!         eq: |this, other| this.0 == other.0;
//!         binary_payload: |this| Cow::Borrowed(this.0.as_slice());
//!     }
//! }
//! ```
//!
//! # Example: decoder registration
//!
//! ```ignore
//! use gleaph_gql::extensions::declare_extension_types;
//! use gleaph_gql::{ExtensionValue, ValueBinaryError};
//!
//! const SESSION_TOKEN_TYPE_NAME: &str = "auth.SessionToken";
//! const SESSION_TOKEN_KIND: u8 = 7;
//!
//! fn decode_session_token(
//!     payload: &[u8],
//! ) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
//!     Ok(Box::new(SessionTokenValue(payload.to_vec())))
//! }
//!
//! declare_extension_types! {
//!     decoder: AppExtensionDecode;
//!     type_names: [SESSION_TOKEN_TYPE_NAME, "SESSION_TOKEN"];
//!     compact: { SESSION_TOKEN_KIND => decode_session_token };
//! }
//!
//! let value = gleaph_gql::Value::from_binary_bytes_with_extensions(
//!     bytes,
//!     &AppExtensionDecode,
//! )?;
//! ```

#[doc(inline)]
pub use gleaph_gql_macros::declare_extension_types;

#[doc(inline)]
pub use gleaph_gql_macros::impl_extension_value;
