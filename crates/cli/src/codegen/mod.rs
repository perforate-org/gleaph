//! Prepared-query client code generation (JSON metadata → TS / JS / Rust).

pub mod rust_lang;
pub mod typescript;

use gleaph_graph::PreparedParameterInfo;
use gleaph_graph::PreparedQueryInfo;
use gleaph_graph::PreparedQueryKind;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

/// Target language for code generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    TypeScript,
    /// `.js` + `.d.ts` (types follow the same emission as TypeScript).
    JavaScript,
    Rust,
}

impl Lang {
    pub fn default_filename(&self) -> &'static str {
        match self {
            Self::TypeScript => "gleaph.prepared.ts",
            Self::JavaScript => "gleaph.prepared.js",
            Self::Rust => "gleaph_prepared.rs",
        }
    }

    /// For [`Lang::JavaScript`], path to write beside the `.js` file (same stem, `.d.ts`).
    pub fn js_declaration_path(js_path: &Path) -> PathBuf {
        js_path.with_extension("d.ts")
    }
}

impl FromStr for Lang {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ts" | "typescript" => Ok(Self::TypeScript),
            "js" | "javascript" => Ok(Self::JavaScript),
            "rust" | "rs" => Ok(Self::Rust),
            _ => Err("expected one of: ts, js, javascript, rust"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsParamStyle {
    Preserve,
    Camel,
}

/// Load prepared metadata from JSON: either `[...]` or `{ "statements": [...] }` (list API shape).
pub fn load_prepared_queries_from_json(json: &str) -> anyhow::Result<Vec<PreparedQueryInfo>> {
    use anyhow::Context;
    let root: serde_json::Value =
        serde_json::from_str(json).context("parse JSON: expected object or array")?;
    if let Some(arr) = root.as_array() {
        return serde_json::from_value(serde_json::Value::Array(arr.clone()))
            .context("deserialize prepared query array");
    }
    #[derive(serde::Deserialize)]
    struct List {
        statements: Vec<PreparedQueryInfo>,
    }
    let list: List =
        serde_json::from_value(root).context("expected `.statements` array or top-level array")?;
    Ok(list.statements)
}

/// Parse JSON file and return prepared definitions.
pub fn load_prepared_queries_from_path(path: &Path) -> anyhow::Result<Vec<PreparedQueryInfo>> {
    let text = std::fs::read_to_string(path)?;
    load_prepared_queries_from_json(&text)
}

pub fn write_codegen_output(path: &Path, content: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn resolve_output_path(lang: Lang, output: Option<&PathBuf>) -> PathBuf {
    match output {
        Some(p) => {
            if matches!(lang, Lang::JavaScript) {
                match p.extension().and_then(|e| e.to_str()) {
                    Some("js") => p.clone(),
                    _ => p.with_extension("js"),
                }
            } else {
                p.clone()
            }
        }
        None => PathBuf::from(lang.default_filename()),
    }
}

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

pub fn sort_key_bindings(info: &PreparedQueryInfo) -> Vec<SortKeyBinding> {
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

pub fn has_dynamic_sort(info: &PreparedQueryInfo) -> bool {
    !info.allowed_sorts.is_empty() && matches!(info.kind, PreparedQueryKind::Query)
}

pub fn needs_prepared_sort_import(stmts: &[PreparedQueryInfo]) -> bool {
    stmts.iter().any(has_dynamic_sort)
}

pub fn sort_key_type_name(ident: &str) -> String {
    format!("{}SortKey", to_pascal_case(ident))
}

pub fn sort_spec_type_name(ident: &str) -> String {
    format!("{}SortSpec", to_pascal_case(ident))
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

pub fn param_bindings(info: &PreparedQueryInfo, style: JsParamStyle) -> Vec<ParamBinding> {
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

pub fn required_param_bindings(info: &PreparedQueryInfo, style: JsParamStyle) -> Vec<ParamBinding> {
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

pub fn optional_param_bindings(info: &PreparedQueryInfo, style: JsParamStyle) -> Vec<ParamBinding> {
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

pub fn params_type_name(ident: &str) -> String {
    format!("{}Params", to_pascal_case(ident))
}

pub fn needs_principal_import(stmts: &[PreparedQueryInfo]) -> bool {
    stmts.iter().any(|s| {
        s.parameters
            .iter()
            .any(|p| p.type_hints.iter().any(|h| is_principal_hint(h)))
    })
}

fn is_principal_hint(h: &str) -> bool {
    matches!(h, "ic.Principal" | "PRINCIPAL")
        || h.ends_with("::Principal")
        || h.eq_ignore_ascii_case("principal")
}

fn normalized_hints(hints: &[String]) -> Vec<String> {
    hints
        .iter()
        .filter(|h| !h.eq_ignore_ascii_case("null"))
        .cloned()
        .collect()
}

fn ts_scalar_from_hint(h: &str) -> Option<&'static str> {
    match h {
        "Int8" | "Int16" | "Int32" => Some("number"),
        "Int64" | "Int128" => Some("bigint"),
        "Int256" => Some("string"),
        "Uint8" | "Uint16" | "Uint32" => Some("number"),
        "Uint64" | "Uint128" => Some("bigint"),
        "Uint256" => Some("string"),
        "Float16" | "Float32" | "Float64" => Some("number"),
        "Text" => Some("string"),
        "Bool" => Some("boolean"),
        "Bytes" => Some("Uint8Array"),
        "Date" => Some("number"),
        "Time" | "LocalTime" => Some("bigint"),
        "DateTime" | "LocalDateTime" => Some("[bigint, number]"),
        "ZonedDateTime" => Some("[bigint, number, number]"),
        "ZonedTime" => Some("[bigint, number]"),
        "Duration" => Some("[number, bigint]"),
        "Decimal" => Some("string"),
        "Null" => Some("null"),
        "List" => Some("unknown[]"),
        "Path" | "Record" => Some("unknown"),
        _ if is_principal_hint(h) => Some("Principal"),
        _ => None,
    }
}

fn ts_single_hint(h: &str) -> String {
    ts_scalar_from_hint(h)
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn ts_param_type(param: &PreparedParameterInfo) -> String {
    let hints = normalized_hints(&param.type_hints);
    if hints.is_empty() {
        return "unknown".into();
    }
    if hints.len() == 1 {
        return ts_single_hint(&hints[0]);
    }
    hints
        .iter()
        .map(|h| ts_single_hint(h))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn rs_scalar_from_hint(h: &str) -> Option<&'static str> {
    match h {
        "Int8" => Some("i8"),
        "Int16" => Some("i16"),
        "Int32" => Some("i32"),
        "Int64" => Some("i64"),
        "Int128" => Some("i128"),
        "Int256" => Some("String"),
        "Uint8" => Some("u8"),
        "Uint16" => Some("u16"),
        "Uint32" => Some("u32"),
        "Uint64" => Some("u64"),
        "Uint128" => Some("u128"),
        "Uint256" => Some("String"),
        "Float16" => Some("f32"),
        "Float32" => Some("f32"),
        "Float64" => Some("f64"),
        "Text" => Some("String"),
        "Bool" => Some("bool"),
        "Bytes" => Some("Vec<u8>"),
        "Date" => Some("i32"),
        "Time" | "LocalTime" => Some("u64"),
        "DateTime" | "LocalDateTime" => Some("(i64, u32)"),
        "ZonedDateTime" => None,
        "ZonedTime" => None,
        "Duration" => Some("(i32, i64)"),
        "Decimal" => Some("String"),
        "Null" => Some("()"),
        "List" => Some("Vec<serde_json::Value>"),
        "Path" | "Record" => None,
        _ if is_principal_hint(h) => Some("candid::Principal"),
        _ => None,
    }
}

fn rs_type_for_hint_list(hints: &[String]) -> Option<String> {
    let non_null = normalized_hints(hints);
    if non_null.len() == 1 {
        return rs_scalar_from_hint(&non_null[0]).map(str::to_owned);
    }
    None
}

pub fn rs_union_enum_name(stmt_ident: &str, param_name: &str) -> String {
    format!(
        "{}Param{}",
        to_pascal_case(stmt_ident),
        to_pascal_case(param_name)
    )
}

pub fn rs_param_type_ctx(
    param: &PreparedParameterInfo,
    stmt_ident: Option<&str>,
) -> Option<String> {
    let non_null = normalized_hints(&param.type_hints);
    let has_null = param
        .type_hints
        .iter()
        .any(|h| h.eq_ignore_ascii_case("null"));
    if non_null.is_empty() {
        return None;
    }
    let base = if non_null.len() == 1 {
        rs_type_for_hint_list(&param.type_hints)?
    } else if let Some(ident) = stmt_ident {
        rs_union_enum_name(ident, &param.name)
    } else {
        return None;
    };
    if has_null || !param.required {
        Some(format!("Option<{base}>"))
    } else {
        Some(base)
    }
}

fn hint_variant_entries(hints: &[String]) -> Vec<(String, String)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    hints
        .iter()
        .map(|h| {
            let mut base = to_pascal_case(h);
            if base.is_empty() {
                base = "Hint".into();
            }
            if base.starts_with(|c: char| c.is_ascii_digit()) {
                base = format!("K{base}");
            }
            let n = counts.entry(base.clone()).or_insert(0);
            *n += 1;
            let vname = if *n == 1 { base } else { format!("{base}{n}") };
            (h.clone(), vname)
        })
        .collect()
}

/// JSON for prepared params / `ApiValue` (Gleaph HTTP API), inlined so generated clients need no `gleaph` / `gleaph-gql`.
pub fn render_rust_wire_module(include_principal: bool) -> String {
    let mut s = String::from(
        r#"mod gleaph_prepared_wire {
    use serde_json::{json, Value};

    pub fn null() -> Value {
        Value::Null
    }

    pub fn int64(v: i64) -> Value {
        json!({"Int64": v})
    }

    pub fn int128(v: i128) -> Value {
        json!({"Int128": v})
    }

    pub fn int256_string(s: &str) -> Value {
        json!({"Int256": s})
    }

    pub fn uint64(v: u64) -> Value {
        json!({"Uint64": v})
    }

    pub fn uint128(v: u128) -> Value {
        json!({"Uint128": v})
    }

    pub fn uint256_string(s: &str) -> Value {
        json!({"Uint256": s})
    }

    pub fn float64(v: f64) -> Value {
        json!({"Float64": v})
    }

    pub fn text(s: &str) -> Value {
        json!({"Text": s})
    }

    pub fn bool_(v: bool) -> Value {
        json!({"Bool": v})
    }

    pub fn bytes(b: Vec<u8>) -> Value {
        json!({"Bytes": b})
    }

    pub fn date(v: i32) -> Value {
        json!({"Date": v})
    }

    pub fn time(v: u64) -> Value {
        json!({"Time": v})
    }

    pub fn local_time(v: u64) -> Value {
        json!({"LocalTime": v})
    }

    pub fn datetime(seconds: i64, nanos: u32) -> Value {
        json!({"DateTime": {"seconds": seconds, "nanos": nanos}})
    }

    pub fn local_datetime(seconds: i64, nanos: u32) -> Value {
        json!({"LocalDateTime": {"seconds": seconds, "nanos": nanos}})
    }

    pub fn duration(months: i32, nanos: i64) -> Value {
        json!({"Duration": {"months": months, "nanos": nanos}})
    }

    pub fn decimal_string(s: &str) -> Value {
        json!({"Decimal": s})
    }

    pub fn list(items: Vec<Value>) -> Value {
        json!({"List": items})
    }

"#,
    );
    if include_principal {
        s.push_str(
            r#"    pub fn principal(p: &candid::Principal) -> Value {
        json!({"Principal": p})
    }

"#,
        );
    }
    s.push_str("}\n\n");
    s
}

/// `serde_json::Value` expression; `access` is `params.field` or `v`.
pub(crate) fn rust_single_hint_wire_expr(hint: &str, access: &str) -> Option<String> {
    let a = access;
    if is_principal_hint(hint) {
        return Some(format!("gleaph_prepared_wire::principal(&{a})"));
    }
    Some(match hint {
        "Int8" | "Int16" | "Int32" => format!("gleaph_prepared_wire::int64({a} as i64)"),
        "Int64" => format!("gleaph_prepared_wire::int64({a})"),
        "Int128" => format!("gleaph_prepared_wire::int128({a})"),
        "Int256" => format!("gleaph_prepared_wire::int256_string({a}.as_str())"),
        "Uint8" | "Uint16" | "Uint32" => format!("gleaph_prepared_wire::uint64({a} as u64)"),
        "Uint64" => format!("gleaph_prepared_wire::uint64({a})"),
        "Uint128" => format!("gleaph_prepared_wire::uint128({a})"),
        "Uint256" => format!("gleaph_prepared_wire::uint256_string({a}.as_str())"),
        "Float16" | "Float32" => format!("gleaph_prepared_wire::float64({a} as f64)"),
        "Float64" => format!("gleaph_prepared_wire::float64({a})"),
        "Text" => format!("gleaph_prepared_wire::text({a}.as_str())"),
        "Bool" => format!("gleaph_prepared_wire::bool_({a})"),
        "Bytes" => format!("gleaph_prepared_wire::bytes({a}.clone())"),
        "Date" => format!("gleaph_prepared_wire::date({a})"),
        "Time" => format!("gleaph_prepared_wire::time({a})"),
        "LocalTime" => format!("gleaph_prepared_wire::local_time({a})"),
        "DateTime" => format!("gleaph_prepared_wire::datetime({a}.0, {a}.1)"),
        "LocalDateTime" => format!("gleaph_prepared_wire::local_datetime({a}.0, {a}.1)"),
        "Duration" => format!("gleaph_prepared_wire::duration({a}.0, {a}.1)"),
        "Decimal" => format!("gleaph_prepared_wire::decimal_string({a}.as_str())"),
        "List" => format!("gleaph_prepared_wire::list({a}.clone())"),
        "Null" => "gleaph_prepared_wire::null()".to_string(),
        _ => return None,
    })
}

fn rust_param_map_entry(
    param: &PreparedParameterInfo,
    binding: &ParamBinding,
    _stmt_ident: &str,
    optional: bool,
) -> String {
    let w = &binding.wire_name;
    let b = &binding.api_name;
    let nn = normalized_hints(&param.type_hints);
    let union_ok = nn.len() >= 2 && nn.iter().all(|h| rs_scalar_from_hint(h).is_some());
    let principal_only = nn.len() == 1 && is_principal_hint(&nn[0]);

    if optional {
        if principal_only {
            return format!(
                r#"params.{b}.map(|v| ("{w}".into(), gleaph_prepared_wire::principal(&v)))"#
            );
        }
        if union_ok {
            return format!(r#"params.{b}.map(|v| ("{w}".into(), serde_json::Value::from(v)))"#);
        }
        if nn.len() == 1
            && let Some(expr) = rust_single_hint_wire_expr(&nn[0], "v")
        {
            return format!(r#"params.{b}.map(|v| ("{w}".into(), {expr}))"#);
        }
        return format!(r#"params.{b}.clone().map(|v| ("{w}".into(), v))"#);
    }

    if principal_only {
        return format!(r#"("{w}".into(), gleaph_prepared_wire::principal(&params.{b}))"#);
    }
    if union_ok {
        return format!(r#"("{w}".into(), serde_json::Value::from(params.{b}))"#);
    }
    if nn.len() == 1
        && let Some(expr) = rust_single_hint_wire_expr(&nn[0], &format!("params.{b}"))
    {
        return format!(r#"("{w}".into(), {expr})"#);
    }
    format!(r#"("{w}".into(), params.{b})"#)
}

pub fn render_rs_union_enums(stmts: &[PreparedQueryInfo]) -> String {
    let mut out = String::new();
    for info in stmts {
        let ident = match sanitize_ident(&info.name) {
            Some(id) => id,
            None => continue,
        };
        for param in &info.parameters {
            let non_null = normalized_hints(&param.type_hints);
            if non_null.len() < 2 {
                continue;
            }
            if non_null.iter().any(|h| rs_scalar_from_hint(h).is_none()) {
                continue;
            }
            let enum_name = rs_union_enum_name(&ident, &param.name);
            let entries = hint_variant_entries(&non_null);
            out.push_str(&format!(
                "/// Union type for parameter `${}` of `{}`.\n",
                param.name, info.name
            ));
            out.push_str(&format!("pub enum {enum_name} {{\n"));
            for (hint, vname) in &entries {
                let ty = rs_scalar_from_hint(hint).expect("filtered above");
                out.push_str(&format!("    {vname}({ty}),\n"));
            }
            out.push_str("}\n\n");
            out.push_str(&format!(
                "impl From<{enum_name}> for serde_json::Value {{\n"
            ));
            out.push_str(&format!("    fn from(v: {enum_name}) -> Self {{\n"));
            out.push_str("        match v {\n");
            for (hint, vname) in &entries {
                let rhs =
                    rust_single_hint_wire_expr(hint, "inner").expect("union hints map to wire");
                out.push_str(&format!(
                    "            {enum_name}::{vname}(inner) => {rhs},\n"
                ));
            }
            out.push_str("        }\n");
            out.push_str("    }\n");
            out.push_str("}\n\n");
            for (hint, vname) in &entries {
                let ty = rs_scalar_from_hint(hint).expect("filtered");
                out.push_str(&format!("impl From<{ty}> for {enum_name} {{\n"));
                out.push_str(&format!(
                    "    fn from(v: {ty}) -> Self {{ Self::{vname}(v) }}\n"
                ));
                out.push_str("}\n\n");
            }
        }
    }
    out
}

pub fn render_ts_param_aliases(stmts: &[PreparedQueryInfo], style: JsParamStyle) -> String {
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

pub fn ts_method_args(info: &PreparedQueryInfo, ident: &str, _style: JsParamStyle) -> Vec<String> {
    let mut args = Vec::new();
    if !info.parameters.is_empty() {
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

pub fn doc_parts(info: &PreparedQueryInfo) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(description) = &info.description {
        let t = description.trim();
        if !t.is_empty() {
            parts.push(t.to_string());
        }
    }
    if !info.columns.is_empty() {
        parts.push(format!(
            "Columns: {}",
            info.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let typed_params: Vec<_> = info
        .parameters
        .iter()
        .filter(|p| !p.type_hints.is_empty())
        .collect();
    if !typed_params.is_empty() {
        let param_descs: Vec<String> = typed_params
            .iter()
            .map(|p| {
                let suffix = if p.inferred { " (inferred)" } else { "" };
                format!("${}: [{}]{}", p.name, p.type_hints.join(", "), suffix)
            })
            .collect();
        parts.push(format!("Parameters: {}", param_descs.join(", ")));
    }
    if !info.type_warnings.is_empty() {
        parts.push(format!(
            "Type diagnostics: {} warning(s)",
            info.type_warnings.len()
        ));
    }
    if info.requires_caller {
        parts.push("Uses caller() — requires authenticated identity.".into());
    }
    parts
}

pub fn render_ts_sort_aliases(stmts: &[PreparedQueryInfo]) -> String {
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

pub fn format_gql_doc(source: &str, comment_prefix: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("{comment_prefix} ```gql\n"));
    for line in source.lines() {
        out.push_str(&format!("{comment_prefix} {}\n", line.trim_end()));
    }
    out.push_str(&format!("{comment_prefix} ```\n"));
    out
}

pub fn render_params_value(info: &PreparedQueryInfo, style: JsParamStyle) -> String {
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
    info: &PreparedQueryInfo,
    ident: &str,
    style: JsParamStyle,
) -> String {
    let method = match info.kind {
        PreparedQueryKind::Query => "executePrepared",
        PreparedQueryKind::Update => "executePreparedMutation",
    };
    let sort_on = has_dynamic_sort(info);
    if info.parameters.is_empty() && !sort_on {
        format!(
            "    {ident}: () =>\n      graph.{method}(\"{}\", {{}})",
            info.name
        )
    } else if info.parameters.is_empty() && sort_on {
        format!(
            "    {ident}: (options) =>\n      graph.{method}(\"{}\", {{}}, options?.sort)",
            info.name
        )
    } else {
        let params_value = render_params_value(info, style);
        format!(
            "    {ident}: (params{}) =>\n      graph.{method}(\"{}\", {}{})",
            if sort_on { ", options" } else { "" },
            info.name,
            params_value,
            if sort_on { ", options?.sort" } else { "" }
        )
    }
}

pub fn rust_params_to_btree_map_expr(info: &PreparedQueryInfo, stmt_ident: &str) -> String {
    let required = required_param_bindings(info, JsParamStyle::Preserve);
    let optional = optional_param_bindings(info, JsParamStyle::Preserve);
    let param_by_name: std::collections::HashMap<&str, &PreparedParameterInfo> = info
        .parameters
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();

    if info.parameters.is_empty() {
        return "std::collections::BTreeMap::new()".to_string();
    }

    let make_entry = |binding: &ParamBinding, is_optional: bool| -> String {
        let param = param_by_name[binding.wire_name.as_str()];
        rust_param_map_entry(param, binding, stmt_ident, is_optional)
    };

    if optional.is_empty() {
        let entries: Vec<_> = required.iter().map(|b| make_entry(b, false)).collect();
        format!("[{}].into_iter().collect()", entries.join(", "))
    } else {
        let req: Vec<_> = required.iter().map(|b| make_entry(b, false)).collect();
        let opt: Vec<_> = optional.iter().map(|b| make_entry(b, true)).collect();
        format!(
            "{{ let mut m: std::collections::BTreeMap<String, serde_json::Value> = [{}].into_iter().collect(); for (k, v) in [{}].into_iter().flatten() {{ m.insert(k, v); }} m }}",
            req.join(", "),
            opt.join(", ")
        )
    }
}

pub fn ts_interface_method_signature(
    info: &PreparedQueryInfo,
    ident: &str,
    style: JsParamStyle,
) -> String {
    let args = ts_method_args(info, ident, style);
    format!(
        "  {ident}({}): Promise<GleaphPreparedExecuteResult>;\n\n",
        args.join(", ")
    )
}

#[cfg(test)]
mod load_tests {
    use super::load_prepared_queries_from_json;

    const MINIMAL: &str = r#"{
        "name": "q1",
        "kind": "Query",
        "requires_caller": false,
        "extension_types": [],
        "source": "RETURN 1",
        "columns": [],
        "parameters": [],
        "type_warnings": [],
        "explain": "",
        "summary": {
            "estimated_rows": null,
            "estimated_cost": null,
            "has_dml": false,
            "dml_error_count": 0,
            "dml_warning_count": 0,
            "type_warning_count": 0
        }
    }"#;

    #[test]
    fn json_top_level_array() {
        let json = format!("[{MINIMAL}]");
        let stmts = load_prepared_queries_from_json(&json).expect("array");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].name, "q1");
    }

    #[test]
    fn json_statements_object() {
        let json = format!(r#"{{ "statements": [{MINIMAL}] }}"#);
        let stmts = load_prepared_queries_from_json(&json).expect("statements");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].name, "q1");
    }
}
