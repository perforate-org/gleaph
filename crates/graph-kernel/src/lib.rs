#[cfg(feature = "canbench")]
mod bench;

pub mod bidirectional_catalog;
pub mod edge_inline_value_profile_store;
pub mod entry;
pub mod federation;
pub mod gql_dialect;
pub mod index;
pub mod path;
pub mod plan_exec;
pub mod scoped_name_catalog;
pub mod stable_layout;
pub mod stable_memory;
pub mod vector_index;

pub mod provisioning;

/// Conservative payload ceiling for inter-canister request arguments.
///
/// ICP permits 2 MiB for ingress and cross-subnet request payloads, while same-subnet
/// requests may be larger. Using the smallest request limit keeps a call portable when
/// canisters are moved across subnets; callers must measure the actual Candid argument bytes.
pub const MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
