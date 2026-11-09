//! No-op stand-ins for the Verus macros used by the intake-harness mirror.
//!
//! The fixture builds in two modes:
//!
//! - with `--features verus` under the pinned Verus launcher: real macros from `vstd`, and the
//!   mirror is verified;
//! - plain `cargo check`: these stubs erase specification and proof code, so the executable
//!   behaviour can be type-checked without a Verus toolchain.
//!
//! The stubs accept arbitrary token trees because specification syntax is not Rust.

use proc_macro::TokenStream;

/// Erases a `verus! { ... }` block outside Verus builds.
#[proc_macro]
pub fn verus(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// Returns the executable item unchanged; its specification tokens are dropped.
#[proc_macro_attribute]
pub fn verus_spec(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Erases a statement-position `proof! { ... }` block.
#[proc_macro]
pub fn proof(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// Erases a statement-position `proof_decl! { ... }` block.
#[proc_macro]
pub fn proof_decl(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
