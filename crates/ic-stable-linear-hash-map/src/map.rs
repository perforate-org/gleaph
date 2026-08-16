use crate::control;
use crate::header::{
    self, BUCKETS_OFFSET, CONTROL_BYTES, ControlRegion, HEADER_SIZE, Header, InitError,
    PAGES_PER_BUCKET, PRIMARY_SLOTS, SLOTS_PER_BUCKET,
};
use crate::memory::{GrowError, grow_to_bytes};
use crate::{StableHashKey, StableMapValue};
use ic_stable_structures::{Memory, Storable};
use rapidhash::v3::{RapidSecrets, rapidhash_v3_inline};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::marker::PhantomData;

const INITIAL_LEVEL: u8 = 3;
const INITIAL_BUCKETS: u64 = 1 << INITIAL_LEVEL;
const PAGE_HEADER_BYTES: u64 = 8;
const SCAN_CURSOR_MAGIC: [u8; 3] = *b"LHV";
const SCAN_CURSOR_VERSION: u8 = 1;
const SCAN_CURSOR_BYTES: usize = 88;
const DEFAULT_HASH_SEED: u64 = 0x243f_6a88_85a3_08d3;
const HASH_DOMAIN_0: u64 = 0x1319_8a2e_0370_7344;
const HASH_DOMAIN_1: u64 = 0xa409_3822_299f_31d0;
const SPLIT_ENTRY_BUDGET: u64 = 1024;
const SPLIT_BYTE_BUDGET: u64 = 16 * 1024 * 1024;
const PAGE_FULL_MASK: u64 = (1u64 << PRIMARY_SLOTS) - 1;

thread_local! {
    static NEXT_SCRUB_LINEAGE: Cell<u64> = const { Cell::new(1) };
    // ponytail: one reusable buffer; add a scratch stack only if nested map reads become real.
    static GET_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn next_scrub_lineage() -> u64 {
    NEXT_SCRUB_LINEAGE.with(|next| {
        let lineage = next.get();
        next.set(lineage.checked_add(1).expect("scrub lineage exhausted"));
        lineage
    })
}

/// A map mutation could not be admitted without changing the persisted logical state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationError {
    /// Both candidate bucket blocks are at the final bounded capacity, or a split cannot
    /// redistribute the source block without exceeding that capacity.
    TablePressure,
    InProgress,
    EpochExhausted,
    InvalidKeyEncoding,
    InvalidValueEncoding,
    OutOfMemory,
    CapacityOverflow,
}

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TablePressure => write!(f, "bounded bucket overflow and split admission failed"),
            Self::InProgress => write!(f, "a mutation is already in progress"),
            Self::EpochExhausted => write!(f, "mutation epoch is exhausted"),
            Self::InvalidKeyEncoding => {
                write!(f, "key serialization did not match the fixed-width header")
            }
            Self::InvalidValueEncoding => write!(
                f,
                "value serialization did not match the fixed-width header"
            ),
            Self::OutOfMemory => write!(f, "failed to grow linear hash map stable memory"),
            Self::CapacityOverflow => write!(f, "linear hash map capacity arithmetic overflowed"),
        }
    }
}

impl std::error::Error for MutationError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetError {
    IncarnationMismatch { current: u64 },
    IncarnationExhausted,
    InProgress,
    EpochExhausted,
    CapacityOverflow,
}

impl fmt::Display for ResetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncarnationMismatch { current } => write!(
                f,
                "reset incarnation mismatch; current incarnation is {current}"
            ),
            Self::IncarnationExhausted => write!(f, "reset incarnation is exhausted"),
            Self::InProgress => write!(f, "a mutation is already in progress"),
            Self::EpochExhausted => write!(f, "mutation epoch is exhausted"),
            Self::CapacityOverflow => write!(f, "initial map extent arithmetic overflowed"),
        }
    }
}

impl std::error::Error for ResetError {}

/// Result of bounded split-debt service. `Pending` means that no split was written because the
/// caller's work budget was smaller than the next complete source block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceStep {
    Idle {
        debt_remaining: u64,
    },
    Progress {
        splits: u32,
        moved_entries: u64,
        moved_bytes: u64,
        debt_remaining: u64,
    },
    Pending {
        debt_remaining: u64,
        required_entries: u64,
        required_bytes: u64,
    },
}

/// Serializable progress through the final V1 physical slot order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanCursor {
    key_storage_schema_id: [u8; 16],
    key_routing_schema_id: [u8; 16],
    value_storage_schema_id: [u8; 16],
    hash_seed: u64,
    incarnation: u64,
    physical_buckets: u64,
    next_slot: u64,
}

impl ScanCursor {
    pub const ENCODED_SIZE: usize = SCAN_CURSOR_BYTES;

    pub fn encode(&self) -> [u8; SCAN_CURSOR_BYTES] {
        let mut bytes = [0; SCAN_CURSOR_BYTES];
        bytes[..3].copy_from_slice(&SCAN_CURSOR_MAGIC);
        bytes[3] = SCAN_CURSOR_VERSION;
        bytes[8..24].copy_from_slice(&self.key_storage_schema_id);
        bytes[24..40].copy_from_slice(&self.key_routing_schema_id);
        bytes[40..56].copy_from_slice(&self.value_storage_schema_id);
        bytes[56..64].copy_from_slice(&self.hash_seed.to_le_bytes());
        bytes[64..72].copy_from_slice(&self.incarnation.to_le_bytes());
        bytes[72..80].copy_from_slice(&self.physical_buckets.to_le_bytes());
        bytes[80..88].copy_from_slice(&self.next_slot.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ScanError> {
        if bytes.len() != SCAN_CURSOR_BYTES
            || bytes[..3] != SCAN_CURSOR_MAGIC
            || bytes[3] != SCAN_CURSOR_VERSION
            || bytes[4..8].iter().any(|byte| *byte != 0)
        {
            return Err(ScanError::InvalidCursor);
        }
        let cursor = Self {
            key_storage_schema_id: bytes[8..24].try_into().expect("scan cursor schema"),
            key_routing_schema_id: bytes[24..40].try_into().expect("scan cursor schema"),
            value_storage_schema_id: bytes[40..56].try_into().expect("scan cursor schema"),
            hash_seed: scan_u64(bytes, 56),
            incarnation: scan_u64(bytes, 64),
            physical_buckets: scan_u64(bytes, 72),
            next_slot: scan_u64(bytes, 80),
        };
        cursor.validate_structure()?;
        Ok(cursor)
    }

    pub fn key_storage_schema_id(&self) -> [u8; 16] {
        self.key_storage_schema_id
    }
    pub fn key_routing_schema_id(&self) -> [u8; 16] {
        self.key_routing_schema_id
    }
    pub fn value_storage_schema_id(&self) -> [u8; 16] {
        self.value_storage_schema_id
    }
    pub fn hash_seed(&self) -> u64 {
        self.hash_seed
    }
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }
    pub fn physical_buckets(&self) -> u64 {
        self.physical_buckets
    }
    pub fn next_slot(&self) -> u64 {
        self.next_slot
    }

    fn validate_structure(&self) -> Result<(), ScanError> {
        let slots = self
            .physical_buckets
            .checked_mul(u64::from(SLOTS_PER_BUCKET))
            .filter(|_| self.physical_buckets >= INITIAL_BUCKETS)
            .ok_or(ScanError::InvalidCursor)?;
        if self.incarnation == 0 || self.next_slot > slots {
            return Err(ScanError::InvalidCursor);
        }
        Ok(())
    }
}

fn scan_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("scan cursor integer"),
    )
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScanPage<K, V> {
    entries: Vec<(K, V)>,
    next_cursor: ScanCursor,
    examined_slots: u64,
    exhausted: bool,
}

impl<K, V> ScanPage<K, V> {
    pub fn entries(&self) -> &[(K, V)] {
        &self.entries
    }
    pub fn into_entries(self) -> Vec<(K, V)> {
        self.entries
    }
    pub fn next_cursor(&self) -> ScanCursor {
        self.next_cursor
    }
    pub fn examined_slots(&self) -> u64 {
        self.examined_slots
    }
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanError {
    ZeroBudget,
    InvalidCursor,
    RestartRequired,
    InProgress,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBudget => write!(f, "scan physical-slot budget must be positive"),
            Self::InvalidCursor => write!(f, "scan cursor is malformed or belongs to another map"),
            Self::RestartRequired => write!(f, "scan cursor is stale; restart from a fresh cursor"),
            Self::InProgress => write!(f, "a mutation overlapped this scan step"),
        }
    }
}

impl std::error::Error for ScanError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrubSnapshot {
    key_storage_schema_id: [u8; 16],
    key_routing_schema_id: [u8; 16],
    value_storage_schema_id: [u8; 16],
    hash_seed: u64,
    mutation_epoch: u64,
    incarnation: u64,
    len: u64,
    physical_buckets: u64,
    split_debt: u64,
    overflow_entries: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrubCursor {
    snapshot: ScrubSnapshot,
    next_primary_bucket: u64,
    occupied: u64,
    lineage: u64,
}

impl ScrubCursor {
    pub fn snapshot(&self) -> ScrubSnapshot {
        self.snapshot
    }
    pub fn next_primary_bucket(&self) -> u64 {
        self.next_primary_bucket
    }
    pub fn occupied(&self) -> u64 {
        self.occupied
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrubStep {
    InProgress(ScrubCursor),
    Complete(ScrubSnapshot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrubError {
    ZeroBudget,
    InvalidCursor,
    Stale,
    InvalidOccupancy { bucket: u64, page: u32 },
    InvalidKeyEncoding { bucket: u64, slot: u32 },
    InvalidValueEncoding { bucket: u64, slot: u32 },
    UnreachablePlacement { bucket: u64, slot: u32 },
    DuplicateKey { bucket: u64, slot: u32 },
    LengthMismatch { expected: u64, actual: u64 },
}

impl fmt::Display for ScrubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "linear hash map scrub error: {self:?}")
    }
}

impl std::error::Error for ScrubError {}

/// Final V1 two-choice linear hash map with one primary and two inline overflow pages per bucket.
pub struct StableLinearHashMap<K: StableHashKey, V: StableMapValue, M: Memory> {
    memory: M,
    header: Header,
    scrub_lineage: u64,
    hash_secrets: [RapidSecrets; 2],
    _marker: PhantomData<(K, V)>,
}

struct MutationGuard<'a, K: StableHashKey, V: StableMapValue, M: Memory> {
    map: &'a StableLinearHashMap<K, V, M>,
    completed_epoch: u64,
}

#[derive(Clone, Copy)]
struct Location {
    bucket: u64,
    page: u32,
    slot: u32,
}

struct BucketImage {
    bytes: Vec<u8>,
    entries: u64,
    overflow_entries: u64,
}

struct BucketInspection {
    block: Vec<u8>,
    existing: Option<Location>,
    free: Option<Location>,
    load: u32,
}

struct SplitPlan {
    observed_epoch: u64,
    next_buckets: u64,
    blocks: Vec<(u64, BucketImage)>,
    len: u64,
    split_debt: u64,
    overflow_entries: u64,
    grow_to: u64,
}

#[derive(Clone, Copy)]
struct SplitGeometry {
    source_bucket: u64,
    new_bucket: u64,
    next_buckets: u64,
}

impl<K: StableHashKey, V: StableMapValue, M: Memory> MutationGuard<'_, K, V, M> {
    fn finish(self) {
        control::write_mutation_epoch(
            &self.map.memory,
            self.map.header.control_offset,
            self.completed_epoch,
        );
    }
}

impl<K: StableHashKey, V: StableMapValue, M: Memory> StableLinearHashMap<K, V, M> {
    pub fn new(memory: M) -> Result<Self, InitError> {
        Self::new_with_hash_seed(memory, DEFAULT_HASH_SEED)
    }

    pub fn new_with_hash_seed(memory: M, hash_seed: u64) -> Result<Self, InitError> {
        Self::create(memory, hash_seed)
    }

    pub fn create(memory: M, hash_seed: u64) -> Result<Self, InitError> {
        if memory.size() != 0 {
            return Err(InitError::NonEmptyMemory);
        }
        let header = Self::expected_header(hash_seed)?;
        let end = Self::bucket_end_for(header, INITIAL_BUCKETS).ok_or(InitError::InvalidLayout)?;
        grow_to_bytes(&memory, end).map_err(|_| InitError::OutOfMemory)?;
        control::write(
            &memory,
            header.control_offset,
            ControlRegion {
                len: 0,
                physical_buckets: INITIAL_BUCKETS,
                mutation_epoch: control::INITIAL_MUTATION_EPOCH,
                incarnation: control::INITIAL_INCARNATION,
                split_debt: 0,
                overflow_entries: 0,
                level: INITIAL_LEVEL,
                split_cursor: 0,
                hash_seed,
            },
        );
        header::write(&memory, header);
        Ok(Self {
            memory,
            header,
            scrub_lineage: next_scrub_lineage(),
            hash_secrets: hash_secrets(hash_seed),
            _marker: PhantomData,
        })
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        Self::open_or_create(memory)
    }

    pub fn init_with_hash_seed(memory: M, hash_seed: u64) -> Result<Self, InitError> {
        if memory.size() == 0 {
            Self::create(memory, hash_seed)
        } else {
            Self::open(memory)
        }
    }

    pub fn open_or_create(memory: M) -> Result<Self, InitError> {
        Self::init_with_hash_seed(memory, DEFAULT_HASH_SEED)
    }

    pub fn open(memory: M) -> Result<Self, InitError> {
        let allocated = memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .ok_or(InitError::InvalidLayout)?;
        if allocated < HEADER_SIZE + CONTROL_BYTES {
            return Err(InitError::InvalidLayout);
        }
        let header = header::read(&memory)?;
        let expected = Self::expected_header(header.hash_seed)?;
        if header.key_size != expected.key_size || header.value_size != expected.value_size {
            return Err(InitError::IncompatibleElementType);
        }
        if header.key_storage_schema_id != expected.key_storage_schema_id {
            return Err(InitError::IncompatibleKeyStorageSchema);
        }
        if header.key_routing_schema_id != expected.key_routing_schema_id {
            return Err(InitError::IncompatibleKeyRoutingSchema);
        }
        if header.value_storage_schema_id != expected.value_storage_schema_id {
            return Err(InitError::IncompatibleValueStorageSchema);
        }
        if header.bucket_page_stride != expected.bucket_page_stride
            || header.bucket_block_stride != expected.bucket_block_stride
        {
            return Err(InitError::InvalidLayout);
        }
        let control = control::read_for_open(&memory, header.control_offset, header.hash_seed)
            .map_err(|_| InitError::InvalidLayout)?;
        Self::validate_control(control)?;
        let end = Self::bucket_end_for(header, control.physical_buckets)
            .ok_or(InitError::InvalidLayout)?;
        if allocated < end {
            return Err(InitError::InvalidLayout);
        }
        Ok(Self {
            memory,
            header,
            scrub_lineage: next_scrub_lineage(),
            hash_secrets: hash_secrets(header.hash_seed),
            _marker: PhantomData,
        })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    pub fn control_region(&self) -> Result<ControlRegion, MutationError> {
        self.read_consistent(|| {
            control::read(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            )
        })
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn len(&self) -> Result<u64, MutationError> {
        self.read_consistent(|| control::read_len(&self.memory, self.header.control_offset))
    }

    pub fn is_empty(&self) -> Result<bool, MutationError> {
        self.len().map(|len| len == 0)
    }

    pub fn hash_seed(&self) -> Result<u64, MutationError> {
        self.read_consistent(|| self.header.hash_seed)
    }

    pub fn scan_start(&self) -> Result<ScanCursor, ScanError> {
        let (_, control) = self.scan_control_before_step()?;
        Ok(ScanCursor {
            key_storage_schema_id: self.header.key_storage_schema_id,
            key_routing_schema_id: self.header.key_routing_schema_id,
            value_storage_schema_id: self.header.value_storage_schema_id,
            hash_seed: self.header.hash_seed,
            incarnation: control.incarnation,
            physical_buckets: control.physical_buckets,
            next_slot: 0,
        })
    }

    pub fn scan_step(
        &self,
        cursor: ScanCursor,
        physical_slot_budget: u64,
    ) -> Result<ScanPage<K, V>, ScanError> {
        if physical_slot_budget == 0 {
            return Err(ScanError::ZeroBudget);
        }
        cursor.validate_structure()?;
        if cursor.key_storage_schema_id != self.header.key_storage_schema_id
            || cursor.key_routing_schema_id != self.header.key_routing_schema_id
            || cursor.value_storage_schema_id != self.header.value_storage_schema_id
            || cursor.hash_seed != self.header.hash_seed
        {
            return Err(ScanError::InvalidCursor);
        }
        let (observed_epoch, before) = self.scan_control_before_step()?;
        if before.incarnation != cursor.incarnation
            || before.physical_buckets != cursor.physical_buckets
        {
            return Err(ScanError::RestartRequired);
        }
        let total_slots = cursor
            .physical_buckets
            .checked_mul(u64::from(SLOTS_PER_BUCKET))
            .ok_or(ScanError::InvalidCursor)?;
        let end = cursor
            .next_slot
            .saturating_add(physical_slot_budget)
            .min(total_slots);
        let entries = self.scan_physical_window(cursor.next_slot, end);
        let after = control::read_for_open(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        )
        .map_err(|_| ScanError::RestartRequired)?;
        if after.incarnation != cursor.incarnation
            || after.physical_buckets != cursor.physical_buckets
        {
            return Err(ScanError::RestartRequired);
        }
        if !after.mutation_epoch.is_multiple_of(2) || after.mutation_epoch != observed_epoch {
            return Err(ScanError::InProgress);
        }
        let mut next = cursor;
        next.next_slot = end;
        Ok(ScanPage {
            entries,
            next_cursor: next,
            examined_slots: end - cursor.next_slot,
            exhausted: end == total_slots,
        })
    }

    pub fn maintenance_step(
        &self,
        entry_budget: u64,
        byte_budget: u64,
    ) -> Result<MaintenanceStep, MutationError> {
        if entry_budget == 0 || byte_budget == 0 {
            return Ok(MaintenanceStep::Pending {
                debt_remaining: self.control_region()?.split_debt,
                required_entries: 1,
                required_bytes: 1,
            });
        }
        let mut remaining_entries = entry_budget;
        let mut remaining_bytes = byte_budget;
        let mut splits = 0u32;
        let mut moved_entries = 0u64;
        let mut moved_bytes = 0u64;
        loop {
            let control = control::read(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            if !control.mutation_epoch.is_multiple_of(2) {
                return Err(MutationError::InProgress);
            }
            if control.split_debt == 0 {
                return Ok(if splits == 0 {
                    MaintenanceStep::Idle { debt_remaining: 0 }
                } else {
                    MaintenanceStep::Progress {
                        splits,
                        moved_entries,
                        moved_bytes,
                        debt_remaining: 0,
                    }
                });
            }
            let source = self.read_bucket_image(control.split_cursor);
            let required_entries = source.entries;
            let required_bytes = required_entries
                .checked_mul(u64::from(self.header.key_size) + u64::from(self.header.value_size))
                .ok_or(MutationError::CapacityOverflow)?;
            if required_entries > remaining_entries || required_bytes > remaining_bytes {
                return Ok(if splits == 0 {
                    MaintenanceStep::Pending {
                        debt_remaining: control.split_debt,
                        required_entries,
                        required_bytes,
                    }
                } else {
                    MaintenanceStep::Progress {
                        splits,
                        moved_entries,
                        moved_bytes,
                        debt_remaining: control.split_debt,
                    }
                });
            }
            let plan = match self.plan_split(control, None) {
                Ok(plan) => plan,
                Err(_error) if splits > 0 => {
                    return Ok(MaintenanceStep::Progress {
                        splits,
                        moved_entries,
                        moved_bytes,
                        debt_remaining: control.split_debt,
                    });
                }
                Err(error) => return Err(error),
            };
            match self.apply_split(plan) {
                Ok(()) => {}
                Err(_error) if splits > 0 => {
                    let debt_remaining = control::read(
                        &self.memory,
                        self.header.control_offset,
                        self.header.hash_seed,
                    )
                    .split_debt;
                    return Ok(MaintenanceStep::Progress {
                        splits,
                        moved_entries,
                        moved_bytes,
                        debt_remaining,
                    });
                }
                Err(error) => return Err(error),
            }
            remaining_entries -= required_entries;
            remaining_bytes -= required_bytes;
            moved_entries += required_entries;
            moved_bytes += required_bytes;
            splits = splits
                .checked_add(1)
                .ok_or(MutationError::CapacityOverflow)?;
        }
    }

    pub fn reset(&self, expected_incarnation: u64) -> Result<u64, ResetError> {
        let control = control::read(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(ResetError::InProgress);
        }
        if control.incarnation != expected_incarnation {
            return Err(ResetError::IncarnationMismatch {
                current: control.incarnation,
            });
        }
        let incarnation = expected_incarnation
            .checked_add(1)
            .ok_or(ResetError::IncarnationExhausted)?;
        let completed_epoch = control
            .mutation_epoch
            .checked_add(2)
            .ok_or(ResetError::EpochExhausted)?;
        let end = Self::bucket_end_for(self.header, INITIAL_BUCKETS)
            .ok_or(ResetError::CapacityOverflow)?;
        let zero_block = vec![0; self.header.bucket_block_stride as usize];
        if self
            .memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .ok_or(ResetError::CapacityOverflow)?
            < end
        {
            return Err(ResetError::CapacityOverflow);
        }
        control::write_mutation_epoch(
            &self.memory,
            self.header.control_offset,
            control.mutation_epoch + 1,
        );
        for bucket in 0..INITIAL_BUCKETS {
            self.memory
                .write(Self::bucket_base(self.header, bucket), &zero_block);
        }
        control::write(
            &self.memory,
            self.header.control_offset,
            ControlRegion {
                len: 0,
                physical_buckets: INITIAL_BUCKETS,
                mutation_epoch: completed_epoch,
                incarnation,
                split_debt: 0,
                overflow_entries: 0,
                level: INITIAL_LEVEL,
                split_cursor: 0,
                hash_seed: self.header.hash_seed,
            },
        );
        Ok(incarnation)
    }

    /// Clears every entry while preserving the persisted incarnation and hash seed.
    ///
    /// Unlike [`Self::reset`], this does not advance the incarnation fence: it is intended for
    /// derived/derivable regions whose contents are reconstructed from a canonical source, so a
    /// concurrent scan cursor keyed on the old incarnation must be invalidated by the caller
    /// (e.g. by also resetting the canonical owner). The mutation is failure-atomic: on a returned
    /// error the logical map bytes, header, length, and control region remain unchanged and
    /// reopenable.
    pub fn clear(&self) -> Result<(), MutationError> {
        let control = control::read(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(MutationError::InProgress);
        }
        let completed_epoch = control
            .mutation_epoch
            .checked_add(2)
            .ok_or(MutationError::EpochExhausted)?;
        let end = Self::bucket_end_for(self.header, INITIAL_BUCKETS)
            .ok_or(MutationError::CapacityOverflow)?;
        let zero_block = vec![0; self.header.bucket_block_stride as usize];
        if self
            .memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .ok_or(MutationError::CapacityOverflow)?
            < end
        {
            return Err(MutationError::CapacityOverflow);
        }
        control::write_mutation_epoch(
            &self.memory,
            self.header.control_offset,
            control.mutation_epoch + 1,
        );
        for bucket in 0..INITIAL_BUCKETS {
            self.memory
                .write(Self::bucket_base(self.header, bucket), &zero_block);
        }
        control::write(
            &self.memory,
            self.header.control_offset,
            ControlRegion {
                len: 0,
                physical_buckets: INITIAL_BUCKETS,
                mutation_epoch: completed_epoch,
                incarnation: control.incarnation,
                split_debt: 0,
                overflow_entries: 0,
                level: INITIAL_LEVEL,
                split_cursor: 0,
                hash_seed: self.header.hash_seed,
            },
        );
        Ok(())
    }

    pub fn scrub_snapshot(&self) -> Result<ScrubCursor, ScrubError> {
        let snapshot = self.current_scrub_snapshot()?;
        Ok(ScrubCursor {
            snapshot,
            next_primary_bucket: 0,
            occupied: 0,
            lineage: self.scrub_lineage,
        })
    }

    pub fn scrub_step(
        &self,
        cursor: ScrubCursor,
        primary_bucket_budget: u64,
    ) -> Result<ScrubStep, ScrubError> {
        if primary_bucket_budget == 0 {
            return Err(ScrubError::ZeroBudget);
        }
        if cursor.lineage != self.scrub_lineage
            || cursor.next_primary_bucket > cursor.snapshot.physical_buckets
        {
            return Err(ScrubError::InvalidCursor);
        }
        self.ensure_scrub_fence(cursor.snapshot)?;
        let end = cursor
            .next_primary_bucket
            .saturating_add(primary_bucket_budget)
            .min(cursor.snapshot.physical_buckets);
        let control = self.scrub_control(cursor.snapshot);
        let mut occupied = cursor.occupied;
        for bucket in cursor.next_primary_bucket..end {
            occupied = occupied
                .checked_add(self.scrub_bucket(bucket, control)?)
                .ok_or(ScrubError::LengthMismatch {
                    expected: cursor.snapshot.len,
                    actual: u64::MAX,
                })?;
        }
        self.ensure_scrub_fence(cursor.snapshot)?;
        if end == cursor.snapshot.physical_buckets {
            if occupied != cursor.snapshot.len {
                return Err(ScrubError::LengthMismatch {
                    expected: cursor.snapshot.len,
                    actual: occupied,
                });
            }
            Ok(ScrubStep::Complete(cursor.snapshot))
        } else {
            Ok(ScrubStep::InProgress(ScrubCursor {
                snapshot: cursor.snapshot,
                next_primary_bucket: end,
                occupied,
                lineage: cursor.lineage,
            }))
        }
    }

    pub fn get(&self, key: &K) -> Result<Option<V>, MutationError> {
        self.read_consistent_hot(|hot| self.get_with_hot(key, hot))
    }

    pub fn get_many(&self, keys: &[K]) -> Result<Vec<Option<V>>, MutationError> {
        self.read_consistent_hot(|hot| keys.iter().map(|key| self.get_with_hot(key, hot)).collect())
    }

    pub fn contains_key(&self, key: &K) -> Result<bool, MutationError> {
        self.get(key).map(|value| value.is_some())
    }

    pub fn insert(&self, key: K, value: V) -> Result<Option<V>, MutationError> {
        let key_bytes = Self::checked_bytes(
            &key,
            self.header.key_size,
            MutationError::InvalidKeyEncoding,
        )?
        .into_owned();
        let value_bytes = Self::checked_bytes(
            &value,
            self.header.value_size,
            MutationError::InvalidValueEncoding,
        )?
        .into_owned();
        let hash_bytes = key.stable_hash_bytes();
        loop {
            let control = control::read(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            if !control.mutation_epoch.is_multiple_of(2) {
                return Err(MutationError::InProgress);
            }
            let candidates = self.candidate_buckets_for_bytes(hash_bytes.as_ref(), control);
            let first = self.inspect_bucket(candidates.0, &key);
            let second =
                (candidates.1 != candidates.0).then(|| self.inspect_bucket(candidates.1, &key));
            let existing = first
                .existing
                .map(|location| (&first, location))
                .or_else(|| {
                    second
                        .as_ref()
                        .and_then(|bucket| bucket.existing.map(|location| (bucket, location)))
                });
            if let Some((inspection, location)) = existing {
                let previous = self.value_from_block(&inspection.block, location);
                let mut page = self.page_from_block(&inspection.block, location.page);
                self.write_value_in_page(&mut page, location.slot, &value_bytes);
                let guard = self.begin_mutation_at(control.mutation_epoch)?;
                self.write_bucket_page(location.bucket, location.page, &page);
                guard.finish();
                return Ok(Some(previous));
            }
            let second_ref = second.as_ref();
            let free = match (first.free, second_ref.and_then(|bucket| bucket.free)) {
                (Some(location), Some(alternate)) => {
                    if first.load <= second_ref.expect("alternate inspection").load {
                        Some((&first, location))
                    } else {
                        Some((second_ref.expect("alternate inspection"), alternate))
                    }
                }
                (Some(location), None) => Some((&first, location)),
                (None, Some(location)) => {
                    Some((second_ref.expect("alternate inspection"), location))
                }
                (None, None) => None,
            };
            if let Some((inspection, location)) = free {
                let mut page = self.page_from_block(&inspection.block, location.page);
                self.write_entry_in_page(&mut page, location.slot, &key_bytes, &value_bytes);
                let mut next = control;
                next.len = next
                    .len
                    .checked_add(1)
                    .ok_or(MutationError::CapacityOverflow)?;
                if location.page > 0 {
                    next.overflow_entries = next
                        .overflow_entries
                        .checked_add(1)
                        .ok_or(MutationError::CapacityOverflow)?;
                }
                next.split_debt = self.debt_after_insert(next, location.page > 0)?;
                let guard = self.begin_mutation_at(control.mutation_epoch)?;
                self.write_bucket_page(location.bucket, location.page, &page);
                next.mutation_epoch = control.mutation_epoch + 1;
                control::write(&self.memory, self.header.control_offset, next);
                guard.finish();
                return Ok(None);
            }
            match self.plan_split(control, Some((&key_bytes, &value_bytes))) {
                Ok(plan) => {
                    self.apply_split(plan)?;
                    return Ok(None);
                }
                Err(MutationError::TablePressure) => {
                    let maintenance = self.plan_split(control, None)?;
                    self.apply_split(maintenance)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn remove(&self, key: &K) -> Result<Option<V>, MutationError> {
        let control = control::read(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(MutationError::InProgress);
        }
        let hash = key.stable_hash_bytes();
        let candidates = self.candidate_buckets_for_bytes(hash.as_ref(), control);
        let Some(location) = self.find_location(candidates, key) else {
            self.ensure_epoch(control.mutation_epoch)?;
            return Ok(None);
        };
        let previous = self.read_value_at(location);
        let mut page = self.read_bucket_page(location.bucket, location.page);
        self.clear_occupancy_in_page(&mut page, location.slot);
        let mut next = control;
        next.len = next
            .len
            .checked_sub(1)
            .ok_or(MutationError::CapacityOverflow)?;
        if location.page > 0 {
            next.overflow_entries = next
                .overflow_entries
                .checked_sub(1)
                .ok_or(MutationError::CapacityOverflow)?;
        }
        if next.overflow_entries == 0 && next.len < Self::split_threshold(next.physical_buckets)? {
            next.split_debt = 0;
        }
        let guard = self.begin_mutation_at(control.mutation_epoch)?;
        self.write_bucket_page(location.bucket, location.page, &page);
        next.mutation_epoch = control.mutation_epoch + 1;
        control::write(&self.memory, self.header.control_offset, next);
        guard.finish();
        Ok(Some(previous))
    }

    fn expected_header(hash_seed: u64) -> Result<Header, InitError> {
        if !K::BOUND.is_fixed_size() || !V::BOUND.is_fixed_size() {
            return Err(InitError::IncompatibleElementType);
        }
        let key_size = K::BOUND.max_size();
        let value_size = V::BOUND.max_size();
        let value_slab_offset = PAGE_HEADER_BYTES
            .checked_add(
                u64::from(PRIMARY_SLOTS)
                    .checked_mul(u64::from(key_size))
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        let bucket_page_stride = value_slab_offset
            .checked_add(
                u64::from(PRIMARY_SLOTS)
                    .checked_mul(u64::from(value_size))
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        let bucket_block_stride = bucket_page_stride
            .checked_mul(u64::from(PAGES_PER_BUCKET))
            .ok_or(InitError::InvalidLayout)?;
        Ok(Header {
            key_size,
            value_size,
            key_storage_schema_id: K::KEY_STORAGE_ID,
            key_routing_schema_id: K::KEY_ROUTING_ID,
            value_storage_schema_id: V::VALUE_STORAGE_ID,
            hash_seed,
            bucket_size: PRIMARY_SLOTS,
            control_offset: HEADER_SIZE,
            control_bytes: CONTROL_BYTES,
            buckets_offset: BUCKETS_OFFSET,
            value_slab_offset,
            bucket_page_stride,
            bucket_block_stride,
        })
    }

    fn validate_control(control: ControlRegion) -> Result<(), InitError> {
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(InitError::RecoveryRequired);
        }
        let base = Self::base_buckets(control.level).ok_or(InitError::InvalidLayout)?;
        let expected = base
            .checked_add(control.split_cursor)
            .ok_or(InitError::InvalidLayout)?;
        let capacity = expected
            .checked_mul(u64::from(SLOTS_PER_BUCKET))
            .ok_or(InitError::InvalidLayout)?;
        if control.level < INITIAL_LEVEL
            || control.split_cursor >= base
            || control.physical_buckets != expected
            || control.len > capacity
            || control.overflow_entries > control.len
            || control.incarnation == 0
        {
            return Err(InitError::InvalidLayout);
        }
        Ok(())
    }

    fn scan_control_before_step(&self) -> Result<(u64, ControlRegion), ScanError> {
        let epoch = control::read_mutation_epoch(&self.memory, self.header.control_offset);
        if !epoch.is_multiple_of(2) {
            return Err(ScanError::InProgress);
        }
        let control = control::read_for_open(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        )
        .map_err(|_| ScanError::RestartRequired)?;
        if control.mutation_epoch != epoch || !control.mutation_epoch.is_multiple_of(2) {
            return Err(ScanError::InProgress);
        }
        Self::validate_control(control).map_err(|_| ScanError::RestartRequired)?;
        Ok((epoch, control))
    }

    fn scan_physical_window(&self, start: u64, end: u64) -> Vec<(K, V)> {
        let mut entries = Vec::new();
        let mut position = start;
        while position < end {
            let bucket = position / u64::from(SLOTS_PER_BUCKET);
            let local_start = (position % u64::from(SLOTS_PER_BUCKET)) as u32;
            let bucket_end = end.min((bucket + 1) * u64::from(SLOTS_PER_BUCKET));
            let local_end = (bucket_end - bucket * u64::from(SLOTS_PER_BUCKET)) as u32;
            let block = self.read_bucket_block(bucket);
            let mut page = local_start / PRIMARY_SLOTS;
            while page < PAGES_PER_BUCKET && page * PRIMARY_SLOTS < local_end {
                let first = if page == local_start / PRIMARY_SLOTS {
                    local_start % PRIMARY_SLOTS
                } else {
                    0
                };
                let last = local_end
                    .saturating_sub(page * PRIMARY_SLOTS)
                    .min(PRIMARY_SLOTS);
                let occupancy = Self::block_occupancy(&block, page);
                for slot in first..last {
                    if occupancy & (1u64 << slot) == 0 {
                        continue;
                    }
                    entries.push(self.decode_block_entry(&block, page, slot));
                }
                page += 1;
            }
            position = bucket_end;
        }
        entries
    }

    fn current_scrub_snapshot(&self) -> Result<ScrubSnapshot, ScrubError> {
        let persisted = header::read(&self.memory).map_err(|_| ScrubError::Stale)?;
        if persisted != self.header {
            return Err(ScrubError::Stale);
        }
        let control = control::read_for_open(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        )
        .map_err(|_| ScrubError::Stale)?;
        Self::validate_control(control).map_err(|_| ScrubError::Stale)?;
        Ok(ScrubSnapshot {
            key_storage_schema_id: self.header.key_storage_schema_id,
            key_routing_schema_id: self.header.key_routing_schema_id,
            value_storage_schema_id: self.header.value_storage_schema_id,
            hash_seed: self.header.hash_seed,
            mutation_epoch: control.mutation_epoch,
            incarnation: control.incarnation,
            len: control.len,
            physical_buckets: control.physical_buckets,
            split_debt: control.split_debt,
            overflow_entries: control.overflow_entries,
        })
    }

    fn ensure_scrub_fence(&self, snapshot: ScrubSnapshot) -> Result<(), ScrubError> {
        (self.current_scrub_snapshot()? == snapshot)
            .then_some(())
            .ok_or(ScrubError::Stale)
    }

    fn scrub_control(&self, snapshot: ScrubSnapshot) -> ControlRegion {
        let level = (u64::BITS - 1 - snapshot.physical_buckets.leading_zeros()) as u8;
        ControlRegion {
            len: snapshot.len,
            physical_buckets: snapshot.physical_buckets,
            mutation_epoch: snapshot.mutation_epoch,
            incarnation: snapshot.incarnation,
            split_debt: snapshot.split_debt,
            overflow_entries: snapshot.overflow_entries,
            level,
            split_cursor: snapshot.physical_buckets - (1u64 << level),
            hash_seed: snapshot.hash_seed,
        }
    }

    fn scrub_bucket(&self, bucket: u64, control: ControlRegion) -> Result<u64, ScrubError> {
        let block = self.read_bucket_block(bucket);
        let mut count = 0u64;
        for page in 0..PAGES_PER_BUCKET {
            let occupancy = Self::block_occupancy(&block, page);
            for slot in 0..PRIMARY_SLOTS {
                if occupancy & (1u64 << slot) == 0 {
                    continue;
                }
                let (key, _) = self.decode_block_entry(&block, page, slot);
                let route = key.stable_hash_bytes();
                let candidates = self.candidate_buckets_for_bytes(route.as_ref(), control);
                if candidates.0 != bucket && candidates.1 != bucket {
                    return Err(ScrubError::UnreachablePlacement {
                        bucket,
                        slot: page * PRIMARY_SLOTS + slot,
                    });
                }
                count = count.checked_add(1).ok_or(ScrubError::LengthMismatch {
                    expected: control.len,
                    actual: u64::MAX,
                })?;
            }
        }
        Ok(count)
    }

    fn read_consistent<T>(&self, read: impl FnOnce() -> T) -> Result<T, MutationError> {
        let epoch = self.idle_epoch()?;
        let value = read();
        (control::read_mutation_epoch(&self.memory, self.header.control_offset) == epoch)
            .then_some(value)
            .ok_or(MutationError::InProgress)
    }

    fn read_consistent_hot<T>(
        &self,
        read: impl FnOnce(control::HotControl) -> T,
    ) -> Result<T, MutationError> {
        let (hot, epoch) = control::read_hot_with_epoch(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        if !epoch.is_multiple_of(2) {
            return Err(MutationError::InProgress);
        }
        let value = read(hot);
        (control::read_mutation_epoch(&self.memory, self.header.control_offset) == epoch)
            .then_some(value)
            .ok_or(MutationError::InProgress)
    }

    fn idle_epoch(&self) -> Result<u64, MutationError> {
        let epoch = control::read_mutation_epoch(&self.memory, self.header.control_offset);
        if !epoch.is_multiple_of(2) {
            return Err(MutationError::InProgress);
        }
        Ok(epoch)
    }

    fn begin_mutation_at(
        &self,
        observed_epoch: u64,
    ) -> Result<MutationGuard<'_, K, V, M>, MutationError> {
        let epoch = self.idle_epoch()?;
        if epoch != observed_epoch {
            return Err(MutationError::InProgress);
        }
        let completed = epoch.checked_add(2).ok_or(MutationError::EpochExhausted)?;
        control::write_mutation_epoch(&self.memory, self.header.control_offset, epoch + 1);
        Ok(MutationGuard {
            map: self,
            completed_epoch: completed,
        })
    }

    fn ensure_epoch(&self, observed_epoch: u64) -> Result<(), MutationError> {
        (control::read_mutation_epoch(&self.memory, self.header.control_offset) == observed_epoch)
            .then_some(())
            .ok_or(MutationError::InProgress)
    }

    fn get_with_hot(&self, key: &K, hot: control::HotControl) -> Option<V> {
        let candidates = self.candidate_buckets_for_key(key, hot);
        self.get_value_in_bucket(candidates.0, key).or_else(|| {
            (candidates.1 != candidates.0)
                .then(|| self.get_value_in_bucket(candidates.1, key))
                .flatten()
        })
    }

    fn candidate_buckets_for_key(&self, key: &K, hot: control::HotControl) -> (u64, u64) {
        let bytes = key.stable_hash_bytes();
        self.candidate_buckets_from_bytes(
            bytes.as_ref(),
            hot.level,
            hot.split_cursor,
            hot.hash_seed,
        )
    }

    fn candidate_buckets_for_bytes(&self, bytes: &[u8], control: ControlRegion) -> (u64, u64) {
        self.candidate_buckets_from_bytes(
            bytes,
            control.level,
            control.split_cursor,
            control.hash_seed,
        )
    }

    fn candidate_buckets_from_bytes(
        &self,
        bytes: &[u8],
        level: u8,
        split_cursor: u64,
        seed: u64,
    ) -> (u64, u64) {
        let secrets = self.secrets_for_seed(seed);
        (
            linear_bucket(hash(bytes, &secrets[0]), level, split_cursor),
            linear_bucket(hash(bytes, &secrets[1]), level, split_cursor),
        )
    }

    fn secrets_for_seed(&self, seed: u64) -> [RapidSecrets; 2] {
        if seed == self.header.hash_seed {
            self.hash_secrets
        } else {
            hash_secrets(seed)
        }
    }

    fn find_location(&self, candidates: (u64, u64), key: &K) -> Option<Location> {
        let first = self.find_in_bucket(candidates.0, key);
        first.or_else(|| {
            (candidates.1 != candidates.0)
                .then(|| self.find_in_bucket(candidates.1, key))
                .flatten()
        })
    }

    fn find_in_bucket(&self, bucket: u64, key: &K) -> Option<Location> {
        let primary = self.read_bucket_page(bucket, 0);
        if let Some(slot) = self.find_in_page_bytes(&primary, key) {
            return Some(Location {
                bucket,
                page: 0,
                slot,
            });
        }
        let tail = self.read_bucket_tail(bucket);
        for page in 1..PAGES_PER_BUCKET {
            let start = (page - 1) as usize * self.header.bucket_page_stride as usize;
            let end = start + self.header.bucket_page_stride as usize;
            if let Some(slot) = self.find_in_page_bytes(&tail[start..end], key) {
                return Some(Location { bucket, page, slot });
            }
        }
        None
    }

    fn inspect_bucket(&self, bucket: u64, key: &K) -> BucketInspection {
        let block = self.read_bucket_block(bucket);
        let mut existing = None;
        let mut free = None;
        let mut load = 0;
        for page in 0..PAGES_PER_BUCKET {
            let occupancy = Self::block_occupancy(&block, page);
            load += occupancy.count_ones();
            if free.is_none() && occupancy != PAGE_FULL_MASK {
                free = Some(Location {
                    bucket,
                    page,
                    slot: (!occupancy).trailing_zeros(),
                });
            }
            let mut bits = occupancy;
            while bits != 0 {
                let slot = bits.trailing_zeros();
                let key_offset = Self::block_key_offset(self.header, page, slot);
                if K::from_bytes(Cow::Borrowed(
                    &block[key_offset..key_offset + self.header.key_size as usize],
                )) == *key
                {
                    existing = Some(Location { bucket, page, slot });
                    break;
                }
                bits &= bits - 1;
            }
            if existing.is_some() {
                break;
            }
        }
        BucketInspection {
            block,
            existing,
            free,
            load,
        }
    }

    fn get_value_in_bucket(&self, bucket: u64, key: &K) -> Option<V> {
        GET_SCRATCH.with(|scratch| {
            let page_stride = self.header.bucket_page_stride as usize;
            let mut scratch = scratch.borrow_mut();
            scratch.resize(page_stride, 0);
            self.memory.read(
                Self::bucket_page_base(self.header, bucket, 0),
                &mut scratch[..page_stride],
            );
            if let Some(slot) = self.find_in_page_bytes(&scratch, key) {
                return Some(self.value_from_page(&scratch, slot));
            }

            let tail_len = page_stride * (PAGES_PER_BUCKET as usize - 1);
            scratch.resize(tail_len, 0);
            self.memory.read(
                Self::bucket_page_base(self.header, bucket, 1),
                &mut scratch[..tail_len],
            );
            for page in 1..PAGES_PER_BUCKET {
                let start = (page - 1) as usize * page_stride;
                let end = start + page_stride;
                let page_bytes = &scratch[start..end];
                if let Some(slot) = self.find_in_page_bytes(page_bytes, key) {
                    return Some(self.value_from_page(page_bytes, slot));
                }
            }
            None
        })
    }

    fn find_in_page_bytes(&self, page: &[u8], key: &K) -> Option<u32> {
        let mut bits = Self::page_occupancy(page);
        while bits != 0 {
            let slot = bits.trailing_zeros();
            let key_offset = Self::page_key_offset(self.header, slot);
            if K::from_bytes(Cow::Borrowed(
                &page[key_offset..key_offset + self.header.key_size as usize],
            )) == *key
            {
                return Some(slot);
            }
            bits &= bits - 1;
        }
        None
    }

    fn value_from_page(&self, page: &[u8], slot: u32) -> V {
        let value_offset = Self::page_value_offset(self.header, slot);
        V::from_bytes(Cow::Borrowed(
            &page[value_offset..value_offset + self.header.value_size as usize],
        ))
    }

    fn value_from_block(&self, block: &[u8], location: Location) -> V {
        let offset = Self::block_value_offset(self.header, location.page, location.slot);
        V::from_bytes(Cow::Owned(
            block[offset..offset + self.header.value_size as usize].to_vec(),
        ))
    }

    fn page_from_block(&self, block: &[u8], page: u32) -> Vec<u8> {
        let stride = self.header.bucket_page_stride as usize;
        let start = page as usize * stride;
        block[start..start + stride].to_vec()
    }

    fn bucket_load(&self, bucket: u64) -> u32 {
        let block = self.read_bucket_block(bucket);
        (0..PAGES_PER_BUCKET)
            .map(|page| Self::block_occupancy(&block, page).count_ones())
            .sum()
    }

    fn read_value_at(&self, location: Location) -> V {
        let mut bytes = vec![0; self.header.value_size as usize];
        self.memory.read(
            Self::value_offset(self.header, location.bucket, location.page, location.slot),
            &mut bytes,
        );
        V::from_bytes(Cow::Owned(bytes))
    }

    fn plan_split(
        &self,
        control: ControlRegion,
        insert: Option<(&[u8], &[u8])>,
    ) -> Result<SplitPlan, MutationError> {
        let (next_level, next_cursor, next_buckets) = Self::next_geometry(control)?;
        let source_bucket = control.split_cursor;
        let new_bucket = source_bucket
            .checked_add(Self::base_buckets(control.level).ok_or(MutationError::CapacityOverflow)?)
            .ok_or(MutationError::CapacityOverflow)?;
        let source = self.read_bucket_image(source_bucket);
        let source_old_overflow = source.overflow_entries;
        let mut source_block = BucketImage::empty(self.header.bucket_block_stride as usize);
        let mut new_block = BucketImage::empty(self.header.bucket_block_stride as usize);
        let next_control = ControlRegion {
            level: next_level,
            split_cursor: next_cursor,
            physical_buckets: next_buckets,
            ..control
        };
        for (key, value, _) in self.entries_from_image(&source) {
            let key_value = K::from_bytes(Cow::Borrowed(&key));
            let hash = key_value.stable_hash_bytes();
            let candidates = self.candidate_buckets_for_bytes(hash.as_ref(), next_control);
            let destination = if candidates.0 == source_bucket || candidates.1 == source_bucket {
                source_bucket
            } else if candidates.0 == new_bucket || candidates.1 == new_bucket {
                new_bucket
            } else {
                return Err(MutationError::TablePressure);
            };
            let target = if destination == source_bucket {
                &mut source_block
            } else {
                &mut new_block
            };
            Self::append_entry_to_image(target, self.header, &key, &value)
                .ok_or(MutationError::TablePressure)?;
        }
        if let Some((key, value)) = insert {
            let key_value = K::from_bytes(Cow::Borrowed(key));
            let hash = key_value.stable_hash_bytes();
            let candidates = self.candidate_buckets_for_bytes(hash.as_ref(), next_control);
            let choice = self.choose_image_location(
                candidates,
                source_bucket,
                &source_block,
                new_bucket,
                &new_block,
            )?;
            match choice {
                ImageChoice::Source => {
                    Self::append_entry_to_image(&mut source_block, self.header, key, value)
                        .ok_or(MutationError::TablePressure)?;
                }
                ImageChoice::New => {
                    Self::append_entry_to_image(&mut new_block, self.header, key, value)
                        .ok_or(MutationError::TablePressure)?;
                }
                ImageChoice::Existing(bucket) => {
                    let mut image = self.read_bucket_image(bucket);
                    Self::append_entry_to_image(&mut image, self.header, key, value)
                        .ok_or(MutationError::TablePressure)?;
                    let blocks = vec![
                        (source_bucket, source_block),
                        (new_bucket, new_block),
                        (bucket, image),
                    ];
                    return self.finish_split_plan(
                        control,
                        SplitGeometry {
                            source_bucket,
                            new_bucket,
                            next_buckets,
                        },
                        blocks,
                        true,
                        source_old_overflow,
                    );
                }
            }
            return self.finish_split_plan(
                control,
                SplitGeometry {
                    source_bucket,
                    new_bucket,
                    next_buckets,
                },
                vec![(source_bucket, source_block), (new_bucket, new_block)],
                true,
                source_old_overflow,
            );
        }
        self.finish_split_plan(
            control,
            SplitGeometry {
                source_bucket,
                new_bucket,
                next_buckets,
            },
            vec![(source_bucket, source_block), (new_bucket, new_block)],
            false,
            source_old_overflow,
        )
    }

    fn finish_split_plan(
        &self,
        control: ControlRegion,
        geometry: SplitGeometry,
        blocks: Vec<(u64, BucketImage)>,
        inserted: bool,
        source_old_overflow: u64,
    ) -> Result<SplitPlan, MutationError> {
        let mut unique = Vec::with_capacity(blocks.len());
        for (bucket, image) in blocks {
            if let Some((_, existing)) = unique.iter_mut().find(|(known, _)| *known == bucket) {
                *existing = image;
            } else {
                unique.push((bucket, image));
            }
        }
        let new_len = control
            .len
            .checked_add(u64::from(inserted))
            .ok_or(MutationError::CapacityOverflow)?;
        let mut overflow = control
            .overflow_entries
            .checked_sub(source_old_overflow)
            .ok_or(MutationError::CapacityOverflow)?;
        for (bucket, image) in &unique {
            if *bucket == geometry.source_bucket || *bucket == geometry.new_bucket {
                overflow = overflow
                    .checked_add(image.overflow_entries)
                    .ok_or(MutationError::CapacityOverflow)?;
            } else {
                overflow = overflow
                    .checked_sub(self.read_bucket_image(*bucket).overflow_entries)
                    .ok_or(MutationError::CapacityOverflow)?
                    .checked_add(image.overflow_entries)
                    .ok_or(MutationError::CapacityOverflow)?;
            }
        }
        let debt = control.split_debt.saturating_sub(1);
        let threshold = Self::split_threshold(geometry.next_buckets)?;
        let split_debt = if overflow > 0 || new_len >= threshold {
            debt.max(1)
        } else {
            debt
        };
        let moved_entries: u64 = unique.iter().map(|(_, image)| image.entries).sum();
        let moved_bytes = moved_entries
            .checked_mul(u64::from(self.header.key_size) + u64::from(self.header.value_size))
            .ok_or(MutationError::CapacityOverflow)?;
        if moved_entries > SPLIT_ENTRY_BUDGET || moved_bytes > SPLIT_BYTE_BUDGET {
            return Err(MutationError::TablePressure);
        }
        let grow_to = Self::bucket_end_for(self.header, geometry.next_buckets)
            .ok_or(MutationError::CapacityOverflow)?;
        Ok(SplitPlan {
            observed_epoch: control.mutation_epoch,
            next_buckets: geometry.next_buckets,
            blocks: unique,
            len: new_len,
            split_debt,
            overflow_entries: overflow,
            grow_to,
        })
    }

    fn choose_image_location(
        &self,
        candidates: (u64, u64),
        source: u64,
        source_image: &BucketImage,
        new: u64,
        new_image: &BucketImage,
    ) -> Result<ImageChoice, MutationError> {
        let mut choices = Vec::new();
        for bucket in [candidates.0, candidates.1] {
            if choices
                .iter()
                .any(|(known, _): &(u64, u64)| *known == bucket)
            {
                continue;
            }
            let load = if bucket == source {
                source_image.entries
            } else if bucket == new {
                new_image.entries
            } else {
                self.bucket_load(bucket).into()
            };
            if load < u64::from(SLOTS_PER_BUCKET) {
                choices.push((bucket, load));
            }
        }
        let Some((bucket, _)) = choices.into_iter().min_by_key(|(_, load)| *load) else {
            return Err(MutationError::TablePressure);
        };
        Ok(if bucket == source {
            ImageChoice::Source
        } else if bucket == new {
            ImageChoice::New
        } else {
            ImageChoice::Existing(bucket)
        })
    }

    fn apply_split(&self, plan: SplitPlan) -> Result<(), MutationError> {
        grow_to_bytes(&self.memory, plan.grow_to).map_err(Self::map_grow_error)?;
        let guard = self.begin_mutation_at(plan.observed_epoch)?;
        for (bucket, image) in &plan.blocks {
            self.write_bucket_block(*bucket, &image.bytes);
        }
        control::publish_split(
            &self.memory,
            self.header.control_offset,
            plan.next_buckets,
            plan.len,
            plan.split_debt,
            plan.overflow_entries,
        );
        guard.finish();
        Ok(())
    }

    fn debt_after_insert(
        &self,
        control: ControlRegion,
        inserted_in_overflow: bool,
    ) -> Result<u64, MutationError> {
        let threshold = Self::split_threshold(control.physical_buckets)?;
        Ok(if inserted_in_overflow || control.len >= threshold {
            control.split_debt.max(1)
        } else {
            control.split_debt
        })
    }

    fn next_geometry(control: ControlRegion) -> Result<(u8, u64, u64), MutationError> {
        let base = Self::base_buckets(control.level).ok_or(MutationError::CapacityOverflow)?;
        let buckets = control
            .physical_buckets
            .checked_add(1)
            .ok_or(MutationError::CapacityOverflow)?;
        if control.split_cursor + 1 == base {
            let level = control
                .level
                .checked_add(1)
                .filter(|value| *value < 63)
                .ok_or(MutationError::CapacityOverflow)?;
            Ok((level, 0, buckets))
        } else {
            Ok((control.level, control.split_cursor + 1, buckets))
        }
    }

    fn split_threshold(physical_buckets: u64) -> Result<u64, MutationError> {
        physical_buckets
            .checked_mul(u64::from(SLOTS_PER_BUCKET))
            .and_then(|capacity| capacity.checked_mul(3))
            .and_then(|value| value.checked_div(4))
            .ok_or(MutationError::CapacityOverflow)
    }

    fn base_buckets(level: u8) -> Option<u64> {
        (level < 63).then(|| 1u64 << level)
    }

    fn map_grow_error(error: GrowError) -> MutationError {
        match error {
            GrowError::OutOfMemory => MutationError::OutOfMemory,
            GrowError::CapacityOverflow => MutationError::CapacityOverflow,
        }
    }

    fn read_bucket_block(&self, bucket: u64) -> Vec<u8> {
        let mut bytes = vec![0; self.header.bucket_block_stride as usize];
        self.memory
            .read(Self::bucket_base(self.header, bucket), &mut bytes);
        bytes
    }

    fn read_bucket_page(&self, bucket: u64, page: u32) -> Vec<u8> {
        let mut bytes = vec![0; self.header.bucket_page_stride as usize];
        self.memory.read(
            Self::bucket_page_base(self.header, bucket, page),
            &mut bytes,
        );
        bytes
    }

    fn read_bucket_tail(&self, bucket: u64) -> Vec<u8> {
        let page_stride = self.header.bucket_page_stride as usize;
        let mut bytes = vec![0; page_stride * (PAGES_PER_BUCKET as usize - 1)];
        self.memory
            .read(Self::bucket_page_base(self.header, bucket, 1), &mut bytes);
        bytes
    }

    fn write_bucket_block(&self, bucket: u64, bytes: &[u8]) {
        self.memory
            .write(Self::bucket_base(self.header, bucket), bytes);
    }

    fn write_bucket_page(&self, bucket: u64, page: u32, bytes: &[u8]) {
        self.memory
            .write(Self::bucket_page_base(self.header, bucket, page), bytes);
    }

    fn image_from_bytes(header: Header, bytes: Vec<u8>) -> BucketImage {
        let mut entries = 0u64;
        let mut overflow_entries = 0u64;
        for page in 0..PAGES_PER_BUCKET {
            let occupancy = Self::block_occupancy(&bytes, page);
            let count = u64::from(occupancy.count_ones());
            entries += count;
            if page > 0 {
                overflow_entries += count;
            }
        }
        let _ = header;
        BucketImage {
            bytes,
            entries,
            overflow_entries,
        }
    }

    fn read_bucket_image(&self, bucket: u64) -> BucketImage {
        Self::image_from_bytes(self.header, self.read_bucket_block(bucket))
    }

    fn entries_from_image(&self, image: &BucketImage) -> Vec<(Vec<u8>, Vec<u8>, u32)> {
        let mut entries = Vec::new();
        for page in 0..PAGES_PER_BUCKET {
            let occupancy = Self::block_occupancy(&image.bytes, page);
            for slot in 0..PRIMARY_SLOTS {
                if occupancy & (1u64 << slot) == 0 {
                    continue;
                }
                let key_offset = Self::block_key_offset(self.header, page, slot);
                let value_offset = Self::block_value_offset(self.header, page, slot);
                entries.push((
                    image.bytes[key_offset..key_offset + self.header.key_size as usize].to_vec(),
                    image.bytes[value_offset..value_offset + self.header.value_size as usize]
                        .to_vec(),
                    page * PRIMARY_SLOTS + slot,
                ));
            }
        }
        entries
    }

    fn decode_block_entry(&self, block: &[u8], page: u32, slot: u32) -> (K, V) {
        let key_offset = Self::block_key_offset(self.header, page, slot);
        let value_offset = Self::block_value_offset(self.header, page, slot);
        (
            K::from_bytes(Cow::Owned(
                block[key_offset..key_offset + self.header.key_size as usize].to_vec(),
            )),
            V::from_bytes(Cow::Owned(
                block[value_offset..value_offset + self.header.value_size as usize].to_vec(),
            )),
        )
    }

    fn write_value_in_page(&self, page: &mut [u8], slot: u32, value: &[u8]) {
        let offset = Self::page_value_offset(self.header, slot);
        page[offset..offset + self.header.value_size as usize].copy_from_slice(value);
    }

    fn clear_occupancy_in_page(&self, page: &mut [u8], slot: u32) {
        let occupancy = Self::page_occupancy(page);
        Self::set_page_occupancy(page, occupancy & !(1u64 << slot));
    }

    fn write_entry_in_page(&self, page: &mut [u8], slot: u32, key: &[u8], value: &[u8]) {
        let key_offset = Self::page_key_offset(self.header, slot);
        let value_offset = Self::page_value_offset(self.header, slot);
        page[key_offset..key_offset + self.header.key_size as usize].copy_from_slice(key);
        page[value_offset..value_offset + self.header.value_size as usize].copy_from_slice(value);
        let occupancy = Self::page_occupancy(page);
        Self::set_page_occupancy(page, occupancy | (1u64 << slot));
    }

    fn append_entry_to_image(
        image: &mut BucketImage,
        header: Header,
        key: &[u8],
        value: &[u8],
    ) -> Option<Location> {
        for page in 0..PAGES_PER_BUCKET {
            let occupancy = Self::block_occupancy(&image.bytes, page);
            if occupancy != PAGE_FULL_MASK {
                let slot = (!occupancy).trailing_zeros();
                let key_offset = Self::block_key_offset(header, page, slot);
                let value_offset = Self::block_value_offset(header, page, slot);
                image.bytes[key_offset..key_offset + header.key_size as usize].copy_from_slice(key);
                image.bytes[value_offset..value_offset + header.value_size as usize]
                    .copy_from_slice(value);
                Self::set_block_occupancy(&mut image.bytes, page, occupancy | (1u64 << slot));
                image.entries += 1;
                if page > 0 {
                    image.overflow_entries += 1;
                }
                return Some(Location {
                    bucket: 0,
                    page,
                    slot,
                });
            }
        }
        None
    }

    fn block_occupancy(block: &[u8], page: u32) -> u64 {
        let offset = page as usize * self_page_stride(block, page);
        u64::from_le_bytes(
            block[offset..offset + 8]
                .try_into()
                .expect("page occupancy"),
        )
    }

    fn set_block_occupancy(block: &mut [u8], page: u32, occupancy: u64) {
        let stride = block.len() / PAGES_PER_BUCKET as usize;
        let offset = page as usize * stride;
        block[offset..offset + 8].copy_from_slice(&occupancy.to_le_bytes());
    }

    fn page_occupancy(page: &[u8]) -> u64 {
        u64::from_le_bytes(page[..8].try_into().expect("page occupancy"))
    }

    fn set_page_occupancy(page: &mut [u8], occupancy: u64) {
        page[..8].copy_from_slice(&occupancy.to_le_bytes());
    }

    fn page_key_offset(header: Header, slot: u32) -> usize {
        PAGE_HEADER_BYTES as usize + slot as usize * header.key_size as usize
    }

    fn page_value_offset(header: Header, slot: u32) -> usize {
        header.value_slab_offset as usize + slot as usize * header.value_size as usize
    }

    fn block_key_offset(header: Header, page: u32, slot: u32) -> usize {
        page as usize * header.bucket_page_stride as usize
            + PAGE_HEADER_BYTES as usize
            + slot as usize * header.key_size as usize
    }
    fn block_value_offset(header: Header, page: u32, slot: u32) -> usize {
        page as usize * header.bucket_page_stride as usize
            + header.value_slab_offset as usize
            + slot as usize * header.value_size as usize
    }
    fn value_offset(header: Header, bucket: u64, page: u32, slot: u32) -> u64 {
        Self::bucket_base(header, bucket)
            + page as u64 * header.bucket_page_stride
            + header.value_slab_offset
            + slot as u64 * header.value_size as u64
    }
    fn bucket_base(header: Header, bucket: u64) -> u64 {
        header.buckets_offset + bucket * header.bucket_block_stride
    }
    fn bucket_page_base(header: Header, bucket: u64, page: u32) -> u64 {
        Self::bucket_base(header, bucket) + page as u64 * header.bucket_page_stride
    }
    fn bucket_end_for(header: Header, buckets: u64) -> Option<u64> {
        header
            .buckets_offset
            .checked_add(buckets.checked_mul(header.bucket_block_stride)?)
    }

    fn checked_bytes<'a, T: Storable>(
        value: &'a T,
        expected: u32,
        error: MutationError,
    ) -> Result<Cow<'a, [u8]>, MutationError> {
        let bytes = value.to_bytes();
        if bytes.len() != expected as usize {
            return Err(error);
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy)]
enum ImageChoice {
    Source,
    New,
    Existing(u64),
}

fn self_page_stride(block: &[u8], _page: u32) -> usize {
    block.len() / PAGES_PER_BUCKET as usize
}

impl BucketImage {
    fn empty(bytes: usize) -> Self {
        Self {
            bytes: vec![0; bytes],
            entries: 0,
            overflow_entries: 0,
        }
    }
}

fn linear_bucket(hash: u64, level: u8, split_cursor: u64) -> u64 {
    let mask = (1u64 << level) - 1;
    let bucket = hash & mask;
    if bucket < split_cursor {
        hash & ((mask << 1) | 1)
    } else {
        bucket
    }
}

fn hash_secrets(seed: u64) -> [RapidSecrets; 2] {
    [
        RapidSecrets::seed(seed ^ HASH_DOMAIN_0),
        RapidSecrets::seed(seed ^ HASH_DOMAIN_1),
    ]
}

fn hash(bytes: &[u8], secrets: &RapidSecrets) -> u64 {
    rapidhash_v3_inline::<true, false, false>(bytes, secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    type Map = StableLinearHashMap<u64, u64, VectorMemory>;

    fn map() -> Map {
        Map::new_with_hash_seed(VectorMemory::default(), 0x6a09_e667_f3bc_c909)
            .expect("create final V1 map")
    }

    #[test]
    fn final_v1_format_reopens_and_rejects_pre_fingerprint_bytes() {
        let map = map();
        let memory = map.into_memory();
        let reopened = Map::open(memory.clone()).expect("reopen final V1 bytes");
        assert_eq!(reopened.len(), Ok(0));
        memory.write(72, &[0; 16]);
        assert!(matches!(Map::open(memory), Err(InitError::InvalidLayout)));
    }

    #[test]
    fn overflow_insert_get_update_remove_and_reopen() {
        let map = map();
        for key in 0..512u64 {
            assert_eq!(map.insert(key, key ^ 0x55), Ok(None), "insert {key}");
        }
        assert_eq!(map.len(), Ok(512));
        for key in 0..512u64 {
            assert_eq!(map.get(&key), Ok(Some(key ^ 0x55)));
        }
        assert!(map.control_region().expect("control").overflow_entries > 0);
        assert_eq!(map.insert(7, 999), Ok(Some(7 ^ 0x55)));
        assert_eq!(map.remove(&7), Ok(Some(999)));
        let reopened = Map::open(map.into_memory()).expect("reopen overflow map");
        assert_eq!(reopened.get(&7), Ok(None));
        assert_eq!(reopened.len(), Ok(511));
    }

    #[test]
    fn split_debt_is_persistent_and_budgeted() {
        let map = map();
        for key in 0..256u64 {
            assert_eq!(map.insert(key, key), Ok(None));
        }
        let before = map.control_region().expect("debt control");
        assert!(before.split_debt > 0);
        let pending = map.maintenance_step(0, 0).expect("zero budget");
        assert!(matches!(pending, MaintenanceStep::Pending { .. }));
        let progress = map
            .maintenance_step(1024, 1024 * 1024)
            .expect("maintenance");
        assert!(matches!(
            progress,
            MaintenanceStep::Progress { .. } | MaintenanceStep::Idle { .. }
        ));
        let reopened = Map::open(map.into_memory()).expect("reopen debt map");
        assert_eq!(
            reopened.control_region().expect("reopened control").len,
            256
        );
    }

    #[test]
    fn scan_reopen_reset_and_stale_cursor_fail_closed() {
        let map = map();
        for key in 0..64u64 {
            map.insert(key, key).expect("scan insert");
        }
        let cursor = map.scan_start().expect("scan start");
        let page = map.scan_step(cursor, 13).expect("scan page");
        assert!(page.examined_slots() > 0);
        let incarnation = map.control_region().expect("scan control").incarnation;
        let memory = map.into_memory();
        let reopened = Map::open(memory).expect("scan reopen");
        let stale = reopened.scan_step(cursor, 1);
        assert!(matches!(
            stale,
            Ok(_) | Err(ScanError::RestartRequired) | Err(ScanError::InProgress)
        ));
        let current = reopened
            .control_region()
            .expect("reset control")
            .incarnation;
        reopened.reset(current).expect("reset");
        assert_eq!(
            reopened.scan_step(cursor, 1),
            Err(ScanError::RestartRequired)
        );
        assert!(reopened.control_region().expect("after reset").incarnation > incarnation);
    }

    #[test]
    fn scrub_covers_inline_overflow_pages() {
        let map = map();
        for key in 0..96u64 {
            map.insert(key, key).expect("scrub insert");
        }
        let mut cursor = map.scrub_snapshot().expect("scrub snapshot");
        while let ScrubStep::InProgress(next) = map.scrub_step(cursor, 3).expect("scrub step") {
            cursor = next;
        }
    }

    #[test]
    fn clear_empties_entries_and_preserves_incarnation_and_seed() {
        let map = map();
        for key in 0..256u64 {
            map.insert(key, key ^ 0xaa).expect("clear insert");
        }
        assert_eq!(map.len(), Ok(256));
        let incarnation = map.control_region().expect("control").incarnation;
        let seed = map.hash_seed().expect("seed");

        map.clear().expect("clear");

        assert_eq!(map.len(), Ok(0));
        assert!(map.is_empty().expect("is empty"));
        for key in 0..256u64 {
            assert_eq!(map.get(&key), Ok(None), "cleared key {key}");
        }
        let control = map.control_region().expect("after clear control");
        assert_eq!(control.incarnation, incarnation, "incarnation preserved");
        assert_eq!(map.hash_seed().expect("seed after clear"), seed);

        // The map remains usable after clear.
        assert_eq!(map.insert(7, 42), Ok(None));
        assert_eq!(map.get(&7), Ok(Some(42)));

        // Clear survives reopen.
        let reopened = Map::open(map.into_memory()).expect("reopen cleared map");
        assert_eq!(reopened.len(), Ok(1));
        assert_eq!(reopened.get(&7), Ok(Some(42)));
    }
}
