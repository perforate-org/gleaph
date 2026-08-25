//! Sole owner of the `VECTOR_INDEX_DEFS` stable-memory region.
//!
//! The region is a destructive pre-release cutover from the clustered hash map to the linear hash
//! map.  Static construction deliberately leaves it unbound: install is the only create path and
//! post-upgrade is the only open path.  Callers can therefore never mistake an unavailable region
//! for an empty index catalog.  The heap-owner state machine, binding protocol, and reset ticket
//! come from the shared region owner ([`super::region_store`]); the lifecycle tests below cover
//! that shared machinery through this concrete instantiation.

use super::memory;
#[cfg(any(test, feature = "canbench"))]
use super::region_store::RegionResetTicket;
use super::region_store::{RegionError, RegionOwner};
use crate::records::VectorIndexDef;
use std::cell::RefCell;

/// Opaque definition-region reset ticket. Construction is private so consumers cannot fabricate
/// a reset fence; the coordinated Vector owner must acquire every other coupled region handle
/// before committing it.
#[cfg(any(test, feature = "canbench"))]
pub(crate) struct DefinitionResetTicket(RegionResetTicket);

type DefinitionOwner = RegionOwner<u32, VectorIndexDef>;

thread_local! {
    static DEFINITION_STORE: RefCell<DefinitionOwner> = RefCell::new(DefinitionOwner::default());
}

/// Strict fresh-install bind.  This never opens or resets nonempty memory.
pub(crate) fn create_for_install(hash_seed: u64) -> Result<(), RegionError> {
    DEFINITION_STORE
        .with_borrow_mut(|owner| owner.create_for_install(memory::defs_memory(), hash_seed))
}

/// Seed-free exact reopen after upgrade.  A failure is retained as `Unavailable` and no fallback
/// create or reset is attempted.
pub(crate) fn open_after_upgrade() -> Result<(), RegionError> {
    DEFINITION_STORE.with_borrow_mut(|owner| owner.open_after_upgrade(memory::defs_memory()))
}

pub(crate) fn get(index_id: u32) -> Result<Option<VectorIndexDef>, RegionError> {
    DEFINITION_STORE.with_borrow(|owner| owner.get(&index_id))
}

pub(crate) fn insert(
    index_id: u32,
    definition: VectorIndexDef,
) -> Result<Option<VectorIndexDef>, RegionError> {
    DEFINITION_STORE.with_borrow(|owner| owner.insert(index_id, definition))
}

/// Preflights a caller-supplied ownership fence and returns an unforgeable commit ticket.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn prepare_reset(
    expected_incarnation: u64,
) -> Result<DefinitionResetTicket, RegionError> {
    DEFINITION_STORE
        .with_borrow(|owner| owner.prepare_reset(expected_incarnation))
        .map(DefinitionResetTicket)
}

/// Commits a previously preflighted reset after the Vector owner has acquired every coupled handle.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn commit_reset(ticket: DefinitionResetTicket) -> Result<u64, RegionError> {
    DEFINITION_STORE.with_borrow(|owner| owner.commit_reset(ticket.0))
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn bind_for_fixture(hash_seed: u64) -> Result<(), RegionError> {
    DEFINITION_STORE
        .with_borrow_mut(|owner| owner.bind_for_fixture(memory::defs_memory(), hash_seed))
}

/// Returns the current owner incarnation for test/benchmark fixture coordination.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn incarnation_for_test_or_bench() -> Result<u64, RegionError> {
    DEFINITION_STORE.with_borrow(RegionOwner::incarnation_for_test_or_bench)
}

#[cfg(test)]
pub(crate) fn reopen_for_test() -> Result<(), RegionError> {
    DEFINITION_STORE.with_borrow_mut(|owner| owner.reopen_for_test(memory::defs_memory()))
}

/// Simulates the post-upgrade pre-open state without fabricating an owner error. The backing
/// MemoryId 4 bytes stay untouched and [`reopen_for_test`] restores the exact-open binding.
///
/// This seam is deliberately limited to a live `Ready` owner. It cannot hide an already
/// unavailable store or silently rebind an uninitialized owner, which keeps the PocketIC failure
/// path distinct from production lifecycle handling.
#[cfg(any(test, feature = "pocket-ic-e2e"))]
pub(crate) fn unbind_for_test() -> Result<(), &'static str> {
    DEFINITION_STORE.with_borrow_mut(|owner| {
        owner
            .unbind_if_ready_for_test()
            .map_err(|_| "definition store owner is not Ready")
    })
}

#[cfg(test)]
mod tests {
    use super::super::region_store::RegionUnavailableReason;
    use super::*;
    use ic_stable_linear_hash_map::{InitError, ResetError};
    use ic_stable_memory_backend::default_memory_impl;
    use ic_stable_structures::Memory as _;
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};

    const CONTROL_MUTATION_EPOCH_OFFSET: u64 = 128 + 16;
    const CONTROL_INCARNATION_OFFSET: u64 = 128 + 24;

    fn test_memory() -> memory::Memory {
        MemoryManager::init(default_memory_impl()).get(MemoryId::new(4))
    }

    fn test_owner() -> RegionOwner<u32, VectorIndexDef> {
        RegionOwner::detached_for_test()
    }

    fn test_definition() -> VectorIndexDef {
        VectorIndexDef {
            kind: gleaph_graph_kernel::vector_index::VectorIndexKind::IvfFlat,
            encoding: gleaph_graph_kernel::vector_index::VectorEncoding::F32,
            dims: 3,
            metric: gleaph_graph_kernel::vector_index::VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: 1,
            stride_bytes: 12,
            pad_stride_bytes: 16,
            meta_stride_bytes: 4,
            run_capacity: 1,
            max_page_bytes: 64,
            slots_per_page: 2,
            levels: crate::records::LEVELS_FLAT,
            nlist_fine: 1,
            code_tier: false,
            code_stride_bytes: 0,
            rotation_seed: 0,
        }
    }

    #[test]
    fn uninitialized_reads_and_writes_fail_without_allocating_memory() {
        let memory = test_memory();
        let store = test_owner();
        let error = RegionError::Unavailable(RegionUnavailableReason::Uninitialized);

        assert_eq!(store.get(&9), Err(error));
        assert_eq!(store.insert(9, test_definition()), Err(error));
        assert_eq!(memory.size(), 0);
    }

    #[test]
    fn strict_create_rejects_nonempty_region_without_writing() {
        let memory = test_memory();
        let mut first = test_owner();
        first
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");

        let bytes_before = snapshot(&memory);
        let mut second = test_owner();
        assert_eq!(
            second.create_for_install(memory.clone(), 0x5678),
            Err(RegionError::Unavailable(
                RegionUnavailableReason::OpenRejected(InitError::NonEmptyMemory)
            ))
        );
        assert_eq!(snapshot(&memory), bytes_before);
    }

    #[test]
    fn second_install_is_rejected_after_a_live_binding_without_writing() {
        let memory = test_memory();
        let mut installed = test_owner();
        installed
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");
        installed.insert(9, test_definition()).expect("insert");

        assert_eq!(
            installed.create_for_install(memory.clone(), 0x5678),
            Err(RegionError::Unavailable(
                RegionUnavailableReason::AlreadyInitialized
            ))
        );
        assert_eq!(
            installed.open_after_upgrade(memory.clone()),
            Err(RegionError::Unavailable(
                RegionUnavailableReason::AlreadyInitialized
            ))
        );
        assert_eq!(installed.get(&9).expect("get"), Some(test_definition()));
    }

    #[test]
    fn exact_open_reuses_persisted_seed_and_definition() {
        let memory = test_memory();
        let definition = test_definition();
        let mut installed = test_owner();
        installed
            .create_for_install(memory.clone(), 0xfeed_cafe)
            .expect("create");
        installed.insert(9, definition).expect("insert");

        let mut reopened = test_owner();
        reopened.open_after_upgrade(memory).expect("exact open");
        assert_eq!(reopened.get(&9).expect("get"), Some(definition));
    }

    #[test]
    fn exact_open_on_incompatible_bytes_becomes_unavailable_without_writing() {
        let memory = test_memory();
        assert_eq!(memory.grow(1), 0);
        memory.write(0, b"CHM");
        let bytes_before = snapshot(&memory);

        let mut store = test_owner();
        let rejected =
            RegionError::Unavailable(RegionUnavailableReason::OpenRejected(InitError::BadMagic {
                actual: *b"CHM",
            }));
        assert_eq!(store.open_after_upgrade(memory.clone()), Err(rejected));
        assert_eq!(store.get(&1), Err(rejected));
        assert_eq!(snapshot(&memory), bytes_before);
    }

    #[test]
    fn reset_prepare_rejects_mismatch_without_writing() {
        let memory = test_memory();
        let mut store = test_owner();
        store
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");
        store.insert(9, test_definition()).expect("insert");
        let before = snapshot(&memory);

        assert_eq!(
            store.prepare_reset(2),
            Err(RegionError::Reset(ResetError::IncarnationMismatch {
                current: 1
            }))
        );
        assert_eq!(snapshot(&memory), before);
        assert_eq!(store.get(&9).expect("get"), Some(test_definition()));
    }

    #[test]
    fn reset_prepare_unavailable_writes_nothing() {
        let memory = test_memory();
        assert_eq!(memory.grow(1), 0);
        memory.write(0, b"CHM");
        let mut store = test_owner();
        store
            .open_after_upgrade(memory.clone())
            .expect_err("open rejects incompatible bytes");
        let before = snapshot(&memory);

        assert_eq!(
            store.prepare_reset(1),
            Err(RegionError::Unavailable(
                RegionUnavailableReason::OpenRejected(InitError::BadMagic { actual: *b"CHM" })
            ))
        );
        assert_eq!(snapshot(&memory), before);
    }

    #[test]
    fn reset_prepare_epoch_exhaustion_writes_nothing() {
        let memory = test_memory();
        let mut store = test_owner();
        store
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");
        memory.write(CONTROL_MUTATION_EPOCH_OFFSET, &(u64::MAX - 1).to_le_bytes());
        let before = snapshot(&memory);

        assert_eq!(
            store.prepare_reset(1),
            Err(RegionError::Reset(ResetError::EpochExhausted))
        );
        assert_eq!(snapshot(&memory), before);
    }

    #[test]
    fn reset_prepare_incarnation_exhaustion_writes_nothing() {
        let memory = test_memory();
        let mut store = test_owner();
        store
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");
        memory.write(CONTROL_INCARNATION_OFFSET, &u64::MAX.to_le_bytes());
        let before = snapshot(&memory);

        assert_eq!(
            store.prepare_reset(u64::MAX),
            Err(RegionError::Reset(ResetError::IncarnationExhausted))
        );
        assert_eq!(snapshot(&memory), before);
    }

    fn snapshot(memory: &memory::Memory) -> Vec<u8> {
        let mut bytes = vec![0; memory.size() as usize * 65_536];
        memory.read(0, &mut bytes);
        bytes
    }
}
