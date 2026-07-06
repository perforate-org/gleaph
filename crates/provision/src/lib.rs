//! Gleaph Provision canister — ADR 0035.
//!
//! Owns durable job/receipt state and the deployment trust binding.
//! Does not own graph topology, tenancy, or routing catalogs.

#![cfg_attr(not(test), allow(dead_code))]

pub mod stable;
pub mod types;

pub mod canister;

#[cfg(test)]
mod candid;

use crate::canister::{ProvisionIngressResult, ProvisionJobView, RouterAckResult, handlers};
use crate::types::{ProvisionRequest, RouterProvisionAck};
use ic_cdk_macros::{init, post_upgrade, query, update};

#[init]
fn init(args: crate::canister::init::ProvisionInitArgs) {
    handlers::init_handler(args);
}

#[post_upgrade]
fn post_upgrade() {
    handlers::post_upgrade_handler();
}

#[update]
fn accept_envelope(req: ProvisionRequest) -> ProvisionIngressResult {
    handlers::accept_envelope_handler(req)
}

#[query]
fn query_job(request_id: String, deployment_id: String) -> Option<ProvisionJobView> {
    handlers::query_job_handler(request_id, deployment_id)
}

#[update]
fn router_ack(ack: RouterProvisionAck) -> RouterAckResult {
    handlers::router_ack_handler(ack)
}

#[cfg(test)]
pub fn export_service_string() -> String {
    __export_service()
}

ic_cdk::export_candid!();

/// IC NNS timestamp in nanoseconds.
///
/// Mirrors `crates/router/src/facade/store.rs:121-128`: returns `ic_cdk::api::time()` on
/// `wasm`, `0` on `not(target_family = "wasm")`. Used by the `handlers` module for
/// `accept_envelope` and `router_ack` transition timestamps; also used by unit tests that drive
/// `*_with_caller` directly.
#[allow(dead_code)]
pub(crate) fn ic_time_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}
