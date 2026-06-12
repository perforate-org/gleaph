//! Candid-shaped init args for the router canister.

use candid::{CandidType, Deserialize, Principal};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct RouterInitArgs {
    /// Installer principal; receives [`gleaph_auth::Role::Admin`] in stable auth.
    pub issuing_principal: Principal,
    #[serde(default)]
    pub initial_admins: Vec<Principal>,
    /// Internet Computer controllers (upgrade / control plane, separate from RBAC).
    #[serde(default)]
    pub controllers: Vec<Principal>,
}

#[cfg(test)]
mod canbench_init_hex {
    use super::*;
    use candid::Encode;

    #[test]
    fn print_router_canbench_init_hex() {
        let admin = Principal::from_slice(&[0xAB; 29]);
        let bytes = Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            controllers: vec![admin],
        })
        .expect("encode");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("router canbench init_args hex: {hex}");
    }
}
