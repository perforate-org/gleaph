pub mod javascript;
pub mod rust_lang;
pub mod typescript;

use gleaph_types::{PreparedKind, PreparedStatementInfo, PreparedValueType};
use std::collections::BTreeMap;

/// Target language for code generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// `.js` + `.d.ts` pair (default)
    JavaScript,
    /// Single `.ts` file
    TypeScript,
    /// Single `.rs` file
    Rust,
}

impl Lang {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "js" => Some(Self::JavaScript),
            "ts" => Some(Self::TypeScript),
            "rust" | "rs" => Some(Self::Rust),
            _ => None,
        }
    }

    pub fn default_filename(&self) -> &'static str {
        match self {
            Self::JavaScript => "gleaph.generated.js",
            Self::TypeScript => "gleaph.generated.ts",
            Self::Rust => "gleaph_prepared.rs",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsParamStyle {
    Preserve,
    Camel,
}

/// Sanitize a prepared statement name into a valid identifier.
/// Replaces `-` with `_`, rejects empty or leading-digit names.
pub fn sanitize_ident(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(s)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamBinding {
    pub wire_name: String,
    pub api_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortKeyBinding {
    pub wire_key: String,
    pub api_name: String,
}

pub fn to_camel_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut uppercase_next = false;
    for (i, ch) in name.chars().enumerate() {
        if ch == '_' || ch == '-' {
            uppercase_next = true;
            continue;
        }
        if i == 0 {
            out.extend(ch.to_lowercase());
            continue;
        }
        if uppercase_next {
            out.extend(ch.to_uppercase());
            uppercase_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn to_pascal_case(name: &str) -> String {
    let filtered: String = name
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
        .collect();
    let camel = to_camel_case(&filtered);
    let mut chars = camel.chars();
    match chars.next() {
        Some(first) => {
            let mut out = String::new();
            if first.is_ascii_digit() {
                out.push('K');
            }
            out.extend(first.to_uppercase());
            out.extend(chars);
            out
        }
        None => "Key".into(),
    }
}

pub fn sort_key_bindings(info: &PreparedStatementInfo) -> Vec<SortKeyBinding> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    info.allowed_sorts
        .iter()
        .map(|sort| {
            let base = to_pascal_case(&sort.key);
            let next = counts.entry(base.clone()).or_insert(0);
            *next += 1;
            let api_name = if *next == 1 {
                base
            } else {
                format!("{base}{next}")
            };
            SortKeyBinding {
                wire_key: sort.key.clone(),
                api_name,
            }
        })
        .collect()
}

pub fn param_bindings(info: &PreparedStatementInfo, style: JsParamStyle) -> Vec<ParamBinding> {
    info.parameters
        .iter()
        .map(|p| {
            let wire_name = p.name.clone();
            let api_name = match style {
                JsParamStyle::Preserve => wire_name.clone(),
                JsParamStyle::Camel => to_camel_case(&wire_name),
            };
            ParamBinding {
                wire_name,
                api_name,
            }
        })
        .collect()
}

pub fn required_param_bindings(
    info: &PreparedStatementInfo,
    style: JsParamStyle,
) -> Vec<ParamBinding> {
    info.parameters
        .iter()
        .filter(|p| p.required)
        .map(|p| {
            let wire_name = p.name.clone();
            let api_name = match style {
                JsParamStyle::Preserve => wire_name.clone(),
                JsParamStyle::Camel => to_camel_case(&wire_name),
            };
            ParamBinding {
                wire_name,
                api_name,
            }
        })
        .collect()
}

pub fn optional_param_bindings(
    info: &PreparedStatementInfo,
    style: JsParamStyle,
) -> Vec<ParamBinding> {
    info.parameters
        .iter()
        .filter(|p| !p.required)
        .map(|p| {
            let wire_name = p.name.clone();
            let api_name = match style {
                JsParamStyle::Preserve => wire_name.clone(),
                JsParamStyle::Camel => to_camel_case(&wire_name),
            };
            ParamBinding {
                wire_name,
                api_name,
            }
        })
        .collect()
}

pub fn has_dynamic_sort(info: &PreparedStatementInfo) -> bool {
    !info.allowed_sorts.is_empty() && matches!(info.kind, PreparedKind::Query)
}

pub fn needs_prepared_sort_import(stmts: &[PreparedStatementInfo]) -> bool {
    stmts.iter().any(has_dynamic_sort)
}

/// Returns `true` when any param across all statements uses the Principal type.
pub fn needs_principal_import(stmts: &[PreparedStatementInfo]) -> bool {
    stmts.iter().any(|s| {
        s.parameters.iter().any(|p| {
            p.types.iter().any(|t| {
                matches!(
                    t,
                    PreparedValueType::Principal
                        | PreparedValueType::TypedList(gleaph_types::PreparedScalarType::Principal)
                )
            })
        })
    })
}

fn prepared_scalar_type_name(s: &gleaph_types::PreparedScalarType) -> &'static str {
    use gleaph_types::PreparedScalarType;
    match s {
        PreparedScalarType::Int8 => "INT8",
        PreparedScalarType::Int16 => "INT16",
        PreparedScalarType::Int32 => "INT32",
        PreparedScalarType::Int64 => "INT64",
        PreparedScalarType::Int128 => "INT128",
        PreparedScalarType::Int256 => "INT256",
        PreparedScalarType::Uint8 => "UINT8",
        PreparedScalarType::Uint16 => "UINT16",
        PreparedScalarType::Uint32 => "UINT32",
        PreparedScalarType::Uint64 => "UINT64",
        PreparedScalarType::Uint128 => "UINT128",
        PreparedScalarType::Uint256 => "UINT256",
        PreparedScalarType::Float32 => "FLOAT32",
        PreparedScalarType::Float64 => "FLOAT64",
        PreparedScalarType::Text => "TEXT",
        PreparedScalarType::Bool => "BOOL",
        PreparedScalarType::Timestamp => "TIMESTAMP",
        PreparedScalarType::Bytes => "BYTES",
        PreparedScalarType::Date => "DATE",
        PreparedScalarType::Time => "TIME",
        PreparedScalarType::DateTime => "DATETIME",
        PreparedScalarType::Duration => "DURATION",
        PreparedScalarType::Principal => "PRINCIPAL",
        PreparedScalarType::Decimal => "DECIMAL",
    }
}

pub fn sort_key_type_name(ident: &str) -> String {
    format!("{}SortKey", to_pascal_case(ident))
}

pub fn sort_spec_type_name(ident: &str) -> String {
    format!("{}SortSpec", to_pascal_case(ident))
}

pub fn params_type_name(ident: &str) -> String {
    format!("{}Params", to_pascal_case(ident))
}

/// Map a `PreparedScalarType` to a TypeScript type string.
fn ts_scalar_type(s: &gleaph_types::PreparedScalarType) -> &'static str {
    use gleaph_types::PreparedScalarType;
    match s {
        PreparedScalarType::Int8 | PreparedScalarType::Int16 | PreparedScalarType::Int32 => {
            "number"
        }
        PreparedScalarType::Int64 | PreparedScalarType::Int128 => "bigint",
        PreparedScalarType::Int256 => "string",
        PreparedScalarType::Uint8 | PreparedScalarType::Uint16 | PreparedScalarType::Uint32 => {
            "number"
        }
        PreparedScalarType::Uint64 | PreparedScalarType::Uint128 => "bigint",
        PreparedScalarType::Uint256 => "string",
        PreparedScalarType::Float32 | PreparedScalarType::Float64 => "number",
        PreparedScalarType::Text => "string",
        PreparedScalarType::Bool => "boolean",
        PreparedScalarType::Timestamp => "bigint",
        PreparedScalarType::Bytes => "Uint8Array",
        PreparedScalarType::Date => "number",
        PreparedScalarType::Time => "bigint",
        PreparedScalarType::DateTime => "[bigint, number]",
        PreparedScalarType::Duration => "[number, bigint]",
        PreparedScalarType::Principal => "Principal",
        PreparedScalarType::Decimal => "string",
    }
}

/// Map a `PreparedValueType` to a TypeScript type string.
pub fn ts_type_for(vt: &PreparedValueType) -> String {
    match vt {
        PreparedValueType::Int8 | PreparedValueType::Int16 | PreparedValueType::Int32 => {
            "number".into()
        }
        PreparedValueType::Int64 | PreparedValueType::Int128 => "bigint".into(),
        PreparedValueType::Int256 => "string".into(),
        PreparedValueType::Uint8 | PreparedValueType::Uint16 | PreparedValueType::Uint32 => {
            "number".into()
        }
        PreparedValueType::Uint64 | PreparedValueType::Uint128 => "bigint".into(),
        PreparedValueType::Uint256 => "string".into(),
        PreparedValueType::Float32 | PreparedValueType::Float64 => "number".into(),
        PreparedValueType::Text => "string".into(),
        PreparedValueType::Bool => "boolean".into(),
        PreparedValueType::Timestamp => "bigint".into(),
        PreparedValueType::List => "Value[]".into(),
        PreparedValueType::TypedList(s) => format!("{}[]", ts_scalar_type(s)),
        PreparedValueType::Null => "null".into(),
        PreparedValueType::Bytes => "Uint8Array".into(),
        PreparedValueType::Date => "number".into(),
        PreparedValueType::Time => "bigint".into(),
        PreparedValueType::DateTime => "[bigint, number]".into(),
        PreparedValueType::Duration => "[number, bigint]".into(),
        PreparedValueType::Principal => "Principal".into(),
        PreparedValueType::Decimal => "string".into(),
    }
}

/// Map a `PreparedScalarType` to a Rust type string.
fn rs_scalar_type(s: &gleaph_types::PreparedScalarType) -> &'static str {
    use gleaph_types::PreparedScalarType;
    match s {
        PreparedScalarType::Int8 => "i8",
        PreparedScalarType::Int16 => "i16",
        PreparedScalarType::Int32 => "i32",
        PreparedScalarType::Int64 => "i64",
        PreparedScalarType::Int128 => "i128",
        PreparedScalarType::Int256 => "String",
        PreparedScalarType::Uint8 => "u8",
        PreparedScalarType::Uint16 => "u16",
        PreparedScalarType::Uint32 => "u32",
        PreparedScalarType::Uint64 => "u64",
        PreparedScalarType::Uint128 => "u128",
        PreparedScalarType::Uint256 => "String",
        PreparedScalarType::Float32 => "f32",
        PreparedScalarType::Float64 => "f64",
        PreparedScalarType::Text => "String",
        PreparedScalarType::Bool => "bool",
        PreparedScalarType::Timestamp => "u64",
        PreparedScalarType::Bytes => "Vec<u8>",
        PreparedScalarType::Date => "i32",
        PreparedScalarType::Time => "u64",
        PreparedScalarType::DateTime => "(i64, u32)",
        PreparedScalarType::Duration => "(i32, i64)",
        PreparedScalarType::Principal => "Principal",
        PreparedScalarType::Decimal => "String",
    }
}

/// Map a `PreparedValueType` to a Rust type string (Phase 2 codegen).
pub fn rs_type_for(vt: &PreparedValueType) -> String {
    match vt {
        PreparedValueType::Int8 => "i8".into(),
        PreparedValueType::Int16 => "i16".into(),
        PreparedValueType::Int32 => "i32".into(),
        PreparedValueType::Int64 => "i64".into(),
        PreparedValueType::Int128 => "i128".into(),
        PreparedValueType::Int256 => "String".into(),
        PreparedValueType::Uint8 => "u8".into(),
        PreparedValueType::Uint16 => "u16".into(),
        PreparedValueType::Uint32 => "u32".into(),
        PreparedValueType::Uint64 => "u64".into(),
        PreparedValueType::Uint128 => "u128".into(),
        PreparedValueType::Uint256 => "String".into(),
        PreparedValueType::Float32 => "f32".into(),
        PreparedValueType::Float64 => "f64".into(),
        PreparedValueType::Text => "String".into(),
        PreparedValueType::Bool => "bool".into(),
        PreparedValueType::Timestamp => "u64".into(),
        PreparedValueType::List => "Vec<Value>".into(),
        PreparedValueType::TypedList(s) => format!("Vec<{}>", rs_scalar_type(s)),
        PreparedValueType::Null => "()".into(),
        PreparedValueType::Bytes => "Vec<u8>".into(),
        PreparedValueType::Date => "i32".into(),
        PreparedValueType::Time => "u64".into(),
        PreparedValueType::DateTime => "(i64, u32)".into(),
        PreparedValueType::Duration => "(i32, i64)".into(),
        PreparedValueType::Principal => "Principal".into(),
        PreparedValueType::Decimal => "String".into(),
    }
}

/// Compute the TS type annotation for a parameter's inferred types.
/// Returns `"unknown"` when no type information is available.
pub fn ts_param_type(param: &gleaph_types::PreparedParameterInfo) -> String {
    let non_null: Vec<_> = param
        .types
        .iter()
        .filter(|t| !matches!(t, PreparedValueType::Null))
        .collect();
    if non_null.is_empty() {
        return "unknown".into();
    }
    if non_null.len() == 1 {
        return ts_type_for(non_null[0]);
    }
    // Union of multiple types
    non_null
        .iter()
        .map(|t| ts_type_for(t))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Compute the Rust type for a parameter (Phase 2 codegen).
/// Returns `None` when no concrete type is available (caller should use `Value`).
/// Delegates to `rs_param_type_ctx` without statement context (no union enum support).
#[allow(dead_code)]
pub fn rs_param_type(param: &gleaph_types::PreparedParameterInfo) -> Option<String> {
    rs_param_type_ctx(param, None)
}

/// Like `rs_param_type` but with optional statement context for union enum naming.
/// When `stmt_ident` is `Some`, multi-type params produce a union enum name.
pub fn rs_param_type_ctx(
    param: &gleaph_types::PreparedParameterInfo,
    stmt_ident: Option<&str>,
) -> Option<String> {
    let non_null: Vec<_> = param
        .types
        .iter()
        .filter(|t| !matches!(t, PreparedValueType::Null))
        .collect();
    let has_null = param
        .types
        .iter()
        .any(|t| matches!(t, PreparedValueType::Null));
    if non_null.is_empty() {
        return None; // unknown — keep as Value
    }
    let base = if non_null.len() == 1 {
        rs_type_for(non_null[0])
    } else if let Some(ident) = stmt_ident {
        // Multi-type union → enum name
        rs_union_enum_name(ident, &param.name)
    } else {
        return None; // no context for enum naming
    };
    if has_null || !param.required {
        Some(format!("Option<{base}>"))
    } else {
        Some(base)
    }
}

/// Generate the enum type name for a multi-type union parameter.
pub fn rs_union_enum_name(stmt_ident: &str, param_name: &str) -> String {
    format!(
        "{}Param{}",
        to_pascal_case(stmt_ident),
        to_pascal_case(param_name)
    )
}

/// Render union enum definitions and their `From` impls for multi-type params.
pub fn render_rs_union_enums(stmts: &[PreparedStatementInfo]) -> String {
    let mut out = String::new();
    for info in stmts {
        let ident = match sanitize_ident(&info.name) {
            Some(id) => id,
            None => continue,
        };
        for param in &info.parameters {
            let non_null: Vec<_> = param
                .types
                .iter()
                .filter(|t| !matches!(t, PreparedValueType::Null))
                .collect();
            if non_null.len() < 2 {
                continue;
            }
            let enum_name = rs_union_enum_name(&ident, &param.name);
            out.push_str(&format!(
                "/// Union type for parameter `${}` of `{}`.\n",
                param.name, info.name
            ));
            out.push_str(&format!("pub enum {enum_name} {{\n"));
            for vt in &non_null {
                let variant = rs_union_variant_name(vt);
                let ty = rs_type_for(vt);
                out.push_str(&format!("    {variant}({ty}),\n"));
            }
            out.push_str("}\n\n");
            // Into<Value> impl
            out.push_str(&format!("impl From<{enum_name}> for Value {{\n"));
            out.push_str(&format!("    fn from(v: {enum_name}) -> Self {{\n"));
            out.push_str("        match v {\n");
            for vt in &non_null {
                let variant = rs_union_variant_name(vt);
                out.push_str(&format!(
                    "            {enum_name}::{variant}(inner) => inner.into(),\n"
                ));
            }
            out.push_str("        }\n");
            out.push_str("    }\n");
            out.push_str("}\n\n");
            // Convenience From impls for each variant type
            for vt in &non_null {
                let variant = rs_union_variant_name(vt);
                let ty = rs_type_for(vt);
                out.push_str(&format!("impl From<{ty}> for {enum_name} {{\n"));
                out.push_str(&format!(
                    "    fn from(v: {ty}) -> Self {{ Self::{variant}(v) }}\n"
                ));
                out.push_str("}\n\n");
            }
        }
    }
    out
}

fn rs_union_variant_name(vt: &PreparedValueType) -> String {
    match vt {
        PreparedValueType::Int8 => "Int8".into(),
        PreparedValueType::Int16 => "Int16".into(),
        PreparedValueType::Int32 => "Int32".into(),
        PreparedValueType::Int64 => "Int64".into(),
        PreparedValueType::Int128 => "Int128".into(),
        PreparedValueType::Int256 => "Int256".into(),
        PreparedValueType::Uint8 => "Uint8".into(),
        PreparedValueType::Uint16 => "Uint16".into(),
        PreparedValueType::Uint32 => "Uint32".into(),
        PreparedValueType::Uint64 => "Uint64".into(),
        PreparedValueType::Uint128 => "Uint128".into(),
        PreparedValueType::Uint256 => "Uint256".into(),
        PreparedValueType::Float32 => "Float32".into(),
        PreparedValueType::Float64 => "Float64".into(),
        PreparedValueType::Text => "Text".into(),
        PreparedValueType::Bool => "Bool".into(),
        PreparedValueType::Timestamp => "Timestamp".into(),
        PreparedValueType::List => "List".into(),
        PreparedValueType::TypedList(s) => format!("{}List", rs_scalar_variant_name(s)),
        PreparedValueType::Null => "Null".into(),
        PreparedValueType::Bytes => "Bytes".into(),
        PreparedValueType::Date => "Date".into(),
        PreparedValueType::Time => "Time".into(),
        PreparedValueType::DateTime => "DateTime".into(),
        PreparedValueType::Duration => "Duration".into(),
        PreparedValueType::Principal => "Principal".into(),
        PreparedValueType::Decimal => "Decimal".into(),
    }
}

fn rs_scalar_variant_name(s: &gleaph_types::PreparedScalarType) -> &'static str {
    use gleaph_types::PreparedScalarType;
    match s {
        PreparedScalarType::Int8 => "Int8",
        PreparedScalarType::Int16 => "Int16",
        PreparedScalarType::Int32 => "Int32",
        PreparedScalarType::Int64 => "Int64",
        PreparedScalarType::Int128 => "Int128",
        PreparedScalarType::Int256 => "Int256",
        PreparedScalarType::Uint8 => "Uint8",
        PreparedScalarType::Uint16 => "Uint16",
        PreparedScalarType::Uint32 => "Uint32",
        PreparedScalarType::Uint64 => "Uint64",
        PreparedScalarType::Uint128 => "Uint128",
        PreparedScalarType::Uint256 => "Uint256",
        PreparedScalarType::Float32 => "Float32",
        PreparedScalarType::Float64 => "Float64",
        PreparedScalarType::Text => "Text",
        PreparedScalarType::Bool => "Bool",
        PreparedScalarType::Timestamp => "Timestamp",
        PreparedScalarType::Bytes => "Bytes",
        PreparedScalarType::Date => "Date",
        PreparedScalarType::Time => "Time",
        PreparedScalarType::DateTime => "DateTime",
        PreparedScalarType::Duration => "Duration",
        PreparedScalarType::Principal => "Principal",
        PreparedScalarType::Decimal => "Decimal",
    }
}

pub fn render_ts_param_aliases(stmts: &[PreparedStatementInfo], style: JsParamStyle) -> String {
    let mut out = String::new();
    for info in stmts {
        let ident = match sanitize_ident(&info.name) {
            Some(id) => id,
            None => continue,
        };
        let bindings = info
            .parameters
            .iter()
            .map(|parameter| {
                let api_name = match style {
                    JsParamStyle::Preserve => parameter.name.clone(),
                    JsParamStyle::Camel => to_camel_case(&parameter.name),
                };
                (parameter, api_name)
            })
            .collect::<Vec<_>>();
        if bindings.is_empty() {
            continue;
        }
        let params_type = params_type_name(&ident);
        out.push_str(&format!("export interface {params_type} {{\n"));
        for (parameter, api_name) in bindings {
            out.push_str(&format!("  /** wire param: {:?} */\n", parameter.name));
            let type_str = ts_param_type(parameter);
            out.push_str(&format!(
                "  {}{}: {};\n",
                api_name,
                if parameter.required { "" } else { "?" },
                type_str,
            ));
        }
        out.push_str("}\n\n");
    }
    out
}

pub fn render_dts_param_aliases(stmts: &[PreparedStatementInfo], style: JsParamStyle) -> String {
    let mut out = String::new();
    for info in stmts {
        let ident = match sanitize_ident(&info.name) {
            Some(id) => id,
            None => continue,
        };
        let bindings = info
            .parameters
            .iter()
            .map(|parameter| {
                let api_name = match style {
                    JsParamStyle::Preserve => parameter.name.clone(),
                    JsParamStyle::Camel => to_camel_case(&parameter.name),
                };
                (parameter, api_name)
            })
            .collect::<Vec<_>>();
        if bindings.is_empty() {
            continue;
        }
        let params_type = params_type_name(&ident);
        out.push_str(&format!("export interface {params_type} {{\n"));
        for (parameter, api_name) in bindings {
            out.push_str(&format!("  /** wire param: {:?} */\n", parameter.name));
            let type_str = ts_param_type(parameter);
            out.push_str(&format!(
                "  {}{}: {};\n",
                api_name,
                if parameter.required { "" } else { "?" },
                type_str,
            ));
        }
        out.push_str("}\n\n");
    }
    out
}

pub fn ts_method_args(
    info: &PreparedStatementInfo,
    ident: &str,
    style: JsParamStyle,
) -> Vec<String> {
    let mut args = Vec::new();
    let bindings = param_bindings(info, style);
    if !bindings.is_empty() {
        args.push(format!("params: {}", params_type_name(ident)));
    }
    if has_dynamic_sort(info) {
        args.push(format!(
            "options?: {{ sort?: {}[] }}",
            sort_spec_type_name(ident)
        ));
    }
    args
}

pub fn prepared_return_type(info: &PreparedStatementInfo) -> &'static str {
    match info.kind {
        PreparedKind::Query => "QueryResultWithContinuation",
        PreparedKind::Mutation => "MutationResult",
    }
}

pub fn prepared_graph_method(info: &PreparedStatementInfo) -> &'static str {
    match info.kind {
        PreparedKind::Query => "executePrepared",
        PreparedKind::Mutation => "executePreparedMutation",
    }
}

pub fn ts_interface_method_signature(
    info: &PreparedStatementInfo,
    ident: &str,
    style: JsParamStyle,
) -> String {
    let args = ts_method_args(info, ident, style);
    format!(
        "  {ident}({}): Promise<{}>;\n\n",
        args.join(", "),
        prepared_return_type(info)
    )
}

pub fn render_params_value(info: &PreparedStatementInfo, style: JsParamStyle) -> String {
    let required = required_param_bindings(info, style);
    let optional = optional_param_bindings(info, style);
    if required.is_empty() && optional.is_empty() {
        "{}".into()
    } else if optional.is_empty()
        && required
            .iter()
            .all(|binding| binding.api_name == binding.wire_name)
    {
        "params".into()
    } else if optional.is_empty() {
        format!(
            "{{ {} }}",
            required
                .iter()
                .map(|binding| format!("{}: params.{}", binding.wire_name, binding.api_name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        let mut parts = Vec::new();
        if !required.is_empty() {
            parts.extend(
                required
                    .iter()
                    .map(|binding| format!("{}: params.{}", binding.wire_name, binding.api_name)),
            );
        }
        let optional_exprs = optional
            .iter()
            .map(|binding| {
                format!(
                    "...(params.{0} !== undefined ? {{ {1}: params.{0} }} : {{}})",
                    binding.api_name, binding.wire_name
                )
            })
            .collect::<Vec<_>>();
        if parts.is_empty() {
            format!("{{ {} }}", optional_exprs.join(", "))
        } else {
            format!("{{ {}, {} }}", parts.join(", "), optional_exprs.join(", "))
        }
    }
}

pub fn ts_factory_method_signature(
    info: &PreparedStatementInfo,
    ident: &str,
    style: JsParamStyle,
) -> String {
    let method = prepared_graph_method(info);
    let sort_enabled = has_dynamic_sort(info);
    if info.parameters.is_empty() && !sort_enabled {
        format!(
            "    {ident}: () =>\n      graph.{method}(\"{}\", {{}})",
            info.name
        )
    } else if info.parameters.is_empty() {
        format!(
            "    {ident}: (options) =>\n      graph.{method}(\"{}\", {{}}, options?.sort)",
            info.name
        )
    } else {
        let params_value = render_params_value(info, style);
        format!(
            "    {ident}: (params{}) =>\n      graph.{method}(\"{}\", {}{})",
            if sort_enabled { ", options" } else { "" },
            info.name,
            params_value,
            if sort_enabled { ", options?.sort" } else { "" }
        )
    }
}

pub fn render_ts_sort_aliases(stmts: &[PreparedStatementInfo]) -> String {
    let mut out = String::new();
    for info in stmts {
        let ident = match sanitize_ident(&info.name) {
            Some(id) => id,
            None => continue,
        };
        if !has_dynamic_sort(info) {
            continue;
        }
        let key_type = sort_key_type_name(&ident);
        let spec_type = sort_spec_type_name(&ident);
        out.push_str(&format!("export const {key_type} = {{\n"));
        for (binding, sort) in sort_key_bindings(info)
            .iter()
            .zip(info.allowed_sorts.iter())
        {
            out.push_str(&format!(
                "  /** wire key: {:?}, expr: {:?} */\n",
                binding.wire_key, sort.expr
            ));
            out.push_str(&format!(
                "  {}: {:?},\n",
                binding.api_name, binding.wire_key
            ));
        }
        out.push_str("} as const;\n");
        out.push_str(&format!(
            "export type {key_type} = (typeof {key_type})[keyof typeof {key_type}];\n"
        ));
        out.push_str(&format!(
            "export type {spec_type} = Omit<PreparedSortSpec, \"key\"> & {{ key: {key_type} }};\n\n"
        ));
    }
    out
}

pub fn render_js_sort_objects(stmts: &[PreparedStatementInfo]) -> String {
    let mut out = String::new();
    for info in stmts {
        let ident = match sanitize_ident(&info.name) {
            Some(id) => id,
            None => continue,
        };
        if !has_dynamic_sort(info) {
            continue;
        }
        let key_type = sort_key_type_name(&ident);
        out.push_str(&format!("export const {key_type} = {{\n"));
        for (binding, sort) in sort_key_bindings(info)
            .iter()
            .zip(info.allowed_sorts.iter())
        {
            out.push_str(&format!(
                "  /** wire key: {:?}, expr: {:?} */\n",
                binding.wire_key, sort.expr
            ));
            out.push_str(&format!(
                "  {}: {:?},\n",
                binding.api_name, binding.wire_key
            ));
        }
        out.push_str("};\n\n");
    }
    out
}

pub fn render_dts_sort_aliases(stmts: &[PreparedStatementInfo]) -> String {
    let mut out = String::new();
    for info in stmts {
        let ident = match sanitize_ident(&info.name) {
            Some(id) => id,
            None => continue,
        };
        if !has_dynamic_sort(info) {
            continue;
        }
        let key_type = sort_key_type_name(&ident);
        let spec_type = sort_spec_type_name(&ident);
        out.push_str(&format!("export declare const {key_type}: {{\n"));
        for (binding, sort) in sort_key_bindings(info)
            .iter()
            .zip(info.allowed_sorts.iter())
        {
            out.push_str(&format!(
                "  /** wire key: {:?}, expr: {:?} */\n",
                binding.wire_key, sort.expr
            ));
            out.push_str(&format!(
                "  readonly {}: {:?};\n",
                binding.api_name, binding.wire_key
            ));
        }
        out.push_str("};\n");
        out.push_str(&format!(
            "export type {key_type} = (typeof {key_type})[keyof typeof {key_type}];\n"
        ));
        out.push_str(&format!(
            "export type {spec_type} = Omit<PreparedSortSpec, \"key\"> & {{ key: {key_type} }};\n\n"
        ));
    }
    out
}

/// Format GQL source as indented doc comment lines.
pub fn format_gql_doc(source: &str, comment_prefix: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("{comment_prefix} ```gql\n"));
    for line in source.lines() {
        out.push_str(&format!("{comment_prefix} {}\n", line.trim()));
    }
    out.push_str(&format!("{comment_prefix} ```\n"));
    out
}

/// Build a JSDoc/doc-comment body from statement info.
pub fn doc_parts(info: &PreparedStatementInfo) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(description) = &info.description
        && !description.trim().is_empty()
    {
        parts.push(description.trim().to_string());
    }
    if !info.columns.is_empty() {
        parts.push(format!("Columns: {}", info.columns.join(", ")));
    }
    // Include parameter type info in docs
    let typed_params: Vec<_> = info
        .parameters
        .iter()
        .filter(|p| !p.types.is_empty())
        .collect();
    if !typed_params.is_empty() {
        let param_descs: Vec<String> = typed_params
            .iter()
            .map(|p| {
                let type_names: Vec<String> = p
                    .types
                    .iter()
                    .map(|t| match t {
                        PreparedValueType::Int8 => "INT8".into(),
                        PreparedValueType::Int16 => "INT16".into(),
                        PreparedValueType::Int32 => "INT32".into(),
                        PreparedValueType::Int64 => "INT64".into(),
                        PreparedValueType::Int128 => "INT128".into(),
                        PreparedValueType::Int256 => "INT256".into(),
                        PreparedValueType::Uint8 => "UINT8".into(),
                        PreparedValueType::Uint16 => "UINT16".into(),
                        PreparedValueType::Uint32 => "UINT32".into(),
                        PreparedValueType::Uint64 => "UINT64".into(),
                        PreparedValueType::Uint128 => "UINT128".into(),
                        PreparedValueType::Uint256 => "UINT256".into(),
                        PreparedValueType::Float32 => "FLOAT32".into(),
                        PreparedValueType::Float64 => "FLOAT64".into(),
                        PreparedValueType::Text => "TEXT".into(),
                        PreparedValueType::Bool => "BOOL".into(),
                        PreparedValueType::Timestamp => "TIMESTAMP".into(),
                        PreparedValueType::List => "LIST".into(),
                        PreparedValueType::TypedList(s) => {
                            format!("LIST<{}>", prepared_scalar_type_name(s))
                        }
                        PreparedValueType::Null => "NULL".into(),
                        PreparedValueType::Bytes => "BYTES".into(),
                        PreparedValueType::Date => "DATE".into(),
                        PreparedValueType::Time => "TIME".into(),
                        PreparedValueType::DateTime => "DATETIME".into(),
                        PreparedValueType::Duration => "DURATION".into(),
                        PreparedValueType::Principal => "PRINCIPAL".into(),
                        PreparedValueType::Decimal => "DECIMAL".into(),
                    })
                    .collect();
                let suffix = if p.inferred { " (inferred)" } else { "" };
                format!("${}: {}{}", p.name, type_names.join(" | "), suffix)
            })
            .collect();
        parts.push(format!("Parameters: {}", param_descs.join(", ")));
    }
    if !info.allowed_sorts.is_empty() {
        parts.push(format!(
            "Allowed sorts: {}",
            info.allowed_sorts
                .iter()
                .map(|sort| format!("{} ({})", sort.key, sort.expr))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(default_sort) = &info.default_sort {
        if !default_sort.is_empty() {
            parts.push(format!(
                "Default sort: {}",
                default_sort
                    .iter()
                    .map(|sort| {
                        let dir = if sort.descending { "DESC" } else { "ASC" };
                        match sort.nulls_first {
                            Some(true) => format!("{} {} NULLS FIRST", sort.key, dir),
                            Some(false) => format!("{} {} NULLS LAST", sort.key, dir),
                            None => format!("{} {}", sort.key, dir),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if info.requires_caller {
        parts.push("Uses caller() — requires authenticated identity.".into());
    }
    parts
}
