//! Candid-shaped init args for the router canister.

use candid::{CandidType, Deserialize, Principal};

/// Validate a provision-canister principal argument. `None` is allowed;
/// a `Some` value must be a non-anonymous principal.
pub(crate) fn validate_provision_principal(p: &Option<Principal>) -> Result<(), &'static str> {
    if *p == Some(Principal::anonymous()) {
        return Err("provision_canister cannot be anonymous");
    }
    Ok(())
}

/// Shared Router init args, single-sourced in `gleaph_graph_kernel::provisioning` so the
/// Account canister and Provision canister can construct/install a Router without depending on
/// this crate.
pub use gleaph_graph_kernel::provisioning::init_args::RouterInitArgs;

/// Upgrade-only args for the router canister (ADR 0039).
///
/// Init-only authority, bootstrap principals, and initial administrators must not be
/// replayed on a routine upgrade. The only operator override exposed here is the
/// provision-canister binding; absence means "preserve the durable stable binding".
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct RouterUpgradeArgs {
    /// Optional provision-canister override.
    #[serde(default)]
    pub provision_canister: Option<Principal>,
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
            provision_canister: None,
        })
        .expect("encode");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("router canbench init_args hex: {hex}");
    }
}
