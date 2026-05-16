//! `gql_extension!` — vendor GQL extensions (wire `ExtensionValue` + decode + member metadata).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Expr, ExprLit, Ident, Lit, LitStr, Pat, Result, Token, Type, braced, bracketed,
    parse_macro_input,
};

struct CompactEntry {
    kind: Pat,
    handler: Expr,
}

struct SortableIndexKeyInput {
    domain: Expr,
    bytes: Expr,
}

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as GqlExtensionInput);
    match parsed.expand() {
        Ok(ts) => TokenStream::from(ts),
        Err(e) => TokenStream::from(e.to_compile_error()),
    }
}

struct GqlExtensionInput {
    prefix: LitStr,
    types: Vec<TypeSpec>,
    functions: Vec<FunctionSpec>,
    path_extensions: Vec<PathExtensionSpec>,
}

impl Parse for GqlExtensionInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut prefix = None;
        let mut types = None;
        let mut functions = None;
        let mut path_extensions = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "prefix" => {
                    if prefix.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`prefix` specified more than once",
                        ));
                    }
                    prefix = Some(input.parse::<LitStr>()?);
                }
                "types" => {
                    if types.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`types` specified more than once",
                        ));
                    }
                    let inner;
                    bracketed!(inner in input);
                    types = Some(parse_braced_list::<TypeSpec>(&inner)?);
                }
                "functions" => {
                    if functions.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`functions` specified more than once",
                        ));
                    }
                    let inner;
                    bracketed!(inner in input);
                    functions = Some(parse_braced_list::<FunctionSpec>(&inner)?);
                }
                "path_extensions" => {
                    if path_extensions.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`path_extensions` specified more than once",
                        ));
                    }
                    let inner;
                    bracketed!(inner in input);
                    path_extensions = Some(parse_braced_list::<PathExtensionSpec>(&inner)?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "expected one of: `prefix`, `types`, `functions`, `path_extensions`",
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            prefix: prefix.ok_or_else(|| input.error("missing required field `prefix`"))?,
            types: types.unwrap_or_default(),
            functions: functions.unwrap_or_default(),
            path_extensions: path_extensions.unwrap_or_default(),
        })
    }
}

fn parse_braced_list<T: Parse>(inner: ParseStream<'_>) -> Result<Vec<T>> {
    let mut out = Vec::new();
    while !inner.is_empty() {
        let item;
        braced!(item in inner);
        out.push(item.parse()?);
        if inner.peek(Token![,]) {
            inner.parse::<Token![,]>()?;
        }
    }
    Ok(out)
}

struct TypeSpec {
    attrs: Vec<Attribute>,
    rust_type: Type,
    type_name: Expr,
    decoder: Ident,
    alias: Vec<Expr>,
    eq: Expr,
    cmp: Option<Expr>,
    sortable_index_key: Option<SortableIndexKeyInput>,
    binary_payload: Option<Expr>,
    compact_kind: Option<Expr>,
    short_blob: Option<Expr>,
    hash_join_key: Option<Expr>,
    short_blob_decode: Option<Expr>,
    compact: Option<Vec<CompactEntry>>,
}

impl Parse for TypeSpec {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let body = input;
        let mut rust_type = None;
        let mut type_name = None;
        let mut decoder = None;
        let mut alias = Vec::new();
        let mut eq = None;
        let mut cmp = None;
        let mut sortable_index_key = None;
        let mut binary_payload = None;
        let mut compact_kind = None;
        let mut short_blob = None;
        let mut hash_join_key = None;
        let mut short_blob_decode = None;
        let mut compact = None;

        while !body.is_empty() {
            let key: Ident = body.parse()?;
            body.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "rust_type" => {
                    if rust_type.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`rust_type` specified more than once",
                        ));
                    }
                    rust_type = Some(body.parse()?);
                }
                "type_name" => {
                    if type_name.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`type_name` specified more than once",
                        ));
                    }
                    type_name = Some(body.parse()?);
                }
                "decoder" => {
                    if decoder.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`decoder` specified more than once",
                        ));
                    }
                    decoder = Some(body.parse()?);
                }
                "alias" => {
                    let a;
                    bracketed!(a in body);
                    let parsed: syn::punctuated::Punctuated<Expr, Token![,]> =
                        a.parse_terminated(Expr::parse, Token![,])?;
                    alias = parsed.into_iter().collect();
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
                    let kb;
                    braced!(kb in body);
                    sortable_index_key = Some(kb.parse()?);
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
                "short_blob_decode" => {
                    if short_blob_decode.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`short_blob_decode` specified more than once",
                        ));
                    }
                    short_blob_decode = Some(body.parse()?);
                }
                "compact" => {
                    if compact.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`compact` specified more than once",
                        ));
                    }
                    let compact_content;
                    braced!(compact_content in body);
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
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown field in `types` item (expected rust_type, type_name, decoder, alias?, eq, ...)",
                    ));
                }
            }
            if body.peek(Token![;]) {
                body.parse::<Token![;]>()?;
            } else if body.peek(Token![,]) {
                body.parse::<Token![,]>()?;
            }
        }

        let rust_type = rust_type.ok_or_else(|| body.error("missing `rust_type`"))?;
        let type_name = type_name.ok_or_else(|| body.error("missing `type_name`"))?;
        let decoder = decoder.ok_or_else(|| body.error("missing `decoder`"))?;
        let eq = eq.ok_or_else(|| body.error("missing `eq`"))?;

        let has_compact_entries = compact.as_ref().is_some_and(|e| !e.is_empty());
        let has_short = short_blob_decode.is_some();
        if !has_compact_entries && !has_short {
            return Err(syn::Error::new(
                body.span(),
                "each `types` item needs `short_blob_decode` (and usually `short_blob`) and/or non-empty `compact`",
            ));
        }

        Ok(Self {
            attrs,
            rust_type,
            type_name,
            decoder,
            alias,
            eq,
            cmp,
            sortable_index_key,
            binary_payload,
            compact_kind,
            short_blob,
            hash_join_key,
            short_blob_decode,
            compact,
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
            if input.peek(Token![;]) {
                input.parse::<Token![;]>()?;
            } else if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            domain: domain.ok_or_else(|| input.error("missing `domain`"))?,
            bytes: bytes.ok_or_else(|| input.error("missing `bytes`"))?,
        })
    }
}

struct FunctionSpec {
    name: LitStr,
    alias: Vec<Expr>,
    eval: Expr,
}

impl Parse for FunctionSpec {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        let mut alias = Vec::new();
        let mut eval = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "name" => {
                    if name.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`name` specified more than once",
                        ));
                    }
                    name = Some(input.parse()?);
                }
                "alias" => {
                    let a;
                    bracketed!(a in input);
                    let parsed: syn::punctuated::Punctuated<Expr, Token![,]> =
                        a.parse_terminated(Expr::parse, Token![,])?;
                    alias = parsed.into_iter().collect();
                }
                "eval" | "function" => {
                    if eval.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`eval` / `function` specified more than once",
                        ));
                    }
                    eval = Some(input.parse()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown field in `functions` item (expected name, alias?, eval | function)",
                    ));
                }
            }
            if input.peek(Token![;]) {
                input.parse::<Token![;]>()?;
            } else if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name`"))?,
            alias,
            eval: eval.ok_or_else(|| input.error("missing `eval` or `function`"))?,
        })
    }
}

struct PathExtensionSpec {
    name: LitStr,
    alias: Vec<Expr>,
    validate_plan: Expr,
}

impl Parse for PathExtensionSpec {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        let mut alias = Vec::new();
        let mut validate_plan = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "name" => {
                    if name.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`name` specified more than once",
                        ));
                    }
                    name = Some(input.parse()?);
                }
                "alias" => {
                    let a;
                    bracketed!(a in input);
                    let parsed: syn::punctuated::Punctuated<Expr, Token![,]> =
                        a.parse_terminated(Expr::parse, Token![,])?;
                    alias = parsed.into_iter().collect();
                }
                "validate_plan" => {
                    if validate_plan.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`validate_plan` specified more than once",
                        ));
                    }
                    validate_plan = Some(input.parse()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown field in `path_extensions` item (expected name, alias?, validate_plan)",
                    ));
                }
            }
            if input.peek(Token![;]) {
                input.parse::<Token![;]>()?;
            } else if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name`"))?,
            alias,
            validate_plan: validate_plan.ok_or_else(|| input.error("missing `validate_plan`"))?,
        })
    }
}

impl GqlExtensionInput {
    fn expand(self) -> Result<TokenStream2> {
        let prefix_lit = &self.prefix;
        let prefix_value = prefix_lit.value();
        if gleaph_gql_keywords::is_reserved_keyword(&prefix_value) {
            return Err(syn::Error::new_spanned(
                prefix_lit.clone(),
                "GQL extension `prefix` must not be a GQL reserved keyword (case-insensitive); choose another prefix or use a delimited spelling in wire names",
            ));
        }
        if gleaph_gql_keywords::is_prereserved_keyword(&prefix_value) {
            return Err(syn::Error::new_spanned(
                prefix_lit.clone(),
                "GQL extension `prefix` must not be a GQL prereserved keyword (case-insensitive); these may become reserved later — pick a distinct vendor prefix",
            ));
        }
        let prefix_upper = slug_upper(prefix_value.as_str());

        let mut streams = Vec::new();

        for t in &self.types {
            streams.push(expand_type_entry(t, prefix_lit)?);
        }

        let mut fn_meta = Vec::new();
        let mut fn_evals = Vec::new();
        for (i, f) in self.functions.iter().enumerate() {
            let names = member_names_const(&prefix_upper, "FN", &f.name, i, &f.alias)?;
            let eval = &f.eval;
            let fn_ident = member_fn_ident(&prefix_upper, f.name.value().as_str(), i);
            fn_meta.push(quote! { #names });
            fn_evals.push(quote! {
                #[inline]
                pub fn #fn_ident() -> ::gleaph_gql::Value {
                    let __eval = #eval;
                    __eval()
                }
            });
        }

        let mut path_meta = Vec::new();
        let mut path_use = Vec::new();
        for (i, p) in self.path_extensions.iter().enumerate() {
            let names = member_names_const(&prefix_upper, "PATH", &p.name, i, &p.alias)?;
            path_meta.push(quote! { #names });
            let plan = &p.validate_plan;
            let plan_path = match plan {
                Expr::Path(ep) => ep.path.clone(),
                _ => {
                    return Err(syn::Error::new_spanned(
                        plan.clone(),
                        "`validate_plan` must be a path (e.g. `crate::plan::my_fn`)",
                    ));
                }
            };
            let alias = use_alias_ident(&prefix_upper, "path_validate", p.name.value().as_str(), i);
            path_use.push(quote! { pub use #plan_path as #alias; });
        }

        let fn_meta = if fn_meta.is_empty() {
            quote! {}
        } else {
            quote! { #( #fn_meta )* }
        };
        let fn_evals = if fn_evals.is_empty() {
            quote! {}
        } else {
            quote! { #( #fn_evals )* }
        };
        let path_meta = if path_meta.is_empty() {
            quote! {}
        } else {
            quote! { #( #path_meta )* }
        };
        let path_use = if path_use.is_empty() {
            quote! {}
        } else {
            quote! { #( #path_use )* }
        };

        let prefix_const = Ident::new(
            &format!("GQL_EXTENSION_{}_PREFIX", prefix_upper),
            proc_macro2::Span::call_site(),
        );

        Ok(quote! {
            pub const #prefix_const: &str = #prefix_lit;

            #( #streams )*

            #fn_meta
            #fn_evals

            #path_meta
            #path_use
        })
    }
}

fn expand_type_entry(t: &TypeSpec, extension_prefix_lit: &LitStr) -> Result<TokenStream2> {
    let ty = &t.rust_type;
    let type_name_field = &t.type_name;
    let eq = &t.eq;
    let decoder = &t.decoder;
    let decoder_attrs = &t.attrs;

    let (type_name_body, decode_names): (TokenStream2, Vec<Expr>) = match type_name_field {
        Expr::Lit(ExprLit {
            lit: Lit::Str(short),
            ..
        }) => {
            let qualified = format!("{}.{}", extension_prefix_lit.value(), short.value());
            let qualified_lit = LitStr::new(&qualified, short.span());
            let mut decode_names = vec![Expr::Lit(ExprLit {
                attrs: Vec::new(),
                lit: Lit::Str(qualified_lit.clone()),
            })];
            decode_names.extend(t.alias.iter().cloned());
            (quote! { #qualified_lit }, decode_names)
        }
        _ => {
            let mut decode_names = vec![type_name_field.clone()];
            decode_names.extend(t.alias.iter().cloned());
            (quote! { #type_name_field }, decode_names)
        }
    };

    let cmp_impl = t.cmp.as_ref().map(|cmp| {
        quote! {
            fn cmp_ext(
                &self,
                other: &dyn ::gleaph_gql::value::ExtensionValue,
            ) -> Option<std::cmp::Ordering> {
                let cmp: fn(&#ty, &#ty) -> std::cmp::Ordering = #cmp;
                other
                    .as_any()
                    .downcast_ref::<#ty>()
                    .map(|other| cmp(self, other))
            }
        }
    });

    let sortable_index_key_impl = t.sortable_index_key.as_ref().map(|key| {
        let domain = &key.domain;
        let bytes = &key.bytes;
        quote! {
            fn sortable_index_key(&self) -> Option<::gleaph_gql::value::ExtensionSortableKey<'_>> {
                let bytes: for<'a> fn(&'a #ty) -> std::borrow::Cow<'a, [u8]> = #bytes;
                Some(::gleaph_gql::value::ExtensionSortableKey {
                    domain: std::borrow::Cow::Borrowed(#domain),
                    bytes: bytes(self),
                })
            }
        }
    });

    let binary_payload_impl = t.binary_payload.as_ref().map(|binary_payload| {
        quote! {
            fn binary_payload(
                &self,
            ) -> Result<std::borrow::Cow<'_, [u8]>, ::gleaph_gql::value::ValueBinaryError> {
                let binary_payload: for<'a> fn(&'a #ty) -> std::borrow::Cow<'a, [u8]> = #binary_payload;
                Ok(binary_payload(self))
            }
        }
    });

    let compact_kind_impl = t.compact_kind.as_ref().map(|compact_kind| {
        quote! {
            fn compact_kind(&self) -> Option<u8> {
                let compact_kind: fn(&#ty) -> u8 = #compact_kind;
                Some(compact_kind(self))
            }
        }
    });

    let short_blob_impl = t.short_blob.as_ref().map(|short_blob| {
        quote! {
            fn short_blob(&self) -> Option<std::borrow::Cow<'_, [u8]>> {
                let short_blob: for<'a> fn(&'a #ty) -> std::borrow::Cow<'a, [u8]> = #short_blob;
                Some(short_blob(self))
            }
        }
    });

    let hash_join_key_impl = t.hash_join_key.as_ref().map(|hash_join_key| {
        quote! {
            fn hash_join_key(&self, hasher: &mut dyn std::hash::Hasher) {
                let hash_join_key: fn(&#ty, &mut dyn std::hash::Hasher) = #hash_join_key;
                hash_join_key(self, hasher)
            }
        }
    });

    let compact_arms = t.compact.as_ref().into_iter().flatten().map(|entry| {
        let kind = &entry.kind;
        let handler = &entry.handler;
        quote! { #kind => (#handler)(payload), }
    });
    let decode_compact_impl = if t.compact.as_ref().is_some() {
        quote! {
            fn decode_extension_compact(
                &self,
                kind: u8,
                payload: &[u8],
            ) -> Result<Box<dyn ::gleaph_gql::value::ExtensionValue>, ::gleaph_gql::value::ValueBinaryError> {
                match kind {
                    #(#compact_arms)*
                    _ => Err(::gleaph_gql::value::ValueBinaryError::UnknownEncodedExtension),
                }
            }
        }
    } else {
        quote! {}
    };

    let decode_short_blob_impl = if let Some(short_handler) = &t.short_blob_decode {
        quote! {
            fn decode_extension_short_blob(
                &self,
                payload: &[u8],
            ) -> Result<Box<dyn ::gleaph_gql::value::ExtensionValue>, ::gleaph_gql::value::ValueBinaryError> {
                (#short_handler)(payload)
            }
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        impl ::gleaph_gql::value::ExtensionValue for #ty {
            fn type_name(&self) -> &str {
                #type_name_body
            }

            fn clone_box(&self) -> Box<dyn ::gleaph_gql::value::ExtensionValue> {
                Box::new(self.clone())
            }

            fn eq_ext(&self, other: &dyn ::gleaph_gql::value::ExtensionValue) -> bool {
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

        #(#decoder_attrs)*
        pub struct #decoder;

        impl #decoder {
            /// Stable singleton for [`ExtensionBinaryDecode`](::gleaph_gql::value::ExtensionBinaryDecode) call sites (`&#decoder::INSTANCE`).
            pub const INSTANCE: Self = Self;

            pub const EXTENSION_TYPE_NAMES: &'static [&'static str] = &[#(#decode_names),*];

            pub fn for_each_extension_type(mut f: impl FnMut(&str)) {
                for name in Self::EXTENSION_TYPE_NAMES {
                    f(name);
                }
            }
        }

        impl ::gleaph_gql::value::ExtensionBinaryDecode for #decoder {
            #decode_compact_impl
            #decode_short_blob_impl
        }
    })
}

fn slug_upper(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn member_names_const(
    prefix_upper: &str,
    kind: &str,
    primary: &LitStr,
    idx: usize,
    alias: &[Expr],
) -> Result<TokenStream2> {
    let member_slug = slug_upper(primary.value().as_str());
    let ident = Ident::new(
        &format!("GQL_EXTENSION_{prefix_upper}_{kind}_{member_slug}_{idx}_NAMES"),
        proc_macro2::Span::call_site(),
    );
    if alias.is_empty() {
        return Ok(quote! {
            pub const #ident: ::gleaph_gql::vendor_extension::GqlVendorMemberNames =
                ::gleaph_gql::vendor_extension::GqlVendorMemberNames::new(#primary, &[]);
        });
    }
    Ok(quote! {
        pub const #ident: ::gleaph_gql::vendor_extension::GqlVendorMemberNames =
            ::gleaph_gql::vendor_extension::GqlVendorMemberNames::new(#primary, &[#(#alias),*]);
    })
}

fn member_fn_ident(prefix_upper: &str, member: &str, idx: usize) -> Ident {
    let member_slug = slug_upper(member);
    Ident::new(
        &format!("gql_extension_eval_{prefix_upper}_{member_slug}_{idx}"),
        proc_macro2::Span::call_site(),
    )
}

fn use_alias_ident(prefix_upper: &str, role: &str, member: &str, idx: usize) -> Ident {
    let member_slug = slug_upper(member);
    Ident::new(
        &format!("__gql_ext_{role}_{prefix_upper}_{member_slug}_{idx}"),
        proc_macro2::Span::call_site(),
    )
}
