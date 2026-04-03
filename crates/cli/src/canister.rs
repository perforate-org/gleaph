//! Fetch [`gleaph_graph::PreparedQueryInfo`] from an Internet Computer canister via `ic-agent`.

use anyhow::{Context, Result, bail};
use candid::Principal;
use gleaph_graph::PreparedQueryInfo;
use ic_agent::Agent;
use ic_agent::identity::AnonymousIdentity;

fn decode_list_prepared_reply(bytes: &[u8]) -> Result<Vec<PreparedQueryInfo>> {
    match candid::decode_args::<(Result<Vec<PreparedQueryInfo>, String>,)>(bytes) {
        Ok((result,)) => result.map_err(|e| anyhow::anyhow!("canister error: {e}")),
        Err(_) => {
            let (rows,): (Vec<PreparedQueryInfo>,) = candid::decode_args(bytes).context(
                "failed to decode Candid (expected Result<vec PreparedQueryInfo, text> or vec)",
            )?;
            Ok(rows)
        }
    }
}

/// Query a canister for prepared-query metadata and decode `PreparedQueryInfo` records.
///
/// The return type must be either `Result<Vec<PreparedQueryInfo>, text>` or a plain
/// `vec PreparedQueryInfo` in Candid. The canister `.did` must match the `CandidType`
/// layout of [`PreparedQueryInfo`] in the `gleaph` crate.
///
/// Uses anonymous identity; the canister ACL must allow the call.
pub async fn fetch_prepared_queries_from_canister(
    canister_id: Principal,
    replica_url: &str,
    fetch_root_key: bool,
    method_name: &str,
) -> Result<Vec<PreparedQueryInfo>> {
    let agent = Agent::builder()
        .with_url(replica_url)
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
        .query(&canister_id, method_name)
        .with_arg(candid::encode_args(()).unwrap())
        .call()
        .await
        .with_context(|| format!("failed to call {method_name} on the canister"))?;

    decode_list_prepared_reply(&response)
}

/// Parse `--canister` with optional `ic:` host prefix (dfx-style principal text).
pub fn parse_canister_id(text: &str) -> Result<Principal> {
    let s = text.strip_prefix("ic:").unwrap_or(text);
    Principal::from_text(s).with_context(|| format!("invalid canister id: {text:?}"))
}

pub fn ensure_input_xor_canister(
    input: Option<&std::path::Path>,
    canister: Option<&str>,
) -> Result<()> {
    match (input, canister) {
        (Some(_), Some(_)) => bail!("pass only one of --input or --canister"),
        (None, None) => bail!("pass --input (JSON file) or --canister (principal text)"),
        _ => Ok(()),
    }
}
