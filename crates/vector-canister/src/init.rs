//! Candid-shaped init args for the vector index canister.

/// Shared Vector canister init args, single-sourced in `gleaph_graph_kernel::provisioning` so the
/// Router can construct `install_args` for a Provision-issued vector canister without depending on
/// this crate.
pub use gleaph_graph_kernel::provisioning::init_args::{
    DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED, VectorCanisterInitArgs,
};
