//! Candid-shaped init args for the index canister.

/// Shared Property Index init args, single-sourced in `gleaph_graph_kernel::provisioning` so the
/// Router can construct `install_args` for a Provision-issued index canister without depending on
/// this crate.
pub use gleaph_graph_kernel::provisioning::init_args::IndexInitArgs;

#[cfg(test)]
mod canbench_init_hex {
    use super::*;
    use candid::Encode;

    #[test]
    fn print_index_canbench_init_hex() {
        let admin = candid::Principal::from_slice(&[0xAB; 29]);
        let bytes = Encode!(&IndexInitArgs {
            router_canister: admin,
        })
        .expect("encode");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("graph-index canbench init_args hex: {hex}");
    }
}
