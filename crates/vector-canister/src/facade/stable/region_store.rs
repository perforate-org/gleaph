//! Shared stable-region lifecycle owner for the definition store and the subject store.
//!
//! One implementation of the heap-owner state machine (`Uninitialized`/`Ready`/`Unavailable`),
//! the strict-create / exact-open binding protocol, and the incarnation-checked reset ticket
//! serves both regions; each store module instantiates [`RegionOwner`] over its own map type.
//! The unified [`RegionError`] vocabulary carries only distinctions some consumer reads:
//! scan restarts (`RestartRequired`) drive physical-scan recovery, `TablePressure` feeds the
//! typed batch terminal outcomes per store, and availability classification separates
//! unavailable regions from pressure. Subject scan machinery stays subject-specific.

use super::memory::Memory;
#[cfg(any(test, feature = "canbench"))]
use ic_stable_linear_hash_map::ResetError;
use ic_stable_linear_hash_map::{
    InitError, MutationError, ScanError, StableHashKey, StableLinearHashMap, StableMapValue,
};
#[cfg(any(test, feature = "canbench"))]
use ic_stable_structures::Memory as _;

/// Why a stable region cannot currently serve requests.
///
/// Open/create rejections retain the underlying [`InitError`] verbatim so no reason taxonomy is
/// duplicated between the linear hash map library and this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionUnavailableReason {
    /// The owner was never bound: fresh install before strict create, or pre-upgrade.
    Uninitialized,
    /// A bind was attempted while the owner already held or had rejected a region.
    AlreadyInitialized,
    /// Strict create or seed-free exact open rejected the region bytes.
    OpenRejected(InitError),
}

/// Unified stable-region error vocabulary shared by both region owners.
///
/// Every variant has at least one structural consumer or test assertion:
/// - [`RegionError::Unavailable`] classifies availability failures for the typed batch path
///   (`StoreUnavailable` / `SubjectStoreUnavailable`) and retains the reason for later accesses;
///   lifecycle tests assert the exact reasons.
/// - [`RegionError::TablePressure`] is bounded-table split admission pressure; the typed batch
///   path acknowledges it as a terminal result, per store.
/// - [`RegionError::Mutation`] groups non-pressure point-operation failures. It never carries
///   `MutationError::TablePressure`; that lifts to [`RegionError::TablePressure`] at the single
///   conversion site below.
/// - [`RegionError::Scan`] distinguishes physical-scan failures: `RestartRequired` restarts the
///   scan in rebuild/detach paths, `InProgress` yields an empty purge step, and `InvalidCursor`
///   fences malformed resume keys.
/// - [`RegionError::Reset`] reports coordinated-reset preflight/commit failures before any
///   region bytes change; the definition-store lifecycle tests assert its members exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionError {
    Unavailable(RegionUnavailableReason),
    TablePressure,
    Mutation(MutationError),
    Scan(ScanError),
    #[cfg(any(test, feature = "canbench"))]
    Reset(ResetError),
}

impl From<MutationError> for RegionError {
    fn from(error: MutationError) -> Self {
        match error {
            MutationError::TablePressure => RegionError::TablePressure,
            error => RegionError::Mutation(error),
        }
    }
}

/// Owner-issued proof that one region passed reset preflight at one incarnation.
///
/// Construction is private so consumers cannot fabricate a reset fence. Each store wraps this
/// ticket in its own opaque newtype so a definition ticket can never commit a subject reset.
#[cfg(any(test, feature = "canbench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegionResetTicket {
    expected_incarnation: u64,
}

#[derive(Default)]
enum RegionState<M> {
    #[default]
    Uninitialized,
    Ready(Box<M>),
    Unavailable(RegionUnavailableReason),
}

/// Heap owner for one bound stable region.
///
/// The state machine itself is intentionally private. Consumers operate through the point
/// operations below, which observe the unavailable boundary on every access, or through
/// [`RegionOwner::map`] for the subject scan machinery that stays subject-specific.
pub(crate) struct RegionOwner<K: StableHashKey, V: StableMapValue> {
    state: RegionState<StableLinearHashMap<K, V, Memory>>,
}

impl<K: StableHashKey, V: StableMapValue> Default for RegionOwner<K, V> {
    fn default() -> Self {
        Self {
            state: RegionState::default(),
        }
    }
}

/// Detached owner for lifecycle unit tests that must not share the thread-local instance.
#[cfg(test)]
impl<K: StableHashKey, V: StableMapValue> RegionOwner<K, V> {
    pub(crate) fn detached_for_test() -> Self {
        Self::default()
    }
}

impl<K: StableHashKey, V: StableMapValue> RegionOwner<K, V> {
    /// Strict fresh-install bind. This never opens or resets nonempty memory.
    pub(crate) fn create_for_install(
        &mut self,
        memory: Memory,
        hash_seed: u64,
    ) -> Result<(), RegionError> {
        if !matches!(self.state, RegionState::Uninitialized) {
            return Err(RegionError::Unavailable(
                RegionUnavailableReason::AlreadyInitialized,
            ));
        }
        self.bind(StableLinearHashMap::create(memory, hash_seed))
    }

    /// Seed-free exact reopen after upgrade. A failure is retained as `Unavailable` and no
    /// fallback create or reset is attempted.
    pub(crate) fn open_after_upgrade(&mut self, memory: Memory) -> Result<(), RegionError> {
        if !matches!(self.state, RegionState::Uninitialized) {
            return Err(RegionError::Unavailable(
                RegionUnavailableReason::AlreadyInitialized,
            ));
        }
        self.bind(StableLinearHashMap::open(memory))
    }

    fn bind(
        &mut self,
        result: Result<StableLinearHashMap<K, V, Memory>, InitError>,
    ) -> Result<(), RegionError> {
        match result {
            Ok(map) => {
                self.state = RegionState::Ready(Box::new(map));
                Ok(())
            }
            Err(error) => {
                let reason = RegionUnavailableReason::OpenRejected(error);
                self.state = RegionState::Unavailable(reason);
                Err(RegionError::Unavailable(reason))
            }
        }
    }

    pub(crate) fn map(&self) -> Result<&StableLinearHashMap<K, V, Memory>, RegionError> {
        match &self.state {
            RegionState::Ready(map) => Ok(map),
            RegionState::Uninitialized => Err(RegionError::Unavailable(
                RegionUnavailableReason::Uninitialized,
            )),
            RegionState::Unavailable(reason) => Err(RegionError::Unavailable(*reason)),
        }
    }

    pub(crate) fn get(&self, key: &K) -> Result<Option<V>, RegionError> {
        self.map()?.get(key).map_err(RegionError::from)
    }

    pub(crate) fn insert(&self, key: K, value: V) -> Result<Option<V>, RegionError> {
        self.map()?.insert(key, value).map_err(RegionError::from)
    }

    pub(crate) fn remove(&self, key: &K) -> Result<Option<V>, RegionError> {
        self.map()?.remove(key).map_err(RegionError::from)
    }

    /// Preflights a caller-supplied ownership fence and returns an unforgeable commit ticket.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn prepare_reset(
        &self,
        expected_incarnation: u64,
    ) -> Result<RegionResetTicket, RegionError> {
        let control = self.map()?.control_region().map_err(RegionError::from)?;
        if control.incarnation != expected_incarnation {
            return Err(RegionError::Reset(ResetError::IncarnationMismatch {
                current: control.incarnation,
            }));
        }
        control
            .incarnation
            .checked_add(1)
            .ok_or(RegionError::Reset(ResetError::IncarnationExhausted))?;
        control
            .mutation_epoch
            .checked_add(2)
            .ok_or(RegionError::Reset(ResetError::EpochExhausted))?;
        Ok(RegionResetTicket {
            expected_incarnation,
        })
    }

    /// Commits a previously preflighted reset after every coupled handle has been acquired.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn commit_reset(&self, ticket: RegionResetTicket) -> Result<u64, RegionError> {
        self.map()?
            .reset(ticket.expected_incarnation)
            .map_err(RegionError::Reset)
    }

    /// Test/benchmark fixture bind: strict-create empty memory, else exact-open existing bytes.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn bind_for_fixture(
        &mut self,
        memory: Memory,
        hash_seed: u64,
    ) -> Result<(), RegionError> {
        match &self.state {
            RegionState::Ready(_) => Ok(()),
            RegionState::Unavailable(reason) => Err(RegionError::Unavailable(*reason)),
            RegionState::Uninitialized => {
                let bound = if memory.size() == 0 {
                    StableLinearHashMap::create(memory, hash_seed)
                } else {
                    StableLinearHashMap::open(memory)
                };
                self.bind(bound)
            }
        }
    }

    /// Returns the current owner incarnation for test/benchmark fixture coordination.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn incarnation_for_test_or_bench(&self) -> Result<u64, RegionError> {
        self.map()?
            .control_region()
            .map(|control| control.incarnation)
            .map_err(RegionError::from)
    }

    #[cfg(test)]
    pub(crate) fn is_empty_for_test(&self) -> Result<bool, RegionError> {
        self.map()?.is_empty().map_err(RegionError::from)
    }

    /// Simulates a post-upgrade exact-open against already-bound region bytes.
    #[cfg(test)]
    pub(crate) fn reopen_for_test(&mut self, memory: Memory) -> Result<(), RegionError> {
        self.state = RegionState::Uninitialized;
        self.open_after_upgrade(memory)
    }

    /// Drops a live `Ready` binding without touching the backing region bytes. Non-ready owners
    /// refuse, keeping the PocketIC failure path distinct from production lifecycle handling.
    #[cfg(any(test, feature = "pocket-ic-e2e"))]
    pub(crate) fn unbind_if_ready_for_test(&mut self) -> Result<(), ()> {
        match self.state {
            RegionState::Ready(_) => {
                self.state = RegionState::Uninitialized;
                Ok(())
            }
            _ => Err(()),
        }
    }
}
