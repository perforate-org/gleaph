//! Proc macros for `gleaph-gql`.

use proc_macro::TokenStream;

mod gql_extension_impl;

/// Declares vendor GQL extensions: wire [`ExtensionValue`](::gleaph_gql::value::ExtensionValue)
/// types, binary decode tables, optional expression intrinsics, and optional path extensions.
///
/// See `gleaph_gql::extensions` module documentation for the DSL.
#[proc_macro]
pub fn define_gql_extension(input: TokenStream) -> TokenStream {
    gql_extension_impl::expand(input)
}
