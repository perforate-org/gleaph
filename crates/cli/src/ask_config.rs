//! Ask configuration commands (ADR 0115/0118): read and set the per-graph abstain threshold and
//! policy version, parameter values, and enablement. Authorization is enforced by the Router
//! (controller or graph tenant).

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::Serialize;
use thiserror::Error;

use crate::ask;
use gleaph_sdk::routing::GeneralDomainValue;

/// `gleaph config` command group.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Ask-configuration subcommand.
    #[command(subcommand)]
    pub command: ConfigCommand,
}

/// Ask-configuration subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Read or set one graph's ask configuration.
    Ask(AskConfigArgs),
    /// Read or set the declared values of a graph's ask parameters (ADR 0117).
    ParameterValues(ParameterValuesArgs),
    /// Enable ask for a graph (ADR 0111).
    EnableAsk(AskEnablementArgs),
    /// Disable ask for a graph.
    DisableAsk(AskEnablementArgs),
}

/// Arguments for `gleaph config ask`.
#[derive(Debug, Args)]
pub struct AskConfigArgs {
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: String,
    /// Inference-enabled graph name.
    #[arg(long, value_name = "NAME")]
    pub graph: String,
    /// Network name (`local`) or an HTTP(S) endpoint.
    #[arg(short, long, default_value = "local", value_name = "NETWORK")]
    pub network: String,
    /// PEM identity; anonymous when omitted.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before the update on a custom endpoint.
    #[arg(long)]
    pub fetch_root_key: bool,
    /// Set the abstain threshold in basis points (0..=10000); omit to read only.
    #[arg(long, value_name = "BPS")]
    pub set_threshold_bps: Option<u32>,
    /// Policy version written with `--set-threshold-bps`.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub policy_version: u32,
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `gleaph config parameter-values`.
#[derive(Debug, Args)]
pub struct ParameterValuesArgs {
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: String,
    /// Inference-enabled graph name.
    #[arg(long, value_name = "NAME")]
    pub graph: String,
    /// Network name (`local`) or an HTTP(S) endpoint.
    #[arg(short, long, default_value = "local", value_name = "NETWORK")]
    pub network: String,
    /// PEM identity; anonymous when omitted.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before the update on a custom endpoint.
    #[arg(long)]
    pub fetch_root_key: bool,
    /// Parameter whose domain is written; required by `--set-values` and `--clear`.
    #[arg(long, value_name = "NAME")]
    pub param: Option<String>,
    /// Comma-separated canonical values to set (trimmed, deduplicated and sorted by the Router).
    #[arg(long, value_name = "V1,V2,...", conflicts_with_all = ["clear", "from_label"])]
    pub set_values: Option<String>,
    /// JSON object mapping a canonical value to its accepted alias strings, e.g.
    /// {"SF映画":["SF","science fiction"]}. Aliases canonicalize to the mapped value.
    #[arg(long, value_name = "PATH")]
    pub aliases: Option<PathBuf>,
    /// Materialize the domain from one node label's property (bounded DISTINCT query).
    #[arg(long, value_name = "LABEL", conflicts_with = "clear")]
    pub from_label: Option<String>,
    /// Property to read when `--from-label` is used.
    #[arg(long, value_name = "PROPERTY", requires = "from_label")]
    pub property: Option<String>,
    /// Row bound for the materialization query.
    #[arg(long, value_name = "N", default_value_t = 64, requires = "from_label")]
    pub limit: u32,
    /// Clear the parameter's domain.
    #[arg(long)]
    pub clear: bool,
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `gleaph config enable-ask` / `disable-ask`.
#[derive(Debug, Args)]
pub struct AskEnablementArgs {
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: String,
    /// Graph name.
    #[arg(long, value_name = "NAME")]
    pub graph: String,
    /// Network name (`local`) or an HTTP(S) endpoint.
    #[arg(short, long, default_value = "local", value_name = "NETWORK")]
    pub network: String,
    /// PEM identity; anonymous when omitted. Enabling requires controller authority.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before the update on a custom endpoint.
    #[arg(long)]
    pub fetch_root_key: bool,
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// One parameter domain as returned by the Router.
#[derive(Debug, Serialize)]
pub struct DomainReport {
    /// Parameter name.
    pub parameter: String,
    /// Accepted graph values and their alias surfaces.
    pub values: Vec<GeneralDomainValue>,
}

/// Routing profile as returned by the Router.
#[derive(Debug, Serialize)]
pub struct ProfileReport {
    /// Graph name.
    pub graph: String,
    /// Current abstain threshold in basis points.
    pub abstain_threshold_bps: u32,
    /// Current policy version.
    pub policy_version: u32,
    /// Whether this invocation wrote the profile.
    pub updated: bool,
}

/// Errors raised by `gleaph config`.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Connecting to the Router or the Router call failed.
    #[error("router: {0}")]
    Router(String),
    /// The profile pair could not be rendered.
    #[error("json: {0}")]
    Json(String),
}

impl From<ask::AskError> for ConfigError {
    fn from(error: ask::AskError) -> Self {
        Self::Router(error.to_string())
    }
}

/// Execute `gleaph config`.
pub fn execute(args: ConfigArgs) -> Result<(), ConfigError> {
    match args.command {
        ConfigCommand::Ask(args) => ask_config(args),
        ConfigCommand::ParameterValues(args) => parameter_values(args),
        ConfigCommand::EnableAsk(args) => set_enabled(args, true),
        ConfigCommand::DisableAsk(args) => set_enabled(args, false),
    }
}

fn set_enabled(args: AskEnablementArgs, enable: bool) -> Result<(), ConfigError> {
    let client = ask::connect_routing_client(
        &args.canister,
        &args.graph,
        &args.network,
        args.identity.as_deref(),
        args.fetch_root_key,
    )?;
    let mut inference_canister = None;
    if enable {
        let canister = client
            .enable_ask(&args.graph)
            .map_err(|error| ConfigError::Router(error.to_string()))?;
        inference_canister = Some(canister.to_text());
    } else {
        client
            .disable_ask(&args.graph)
            .map_err(|error| ConfigError::Router(error.to_string()))?;
    }
    if args.json {
        let report = serde_json::json!({
            "graph": args.graph,
            "enabled": enable,
            "inference_canister": inference_canister,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| ConfigError::Json(error.to_string()))?
        );
    } else {
        println!("graph: {}", args.graph);
        println!("enabled: {}", enable);
        if let Some(canister) = inference_canister {
            println!("inference_canister: {canister}");
        }
    }
    Ok(())
}

fn parameter_values(args: ParameterValuesArgs) -> Result<(), ConfigError> {
    if (args.set_values.is_some() || args.clear) && args.param.is_none() {
        return Err(ConfigError::Router(
            "--set-values and --clear require --param".to_owned(),
        ));
    }
    let alias_map = args
        .aliases
        .as_deref()
        .map(parse_alias_map)
        .transpose()?
        .unwrap_or_default();
    if !alias_map.is_empty() && args.set_values.is_none() && args.from_label.is_none() {
        return Err(ConfigError::Router(
            "--aliases requires --set-values or --from-label".to_owned(),
        ));
    }
    let client = ask::connect_routing_client(
        &args.canister,
        &args.graph,
        &args.network,
        args.identity.as_deref(),
        args.fetch_root_key,
    )?;
    let written = args.set_values.is_some() || args.clear || args.from_label.is_some();
    let mut entries = Vec::new();
    if let Some(raw) = &args.set_values {
        let values = parse_domain_values(raw)?;
        entries = attach_aliases(values, &alias_map)?;
    } else if let Some(label) = &args.from_label {
        let property = args
            .property
            .as_deref()
            .ok_or_else(|| ConfigError::Router("--from-label requires --property".to_owned()))?;
        let transport = crate::remote::RemoteTransport::connect(
            &args.canister,
            &args.network,
            args.identity.as_deref(),
            args.fetch_root_key,
            None,
        )
        .map_err(ConfigError::Router)?;
        let values = materialize_domain(&transport, label, property, args.limit)?;
        if values.is_empty() {
            return Err(ConfigError::Router(format!(
                "no values found for {label}.{property}"
            )));
        }
        entries = attach_aliases(values, &alias_map)?;
    }
    if args.set_values.is_some() || args.from_label.is_some() {
        client
            .set_ask_parameter_values(
                &args.graph,
                args.param.as_deref().expect("checked"),
                &entries,
            )
            .map_err(|error| ConfigError::Router(error.to_string()))?;
    } else if args.clear {
        client
            .set_ask_parameter_values(&args.graph, args.param.as_deref().expect("checked"), &[])
            .map_err(|error| ConfigError::Router(error.to_string()))?;
    }
    let domains: Vec<DomainReport> = client
        .get_ask_parameter_values(&args.graph)
        .map_err(|error| ConfigError::Router(error.to_string()))?
        .into_iter()
        .map(|(parameter, values)| DomainReport { parameter, values })
        .collect();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&domains)
                .map_err(|error| ConfigError::Json(error.to_string()))?
        );
    } else {
        println!("graph: {}", args.graph);
        if domains.is_empty() {
            println!("domains: (none)");
        }
        for report in &domains {
            let rendered: Vec<String> = report
                .values
                .iter()
                .map(|entry| {
                    if entry.aliases.is_empty() {
                        entry.value.clone()
                    } else {
                        format!("{} ({})", entry.value, entry.aliases.join(", "))
                    }
                })
                .collect();
            println!("{}: {}", report.parameter, rendered.join(", "));
        }
        if written {
            println!("updated: true");
        }
    }
    Ok(())
}

/// Read a canonical-value -> alias-array JSON map.
fn parse_alias_map(
    path: &Path,
) -> Result<std::collections::BTreeMap<String, Vec<String>>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        ConfigError::Router(format!("read aliases {}: {error}", path.display()))
    })?;
    let raw: std::collections::BTreeMap<String, Vec<String>> = serde_json::from_str(&text)
        .map_err(|error| ConfigError::Json(format!("parse aliases {}: {error}", path.display())))?;
    let mut map = std::collections::BTreeMap::new();
    for (value, aliases) in raw {
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err(ConfigError::Router(
                "alias map contains an empty canonical value".to_owned(),
            ));
        }
        let aliases: Vec<String> = aliases
            .into_iter()
            .map(|alias| alias.trim().to_owned())
            .filter(|alias| !alias.is_empty())
            .collect();
        if aliases.is_empty() {
            return Err(ConfigError::Router(format!(
                "alias map for {value:?} has no non-empty aliases"
            )));
        }
        if map.insert(value.clone(), aliases).is_some() {
            return Err(ConfigError::Router(format!(
                "alias map repeats canonical value {value:?}"
            )));
        }
    }
    Ok(map)
}

/// Attach aliases to materialized canonical values, rejecting aliases for values that do not exist.
fn attach_aliases(
    values: Vec<String>,
    alias_map: &std::collections::BTreeMap<String, Vec<String>>,
) -> Result<Vec<GeneralDomainValue>, ConfigError> {
    let known: std::collections::BTreeSet<&str> = values.iter().map(String::as_str).collect();
    for value in alias_map.keys() {
        if !known.contains(value.as_str()) {
            return Err(ConfigError::Router(format!(
                "alias map names {value:?}, which is not in the domain"
            )));
        }
    }
    Ok(values
        .into_iter()
        .map(|value| GeneralDomainValue {
            aliases: alias_map.get(&value).cloned().unwrap_or_default(),
            value,
        })
        .collect())
}

/// Run a bounded `DISTINCT` query and return the `value` column as text.
///
/// The domain is materialized from graph data, so no vocabulary is written by hand and no language
/// rule is needed; the same command works for cities, genes or products (ADR 0117).
fn materialize_domain(
    transport: &crate::remote::RemoteTransport,
    label: &str,
    property: &str,
    limit: u32,
) -> Result<Vec<String>, ConfigError> {
    validate_identifier(label)?;
    validate_identifier(property)?;
    if limit == 0 {
        return Err(ConfigError::Router("--limit must be positive".to_owned()));
    }
    let query = format!(
        "MATCH (n:{label}) RETURN DISTINCT n.{property} AS value ORDER BY value LIMIT {limit}"
    );
    let result = crate::gql::execute(transport, &query, Vec::new()).map_err(ConfigError::Router)?;
    let Some(blob) = &result.rows_blob else {
        return Ok(Vec::new());
    };
    let wire = gleaph_gql_ic_wire::GqlWireRows::decode_blob(blob)
        .map_err(|error| ConfigError::Router(format!("decode rows blob: {error}")))?;
    let mut values = Vec::new();
    for row in &wire.rows {
        for (name, value) in &row.columns {
            if name == "value"
                && let gleaph_gql_ic_wire::GqlWireValue::Text(text) = value
            {
                values.push(text.clone());
            }
        }
    }
    Ok(values)
}

/// Accept only plain GQL identifiers, so a label or property cannot inject query text.
fn validate_identifier(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    let valid_first = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    if !valid_first || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ConfigError::Router(format!(
            "invalid GQL identifier {name:?}: expected [A-Za-z_][A-Za-z0-9_]*"
        )));
    }
    Ok(())
}

/// Split a comma-separated domain list, trimming entries and rejecting empty ones.
fn parse_domain_values(raw: &str) -> Result<Vec<String>, ConfigError> {
    let values: Vec<String> = raw
        .split(',')
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect();
    if values.is_empty() {
        return Err(ConfigError::Router(
            "--set-values needs at least one non-empty value".to_owned(),
        ));
    }
    Ok(values)
}

fn ask_config(args: AskConfigArgs) -> Result<(), ConfigError> {
    if let Some(bps) = args.set_threshold_bps
        && bps > 10_000
    {
        return Err(ConfigError::Router(format!(
            "abstain threshold must be in 0..=10000 basis points, got {bps}"
        )));
    }
    let client = ask::connect_routing_client(
        &args.canister,
        &args.graph,
        &args.network,
        args.identity.as_deref(),
        args.fetch_root_key,
    )?;
    let updated = args.set_threshold_bps.is_some();
    if let Some(bps) = args.set_threshold_bps {
        client
            .set_ask_config(&args.graph, bps, args.policy_version)
            .map_err(|error| ConfigError::Router(error.to_string()))?;
    }
    let (abstain_threshold_bps, policy_version) = client
        .get_ask_config(&args.graph)
        .map_err(|error| ConfigError::Router(error.to_string()))?;
    let report = ProfileReport {
        graph: args.graph,
        abstain_threshold_bps,
        policy_version,
        updated,
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| ConfigError::Json(error.to_string()))?
        );
    } else {
        println!("graph: {}", report.graph);
        println!("abstain_threshold_bps: {}", report.abstain_threshold_bps);
        println!("policy_version: {}", report.policy_version);
        if report.updated {
            println!("updated: true");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialization_rejects_injected_identifiers() {
        assert!(validate_identifier("Genre").is_ok());
        assert!(validate_identifier("_internal").is_ok());
        assert!(validate_identifier("Genre) RETURN n LIMIT 1 //").is_err());
        assert!(validate_identifier("").is_err());
    }

    #[test]
    fn parses_domain_values_and_rejects_empty_lists() {
        assert_eq!(
            parse_domain_values(" SF映画 , ホラー映画 ,, SF映画 ").expect("values"),
            vec![
                "SF映画".to_owned(),
                "ホラー映画".to_owned(),
                "SF映画".to_owned()
            ]
        );
        assert!(parse_domain_values(" , ").is_err());
    }

    #[test]
    fn aliases_attach_only_to_known_canonical_values() {
        let values = vec!["SF映画".to_owned(), "ホラー映画".to_owned()];
        let mut aliases = std::collections::BTreeMap::new();
        aliases.insert(
            "SF映画".to_owned(),
            vec!["SF".to_owned(), "science fiction".to_owned()],
        );
        let entries = attach_aliases(values, &aliases).expect("aliases");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].value, "SF映画");
        assert_eq!(entries[0].aliases, vec!["SF", "science fiction"]);
        assert!(entries[1].aliases.is_empty());

        let mut unknown = std::collections::BTreeMap::new();
        unknown.insert("unknown".to_owned(), vec!["alias".to_owned()]);
        assert!(attach_aliases(vec!["SF映画".to_owned()], &unknown).is_err());
    }

    #[test]
    fn domain_writes_require_a_parameter() {
        let error = execute(ConfigArgs {
            command: ConfigCommand::ParameterValues(ParameterValuesArgs {
                canister: "aaaaa-aa".to_owned(),
                graph: "movies".to_owned(),
                network: "local".to_owned(),
                identity: None,
                fetch_root_key: false,
                param: None,
                set_values: Some("a,b".to_owned()),
                aliases: None,
                from_label: None,
                property: None,
                limit: 64,
                clear: false,
                json: false,
            }),
        })
        .expect_err("a write needs --param");
        assert!(matches!(error, ConfigError::Router(message) if message.contains("--param")));
    }

    #[test]
    fn rejects_out_of_range_threshold_before_connecting() {
        let error = execute(ConfigArgs {
            command: ConfigCommand::Ask(AskConfigArgs {
                canister: "aaaaa-aa".to_owned(),
                graph: "movies".to_owned(),
                network: "local".to_owned(),
                identity: None,
                fetch_root_key: false,
                set_threshold_bps: Some(10_001),
                policy_version: 1,
                json: false,
            }),
        })
        .expect_err("threshold is out of range");
        assert!(matches!(error, ConfigError::Router(message) if message.contains("0..=10000")));
    }
}
