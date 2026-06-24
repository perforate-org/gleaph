//! Global derived-vector-dispatch activation flag (ADR 0031 Slice 4).
//!
//! The Router owns a single stable, reversible `bool` that gates **all** production vector
//! dispatch/backfill. It defaults to `false` (off) so an upgraded Router stays fail-closed until an
//! operator explicitly enables dispatch. The flag is necessary but not sufficient: a graph also
//! needs every live shard vector-attached (see `RouterStore::graph_vector_dispatch_ready`).

use super::ROUTER_VECTOR_DISPATCH_ACTIVATION;

/// Reads the global activation flag. `false` keeps dispatch/backfill fail-closed.
pub(crate) fn vector_dispatch_globally_enabled() -> bool {
    ROUTER_VECTOR_DISPATCH_ACTIVATION.with_borrow(|cell| *cell.get())
}

/// Flips the global activation flag (RBAC-gated at the endpoint). Reversible.
pub(crate) fn set_vector_dispatch_globally_enabled(enabled: bool) {
    ROUTER_VECTOR_DISPATCH_ACTIVATION.with_borrow_mut(|cell| {
        cell.set(enabled);
    });
}
