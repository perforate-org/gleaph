//! `gleaph gql` — run one read-only GQL query through Router with shell parameters.
//!
//! This is the ad-hoc counterpart to prepared queries: the text path (`text_score` over a TEXT
//! index) is lowered by the Router into `TextScan`, and the prepared-query executor deliberately
//! accepts a narrower operator set, so text search is exercised through this entry point.

use std::path::PathBuf;

use clap::Args;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};

use crate::prepared;
use crate::remote::RemoteTransport;

/// Arguments for `gleaph gql`.
#[derive(Debug, Args)]
pub struct GqlArgs {
    /// GQL query text.
    #[arg(long, value_name = "GQL", conflicts_with = "query_file")]
    pub query: Option<String>,
    /// File containing the GQL query.
    #[arg(long = "file", value_name = "PATH", conflicts_with = "query")]
    pub query_file: Option<PathBuf>,
    /// Parameter binding `NAME=VALUE` where VALUE is a JSON scalar or array; repeatable.
    #[arg(long = "param", value_name = "NAME=VALUE")]
    pub param: Vec<String>,
    /// Binary parameter binding `NAME=BASE64`; repeatable. Used for encoded ELEMENT_ID anchors.
    #[arg(long = "param-bytes", value_name = "NAME=BASE64")]
    pub param_bytes: Vec<String>,
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Network name (`ic`/`local`) or endpoint.
    #[arg(long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM identity file.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long)]
    pub fetch_root_key: bool,
    /// Execute as an idempotent GQL update (`gql_mutate`) instead of a query.
    #[arg(long)]
    pub update: bool,
    /// Client mutation key for `--update`; defaults to a fresh key per invocation. Pass the same
    /// key to retry the same logical mutation.
    #[arg(long, value_name = "KEY", requires = "update")]
    pub mutation_key: Option<String>,
    /// Print the raw result payload as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// Resolve the query text and pre-encode parameters before any network call.
pub fn prepare(args: &GqlArgs) -> Result<(String, Vec<u8>), String> {
    let query = match (&args.query, &args.query_file) {
        (Some(text), None) => text.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|error| format!("read query file {}: {error}", path.display()))?,
        (None, None) => return Err("pass --query <GQL> or --file <PATH>".into()),
        (Some(_), Some(_)) => unreachable!("clap rejects both query sources"),
    };
    if query.trim().is_empty() {
        return Err("GQL query must not be empty".into());
    }
    let mut params = prepared::parse_run_params(&args.param).map_err(|error| error.to_string())?;
    let binary =
        prepared::parse_run_param_bytes(&args.param_bytes).map_err(|error| error.to_string())?;
    for (name, value) in binary {
        if params.iter().any(|(existing, _)| existing == &name) {
            return Err(format!("duplicate parameter name {name:?}"));
        }
        params.push((name, value));
    }
    let params_blob = prepared::encode_run_params(params).map_err(|error| error.to_string())?;
    Ok((query, params_blob))
}

/// Execute one GQL query through Router (`gql_query`).
pub fn execute(
    transport: &RemoteTransport,
    query: &str,
    params_blob: Vec<u8>,
) -> Result<GqlQueryResult, String> {
    match transport
        .query_args::<GqlQueryResult, RouterError>(
            "gql_query",
            (query.to_owned(), params_blob, ReadMode::Eventual),
        )
        .map_err(|error| format!("query gql_query: {error}"))?
    {
        Ok(result) => Ok(result),
        Err(error) => Err(format!("Router rejected gql_query: {error:?}")),
    }
}

/// Execute one GQL update through Router (`gql_mutate`).
pub fn execute_update(
    transport: &RemoteTransport,
    query: &str,
    params_blob: Vec<u8>,
    mutation_key: &str,
) -> Result<GqlQueryResult, String> {
    match transport
        .update_args::<GqlQueryResult, RouterError>(
            "gql_mutate",
            (query, params_blob, mutation_key),
        )
        .map_err(|error| format!("update gql_mutate: {error}"))?
    {
        Ok(result) => Ok(result),
        Err(error) => Err(format!("Router rejected gql_mutate: {error:?}")),
    }
}

/// A fresh client mutation key for an ad-hoc `--update` invocation.
pub fn fresh_mutation_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("cli-gql-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> GqlArgs {
        GqlArgs {
            query: None,
            query_file: None,
            param: Vec::new(),
            param_bytes: Vec::new(),
            canister: None,
            network: None,
            identity: None,
            fetch_root_key: false,
            update: false,
            mutation_key: None,
            json: false,
        }
    }

    #[test]
    fn prepare_requires_exactly_one_query_source() {
        let error = prepare(&args()).expect_err("no query source");
        assert!(error.contains("--query"));

        let mut args = args();
        args.query = Some("MATCH (n) RETURN n".into());
        let (query, blob) = prepare(&args).expect("inline query");
        assert_eq!(query, "MATCH (n) RETURN n");
        assert!(!blob.is_empty());
    }

    #[test]
    fn prepare_reads_files_and_rejects_empty_queries() {
        let path = std::env::temp_dir().join(format!("gleaph-gql-{}.gql", std::process::id()));
        std::fs::write(&path, "MATCH (n) RETURN n\n").expect("write");
        let mut args = args();
        args.query_file = Some(path.clone());
        assert_eq!(
            prepare(&args).expect("file query").0,
            "MATCH (n) RETURN n\n"
        );

        args.query_file = None;
        args.query = Some("   ".into());
        assert!(prepare(&args).expect_err("empty query").contains("empty"));
        let _ = std::fs::remove_file(path);
    }
}
