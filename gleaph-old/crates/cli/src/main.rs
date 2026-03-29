mod agent;
mod codegen;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use candid::Principal;
use clap::{Parser, Subcommand, ValueEnum};

use codegen::{JsParamStyle, Lang};

const DEFAULT_IC_HOST: &str = "https://icp-api.io";
const LOCAL_HOST: &str = "http://127.0.0.1:4943";

#[derive(Parser)]
#[command(name = "gleaph", about = "Gleaph graph database CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate type-safe client code from prepared statements.
    Codegen {
        /// Graph canister principal.
        #[arg(short = 'c', long = "canister", alias = "canister-id")]
        canister: String,

        /// Target language: js (default), ts, rust.
        /// Can be specified multiple times for multi-language output.
        #[arg(long = "lang", default_value = "js")]
        lang: Vec<String>,

        /// Output file or directory.
        /// Defaults to the language's standard filename in the current directory.
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,

        /// Parameter naming style for generated JS/TS APIs.
        #[arg(long = "js-param-style", value_enum, default_value = "camel")]
        js_param_style: CliJsParamStyle,

        /// Use local replica (http://127.0.0.1:4943).
        #[arg(long)]
        local: bool,

        /// Custom IC host URL.
        #[arg(long)]
        host: Option<String>,
    },

    /// List prepared statements on a canister.
    List {
        /// Graph canister principal.
        #[arg(short = 'c', long = "canister", alias = "canister-id")]
        canister: String,

        /// Use local replica.
        #[arg(long)]
        local: bool,

        /// Custom IC host URL.
        #[arg(long)]
        host: Option<String>,
    },

    /// Execute a read-only GQL query, or print its planner explanation.
    Query {
        /// Graph canister principal.
        #[arg(short = 'c', long = "canister", alias = "canister-id")]
        canister: String,

        /// GQL source text.
        gql: String,

        /// Return planner/semantic explain lines instead of executing the query.
        #[arg(long)]
        explain: bool,

        /// Use local replica.
        #[arg(long)]
        local: bool,

        /// Custom IC host URL.
        #[arg(long)]
        host: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliJsParamStyle {
    Preserve,
    Camel,
}

impl From<CliJsParamStyle> for JsParamStyle {
    fn from(value: CliJsParamStyle) -> Self {
        match value {
            CliJsParamStyle::Preserve => JsParamStyle::Preserve,
            CliJsParamStyle::Camel => JsParamStyle::Camel,
        }
    }
}

fn resolve_host(local: bool, host: &Option<String>) -> &str {
    if let Some(h) = host {
        h.as_str()
    } else if local {
        LOCAL_HOST
    } else {
        DEFAULT_IC_HOST
    }
}

fn parse_langs(lang_args: &[String]) -> Result<Vec<Lang>> {
    let mut langs = Vec::new();
    for s in lang_args {
        match Lang::from_str(s) {
            Some(l) => {
                if !langs.contains(&l) {
                    langs.push(l);
                }
            }
            None => bail!("unknown language: {s} (expected: js, ts, rust)"),
        }
    }
    Ok(langs)
}

fn write_output(path: &PathBuf, content: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("failed to write: {}", path.display()))?;
    eprintln!("  wrote {}", path.display());
    Ok(())
}

fn summarize_allowed_sorts(info: &gleaph_types::PreparedStatementInfo) -> String {
    if info.allowed_sorts.is_empty() {
        "(none)".into()
    } else {
        info.allowed_sorts
            .iter()
            .map(|sort| format!("{} ({})", sort.key, sort.expr))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn summarize_default_sort(info: &gleaph_types::PreparedStatementInfo) -> String {
    match &info.default_sort {
        Some(sorts) if !sorts.is_empty() => sorts
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
            .join(", "),
        _ => "(none)".into(),
    }
}

fn format_value(value: &gleaph_types::Value) -> String {
    match value {
        gleaph_types::Value::Null => "null".into(),
        gleaph_types::Value::Bool(v) => v.to_string(),
        gleaph_types::Value::Int8(v) => v.to_string(),
        gleaph_types::Value::Int16(v) => v.to_string(),
        gleaph_types::Value::Int32(v) => v.to_string(),
        gleaph_types::Value::Int64(v) => v.to_string(),
        gleaph_types::Value::Int128(v) => v.to_string(),
        gleaph_types::Value::Int256(v) => v.0.to_string(),
        gleaph_types::Value::Uint8(v) => v.to_string(),
        gleaph_types::Value::Uint16(v) => v.to_string(),
        gleaph_types::Value::Uint32(v) => v.to_string(),
        gleaph_types::Value::Uint64(v) => v.to_string(),
        gleaph_types::Value::Uint128(v) => v.to_string(),
        gleaph_types::Value::Uint256(v) => v.0.to_string(),
        gleaph_types::Value::Float32(v) => v.to_string(),
        gleaph_types::Value::Float64(v) => v.to_string(),
        gleaph_types::Value::Text(v) => v.clone(),
        gleaph_types::Value::Timestamp(v) => v.to_string(),
        gleaph_types::Value::List(v) => format!(
            "[{}]",
            v.iter().map(format_value).collect::<Vec<_>>().join(", ")
        ),
        gleaph_types::Value::Path(v) => format!("{v:?}"),
        gleaph_types::Value::Bytes(v) => format!("{v:?}"),
        gleaph_types::Value::Date(v) => v.to_string(),
        gleaph_types::Value::Time(v) => v.to_string(),
        gleaph_types::Value::DateTime(a, b) => format!("({a}, {b})"),
        gleaph_types::Value::Duration(a, b) => format!("({a}, {b})"),
        gleaph_types::Value::Principal(v) => v.to_text(),
        gleaph_types::Value::Decimal(v) => v.to_string(),
    }
}

fn print_query_result(result: &gleaph_types::QueryResult) {
    if result.columns.is_empty() {
        print_type_diagnostics(&result.warnings);
        return;
    }
    println!("{}", result.columns.join("\t"));
    for row in &result.rows {
        println!(
            "{}",
            row.iter().map(format_value).collect::<Vec<_>>().join("\t")
        );
    }
    print_type_diagnostics(&result.warnings);
}

fn format_type_diagnostic_kind(kind: gleaph_types::TypeDiagnosticKind) -> &'static str {
    match kind {
        gleaph_types::TypeDiagnosticKind::Info => "Info",
        gleaph_types::TypeDiagnosticKind::BinaryOpMismatch => "BinaryOpMismatch",
        gleaph_types::TypeDiagnosticKind::NonBooleanCondition => "NonBooleanCondition",
        gleaph_types::TypeDiagnosticKind::FunctionArgMismatch => "FunctionArgMismatch",
        gleaph_types::TypeDiagnosticKind::ComparisonMismatch => "ComparisonMismatch",
        gleaph_types::TypeDiagnosticKind::NullCheckOnNonNull => "NullCheckOnNonNull",
        gleaph_types::TypeDiagnosticKind::ImpossiblePattern => "ImpossiblePattern",
        gleaph_types::TypeDiagnosticKind::GroupingViolation => "GroupingViolation",
        gleaph_types::TypeDiagnosticKind::ParameterInferenceConflict => {
            "ParameterInferenceConflict"
        }
    }
}

fn print_type_diagnostics(warnings: &[gleaph_types::TypeDiagnostic]) {
    for warning in warnings {
        eprintln!(
            "warning [{}] {}",
            format_type_diagnostic_kind(warning.kind),
            warning.message
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Codegen {
            canister,
            lang,
            output,
            js_param_style,
            local,
            host,
        } => {
            let canister_id = Principal::from_text(&canister)
                .with_context(|| format!("invalid canister principal: {canister}"))?;
            let ic_host = resolve_host(local, &host);
            let langs = parse_langs(&lang)?;

            // Multi-lang with a file path → error
            if langs.len() > 1
                && let Some(ref p) = output
                && p.extension().is_some()
            {
                bail!(
                    "-o must be a directory when generating multiple languages, got: {}",
                    p.display()
                );
            }

            eprintln!("fetching prepared statements from {canister}...");
            let stmts = agent::fetch_prepared_statements(canister_id, ic_host, local).await?;

            if stmts.is_empty() {
                eprintln!("warning: no prepared statements found");
            } else {
                eprintln!("found {} prepared statement(s)", stmts.len());
            }
            let js_param_style = JsParamStyle::from(js_param_style);

            for l in &langs {
                let canister_str = canister.as_str();
                match l {
                    Lang::JavaScript => {
                        let js =
                            codegen::javascript::generate_js(&stmts, canister_str, js_param_style);
                        let dts =
                            codegen::javascript::generate_dts(&stmts, canister_str, js_param_style);
                        let js_path = match &output {
                            Some(p) if langs.len() == 1 => p.clone(),
                            Some(dir) => dir.join(l.default_filename()),
                            None => PathBuf::from(l.default_filename()),
                        };
                        let dts_path = js_path.with_extension("d.ts");
                        write_output(&js_path, &js)?;
                        write_output(&dts_path, &dts)?;
                    }
                    Lang::TypeScript => {
                        let ts =
                            codegen::typescript::generate_ts(&stmts, canister_str, js_param_style);
                        let path = match &output {
                            Some(p) if langs.len() == 1 => p.clone(),
                            Some(dir) => dir.join(l.default_filename()),
                            None => PathBuf::from(l.default_filename()),
                        };
                        write_output(&path, &ts)?;
                    }
                    Lang::Rust => {
                        let rs = codegen::rust_lang::generate_rs(&stmts, canister_str);
                        let path = match &output {
                            Some(p) if langs.len() == 1 => p.clone(),
                            Some(dir) => dir.join(l.default_filename()),
                            None => PathBuf::from(l.default_filename()),
                        };
                        write_output(&path, &rs)?;
                    }
                }
            }

            eprintln!("done.");
        }

        Command::List {
            canister,
            local,
            host,
        } => {
            let canister_id = Principal::from_text(&canister)
                .with_context(|| format!("invalid canister principal: {canister}"))?;
            let ic_host = resolve_host(local, &host);

            let stmts = agent::fetch_prepared_statements(canister_id, ic_host, local).await?;

            if stmts.is_empty() {
                println!("No prepared statements.");
                return Ok(());
            }

            println!(
                "{:<24} {:<10} {:<8} {:<10} {:<24} {:<24} {:<28} {}",
                "NAME",
                "KIND",
                "CALLER",
                "WARNINGS",
                "PARAMETERS",
                "COLUMNS",
                "SORTS",
                "DEFAULT SORT"
            );
            println!("{}", "-".repeat(176));
            for info in &stmts {
                let kind = match info.kind {
                    gleaph_types::PreparedKind::Query => "query",
                    gleaph_types::PreparedKind::Mutation => "mutation",
                };
                let caller = if info.requires_caller { "yes" } else { "no" };
                let warning_count = info.type_warnings.len();
                let params = if info.parameters.is_empty() {
                    "(none)".to_string()
                } else {
                    info.parameters
                        .iter()
                        .map(|param| {
                            if param.required {
                                param.name.clone()
                            } else {
                                format!("{}?", param.name)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let cols = if info.columns.is_empty() {
                    "(none)".to_string()
                } else {
                    info.columns.join(", ")
                };
                let sorts = summarize_allowed_sorts(info);
                let default_sort = summarize_default_sort(info);
                println!(
                    "{:<24} {:<10} {:<8} {:<10} {:<24} {:<24} {:<28} {}",
                    info.name, kind, caller, warning_count, params, cols, sorts, default_sort
                );
            }
        }

        Command::Query {
            canister,
            gql,
            explain,
            local,
            host,
        } => {
            let canister_id = Principal::from_text(&canister)
                .with_context(|| format!("invalid canister principal: {canister}"))?;
            let ic_host = resolve_host(local, &host);

            if explain {
                let result = agent::explain_query(canister_id, ic_host, local, &gql).await?;
                for row in &result.rows {
                    if let Some(gleaph_types::Value::Text(line)) = row.first() {
                        println!("{line}");
                    }
                }
                print_type_diagnostics(&result.warnings);
            } else {
                let result = agent::run_query(canister_id, ic_host, local, &gql).await?;
                print_query_result(&result.result);
                if result.continuation.is_some() {
                    eprintln!("more rows available; continuation token returned by canister");
                }
            }
        }
    }

    Ok(())
}
