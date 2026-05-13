use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Expr, Ident, Pat, Path, Result, Token, Type, braced, bracketed, parse_macro_input,
};

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

struct SortableIndexKeyInput {
    domain: Expr,
    bytes: Expr,
}

struct ImplExtensionValueInput {
    trait_path: Path,
    ty: Type,
    type_name: Expr,
    eq: Expr,
    cmp: Option<Expr>,
    sortable_index_key: Option<SortableIndexKeyInput>,
    binary_payload: Option<Expr>,
    compact_kind: Option<Expr>,
    short_blob: Option<Expr>,
    hash_join_key: Option<Expr>,
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

impl Parse for SortableIndexKeyInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut domain = None;
        let mut bytes = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "domain" => {
                    if domain.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`domain` specified more than once",
                        ));
                    }
                    domain = Some(input.parse()?);
                }
                "bytes" => {
                    if bytes.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`bytes` specified more than once",
                        ));
                    }
                    bytes = Some(input.parse()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "expected one of: `domain`, `bytes`",
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            domain: domain.ok_or_else(|| syn::Error::new(input.span(), "missing `domain`"))?,
            bytes: bytes.ok_or_else(|| syn::Error::new(input.span(), "missing `bytes`"))?,
        })
    }
}

impl Parse for ImplExtensionValueInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        input.parse::<Token![impl]>()?;
        let trait_path: Path = input.parse()?;
        input.parse::<Token![for]>()?;
        let ty: Type = input.parse()?;

        let body;
        braced!(body in input);

        let mut type_name = None;
        let mut eq = None;
        let mut cmp = None;
        let mut sortable_index_key = None;
        let mut binary_payload = None;
        let mut compact_kind = None;
        let mut short_blob = None;
        let mut hash_join_key = None;

        while !body.is_empty() {
            let key: Ident = body.parse()?;
            body.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "type_name" => {
                    if type_name.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`type_name` specified more than once",
                        ));
                    }
                    type_name = Some(body.parse()?);
                }
                "eq" => {
                    if eq.is_some() {
                        return Err(syn::Error::new(key.span(), "`eq` specified more than once"));
                    }
                    eq = Some(body.parse()?);
                }
                "cmp" => {
                    if cmp.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`cmp` specified more than once",
                        ));
                    }
                    cmp = Some(body.parse()?);
                }
                "sortable_index_key" => {
                    if sortable_index_key.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`sortable_index_key` specified more than once",
                        ));
                    }
                    let key_body;
                    braced!(key_body in body);
                    sortable_index_key = Some(key_body.parse()?);
                }
                "binary_payload" => {
                    if binary_payload.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`binary_payload` specified more than once",
                        ));
                    }
                    binary_payload = Some(body.parse()?);
                }
                "compact_kind" => {
                    if compact_kind.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`compact_kind` specified more than once",
                        ));
                    }
                    compact_kind = Some(body.parse()?);
                }
                "short_blob" => {
                    if short_blob.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`short_blob` specified more than once",
                        ));
                    }
                    short_blob = Some(body.parse()?);
                }
                "hash_join_key" => {
                    if hash_join_key.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`hash_join_key` specified more than once",
                        ));
                    }
                    hash_join_key = Some(body.parse()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "expected one of: `type_name`, `eq`, `cmp`, `sortable_index_key`, `binary_payload`, `compact_kind`, `short_blob`, `hash_join_key`",
                    ));
                }
            }
            if body.peek(Token![;]) {
                body.parse::<Token![;]>()?;
            }
        }

        Ok(Self {
            trait_path,
            ty,
            type_name: type_name.ok_or_else(|| {
                syn::Error::new(body.span(), "missing required field `type_name`")
            })?,
            eq: eq.ok_or_else(|| syn::Error::new(body.span(), "missing required field `eq`"))?,
            cmp,
            sortable_index_key,
            binary_payload,
            compact_kind,
            short_blob,
            hash_join_key,
        })
    }
}

/// Implement `ExtensionValue` for a concrete extension type with minimal boilerplate.
///
/// Use this macro for the value implementation, then use [`declare_extension_types!`] to
/// declare how stored bytes are decoded in the consuming crate.
///
/// ```ignore
/// use std::borrow::Cow;
/// use gleaph_gql::extensions::impl_extension_value;
/// use gleaph_gql::ExtensionValue;
///
/// impl_extension_value! {
///     impl ExtensionValue for SessionTokenValue {
///         type_name: "auth.SessionToken";
///         eq: |this, other| this.0 == other.0;
///         binary_payload: |this| Cow::Borrowed(this.0.as_slice());
///     }
/// }
///
/// impl_extension_value! {
///     impl ExtensionValue for PrincipalValue {
///         type_name: "ic.Principal";
///         eq: |this, other| this.0 == other.0;
///         cmp: |this, other| this.0.cmp(&other.0);
///         sortable_index_key: {
///             domain: "ic.Principal/v1",
///             bytes: |this| Cow::Borrowed(this.0.as_slice()),
///         };
///         short_blob: |this| Cow::Borrowed(this.0.as_slice());
///     }
/// }
/// ```
#[proc_macro]
pub fn impl_extension_value(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as ImplExtensionValueInput);
    let trait_path = parsed.trait_path;
    let ty = parsed.ty;
    let type_name = parsed.type_name;
    let eq = parsed.eq;

    let cmp_impl = parsed.cmp.map(|cmp| {
        quote! {
            fn cmp_ext(
                &self,
                other: &dyn gleaph_gql::value::ExtensionValue,
            ) -> Option<std::cmp::Ordering> {
                let cmp: fn(&#ty, &#ty) -> std::cmp::Ordering = #cmp;
                other
                    .as_any()
                    .downcast_ref::<#ty>()
                    .map(|other| cmp(self, other))
            }
        }
    });

    let sortable_index_key_impl = parsed.sortable_index_key.map(|key| {
        let domain = key.domain;
        let bytes = key.bytes;
        quote! {
            fn sortable_index_key(&self) -> Option<gleaph_gql::value::ExtensionSortableKey<'_>> {
                let bytes: for<'a> fn(&'a #ty) -> std::borrow::Cow<'a, [u8]> = #bytes;
                Some(gleaph_gql::value::ExtensionSortableKey {
                    domain: std::borrow::Cow::Borrowed(#domain),
                    bytes: bytes(self),
                })
            }
        }
    });

    let binary_payload_impl = parsed.binary_payload.map(|binary_payload| {
        quote! {
            fn binary_payload(
                &self,
            ) -> Result<std::borrow::Cow<'_, [u8]>, gleaph_gql::value::ValueBinaryError> {
                let binary_payload: for<'a> fn(&'a #ty) -> std::borrow::Cow<'a, [u8]> = #binary_payload;
                Ok(binary_payload(self))
            }
        }
    });

    let compact_kind_impl = parsed.compact_kind.map(|compact_kind| {
        quote! {
            fn compact_kind(&self) -> Option<u8> {
                let compact_kind: fn(&#ty) -> u8 = #compact_kind;
                Some(compact_kind(self))
            }
        }
    });

    let short_blob_impl = parsed.short_blob.map(|short_blob| {
        quote! {
            fn short_blob(&self) -> Option<std::borrow::Cow<'_, [u8]>> {
                let short_blob: for<'a> fn(&'a #ty) -> std::borrow::Cow<'a, [u8]> = #short_blob;
                Some(short_blob(self))
            }
        }
    });

    let hash_join_key_impl = parsed.hash_join_key.map(|hash_join_key| {
        quote! {
            fn hash_join_key(&self, hasher: &mut dyn std::hash::Hasher) {
                let hash_join_key: fn(&#ty, &mut dyn std::hash::Hasher) = #hash_join_key;
                hash_join_key(self, hasher)
            }
        }
    });

    TokenStream::from(quote! {
        impl #trait_path for #ty {
            fn type_name(&self) -> &str {
                #type_name
            }

            fn clone_box(&self) -> Box<dyn gleaph_gql::value::ExtensionValue> {
                Box::new(self.clone())
            }

            fn eq_ext(&self, other: &dyn gleaph_gql::value::ExtensionValue) -> bool {
                let eq: fn(&#ty, &#ty) -> bool = #eq;
                other
                    .as_any()
                    .downcast_ref::<#ty>()
                    .is_some_and(|other| eq(self, other))
            }

            #cmp_impl

            #sortable_index_key_impl

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }

            #binary_payload_impl

            #compact_kind_impl

            #short_blob_impl

            #hash_join_key_impl
        }
    })
}

/// Declare binary decode paths and exported names for host extension values.
///
/// Use this after implementing one or more concrete extension values with
/// `impl_extension_value!`. `declare_extension_types!` does **not** implement
/// the value type itself; it tells `Value::from_binary_bytes_with_extensions`
/// how to turn stored extension payload bytes back into boxed `ExtensionValue`s.
///
/// This macro is intended for host integration crates, for example `gleaph-gql-ic`,
/// that define concrete extension values while `gleaph-gql` stays runtime-agnostic.
///
/// # When to use this
///
/// Use `declare_extension_types!` in the crate that owns the extension decoding policy:
///
/// - A graph/runtime integration crate that needs to read stored extension values.
/// - A host bridge crate that exposes extension type names to an allowlist.
/// - A test crate that needs a small decoder for mock extension values.
///
/// If you only need a runtime-only `Value::Extension` that is never encoded and decoded,
/// you do not need this macro.
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
///   - Host-visible extension type names for allowlists and diagnostics.
///   - Include the canonical `ExtensionValue::type_name()` and any accepted aliases.
///   - Exposed as `Decoder::EXTENSION_TYPE_NAMES` and consumed by host allowlists.
/// - `compact: { kind => handler, ... };`
///   - Type requirement:
///     - `kind`: pattern matching `u8` (typically an integer literal such as `7`,
///       or a `const u8` name).
///     - `handler`: `fn(&[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError>`.
///   - Dispatch table for the "kind + payload" extension format.
///   - This decodes values whose `ExtensionValue::compact_kind()` chose tag **33**.
///   - Use this when a single decoder supports multiple extension kinds.
///   - Optional.
/// - `short_blob: handler;`
///   - Type requirement: expression resolving to a callable with signature
///     `fn(&[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError>`.
///   - Decoder for the "short raw-bytes" extension format.
///   - This decodes values whose `ExtensionValue::short_blob()` chose tag **34**.
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
/// # Runtime use
///
/// Pass the generated decoder wherever bytes are decoded:
///
/// ```ignore
/// let value = Value::from_binary_bytes_with_extensions(bytes, &AppExtensionDecode)?;
/// ```
///
/// For rkyv deserialization, install the decoder with the rkyv hook used by your
/// integration crate. For example, `gleaph-gql-ic` exposes
/// `install_ic_extension_binary_decode_for_rkyv()`.
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
/// let value = gleaph_gql::Value::from_binary_bytes_with_extensions(
///     bytes,
///     &AppExtensionDecode,
/// )?;
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
