//! Sole owner of the `VECTOR_INDEX_DEFS` stable-memory region.
//!
//! The region is a destructive pre-release cutover from the clustered hash map to the linear hash
//! map.  Static construction deliberately leaves it unbound: install is the only create path and
//! post-upgrade is the only open path.  Callers can therefore never mistake an unavailable region
//! for an empty index catalog.

use super::memory::{self, Memory};
use crate::records::VectorIndexDef;
#[cfg(any(test, feature = "canbench"))]
use ic_stable_linear_hash_map::ResetError;
use ic_stable_linear_hash_map::{InitError, MutationError, StableLinearHashMap};
#[cfg(any(test, feature = "canbench"))]
use ic_stable_structures::Memory as _;
use std::cell::RefCell;

type StableDefsMap = StableLinearHashMap<u32, VectorIndexDef, Memory>;

/// Why the definition region cannot currently serve requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefinitionStoreUnavailableReason {
    Uninitialized,
    AlreadyInitialized,
    NonEmptyMemory,
    BadMagic,
    IncompatibleVersion,
    IncompatibleElementType,
    IncompatibleKeyStorageSchema,
    IncompatibleKeyRoutingSchema,
    IncompatibleValueStorageSchema,
    InvalidLayout,
    RecoveryRequired,
    OutOfMemory,
}

/// A point operation that did not become a terminal definition-table admission result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefinitionStoreMutationError {
    InProgress,
    EpochExhausted,
    InvalidKeyEncoding,
    InvalidValueEncoding,
    OutOfMemory,
    CapacityOverflow,
}

/// A coordinated owner reset failed before changing the definition region.
#[cfg(any(test, feature = "canbench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefinitionStoreResetError {
    IncarnationMismatch { current: u64 },
    IncarnationExhausted,
    InProgress,
    EpochExhausted,
    CapacityOverflow,
}

/// Definition-store failures preserve table pressure separately from availability and retryable
/// mutation failures.  The legacy endpoint still projects these to its existing error surface;
/// the later typed batch endpoint will consume `TablePressure` directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefinitionStoreError {
    Unavailable(DefinitionStoreUnavailableReason),
    TablePressure,
    Mutation(DefinitionStoreMutationError),
    #[cfg(any(test, feature = "canbench"))]
    Reset(DefinitionStoreResetError),
}

/// Owner-issued proof that the definition region passed reset preflight at one incarnation.
///
/// Construction is private so consumers cannot fabricate a reset fence. The coordinated Vector
/// owner must acquire every other coupled region handle before committing this ticket.
#[cfg(any(test, feature = "canbench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DefinitionResetTicket {
    expected_incarnation: u64,
}

#[derive(Default)]
enum DefinitionStoreState {
    #[default]
    Uninitialized,
    Ready(Box<StableDefsMap>),
    Unavailable(DefinitionStoreUnavailableReason),
}

/// Heap owner for the one MemoryId 4 binding.
///
/// The state machine itself is intentionally private.  Consumers can only make point operations
/// through the module functions below, which guarantees the unavailable boundary is observed by
/// every production access.
#[derive(Default)]
struct DefinitionStore {
    state: DefinitionStoreState,
}

impl DefinitionStore {
    fn create_for_install(
        &mut self,
        memory: Memory,
        hash_seed: u64,
    ) -> Result<(), DefinitionStoreError> {
        if !matches!(self.state, DefinitionStoreState::Uninitialized) {
            return Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::AlreadyInitialized,
            ));
        }
        self.bind(StableDefsMap::create(memory, hash_seed))
    }

    fn open_after_upgrade(&mut self, memory: Memory) -> Result<(), DefinitionStoreError> {
        if !matches!(self.state, DefinitionStoreState::Uninitialized) {
            return Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::AlreadyInitialized,
            ));
        }
        self.bind(StableDefsMap::open(memory))
    }

    fn bind(
        &mut self,
        result: Result<StableDefsMap, InitError>,
    ) -> Result<(), DefinitionStoreError> {
        match result {
            Ok(map) => {
                self.state = DefinitionStoreState::Ready(Box::new(map));
                Ok(())
            }
            Err(error) => {
                let reason = unavailable_reason(&error);
                self.state = DefinitionStoreState::Unavailable(reason);
                Err(DefinitionStoreError::Unavailable(reason))
            }
        }
    }

    fn get(&self, index_id: u32) -> Result<Option<VectorIndexDef>, DefinitionStoreError> {
        match &self.state {
            DefinitionStoreState::Uninitialized => Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::Uninitialized,
            )),
            DefinitionStoreState::Unavailable(reason) => {
                Err(DefinitionStoreError::Unavailable(*reason))
            }
            DefinitionStoreState::Ready(map) => map.get(&index_id).map_err(mutation_error),
        }
    }

    fn insert(
        &self,
        index_id: u32,
        definition: VectorIndexDef,
    ) -> Result<Option<VectorIndexDef>, DefinitionStoreError> {
        match &self.state {
            DefinitionStoreState::Uninitialized => Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::Uninitialized,
            )),
            DefinitionStoreState::Unavailable(reason) => {
                Err(DefinitionStoreError::Unavailable(*reason))
            }
            DefinitionStoreState::Ready(map) => {
                map.insert(index_id, definition).map_err(mutation_error)
            }
        }
    }

    #[cfg(any(test, feature = "canbench"))]
    fn prepare_reset(
        &self,
        expected_incarnation: u64,
    ) -> Result<DefinitionResetTicket, DefinitionStoreError> {
        let map = match &self.state {
            DefinitionStoreState::Uninitialized => {
                return Err(DefinitionStoreError::Unavailable(
                    DefinitionStoreUnavailableReason::Uninitialized,
                ));
            }
            DefinitionStoreState::Unavailable(reason) => {
                return Err(DefinitionStoreError::Unavailable(*reason));
            }
            DefinitionStoreState::Ready(map) => map,
        };
        let control = map.control_region().map_err(mutation_error)?;
        if control.incarnation != expected_incarnation {
            return Err(DefinitionStoreError::Reset(
                DefinitionStoreResetError::IncarnationMismatch {
                    current: control.incarnation,
                },
            ));
        }
        control
            .incarnation
            .checked_add(1)
            .ok_or(DefinitionStoreError::Reset(
                DefinitionStoreResetError::IncarnationExhausted,
            ))?;
        control
            .mutation_epoch
            .checked_add(2)
            .ok_or(DefinitionStoreError::Reset(
                DefinitionStoreResetError::EpochExhausted,
            ))?;
        Ok(DefinitionResetTicket {
            expected_incarnation,
        })
    }

    #[cfg(any(test, feature = "canbench"))]
    fn commit_reset(&self, ticket: DefinitionResetTicket) -> Result<u64, DefinitionStoreError> {
        let map = match &self.state {
            DefinitionStoreState::Uninitialized => {
                return Err(DefinitionStoreError::Unavailable(
                    DefinitionStoreUnavailableReason::Uninitialized,
                ));
            }
            DefinitionStoreState::Unavailable(reason) => {
                return Err(DefinitionStoreError::Unavailable(*reason));
            }
            DefinitionStoreState::Ready(map) => map,
        };
        map.reset(ticket.expected_incarnation).map_err(reset_error)
    }

    #[cfg(any(test, feature = "canbench"))]
    fn bind_for_fixture(
        &mut self,
        memory: Memory,
        hash_seed: u64,
    ) -> Result<(), DefinitionStoreError> {
        match self.state {
            DefinitionStoreState::Uninitialized if memory.size() == 0 => {
                self.create_for_install(memory, hash_seed)
            }
            DefinitionStoreState::Uninitialized => self.open_after_upgrade(memory),
            DefinitionStoreState::Ready(_) => Ok(()),
            DefinitionStoreState::Unavailable(reason) => {
                Err(DefinitionStoreError::Unavailable(reason))
            }
        }
    }

    #[cfg(test)]
    fn reopen_for_test(&mut self, memory: Memory) -> Result<(), DefinitionStoreError> {
        self.state = DefinitionStoreState::Uninitialized;
        self.open_after_upgrade(memory)
    }
}

fn unavailable_reason(error: &InitError) -> DefinitionStoreUnavailableReason {
    match error {
        InitError::NonEmptyMemory => DefinitionStoreUnavailableReason::NonEmptyMemory,
        InitError::BadMagic { .. } => DefinitionStoreUnavailableReason::BadMagic,
        InitError::IncompatibleVersion(_) => DefinitionStoreUnavailableReason::IncompatibleVersion,
        InitError::IncompatibleElementType => {
            DefinitionStoreUnavailableReason::IncompatibleElementType
        }
        InitError::IncompatibleKeyStorageSchema => {
            DefinitionStoreUnavailableReason::IncompatibleKeyStorageSchema
        }
        InitError::IncompatibleKeyRoutingSchema => {
            DefinitionStoreUnavailableReason::IncompatibleKeyRoutingSchema
        }
        InitError::IncompatibleValueStorageSchema => {
            DefinitionStoreUnavailableReason::IncompatibleValueStorageSchema
        }
        InitError::InvalidLayout => DefinitionStoreUnavailableReason::InvalidLayout,
        InitError::RecoveryRequired => DefinitionStoreUnavailableReason::RecoveryRequired,
        InitError::OutOfMemory => DefinitionStoreUnavailableReason::OutOfMemory,
    }
}

fn mutation_error(error: MutationError) -> DefinitionStoreError {
    match error {
        MutationError::TablePressure => DefinitionStoreError::TablePressure,
        MutationError::InProgress => {
            DefinitionStoreError::Mutation(DefinitionStoreMutationError::InProgress)
        }
        MutationError::EpochExhausted => {
            DefinitionStoreError::Mutation(DefinitionStoreMutationError::EpochExhausted)
        }
        MutationError::InvalidKeyEncoding => {
            DefinitionStoreError::Mutation(DefinitionStoreMutationError::InvalidKeyEncoding)
        }
        MutationError::InvalidValueEncoding => {
            DefinitionStoreError::Mutation(DefinitionStoreMutationError::InvalidValueEncoding)
        }
        MutationError::OutOfMemory => {
            DefinitionStoreError::Mutation(DefinitionStoreMutationError::OutOfMemory)
        }
        MutationError::CapacityOverflow => {
            DefinitionStoreError::Mutation(DefinitionStoreMutationError::CapacityOverflow)
        }
    }
}

#[cfg(any(test, feature = "canbench"))]
fn reset_error(error: ResetError) -> DefinitionStoreError {
    let error = match error {
        ResetError::IncarnationMismatch { current } => {
            DefinitionStoreResetError::IncarnationMismatch { current }
        }
        ResetError::IncarnationExhausted => DefinitionStoreResetError::IncarnationExhausted,
        ResetError::InProgress => DefinitionStoreResetError::InProgress,
        ResetError::EpochExhausted => DefinitionStoreResetError::EpochExhausted,
        ResetError::CapacityOverflow => DefinitionStoreResetError::CapacityOverflow,
    };
    DefinitionStoreError::Reset(error)
}

thread_local! {
    static DEFINITION_STORE: RefCell<DefinitionStore> = RefCell::new(DefinitionStore::default());
}

/// Strict fresh-install bind.  This never opens or resets nonempty memory.
pub(crate) fn create_for_install(hash_seed: u64) -> Result<(), DefinitionStoreError> {
    DEFINITION_STORE
        .with_borrow_mut(|store| store.create_for_install(memory::defs_memory(), hash_seed))
}

/// Seed-free exact reopen after upgrade.  A failure is retained as `Unavailable` and no fallback
/// create or reset is attempted.
pub(crate) fn open_after_upgrade() -> Result<(), DefinitionStoreError> {
    DEFINITION_STORE.with_borrow_mut(|store| store.open_after_upgrade(memory::defs_memory()))
}

pub(crate) fn get(index_id: u32) -> Result<Option<VectorIndexDef>, DefinitionStoreError> {
    DEFINITION_STORE.with_borrow(|store| store.get(index_id))
}

pub(crate) fn insert(
    index_id: u32,
    definition: VectorIndexDef,
) -> Result<Option<VectorIndexDef>, DefinitionStoreError> {
    DEFINITION_STORE.with_borrow(|store| store.insert(index_id, definition))
}

/// Preflights a caller-supplied ownership fence and returns an unforgeable commit ticket.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn prepare_reset(
    expected_incarnation: u64,
) -> Result<DefinitionResetTicket, DefinitionStoreError> {
    DEFINITION_STORE.with_borrow(|store| store.prepare_reset(expected_incarnation))
}

/// Commits a previously preflighted reset after the Vector owner has acquired every coupled handle.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn commit_reset(ticket: DefinitionResetTicket) -> Result<u64, DefinitionStoreError> {
    DEFINITION_STORE.with_borrow(|store| store.commit_reset(ticket))
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn bind_for_fixture(hash_seed: u64) -> Result<(), DefinitionStoreError> {
    DEFINITION_STORE
        .with_borrow_mut(|store| store.bind_for_fixture(memory::defs_memory(), hash_seed))
}

/// Returns the current owner incarnation for test/benchmark fixture coordination.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn incarnation_for_test_or_bench() -> Result<u64, DefinitionStoreError> {
    DEFINITION_STORE.with_borrow(|store| match &store.state {
        DefinitionStoreState::Uninitialized => Err(DefinitionStoreError::Unavailable(
            DefinitionStoreUnavailableReason::Uninitialized,
        )),
        DefinitionStoreState::Unavailable(reason) => {
            Err(DefinitionStoreError::Unavailable(*reason))
        }
        DefinitionStoreState::Ready(map) => map
            .control_region()
            .map(|control| control.incarnation)
            .map_err(mutation_error),
    })
}

#[cfg(test)]
pub(crate) fn reopen_for_test() -> Result<(), DefinitionStoreError> {
    DEFINITION_STORE.with_borrow_mut(|store| store.reopen_for_test(memory::defs_memory()))
}

/// Simulates the post-upgrade pre-open state without fabricating an owner error. The backing
/// MemoryId 4 bytes stay untouched and [`reopen_for_test`] restores the exact-open binding.
///
/// This seam is deliberately limited to a live `Ready` owner. It cannot hide an already
/// unavailable store or silently rebind an uninitialized owner, which keeps the PocketIC failure
/// path distinct from production lifecycle handling.
#[cfg(any(test, feature = "pocket-ic-e2e"))]
pub(crate) fn unbind_for_test() -> Result<(), &'static str> {
    DEFINITION_STORE.with_borrow_mut(|store| {
        if !matches!(&store.state, DefinitionStoreState::Ready(_)) {
            return Err("definition store owner is not Ready");
        }
        store.state = DefinitionStoreState::Uninitialized;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_memory_backend::default_memory_impl;
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};

    const CONTROL_MUTATION_EPOCH_OFFSET: u64 = 128 + 16;
    const CONTROL_INCARNATION_OFFSET: u64 = 128 + 24;

    fn test_memory() -> Memory {
        MemoryManager::init(default_memory_impl()).get(MemoryId::new(4))
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
        }
    }

    #[test]
    fn uninitialized_reads_and_writes_fail_without_allocating_memory() {
        let memory = test_memory();
        let store = DefinitionStore::default();
        let error =
            DefinitionStoreError::Unavailable(DefinitionStoreUnavailableReason::Uninitialized);

        assert_eq!(store.get(9), Err(error));
        assert_eq!(store.insert(9, test_definition()), Err(error));
        assert_eq!(memory.size(), 0);
    }

    #[test]
    fn strict_create_rejects_nonempty_region_without_writing() {
        let memory = test_memory();
        let mut first = DefinitionStore::default();
        first
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");

        let bytes_before = {
            let mut bytes = vec![0; memory.size() as usize * 65_536];
            memory.read(0, &mut bytes);
            bytes
        };
        let mut second = DefinitionStore::default();
        assert_eq!(
            second.create_for_install(memory.clone(), 0x5678),
            Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::NonEmptyMemory
            ))
        );
        let mut bytes_after = vec![0; memory.size() as usize * 65_536];
        memory.read(0, &mut bytes_after);
        assert_eq!(bytes_after, bytes_before);
    }

    #[test]
    fn exact_open_reuses_persisted_seed_and_definition() {
        let memory = test_memory();
        let definition = test_definition();
        let mut installed = DefinitionStore::default();
        installed
            .create_for_install(memory.clone(), 0xfeed_cafe)
            .expect("create");
        installed.insert(9, definition).expect("insert");

        let mut reopened = DefinitionStore::default();
        reopened.open_after_upgrade(memory).expect("exact open");
        assert_eq!(reopened.get(9).expect("get"), Some(definition));
    }

    #[test]
    fn exact_open_on_incompatible_bytes_becomes_unavailable_without_writing() {
        let memory = test_memory();
        assert_eq!(memory.grow(1), 0);
        memory.write(0, b"CHM");
        let mut bytes_before = vec![0; 65_536];
        memory.read(0, &mut bytes_before);

        let mut store = DefinitionStore::default();
        assert_eq!(
            store.open_after_upgrade(memory.clone()),
            Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::BadMagic
            ))
        );
        assert_eq!(
            store.get(1),
            Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::BadMagic
            ))
        );
        let mut bytes_after = vec![0; 65_536];
        memory.read(0, &mut bytes_after);
        assert_eq!(bytes_after, bytes_before);
    }

    #[test]
    fn reset_prepare_rejects_mismatch_without_writing() {
        let memory = test_memory();
        let mut store = DefinitionStore::default();
        store
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");
        store.insert(9, test_definition()).expect("insert");
        let before = snapshot(&memory);

        assert_eq!(
            store.prepare_reset(2),
            Err(DefinitionStoreError::Reset(
                DefinitionStoreResetError::IncarnationMismatch { current: 1 }
            ))
        );
        assert_eq!(snapshot(&memory), before);
        assert_eq!(store.get(9).expect("get"), Some(test_definition()));
    }

    #[test]
    fn reset_prepare_unavailable_writes_nothing() {
        let memory = test_memory();
        let store = DefinitionStore {
            state: DefinitionStoreState::Unavailable(
                DefinitionStoreUnavailableReason::RecoveryRequired,
            ),
        };

        assert_eq!(
            store.prepare_reset(1),
            Err(DefinitionStoreError::Unavailable(
                DefinitionStoreUnavailableReason::RecoveryRequired
            ))
        );
        assert_eq!(memory.size(), 0);
    }

    #[test]
    fn reset_prepare_epoch_exhaustion_writes_nothing() {
        let memory = test_memory();
        let mut store = DefinitionStore::default();
        store
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");
        memory.write(CONTROL_MUTATION_EPOCH_OFFSET, &(u64::MAX - 1).to_le_bytes());
        let before = snapshot(&memory);

        assert_eq!(
            store.prepare_reset(1),
            Err(DefinitionStoreError::Reset(
                DefinitionStoreResetError::EpochExhausted
            ))
        );
        assert_eq!(snapshot(&memory), before);
    }

    #[test]
    fn reset_prepare_incarnation_exhaustion_writes_nothing() {
        let memory = test_memory();
        let mut store = DefinitionStore::default();
        store
            .create_for_install(memory.clone(), 0x1234)
            .expect("create");
        memory.write(CONTROL_INCARNATION_OFFSET, &u64::MAX.to_le_bytes());
        let before = snapshot(&memory);

        assert_eq!(
            store.prepare_reset(u64::MAX),
            Err(DefinitionStoreError::Reset(
                DefinitionStoreResetError::IncarnationExhausted
            ))
        );
        assert_eq!(snapshot(&memory), before);
    }

    fn snapshot(memory: &Memory) -> Vec<u8> {
        let mut bytes = vec![0; memory.size() as usize * 65_536];
        memory.read(0, &mut bytes);
        bytes
    }
}
