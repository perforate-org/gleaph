//! Host extension values and the `gql_extension!` registration macro.
//!
//! Typical setup in a vendor integration crate:
//!
//! ```ignore
//! use std::borrow::Cow;
//! use gleaph_gql::extensions::gql_extension;
//!
//! gql_extension! {
//!     prefix: "IC",
//!     types: [
//!         {
//!             rust_type: PrincipalValue,
//!             type_name: "PRINCIPAL",
//!             decoder: IcExtensionBinaryDecode,
//!             eq: |this, other| this.0 == other.0,
//!             cmp: |this, other| this.0.cmp(&other.0),
//!             sortable_index_key: {
//!                 domain: PRINCIPAL_EXTENSION_SORTABLE_DOMAIN,
//!                 bytes: |this| Cow::Borrowed(this.0.as_slice()),
//!             },
//!             binary_payload: |this| Cow::Borrowed(this.0.as_slice()),
//!             short_blob: |this| Cow::Borrowed(this.0.as_slice()),
//!             short_blob_decode: decode_principal_payload,
//!         },
//!     ],
//!     functions: [
//!         {
//!             name: "MSG_CALLER",
//!             alias: ["msg_caller"],
//!             function: || principal_to_value(ic_cdk::api::msg_caller()),
//!         },
//!     ],
//!     path_extensions: [
//!         {
//!             name: "COST",
//!             alias: ["LEGACY_COST"],
//!             validate_plan: crate::plan::gleaph_cost_by,
//!         },
//!     ],
//! }
//! ```
//!
//! - In `types`, a **string literal** `type_name` is always canonicalized to `{prefix}.{type_name}`
//!   for [`ExtensionValue::type_name`](crate::value::ExtensionValue::type_name) and the decoder's
//!   `EXTENSION_TYPE_NAMES` list (plus any `alias` entries). Extra `.` segments in the literal are
//!   treated as part of the vendor-chosen name (the GQL parser does not assign special meaning to
//!   dots after the first). Use a non-literal `type_name` expression (e.g. a `const` path) to supply
//!   a fixed spelling without macro concatenation.
//! - `prefix` must not equal a GQL **reserved** or **prereserved** keyword (case-insensitive); the macro reports which category matched.
//! - `alias` is optional in `types`, `functions`, and `path_extensions` (omit the field or use `alias: []`).
//! - `function` and `eval` are synonyms for the callable in `functions`.
//! - `types` items declare [`ExtensionValue`](crate::value::ExtensionValue) plus an
//!   [`ExtensionBinaryDecode`](crate::value::ExtensionBinaryDecode) decoder struct.
//!   Each decoder exposes `pub const INSTANCE: Self = Self` (use `&MyDecoder::INSTANCE` where a `&dyn ExtensionBinaryDecode` is required).
//! - `functions` / `path_extensions` emit [`crate::vendor_extension::GqlVendorMemberNames`] constants
//!   (and `functions` may emit zero-argument eval wrappers). Wire decode is only from `types`.

#[doc(inline)]
pub use gleaph_gql_macros::define_gql_extension as gql_extension;
