use anyhow::{Context, Result};
use candid::Principal;
use gleaph_types::{PreparedStatementInfo, QueryResult, QueryResultWithContinuation};
use ic_agent::Agent;
use ic_agent::identity::AnonymousIdentity;

/// Fetch all prepared statements from a graph canister.
pub async fn fetch_prepared_statements(
    canister_id: Principal,
    host: &str,
    fetch_root_key: bool,
) -> Result<Vec<PreparedStatementInfo>> {
    let agent = Agent::builder()
        .with_url(host)
        .with_identity(AnonymousIdentity)
        .build()
        .context("failed to build IC agent")?;

    if fetch_root_key {
        agent
            .fetch_root_key()
            .await
            .context("failed to fetch root key (is the local replica running?)")?;
    }

    let response = agent
        .query(&canister_id, "list_prepared")
        .with_arg(candid::encode_args(()).unwrap())
        .call()
        .await
        .context("failed to call list_prepared on the canister")?;

    let (result,): (Result<Vec<PreparedStatementInfo>, gleaph_types::GleaphError>,) =
        candid::decode_args(&response).context("failed to decode Candid response")?;

    result.map_err(|e| anyhow::anyhow!("canister error: {e}"))
}

pub async fn run_query(
    canister_id: Principal,
    host: &str,
    fetch_root_key: bool,
    gql: &str,
) -> Result<QueryResultWithContinuation> {
    let agent = Agent::builder()
        .with_url(host)
        .with_identity(AnonymousIdentity)
        .build()
        .context("failed to build IC agent")?;

    if fetch_root_key {
        agent
            .fetch_root_key()
            .await
            .context("failed to fetch root key (is the local replica running?)")?;
    }

    let response = agent
        .query(&canister_id, "query")
        .with_arg(
            candid::encode_args((
                gql.to_string(),
                Option::<Vec<(String, gleaph_types::Value)>>::None,
            ))
            .unwrap(),
        )
        .call()
        .await
        .context("failed to call query on the canister")?;

    let (result,): (Result<QueryResultWithContinuation, gleaph_types::GleaphError>,) =
        candid::decode_args(&response).context("failed to decode Candid response")?;

    result.map_err(|e| anyhow::anyhow!("canister error: {e}"))
}

pub async fn explain_query(
    canister_id: Principal,
    host: &str,
    fetch_root_key: bool,
    gql: &str,
) -> Result<QueryResult> {
    let agent = Agent::builder()
        .with_url(host)
        .with_identity(AnonymousIdentity)
        .build()
        .context("failed to build IC agent")?;

    if fetch_root_key {
        agent
            .fetch_root_key()
            .await
            .context("failed to fetch root key (is the local replica running?)")?;
    }

    let response = agent
        .query(&canister_id, "explain")
        .with_arg(candid::encode_args((gql.to_string(),)).unwrap())
        .call()
        .await
        .context("failed to call explain on the canister")?;

    let (result,): (Result<QueryResult, gleaph_types::GleaphError>,) =
        candid::decode_args(&response).context("failed to decode Candid response")?;

    result.map_err(|e| anyhow::anyhow!("canister error: {e}"))
}
