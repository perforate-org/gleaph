//! Global derived-vector-dispatch activation flag (ADR 0031 Slice 4).
//!
//! The Router owns a single stable, reversible `bool` that gates **all** production vector
//! dispatch/backfill. It defaults to `true`: per-index readiness is the index lifecycle's job
//! (target + shard attach, `activation_block_reason`), while this flag is the fleet-level
//! circuit breaker — defaulting it on keeps the required setup steps standard (cf. Milvus'
//! per-collection load, PG/Neo4j index lifecycles) and `set_vector_dispatch_enabled(false)`
//! becomes the incident-response step. `Cell::init` preserves an operator-set `false` across
//! upgrades. The flag is necessary but not sufficient: a graph also needs every live shard
//! vector-attached (see `RouterStore::graph_vector_dispatch_ready`).

use super::ROUTER_VECTOR_DISPATCH_ACTIVATION;

/// Reads the global activation flag. `false` keeps dispatch/backfill fail-closed (the
/// incident-response position; the default is `true`).
pub(crate) fn vector_dispatch_globally_enabled() -> bool {
    ROUTER_VECTOR_DISPATCH_ACTIVATION.with_borrow(|cell| *cell.get())
}

/// Flips the global activation flag (RBAC-gated at the endpoint). Reversible.
pub(crate) fn set_vector_dispatch_globally_enabled(enabled: bool) {
    ROUTER_VECTOR_DISPATCH_ACTIVATION.with_borrow_mut(|cell| {
        cell.set(enabled);
    });
}
