use gleaph_graph_kernel::entry::VertexEdgeId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

pub struct VertexEdgeIdAllocator<M: Memory> {
    next_ids: StableBTreeMap<u32, VertexEdgeId, M>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexEdgeIdAllocatorError {
    Exhausted { owner_vertex_id: VertexId },
}

impl fmt::Display for VertexEdgeIdAllocatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted { owner_vertex_id } => {
                let owner = u32::from_le_bytes(owner_vertex_id.to_le_bytes());
                write!(f, "vertex edge ids are exhausted for owner vertex {owner}")
            }
        }
    }
}

impl std::error::Error for VertexEdgeIdAllocatorError {}

impl<M: Memory> VertexEdgeIdAllocator<M> {
    pub fn init(memory: M) -> Self {
        Self {
            next_ids: StableBTreeMap::init(memory),
        }
    }

    pub fn allocate_for_owner(
        &mut self,
        owner_vertex_id: VertexId,
    ) -> Result<VertexEdgeId, VertexEdgeIdAllocatorError> {
        let owner = owner_key(owner_vertex_id);
        let next = self
            .next_ids
            .get(&owner)
            .unwrap_or_else(|| VertexEdgeId::from_raw(1));

        if next.raw() == 0 {
            return Err(VertexEdgeIdAllocatorError::Exhausted { owner_vertex_id });
        }

        let following = next
            .raw()
            .checked_add(1)
            .map(VertexEdgeId::from_raw)
            .unwrap_or_default();
        self.next_ids.insert(owner, following);
        Ok(next)
    }

    pub fn allocate_directed(
        &mut self,
        source_vertex_id: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        let id = self.allocate_for_owner(source_vertex_id)?;
        Ok((source_vertex_id, id))
    }

    pub fn allocate_undirected(
        &mut self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        let owner = canonical_undirected_owner(endpoint_a, endpoint_b);
        let id = self.allocate_for_owner(owner)?;
        Ok((owner, id))
    }

    pub fn next_for_owner(&self, owner_vertex_id: VertexId) -> Option<VertexEdgeId> {
        self.next_ids.get(&owner_key(owner_vertex_id))
    }

    pub fn into_memory(self) -> M {
        self.next_ids.into_memory()
    }
}

/// Picks the endpoint with the greater numeric key (`VertexId` interpreted as little-endian `u32`)
/// so undirected edge ids are allocated on the higher-numbered vertex (spreading load when ids grow over time).
pub fn canonical_undirected_owner(endpoint_a: VertexId, endpoint_b: VertexId) -> VertexId {
    if owner_key(endpoint_a) >= owner_key(endpoint_b) {
        endpoint_a
    } else {
        endpoint_b
    }
}

fn owner_key(vertex_id: VertexId) -> u32 {
    u32::from_le_bytes(vertex_id.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn allocator() -> VertexEdgeIdAllocator<VectorMemory> {
        VertexEdgeIdAllocator::init(VectorMemory::default())
    }

    #[test]
    fn allocates_ids_per_owner_from_one() {
        let mut allocator = allocator();
        let alice = VertexId::from(7);
        let bob = VertexId::from(8);

        assert_eq!(
            allocator.allocate_for_owner(alice).unwrap(),
            VertexEdgeId::from_raw(1)
        );
        assert_eq!(
            allocator.allocate_for_owner(alice).unwrap(),
            VertexEdgeId::from_raw(2)
        );
        assert_eq!(
            allocator.allocate_for_owner(bob).unwrap(),
            VertexEdgeId::from_raw(1)
        );
        assert_eq!(
            allocator.next_for_owner(alice),
            Some(VertexEdgeId::from_raw(3))
        );
        assert_eq!(
            allocator.next_for_owner(bob),
            Some(VertexEdgeId::from_raw(2))
        );
    }

    #[test]
    fn directed_owner_is_source_vertex() {
        let mut allocator = allocator();
        let source = VertexId::from(7);

        assert_eq!(
            allocator.allocate_directed(source).unwrap(),
            (source, VertexEdgeId::from_raw(1))
        );
    }

    #[test]
    fn undirected_owner_is_higher_endpoint() {
        let mut allocator = allocator();
        let high = VertexId::from(10);
        let low = VertexId::from(3);

        assert_eq!(canonical_undirected_owner(high, low), high);
        assert_eq!(
            allocator.allocate_undirected(high, low).unwrap(),
            (high, VertexEdgeId::from_raw(1))
        );
    }

    #[test]
    fn persists_next_ids_across_reopen() {
        let mut allocator = allocator();
        let owner = VertexId::from(7);

        allocator.allocate_for_owner(owner).unwrap();
        allocator.allocate_for_owner(owner).unwrap();
        let memory = allocator.into_memory();

        let mut reopened = VertexEdgeIdAllocator::init(memory);

        assert_eq!(
            reopened.allocate_for_owner(owner).unwrap(),
            VertexEdgeId::from_raw(3)
        );
    }

    #[test]
    fn allocates_max_id_then_marks_owner_exhausted() {
        let mut allocator = allocator();
        let owner = VertexId::from(7);

        allocator
            .next_ids
            .insert(owner_key(owner), VertexEdgeId::from_raw(u32::MAX));

        assert_eq!(
            allocator.allocate_for_owner(owner).unwrap(),
            VertexEdgeId::from_raw(u32::MAX)
        );
        assert_eq!(
            allocator.allocate_for_owner(owner),
            Err(VertexEdgeIdAllocatorError::Exhausted {
                owner_vertex_id: owner
            })
        );
    }
}
