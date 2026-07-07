// Durable runtime config for the provision-canister binding (ADR 0035 Slice 5).

use crate::facade::stable::ROUTER_PROVISION_CONFIG;
use crate::provisioning::config::ProvisionRuntimeConfig;

pub(crate) fn save_provision_runtime_config(config: &ProvisionRuntimeConfig) {
    ROUTER_PROVISION_CONFIG.with_borrow_mut(|cell| {
        cell.set(config.clone());
    });
}

pub(crate) fn load_provision_runtime_config() -> ProvisionRuntimeConfig {
    ROUTER_PROVISION_CONFIG.with_borrow(|cell| cell.get().clone())
}
