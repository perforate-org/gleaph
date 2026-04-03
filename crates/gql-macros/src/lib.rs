use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Expr, Ident, Pat, Result, Token, braced, bracketed, parse_macro_input};

struct CompactEntry {
    kind: Pat,
    handler: Expr,
}

struct DeclareExtensionTypesInput {
    decoder_attrs: Vec<Attribute>,
    decoder: Ident,
    type_names: Vec<Expr>,
    compact: Option<Vec<CompactEntry>>,
    short_handler: Option<Expr>,
}

impl Parse for DeclareExtensionTypesInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let decoder_attrs = input.call(Attribute::parse_outer)?;
        let decoder_key: Ident = input.parse()?;
        if decoder_key != "decoder" {
            return Err(syn::Error::new(decoder_key.span(), "expected `decoder`"));
        }
        input.parse::<Token![:]>()?;
        let decoder: Ident = input.parse()?;
        input.parse::<Token![;]>()?;

        let mut type_names: Option<Vec<Expr>> = None;
        let mut compact: Option<Vec<CompactEntry>> = None;
        let mut short_handler: Option<Expr> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "type_names" => {
                    if type_names.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`type_names` specified more than once",
                        ));
                    }
                    let type_names_content;
                    bracketed!(type_names_content in input);
                    let parsed: syn::punctuated::Punctuated<Expr, Token![,]> =
                        type_names_content.parse_terminated(Expr::parse, Token![,])?;
                    type_names = Some(parsed.into_iter().collect());
                }
                "compact" => {
                    if compact.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`compact` specified more than once",
                        ));
                    }
                    let compact_content;
                    braced!(compact_content in input);
                    let mut entries = Vec::new();
                    while !compact_content.is_empty() {
                        let kind: Pat = Pat::parse_single(&compact_content)?;
                        compact_content.parse::<Token![=>]>()?;
                        let handler: Expr = compact_content.parse()?;
                        entries.push(CompactEntry { kind, handler });
                        if compact_content.peek(Token![,]) {
                            compact_content.parse::<Token![,]>()?;
                        }
                    }
                    compact = Some(entries);
                }
                "short_blob" => {
                    if short_handler.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`short_blob` specified more than once",
                        ));
                    }
                    short_handler = Some(input.parse()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "expected one of: `type_names`, `compact`, `short_blob`",
                    ));
                }
            }
            if input.peek(Token![;]) {
                input.parse::<Token![;]>()?;
            }
        }

        let type_names = type_names
            .ok_or_else(|| syn::Error::new(input.span(), "missing required field `type_names`"))?;
        let has_compact_entries = compact.as_ref().is_some_and(|entries| !entries.is_empty());
        let has_short_blob = short_handler.is_some();
        if !has_compact_entries && !has_short_blob {
            return Err(syn::Error::new(
                input.span(),
                "at least one decoding path is required: provide non-empty `compact` and/or `short_blob`",
            ));
        }

        Ok(Self {
            decoder_attrs,
            decoder,
            type_names,
            compact,
            short_handler,
        })
    }
}

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
///     `gleaph_gql::value::ExtensionBinaryDecode`.
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
/// ```
#[proc_macro]
pub fn declare_extension_types(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeclareExtensionTypesInput);
    let decoder_attrs = parsed.decoder_attrs;
    let decoder = parsed.decoder;
    let type_names = parsed.type_names;
    let short_handler = parsed.short_handler;
    let compact_arms = parsed.compact.as_ref().into_iter().flatten().map(|entry| {
        let kind = &entry.kind;
        let handler = &entry.handler;
        quote! { #kind => (#handler)(payload), }
    });
    let decode_compact_impl = if parsed.compact.is_some() {
        quote! {
            fn decode_extension_compact(
                &self,
                kind: u8,
                payload: &[u8],
            ) -> Result<Box<dyn gleaph_gql::value::ExtensionValue>, gleaph_gql::value::ValueBinaryError> {
                match kind {
                    #(#compact_arms)*
                    _ => Err(gleaph_gql::value::ValueBinaryError::UnknownEncodedExtension),
                }
            }
        }
    } else {
        quote! {}
    };
    let decode_short_blob_impl = if let Some(short_handler) = short_handler {
        quote! {
            fn decode_extension_short_blob(
                &self,
                payload: &[u8],
            ) -> Result<Box<dyn gleaph_gql::value::ExtensionValue>, gleaph_gql::value::ValueBinaryError> {
                (#short_handler)(payload)
            }
        }
    } else {
        quote! {}
    };

    TokenStream::from(quote! {
        #(#decoder_attrs)*
        pub struct #decoder;

        impl #decoder {
            /// Extension type names that the host should register in its allowlist.
            pub const EXTENSION_TYPE_NAMES: &'static [&'static str] = &[#(#type_names),*];

            /// Calls `f` for each declared extension type name.
            pub fn for_each_extension_type(mut f: impl FnMut(&str)) {
                for name in Self::EXTENSION_TYPE_NAMES {
                    f(name);
                }
            }
        }

        impl gleaph_gql::value::ExtensionBinaryDecode for #decoder {
            #decode_compact_impl
            #decode_short_blob_impl
        }
    })
}
