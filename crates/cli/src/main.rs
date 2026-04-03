//! `gleaph-codegen` — generate TS / JS / Rust from prepared-query JSON
//! (`PreparedQueryInfo[]` or `{ "statements": [...] }`), or fetch the same metadata from a canister.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use gleaph_cli::{
    JsParamStyle, Lang, ensure_input_xor_canister, fetch_prepared_queries_from_canister,
    generate_dts, generate_js, generate_rs, generate_ts, load_prepared_queries_from_path,
    parse_canister_id, resolve_output_path, write_codegen_output,
};

#[derive(Parser)]
#[command(
    name = "gleaph-codegen",
    about = "Generate TypeScript / JavaScript / Rust clients from Gleaph prepared-query metadata (JSON or canister query)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Emit client code for one or more languages.
    Codegen {
        /// JSON file: array of prepared definitions or `{ "statements": [...] }`.
        #[arg(short, long, conflicts_with = "canister")]
        input: Option<PathBuf>,

        /// Canister principal (text). Queries `--query-method` with `()` using anonymous identity.
        /// Conflicts with `--input`. Requires `--replica-url` / `--fetch-root-key` as for `ic-agent`.
        #[arg(long, conflicts_with = "input")]
        canister: Option<String>,

        #[arg(long, default_value = "https://ic0.app")]
        replica_url: String,

        /// Call `Agent::fetch_root_key` first (required for local replica, e.g. port 4943).
        #[arg(long)]
        fetch_root_key: bool,

        /// Candid query method name (no arguments); must return `vec PreparedQueryInfo` or `Result<vec, text>`.
        #[arg(long, default_value = "list_prepared")]
        query_method: String,

        /// Target language(s): `ts`, `js`, `rust` (repeatable).
        #[arg(long = "lang", default_value = "ts")]
        lang: Vec<String>,

        /// Output path (file). When emitting multiple languages, use a directory with `--output-dir`.
        /// For `--lang js`, writes `<path>.js` and `<path>.d.ts` (use `.js` suffix or stem is adjusted).
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Output directory when generating more than one language.
        #[arg(long)]
        output_dir: Option<PathBuf>,

        /// Parameter naming for JS/TS (`camel` vs preserve wire names as fields for `preserve`).
        #[arg(long, value_enum, default_value_t = CliJsParamStyle::Camel)]
        js_param_style: CliJsParamStyle,

        /// Label in the generated file header (canister id, service name, etc.).
        #[arg(long, default_value = "prepared-queries.json")]
        source_label: String,
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
            CliJsParamStyle::Preserve => Self::Preserve,
            CliJsParamStyle::Camel => Self::Camel,
        }
    }
}

fn parse_langs(lang_args: &[String]) -> Result<Vec<Lang>> {
    let mut langs = Vec::new();
    for s in lang_args {
        match s.parse::<Lang>() {
            Ok(l) => {
                if !langs.contains(&l) {
                    langs.push(l);
                }
            }
            Err(e) => bail!("unknown language: {s} ({e})"),
        }
    }
    if langs.is_empty() {
        bail!("pass at least one --lang");
    }
    Ok(langs)
}

fn run_codegen(
    stmts: &[gleaph_graph::PreparedQueryInfo],
    langs: &[Lang],
    output: Option<&PathBuf>,
    output_dir: Option<&PathBuf>,
    style: JsParamStyle,
    source_label: &str,
) -> Result<()> {
    if langs.len() > 1 {
        let dir = output_dir
            .cloned()
            .or_else(|| output.filter(|p| p.is_dir()).cloned())
            .context("multiple --lang requires --output-dir (a directory)")?;
        for l in langs {
            match l {
                Lang::JavaScript => {
                    let js_path = dir.join(l.default_filename());
                    let dts_path = Lang::js_declaration_path(&js_path);
                    let js = generate_js(stmts, source_label, style);
                    let dts = generate_dts(stmts, source_label, style);
                    write_codegen_output(&js_path, &js)?;
                    write_codegen_output(&dts_path, &dts)?;
                    eprintln!("  wrote {}", js_path.display());
                    eprintln!("  wrote {}", dts_path.display());
                }
                Lang::TypeScript => {
                    let path = dir.join(l.default_filename());
                    let body = generate_ts(stmts, source_label, style);
                    write_codegen_output(&path, &body)?;
                    eprintln!("  wrote {}", path.display());
                }
                Lang::Rust => {
                    let path = dir.join(l.default_filename());
                    let body = generate_rs(stmts, source_label);
                    write_codegen_output(&path, &body)?;
                    eprintln!("  wrote {}", path.display());
                }
            }
        }
    } else {
        let l = langs[0];
        match l {
            Lang::JavaScript => {
                let js_path = resolve_output_path(l, output);
                let dts_path = Lang::js_declaration_path(&js_path);
                let js = generate_js(stmts, source_label, style);
                let dts = generate_dts(stmts, source_label, style);
                write_codegen_output(&js_path, &js)?;
                write_codegen_output(&dts_path, &dts)?;
                eprintln!("  wrote {}", js_path.display());
                eprintln!("  wrote {}", dts_path.display());
            }
            Lang::TypeScript => {
                let path = resolve_output_path(l, output);
                let body = generate_ts(stmts, source_label, style);
                write_codegen_output(&path, &body)?;
                eprintln!("  wrote {}", path.display());
            }
            Lang::Rust => {
                let path = resolve_output_path(l, output);
                let body = generate_rs(stmts, source_label);
                write_codegen_output(&path, &body)?;
                eprintln!("  wrote {}", path.display());
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Codegen {
            input,
            canister,
            replica_url,
            fetch_root_key,
            query_method,
            lang,
            output,
            output_dir,
            js_param_style,
            source_label,
        } => {
            ensure_input_xor_canister(input.as_deref(), canister.as_deref())?;

            let stmts = if let Some(path) = &input {
                load_prepared_queries_from_path(path)
                    .with_context(|| format!("read {}", path.display()))?
            } else {
                let cid_text = canister.as_deref().expect("checked");
                let principal = parse_canister_id(cid_text)?;
                fetch_prepared_queries_from_canister(
                    principal,
                    &replica_url,
                    fetch_root_key,
                    &query_method,
                )
                .await?
            };

            if stmts.is_empty() {
                eprintln!("warning: no prepared statements in input");
            } else {
                eprintln!("loaded {} prepared statement(s)", stmts.len());
            }

            let langs = parse_langs(&lang)?;
            let style = JsParamStyle::from(js_param_style);

            run_codegen(
                &stmts,
                &langs,
                output.as_ref(),
                output_dir.as_ref(),
                style,
                &source_label,
            )?;

            eprintln!("done.");
        }
    }

    Ok(())
}
