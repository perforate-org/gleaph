//! Property-index mutation telemetry types for diagnostics and integration tests.
//!
//! The persisted index is a single [`StableBTreeMap`](ic_stable_structures::StableBTreeMap); these
//! kinds describe logical mutation steps on the in-memory node-store view.

use super::PropertyIndexNodeId;

/// Difference summary between two persisted node-store states (retained for projection shapes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyIndexNodeStoreDelta {
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
}

impl PropertyIndexNodeStoreDelta {
    pub const fn empty() -> Self {
        Self {
            touched_node_ids: Vec::new(),
            allocated_node_ids: Vec::new(),
            freed_node_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyIndexNodeStoreMutationKind {
    LocalUpdate,
    Redistribute,
    ThreeLeafRepack,
    Split,
    Merge,
    Collapse,
    Rebuild,
}
