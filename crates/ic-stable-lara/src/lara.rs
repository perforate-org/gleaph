//! LARA store primitives.
//!
//! This module owns the persistence primitives shared by the labeled graph layer:
//! the vertex column, edge slab, per-segment overflow logs, segment counts,
//! segment span metadata, free span manager, and deferred-maintenance contracts.
//!
//! The scan / update boundary contracts live with the store implementations:
//! clean scans read one vertex row plus **live** slab records and the core
//! overflow log; update paths additionally use CSR neighbor bases, PMA counts,
//! and slab geometry to decide slab insert windows and relocation.

#[expect(
    dead_code,
    reason = "edge store includes maintenance helpers used by feature-specific paths"
)]
pub mod edge;
#[expect(
    dead_code,
    reason = "inline property bytes log helpers are used by targeted edge-value maintenance paths"
)]
pub mod edge_inline_property;
pub mod maintenance;
pub mod operation_error;
mod reserved;
pub mod vertex;

use crate::{
    VertexId,
    lara::{operation_error::VertexAccess, vertex::VertexStore},
    traits::CsrVertex,
};
use ic_stable_structures::Memory;

impl<V: CsrVertex, M: Memory> VertexAccess<V> for VertexStore<V, M> {
    fn len(&self) -> u32 {
        self.len()
    }

    fn get(&self, id: VertexId) -> V {
        self.get(id)
    }

    fn set(&self, id: VertexId, item: &V) {
        self.set(id, item);
    }
}
