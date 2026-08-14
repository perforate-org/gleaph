//! Sole owner of the Vector MemoryId 7 subject map.
//!
//! MemoryId 7 is a destructive pre-release cutover from the clustered hash map to the linear
//! hash map. Static construction never opens the region: fresh install calls strict `create` and
//! post-upgrade calls seed-free exact `open`. Every point access and physical scan is routed
//! through this owner so an unavailable region cannot be mistaken for an empty subject catalog.

use super::memory::{self, Memory};
use crate::records::{
    FixedSubjectMapEntry, SubjectKey, SubjectScanCursor, SubjectScanCursorError, SubjectScanScope,
};
#[cfg(test)]
use gleaph_graph_kernel::federation::ShardId;
#[cfg(test)]
use gleaph_graph_kernel::vector_index::VectorSubject;
#[cfg(any(test, feature = "canbench"))]
use ic_stable_linear_hash_map::ResetError;
use ic_stable_linear_hash_map::{
    InitError, MutationError, ScanError, ScanPage, StableLinearHashMap,
};
#[cfg(any(test, feature = "canbench"))]
use ic_stable_structures::Memory as _;
use std::cell::RefCell;

type StableSubjectsMap = StableLinearHashMap<SubjectKey, FixedSubjectMapEntry, Memory>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubjectStoreUnavailableReason {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubjectStoreMutationError {
    InProgress,
    EpochExhausted,
    RelocationGenerationExhausted,
    InvalidKeyEncoding,
    InvalidValueEncoding,
    OutOfMemory,
    CapacityOverflow,
}

#[cfg(any(test, feature = "canbench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubjectStoreResetError {
    IncarnationMismatch { current: u64 },
    IncarnationExhausted,
    InProgress,
    EpochExhausted,
    CapacityOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubjectStoreScanError {
    ZeroBudget,
    InvalidCursor,
    RestartRequired,
    InProgress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubjectStoreError {
    Unavailable(SubjectStoreUnavailableReason),
    TablePressure,
    Mutation(SubjectStoreMutationError),
    Scan(SubjectStoreScanError),
    #[cfg(any(test, feature = "canbench"))]
    Reset(SubjectStoreResetError),
}

#[cfg(any(test, feature = "canbench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SubjectResetTicket {
    expected_incarnation: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SubjectScanPage {
    pub entries: Vec<(SubjectKey, FixedSubjectMapEntry)>,
    pub next_cursor: SubjectScanCursor,
    pub examined_slots: u64,
    pub exhausted: bool,
}

#[derive(Default)]
enum SubjectStoreState {
    #[default]
    Uninitialized,
    Ready(Box<StableSubjectsMap>),
    Unavailable(SubjectStoreUnavailableReason),
}

#[derive(Default)]
struct SubjectStore {
    state: SubjectStoreState,
}

impl SubjectStore {
    fn create_for_install(
        &mut self,
        memory: Memory,
        hash_seed: u64,
    ) -> Result<(), SubjectStoreError> {
        if !matches!(self.state, SubjectStoreState::Uninitialized) {
            return Err(SubjectStoreError::Unavailable(
                SubjectStoreUnavailableReason::AlreadyInitialized,
            ));
        }
        self.bind(StableSubjectsMap::create(memory, hash_seed))
    }

    fn open_after_upgrade(&mut self, memory: Memory) -> Result<(), SubjectStoreError> {
        if !matches!(self.state, SubjectStoreState::Uninitialized) {
            return Err(SubjectStoreError::Unavailable(
                SubjectStoreUnavailableReason::AlreadyInitialized,
            ));
        }
        self.bind(StableSubjectsMap::open(memory))
    }

    fn bind(
        &mut self,
        result: Result<StableSubjectsMap, InitError>,
    ) -> Result<(), SubjectStoreError> {
        match result {
            Ok(map) => {
                self.state = SubjectStoreState::Ready(Box::new(map));
                Ok(())
            }
            Err(error) => {
                let reason = unavailable_reason(&error);
                self.state = SubjectStoreState::Unavailable(reason);
                Err(SubjectStoreError::Unavailable(reason))
            }
        }
    }

    fn map(&self) -> Result<&StableSubjectsMap, SubjectStoreError> {
        match &self.state {
            SubjectStoreState::Uninitialized => Err(SubjectStoreError::Unavailable(
                SubjectStoreUnavailableReason::Uninitialized,
            )),
            SubjectStoreState::Unavailable(reason) => Err(SubjectStoreError::Unavailable(*reason)),
            SubjectStoreState::Ready(map) => Ok(map),
        }
    }

    fn get(&self, key: &SubjectKey) -> Result<Option<FixedSubjectMapEntry>, SubjectStoreError> {
        self.map()?.get(key).map_err(mutation_error)
    }

    fn insert(
        &self,
        key: SubjectKey,
        value: FixedSubjectMapEntry,
    ) -> Result<Option<FixedSubjectMapEntry>, SubjectStoreError> {
        self.map()?.insert(key, value).map_err(mutation_error)
    }

    fn remove(&self, key: &SubjectKey) -> Result<Option<FixedSubjectMapEntry>, SubjectStoreError> {
        self.map()?.remove(key).map_err(mutation_error)
    }

    fn scan_start(&self, scope: SubjectScanScope) -> Result<SubjectScanCursor, SubjectStoreError> {
        let cursor = self.map()?.scan_start().map_err(scan_error)?;
        Ok(SubjectScanCursor::from_lhm(scope, cursor))
    }

    fn scan_step(
        &self,
        scope: SubjectScanScope,
        cursor: SubjectScanCursor,
        physical_slot_budget: u64,
    ) -> Result<SubjectScanPage, SubjectStoreError> {
        cursor.validate(scope).map_err(cursor_error)?;
        let page: ScanPage<SubjectKey, FixedSubjectMapEntry> = self
            .map()?
            .scan_step(
                cursor.lhm_cursor().map_err(cursor_error)?,
                physical_slot_budget,
            )
            .map_err(scan_error)?;
        let next_cursor = SubjectScanCursor::from_lhm(scope, page.next_cursor());
        let entries = page.entries().to_vec();
        Ok(SubjectScanPage {
            entries,
            next_cursor,
            examined_slots: page.examined_slots(),
            exhausted: page.exhausted(),
        })
    }

    #[cfg(any(test, feature = "canbench"))]
    fn prepare_reset(
        &self,
        expected_incarnation: u64,
    ) -> Result<SubjectResetTicket, SubjectStoreError> {
        let control = self.map()?.control_region().map_err(mutation_error)?;
        if control.incarnation != expected_incarnation {
            return Err(SubjectStoreError::Reset(
                SubjectStoreResetError::IncarnationMismatch {
                    current: control.incarnation,
                },
            ));
        }
        control
            .incarnation
            .checked_add(1)
            .ok_or(SubjectStoreError::Reset(
                SubjectStoreResetError::IncarnationExhausted,
            ))?;
        control
            .mutation_epoch
            .checked_add(2)
            .ok_or(SubjectStoreError::Reset(
                SubjectStoreResetError::EpochExhausted,
            ))?;
        Ok(SubjectResetTicket {
            expected_incarnation,
        })
    }

    #[cfg(any(test, feature = "canbench"))]
    fn commit_reset(&self, ticket: SubjectResetTicket) -> Result<u64, SubjectStoreError> {
        self.map()?
            .reset(ticket.expected_incarnation)
            .map_err(reset_error)
    }

    #[cfg(any(test, feature = "canbench"))]
    fn bind_for_fixture(
        &mut self,
        memory: Memory,
        hash_seed: u64,
    ) -> Result<(), SubjectStoreError> {
        match self.state {
            SubjectStoreState::Uninitialized if memory.size() == 0 => {
                self.create_for_install(memory, hash_seed)
            }
            SubjectStoreState::Uninitialized => self.open_after_upgrade(memory),
            SubjectStoreState::Ready(_) => Ok(()),
            SubjectStoreState::Unavailable(reason) => Err(SubjectStoreError::Unavailable(reason)),
        }
    }
}

fn unavailable_reason(error: &InitError) -> SubjectStoreUnavailableReason {
    match error {
        InitError::NonEmptyMemory => SubjectStoreUnavailableReason::NonEmptyMemory,
        InitError::BadMagic { .. } => SubjectStoreUnavailableReason::BadMagic,
        InitError::IncompatibleVersion(_) => SubjectStoreUnavailableReason::IncompatibleVersion,
        InitError::IncompatibleElementType => {
            SubjectStoreUnavailableReason::IncompatibleElementType
        }
        InitError::IncompatibleKeyStorageSchema => {
            SubjectStoreUnavailableReason::IncompatibleKeyStorageSchema
        }
        InitError::IncompatibleKeyRoutingSchema => {
            SubjectStoreUnavailableReason::IncompatibleKeyRoutingSchema
        }
        InitError::IncompatibleValueStorageSchema => {
            SubjectStoreUnavailableReason::IncompatibleValueStorageSchema
        }
        InitError::InvalidLayout => SubjectStoreUnavailableReason::InvalidLayout,
        InitError::RecoveryRequired => SubjectStoreUnavailableReason::RecoveryRequired,
        InitError::OutOfMemory => SubjectStoreUnavailableReason::OutOfMemory,
    }
}

fn mutation_error(error: MutationError) -> SubjectStoreError {
    match error {
        MutationError::TablePressure => SubjectStoreError::TablePressure,
        MutationError::InProgress => {
            SubjectStoreError::Mutation(SubjectStoreMutationError::InProgress)
        }
        MutationError::EpochExhausted => {
            SubjectStoreError::Mutation(SubjectStoreMutationError::EpochExhausted)
        }
        MutationError::RelocationGenerationExhausted => {
            SubjectStoreError::Mutation(SubjectStoreMutationError::RelocationGenerationExhausted)
        }
        MutationError::InvalidKeyEncoding => {
            SubjectStoreError::Mutation(SubjectStoreMutationError::InvalidKeyEncoding)
        }
        MutationError::InvalidValueEncoding => {
            SubjectStoreError::Mutation(SubjectStoreMutationError::InvalidValueEncoding)
        }
        MutationError::OutOfMemory => {
            SubjectStoreError::Mutation(SubjectStoreMutationError::OutOfMemory)
        }
        MutationError::CapacityOverflow => {
            SubjectStoreError::Mutation(SubjectStoreMutationError::CapacityOverflow)
        }
    }
}

fn scan_error(error: ScanError) -> SubjectStoreError {
    SubjectStoreError::Scan(match error {
        ScanError::ZeroBudget => SubjectStoreScanError::ZeroBudget,
        ScanError::InvalidCursor => SubjectStoreScanError::InvalidCursor,
        ScanError::RestartRequired => SubjectStoreScanError::RestartRequired,
        ScanError::InProgress => SubjectStoreScanError::InProgress,
    })
}

fn cursor_error(error: SubjectScanCursorError) -> SubjectStoreError {
    SubjectStoreError::Scan(match error {
        SubjectScanCursorError::Malformed
        | SubjectScanCursorError::VersionMismatch
        | SubjectScanCursorError::ScopeMismatch
        | SubjectScanCursorError::Lhm(ScanError::InvalidCursor) => {
            SubjectStoreScanError::InvalidCursor
        }
        SubjectScanCursorError::Lhm(ScanError::RestartRequired) => {
            SubjectStoreScanError::RestartRequired
        }
        SubjectScanCursorError::Lhm(ScanError::InProgress) => SubjectStoreScanError::InProgress,
        SubjectScanCursorError::Lhm(ScanError::ZeroBudget) => SubjectStoreScanError::ZeroBudget,
    })
}

#[cfg(any(test, feature = "canbench"))]
fn reset_error(error: ResetError) -> SubjectStoreError {
    let error = match error {
        ResetError::IncarnationMismatch { current } => {
            SubjectStoreResetError::IncarnationMismatch { current }
        }
        ResetError::IncarnationExhausted => SubjectStoreResetError::IncarnationExhausted,
        ResetError::InProgress => SubjectStoreResetError::InProgress,
        ResetError::EpochExhausted => SubjectStoreResetError::EpochExhausted,
        ResetError::CapacityOverflow => SubjectStoreResetError::CapacityOverflow,
    };
    SubjectStoreError::Reset(error)
}

thread_local! {
    static SUBJECT_STORE: RefCell<SubjectStore> = RefCell::new(SubjectStore::default());
}

pub(crate) fn create_for_install(hash_seed: u64) -> Result<(), SubjectStoreError> {
    SUBJECT_STORE
        .with_borrow_mut(|store| store.create_for_install(memory::subject_memory(), hash_seed))
}

pub(crate) fn open_after_upgrade() -> Result<(), SubjectStoreError> {
    SUBJECT_STORE.with_borrow_mut(|store| store.open_after_upgrade(memory::subject_memory()))
}

pub(crate) fn get(key: &SubjectKey) -> Result<Option<FixedSubjectMapEntry>, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.get(key))
}

pub(crate) fn insert(
    key: SubjectKey,
    value: FixedSubjectMapEntry,
) -> Result<Option<FixedSubjectMapEntry>, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.insert(key, value))
}

pub(crate) fn remove(key: &SubjectKey) -> Result<Option<FixedSubjectMapEntry>, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.remove(key))
}

pub(crate) fn scan_start(scope: SubjectScanScope) -> Result<SubjectScanCursor, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.scan_start(scope))
}

pub(crate) fn scan_step(
    scope: SubjectScanScope,
    cursor: SubjectScanCursor,
    physical_slot_budget: u64,
) -> Result<SubjectScanPage, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.scan_step(scope, cursor, physical_slot_budget))
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn prepare_reset(
    expected_incarnation: u64,
) -> Result<SubjectResetTicket, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.prepare_reset(expected_incarnation))
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn commit_reset(ticket: SubjectResetTicket) -> Result<u64, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.commit_reset(ticket))
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn bind_for_fixture(hash_seed: u64) -> Result<(), SubjectStoreError> {
    SUBJECT_STORE
        .with_borrow_mut(|store| store.bind_for_fixture(memory::subject_memory(), hash_seed))
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn incarnation_for_test_or_bench() -> Result<u64, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| {
        store
            .map()?
            .control_region()
            .map(|control| control.incarnation)
            .map_err(mutation_error)
    })
}

#[cfg(test)]
pub(crate) fn is_empty_for_test() -> Result<bool, SubjectStoreError> {
    SUBJECT_STORE.with_borrow(|store| store.map()?.is_empty().map_err(mutation_error))
}

/// Fills the live MemoryId 7 owner with real LHM insertions until the next subject admission
/// returns `TablePressure`. The failed key is not written and can be used as an exact terminal
/// probe by the typed batch tests.
#[cfg(test)]
pub(crate) fn fill_until_table_pressure_for_test(index_id: u32, first_vertex_id: u32) -> u32 {
    const MAX_ATTEMPTS: u32 = 50_000;
    for offset in 0..MAX_ATTEMPTS {
        let vertex_id = first_vertex_id
            .checked_add(offset)
            .expect("test subject pressure vertex range overflow");
        let key = SubjectKey::new(
            index_id,
            VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id,
            },
        );
        let entry = FixedSubjectMapEntry {
            stamp: 1,
            deleted: false,
            slot: None,
            shadow_slot: None,
        };
        match insert(key, entry) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("test subject pressure key unexpectedly replaced"),
            Err(SubjectStoreError::TablePressure) => return vertex_id,
            Err(error) => panic!("test subject pressure fixture owner error: {error:?}"),
        }
    }
    panic!("real LHM subject table pressure was not reached within {MAX_ATTEMPTS} inserts");
}
