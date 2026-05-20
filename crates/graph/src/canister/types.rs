//! Candid-shaped types for canister `init`.

use candid::{CandidType, Deserialize, Principal};
use gleaph_graph_kernel::federation::ShardId;

/// Arguments supplied by the registry (or installer) on first `init`.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct GraphInitArgs {
    pub logical_graph_name: Option<String>,
    /// Router canister for federation (required together with `shard_id`).
    #[serde(default)]
    pub router_canister: Option<Principal>,
    #[serde(default)]
    pub shard_id: Option<ShardId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_init_args_candid_hex() {
        let args = GraphInitArgs {
            logical_graph_name: None,
            router_canister: None,
            shard_id: None,
        };
        let bytes = candid::encode_one(args).expect("encode GraphInitArgs");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("canbench init_args hex: {hex}");
        assert!(!hex.is_empty());
    }
}
