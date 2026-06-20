//! PocketIC: the router registry-invariant oracle must be exposed and must hold
//! across a canister upgrade.
//!
//! The router's `check_registry_invariants` verifies bidirectional consistency of
//! the denormalized registry regions (`ROUTER_GRAPHS`, `ROUTER_SHARDS`, the two
//! derived indexes, runtime config, and the graph catalog). Per-commit
//! verification is disabled in production for cost, so before this oracle was
//! exposed nothing checked that the registry decoded consistently after an
//! upgrade. This test registers a two-shard graph, runs the oracle, upgrades the
//! router in place, and requires the oracle to still pass — any stable-layout or
//! Storable skew in the registry regions would surface as a divergence here.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, install_federation, router_check_registry_invariants, wasm_bytes,
};

fn upgrade_router(env: &FederationEnv) {
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty, None)
        .expect("upgrade router canister");
}

#[test]
fn registry_invariants_hold_across_router_upgrade() {
    let env = install_federation();

    router_check_registry_invariants(&env)
        .expect("registry invariants hold for the freshly registered two-shard graph");

    upgrade_router(&env);

    // The registry is stored entirely in stable structures with no router upgrade
    // hook; the oracle must observe the exact same consistent registry after the
    // reinstall. A regression in stable layout, Storable encoding, or the derived
    // index commit-sync would diverge here instead of silently mis-routing later.
    router_check_registry_invariants(&env)
        .expect("registry invariants must still hold after the router upgrade");

    // A second upgrade must remain stable (guards against a layout that drifts on
    // each cycle).
    upgrade_router(&env);
    router_check_registry_invariants(&env)
        .expect("registry invariants must hold after a repeated router upgrade");
}

#[test]
fn registry_invariant_oracle_requires_admin() {
    let env = install_federation();
    let non_admin = Principal::from_slice(&[0x11; 29]);

    let bytes = env
        .pic
        .query_call(
            env.router,
            non_admin,
            "admin_check_registry_invariants",
            Encode!().expect("encode admin_check_registry_invariants"),
        )
        .expect("query call dispatch");
    let result = Decode!(&bytes, Result<(), RouterError>).expect("decode oracle result");
    assert!(
        matches!(result, Err(RouterError::NotAuthorized)),
        "non-admin caller must be rejected, got {result:?}"
    );
}
