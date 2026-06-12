//! Candid-shaped init args for the index canister.

use candid::{CandidType, Deserialize, Principal};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct IndexInitArgs {
    /// Principals allowed to call index admin APIs other than router-driven shard owner updates.
    #[serde(default)]
    pub controllers: Vec<Principal>,
    /// Router canister allowed to call `admin_set_shard_owner` / `admin_clear_shard_owner`.
    pub router_canister: Principal,
}

#[cfg(test)]
mod canbench_init_hex {
    use super::*;
    use candid::Encode;

    #[test]
    fn print_index_canbench_init_hex() {
        let admin = Principal::from_slice(&[0xAB; 29]);
        let bytes = Encode!(&IndexInitArgs {
            controllers: vec![admin],
            router_canister: admin,
        })
        .expect("encode");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("graph-index canbench init_args hex: {hex}");
    }
}
