//! Helpers for defining host extension-type decoders.
//!
//! `declare_extension_types!` generates:
//! - A decoder struct implementing [`crate::value::ExtensionBinaryDecode`]
//! - A `EXTENSION_TYPE_NAMES` constant for host-side allowlist registration
//! - A `for_each_extension_type` helper

/// Declare a binary extension decoder and exported extension type names.
///
/// This macro is intended for host integration crates (for example `gleaph-gql-ic`)
/// that define concrete extension values while `gleaph-gql` stays runtime-agnostic.
///
/// # Inputs
///
/// - `decoder: <Decoder>;`
///   - Type requirement: Rust identifier (type name), for example `AppExtensionDecode`.
///   - Declares the decoder type (generated as `pub struct`) that will implement
///     [`crate::value::ExtensionBinaryDecode`].
/// - `type_names: [ ... ];`
///   - Type requirement: array of string expressions coercible to `&'static str`
///     (string literals and `const &str` values are typical).
///   - Host-visible extension type names (for example `"SESSION_TOKEN"`).
///   - Exposed as `Decoder::EXTENSION_TYPE_NAMES` and consumed by host allowlists.
/// - `compact: { kind => handler, ... };`
///   - Type requirement:
///     - `kind`: pattern matching `u8` (typically an integer literal such as `7`,
///       or a `const u8` name).
///     - `handler`: `fn(&[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError>`.
///   - Dispatch table for the "kind + payload" extension format.
///   - Use this when a single decoder supports multiple extension kinds.
///   - Optional.
/// - `short_blob: handler;`
///   - Type requirement: expression resolving to a callable with signature
///     `fn(&[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError>`.
///   - Decoder for the "short raw-bytes" extension format.
///   - Use this for compact, single-kind payloads where no explicit kind id is needed.
///   - Same handler signature as compact handlers.
///   - Optional.
/// - `compact` and `short_blob` relationship:
///   - At least one decoding path is required.
///   - You can provide either one, or both.
///   - If both are omitted, or `compact` is present but empty while `short_blob` is omitted,
///     macro expansion fails with an error.
///
/// # Generated Items
///
/// - `<Decoder>` type
/// - `<Decoder>::EXTENSION_TYPE_NAMES`
/// - `<Decoder>::for_each_extension_type(...)`
/// - `impl ExtensionBinaryDecode for <Decoder>`
///
/// # Example
///
/// ```ignore
/// use gleaph_gql::extensions::declare_extension_types;
/// use gleaph_gql::{ExtensionValue, ValueBinaryError};
///
/// const TOKEN_EXTENSION_TYPE_NAME: &str = "auth.SessionToken";
/// const TOKEN_KIND: u8 = 7;
///
/// fn decode_token_payload(
///     payload: &[u8],
/// ) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
///     // parse payload -> SessionTokenValue
///     # unimplemented!()
/// }
///
/// fn decode_short_token_payload(
///     payload: &[u8],
/// ) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
///     // parse short payload variant -> SessionTokenValue
///     # unimplemented!()
/// }
///
/// declare_extension_types! {
///     decoder: AppExtensionDecode;
///     type_names: [TOKEN_EXTENSION_TYPE_NAME, "SESSION_TOKEN"];
///     compact: { TOKEN_KIND => decode_token_payload };
///     short_blob: decode_short_token_payload;
/// }
///
/// // Only short raw-bytes format:
/// declare_extension_types! {
///     decoder: AppShortOnlyDecode;
///     type_names: [TOKEN_EXTENSION_TYPE_NAME];
///     short_blob: decode_short_token_payload;
/// }
///
/// // Only kind + payload format:
/// declare_extension_types! {
///     decoder: AppCompactOnlyDecode;
///     type_names: [TOKEN_EXTENSION_TYPE_NAME];
///     compact: { TOKEN_KIND => decode_token_payload };
/// }
///
/// // Host-side allowlist registration:
/// // AppExtensionDecode::for_each_extension_type(|name| service.register_extension_type(name));
/// ```
#[doc(inline)]
pub use gleaph_gql_macros::declare_extension_types;
