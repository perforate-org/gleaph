use crate::test_support::TestEdge;
#[cfg(test)]
use ic_stable_structures::Memory;
use ic_stable_structures::{
    DefaultMemoryImpl,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};

pub(crate) const SMALL_N: u64 = 256;
pub(crate) const MEDIUM_N: u64 = 1024;
pub(crate) const LARGE_N: u64 = 4096;

pub(crate) type BenchMemory = VirtualMemory<DefaultMemoryImpl>;
/// Highest usable `MemoryId`; `u8::MAX` is reserved internally by `MemoryManager`.
pub(crate) const MEASUREMENT_MEMORY_ID_MAX: u8 = u8::MAX - 1;

#[allow(
    dead_code,
    reason = "ScanOnly and Published are consumed by the Plan 0147 fixture adapter"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MeasurementRepresentation {
    AliasOnly,
    ScanOnly,
    Published,
}

/// Owns one isolated measurement memory bundle. Each candidate gets a fresh manager and allocates
/// IDs from the high end so benchmark regions cannot overlap the production low-ID layout.
pub(crate) struct MeasurementMemoryBundle {
    manager: MemoryManager<DefaultMemoryImpl>,
    next_id: u8,
    allocated_ids: Vec<u8>,
    #[allow(
        dead_code,
        reason = "candidate tag is consumed by the Plan 0147 fixture adapter"
    )]
    representation: MeasurementRepresentation,
}

impl MeasurementMemoryBundle {
    pub(crate) fn new() -> Self {
        Self::with_representation(MeasurementRepresentation::AliasOnly)
    }

    pub(crate) fn with_representation(representation: MeasurementRepresentation) -> Self {
        Self {
            manager: MemoryManager::init(DefaultMemoryImpl::default()),
            // Bench-only regions are allocated from the top of the u8 MemoryId space so future
            // production layouts can continue allocating from the low end without collisions.
            next_id: MEASUREMENT_MEMORY_ID_MAX,
            allocated_ids: Vec::new(),
            representation,
        }
    }

    pub(crate) fn memory(&mut self) -> BenchMemory {
        let id = self.next_id;
        self.allocated_ids.push(id);
        self.next_id = self
            .next_id
            .checked_sub(1)
            .expect("benchmark memory id overflow");
        self.manager.get(MemoryId::new(id))
    }

    #[allow(
        dead_code,
        reason = "candidate tag is consumed by the Plan 0147 fixture adapter"
    )]
    pub(crate) const fn representation(&self) -> MeasurementRepresentation {
        self.representation
    }
}

pub(crate) type BenchMemoryFactory = MeasurementMemoryBundle;

#[inline]
pub(crate) fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline]
pub(crate) fn test_edge(seed: u64) -> TestEdge {
    TestEdge((splitmix64(seed) as u32) & 0x00ff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_bundles_are_independent_and_descend_from_max_id() {
        let mut alias =
            MeasurementMemoryBundle::with_representation(MeasurementRepresentation::AliasOnly);
        let mut published =
            MeasurementMemoryBundle::with_representation(MeasurementRepresentation::Published);
        let alias_memory = alias.memory();
        let published_memory = published.memory();
        let _alias_second_memory = alias.memory();
        let _published_second_memory = published.memory();

        assert_eq!(alias.representation(), MeasurementRepresentation::AliasOnly);
        assert_eq!(
            published.representation(),
            MeasurementRepresentation::Published
        );
        assert_eq!(
            alias.allocated_ids,
            vec![MEASUREMENT_MEMORY_ID_MAX, MEASUREMENT_MEMORY_ID_MAX - 1]
        );
        assert_eq!(
            published.allocated_ids,
            vec![MEASUREMENT_MEMORY_ID_MAX, MEASUREMENT_MEMORY_ID_MAX - 1]
        );
        assert_eq!(alias.next_id, MEASUREMENT_MEMORY_ID_MAX - 2);
        assert_eq!(published.next_id, MEASUREMENT_MEMORY_ID_MAX - 2);

        alias_memory.grow(1);
        assert_eq!(alias_memory.size(), 1);
        assert_eq!(published_memory.size(), 0);
    }
}
