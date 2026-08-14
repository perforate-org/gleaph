use crate::control;
use crate::header::{
    self, BUCKETS_OFFSET, CONTROL_BYTES, ControlRegion, HEADER_SIZE, Header, InitError,
};
use crate::memory::{GrowError, grow_to_bytes};
use crate::{StableHashKey, StableMapValue};
use ic_stable_structures::{Memory, Storable};
use rapidhash::v3::{RapidSecrets, rapidhash_v3_inline};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::marker::PhantomData;
#[cfg(not(target_family = "wasm"))]
use std::panic::{AssertUnwindSafe, catch_unwind};

pub const BUCKET_SIZE: u32 = 8;
const INITIAL_LEVEL: u8 = 3;
const INITIAL_BUCKETS: u64 = 1 << INITIAL_LEVEL;
const BUCKET_HEADER_BYTES: u64 = 8;
const BULK_SCAN_MAX_BYTES: u64 = 1024;
const SCAN_CURSOR_MAGIC: [u8; 3] = *b"LHS";
const SCAN_CURSOR_VERSION: u8 = 1;
const SCAN_CURSOR_BYTES: usize = 88;
const DEFAULT_HASH_SEED: u64 = 0x243f_6a88_85a3_08d3;
const HASH_DOMAIN_0: u64 = 0x1319_8a2e_0370_7344;
const HASH_DOMAIN_1: u64 = 0xa409_3822_299f_31d0;

thread_local! {
    static NEXT_SCRUB_LINEAGE: Cell<u64> = const { Cell::new(1) };
}

fn next_scrub_lineage() -> u64 {
    NEXT_SCRUB_LINEAGE.with(|next| {
        let lineage = next.get();
        next.set(lineage.checked_add(1).expect("scrub lineage exhausted"));
        lineage
    })
}

#[cfg(not(target_family = "wasm"))]
fn scrub_callback<T>(callback: impl FnOnce() -> T, error: ScrubError) -> Result<T, ScrubError> {
    catch_unwind(AssertUnwindSafe(callback)).map_err(|_| error)
}

#[cfg(target_family = "wasm")]
fn scrub_callback<T>(callback: impl FnOnce() -> T, _error: ScrubError) -> Result<T, ScrubError> {
    // The production wasm build uses panic-abort. A user callback panic traps and fails closed at
    // the IC update boundary; it cannot be translated into a typed scrub result.
    Ok(callback())
}

/// An operation could not run because a mutation is in progress or a new key has no free
/// candidate slot.
#[derive(Debug, PartialEq, Eq)]
pub enum MutationError {
    /// Both candidate buckets are full.
    TablePressure,
    /// The persisted mutation epoch is odd or changed during a read.
    InProgress,
    /// Publishing a new odd/even mutation-epoch pair would overflow `u64`.
    EpochExhausted,
    /// `K::to_bytes()` did not match the fixed key width recorded by the header.
    InvalidKeyEncoding,
    /// `V::to_bytes()` did not match the fixed value width recorded by the header.
    InvalidValueEncoding,
    /// Growing stable memory for one appended bucket was rejected by the memory implementation.
    OutOfMemory,
    /// Geometry or stable-memory capacity arithmetic overflowed before a mutation began.
    CapacityOverflow,
}

/// A map-local destructive reset was rejected before changing stable bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum ResetError {
    /// The caller's ownership fence is stale; the current incarnation is returned for diagnosis.
    IncarnationMismatch { current: u64 },
    /// The current incarnation cannot be advanced without wrapping.
    IncarnationExhausted,
    /// The persisted mutation epoch is odd or changed before reset acquired the write fence.
    InProgress,
    /// Publishing a new odd/even mutation-epoch pair would overflow `u64`.
    EpochExhausted,
    /// Initial-extent address arithmetic overflowed before the first write.
    CapacityOverflow,
}

impl fmt::Display for ResetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncarnationMismatch { current } => {
                write!(
                    f,
                    "reset incarnation mismatch; current incarnation is {current}"
                )
            }
            Self::IncarnationExhausted => write!(f, "reset incarnation is exhausted"),
            Self::InProgress => write!(f, "a mutation is already in progress"),
            Self::EpochExhausted => write!(f, "mutation epoch is exhausted"),
            Self::CapacityOverflow => write!(f, "initial map extent arithmetic overflowed"),
        }
    }
}

impl std::error::Error for ResetError {}

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TablePressure => write!(f, "both candidate buckets are full"),
            Self::InProgress => write!(f, "a mutation is already in progress"),
            Self::EpochExhausted => write!(f, "mutation epoch is exhausted"),
            Self::InvalidKeyEncoding => {
                write!(f, "key serialization did not match the fixed-width header")
            }
            Self::InvalidValueEncoding => {
                write!(
                    f,
                    "value serialization did not match the fixed-width header"
                )
            }
            Self::OutOfMemory => write!(f, "failed to allocate one linear hash map bucket"),
            Self::CapacityOverflow => write!(f, "linear hash map capacity arithmetic overflowed"),
        }
    }
}

impl std::error::Error for MutationError {}

/// Serializable progress through the map's physical slot order.
///
/// Unlike [`ScrubCursor`], this cursor is intentionally portable across exact reopen and canister
/// upgrade. It captures only immutable schema/seed identity, the reset incarnation, the physical
/// bucket bound, and the next physical slot. It does not capture a mutation epoch or length, so a
/// multi-call scan does not fence the map for an entire lap.
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
    /// Exact byte length produced by [`Self::encode`].
    pub const ENCODED_SIZE: usize = SCAN_CURSOR_BYTES;

    /// Encodes the cursor into its versioned, upgrade-stable representation.
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

    /// Decodes and structurally validates a serialized cursor.
    ///
    /// Map identity and freshness are validated by [`StableLinearHashMap::scan_step`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ScanError> {
        if bytes.len() != SCAN_CURSOR_BYTES
            || bytes[..3] != SCAN_CURSOR_MAGIC
            || bytes[3] != SCAN_CURSOR_VERSION
            || bytes[4..8].iter().any(|byte| *byte != 0)
        {
            return Err(ScanError::InvalidCursor);
        }
        let cursor = Self {
            key_storage_schema_id: bytes[8..24]
                .try_into()
                .expect("fixed scan cursor key-storage identity"),
            key_routing_schema_id: bytes[24..40]
                .try_into()
                .expect("fixed scan cursor key-routing identity"),
            value_storage_schema_id: bytes[40..56]
                .try_into()
                .expect("fixed scan cursor value-storage identity"),
            hash_seed: scan_cursor_u64(bytes, 56),
            incarnation: scan_cursor_u64(bytes, 64),
            physical_buckets: scan_cursor_u64(bytes, 72),
            next_slot: scan_cursor_u64(bytes, 80),
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
            .checked_mul(u64::from(BUCKET_SIZE))
            .filter(|_| self.physical_buckets >= INITIAL_BUCKETS)
            .ok_or(ScanError::InvalidCursor)?;
        if self.incarnation == 0 || self.next_slot > slots {
            return Err(ScanError::InvalidCursor);
        }
        Ok(())
    }
}

fn scan_cursor_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed scan cursor field"),
    )
}

/// One bounded physical scan result.
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

/// A bounded physical scan could not produce one internally consistent step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// The caller supplied no physical-slot work budget.
    ZeroBudget,
    /// The cursor encoding, bounds, or immutable map identity is invalid.
    InvalidCursor,
    /// Reset or split changed the cursor's incarnation or physical bucket bound.
    RestartRequired,
    /// The mutation epoch was odd or changed while this step was reading entries.
    InProgress,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBudget => write!(f, "scan physical-slot budget must be positive"),
            Self::InvalidCursor => write!(f, "scan cursor is malformed or belongs to another map"),
            Self::RestartRequired => {
                write!(
                    f,
                    "scan incarnation or geometry changed; restart from a fresh cursor"
                )
            }
            Self::InProgress => write!(f, "a mutation overlapped this scan step"),
        }
    }
}

impl std::error::Error for ScanError {}

/// The immutable map identity and mutable control fence captured for one scrub.
///
/// Fields are intentionally opaque: callers may retain and compare a snapshot, but only the map
/// can construct one from validated persisted state.
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
}

/// Handle-session progress for a bounded integrity scrub.
///
/// The map never persists this cursor, and private fields plus its handle lineage make it an
/// in-process capability rather than a wire or stable encoding. Passing the same cursor again on
/// its originating handle repeats the same bounded work while its captured fence remains current.
/// Reopen, upgrade, or construction of another alias starts a new scrub session at bucket zero.
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

/// Result of one bounded scrub step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrubStep {
    InProgress(ScrubCursor),
    Complete(ScrubSnapshot),
}

/// A scrub cursor is stale, malformed, or found invalid persisted bucket data.
///
/// Invalid fixed-width bytes that decode and then re-encode noncanonically produce typed encoding
/// errors. A panic in a user-defined `Storable` decode/encode, `StableHashKey` hash, or `Eq`
/// implementation is caught only on unwind-enabled host targets. The wasm panic-abort build traps,
/// so the IC update boundary fails closed instead of returning one of these typed errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrubError {
    ZeroBudget,
    InvalidCursor,
    Stale,
    InvalidOccupancy { bucket: u64 },
    InvalidKeyEncoding { bucket: u64, slot: u32 },
    InvalidValueEncoding { bucket: u64, slot: u32 },
    UnreachablePlacement { bucket: u64, slot: u32 },
    DuplicateKey { bucket: u64, slot: u32 },
    LengthMismatch { expected: u64, actual: u64 },
}

impl fmt::Display for ScrubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBudget => write!(f, "scrub primary-bucket budget must be positive"),
            Self::InvalidCursor => write!(f, "scrub cursor is outside its captured bounds"),
            Self::Stale => write!(f, "scrub cursor fence is stale"),
            Self::InvalidOccupancy { bucket } => {
                write!(f, "bucket {bucket} has nonzero reserved occupancy bits")
            }
            Self::InvalidKeyEncoding { bucket, slot } => {
                write!(f, "bucket {bucket} slot {slot} has invalid key encoding")
            }
            Self::InvalidValueEncoding { bucket, slot } => {
                write!(f, "bucket {bucket} slot {slot} has invalid value encoding")
            }
            Self::UnreachablePlacement { bucket, slot } => {
                write!(f, "bucket {bucket} slot {slot} is not reachable by its key")
            }
            Self::DuplicateKey { bucket, slot } => {
                write!(
                    f,
                    "bucket {bucket} slot {slot} duplicates a candidate placement"
                )
            }
            Self::LengthMismatch { expected, actual } => {
                write!(f, "scrub counted {actual} entries, expected {expected}")
            }
        }
    }
}

impl std::error::Error for ScrubError {}

/// Fixed-geometry two-choice stable-memory map.
///
/// Calls that read or mutate live state return [`MutationError::InProgress`] when another handle
/// has an incomplete mutation. Mutation uses a persisted odd/even epoch so an alias cannot start
/// nested mutation and a read detects a completed nested mutation before it returns a snapshot.
/// An odd epoch on reopen fails closed until a future journal/recovery design owns recovery.
pub struct StableLinearHashMap<K: StableHashKey, V: StableMapValue, M: Memory> {
    memory: M,
    header: Header,
    scrub_lineage: u64,
    hash_secrets: RefCell<CachedHashSecrets>,
    _marker: PhantomData<(K, V)>,
}

#[derive(Clone, Copy)]
struct CachedHashSecrets {
    seed: u64,
    secrets: [RapidSecrets; 2],
}

/// Owns one persisted odd mutation epoch until the caller explicitly publishes the next even
/// epoch. Deliberately has no `Drop` cleanup: unwinding after a write must leave reopen fail-closed
/// until a future journal owns recovery.
struct MutationGuard<'a, K: StableHashKey, V: StableMapValue, M: Memory> {
    map: &'a StableLinearHashMap<K, V, M>,
    completed_epoch: u64,
}

/// A fully prepared public insert bound to the even epoch that supplied its bytes and geometry.
///
/// Planning owns every fallible read, decode, equality comparison, routing calculation, and
/// placement decision.  Applying this value performs writes only after the same epoch is changed
/// to odd.
enum InsertPlan<V> {
    Overwrite {
        observed_epoch: u64,
        bucket: u64,
        slot: u32,
        value: Vec<u8>,
        previous: V,
    },
    DirectInsert {
        observed_epoch: u64,
        bucket: u64,
        slot: u32,
        occupancy: u8,
        key: Vec<u8>,
        value: Vec<u8>,
        len: u64,
    },
    OneHopInsert {
        observed_epoch: u64,
        source_bucket: u64,
        source_page: Vec<u8>,
        destination_bucket: u64,
        destination_page: Vec<u8>,
        len: u64,
    },
    SplitInsert {
        observed_epoch: u64,
        source_bucket: u64,
        source_page: Vec<u8>,
        new_bucket: u64,
        new_page: Vec<u8>,
        target: Option<PreparedEntry>,
        level: u8,
        split_cursor: u64,
        physical_buckets: u64,
        len: u64,
        grow_to: u64,
    },
}

/// A remove operation fully planned from one observed even epoch.
///
/// The matching key, decoded previous value, and replacement metadata are all resolved before
/// the mutation epoch becomes odd, so user-defined key or value operations cannot leave an
/// interrupted mutation behind.
enum RemovePlan<V> {
    Absent,
    Remove {
        observed_epoch: u64,
        bucket: u64,
        slot: u32,
        occupancy: u8,
        len: u64,
        previous: V,
    },
}

struct PreparedEntry {
    bucket: u64,
    slot: u32,
    occupancy: u8,
    key: Vec<u8>,
    value: Vec<u8>,
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
        let end = header
            .buckets_offset
            .checked_add(
                INITIAL_BUCKETS
                    .checked_mul(header.bucket_page_stride)
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        grow_to_bytes(&memory, end).map_err(|_| InitError::OutOfMemory)?;
        control::write(
            &memory,
            header.control_offset,
            ControlRegion {
                len: 0,
                physical_buckets: INITIAL_BUCKETS,
                mutation_epoch: control::INITIAL_MUTATION_EPOCH,
                incarnation: control::INITIAL_INCARNATION,
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
            hash_secrets: RefCell::new(CachedHashSecrets {
                seed: hash_seed,
                secrets: hash_secrets(hash_seed),
            }),
            _marker: PhantomData,
        })
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        Self::open_or_create(memory)
    }

    pub fn init_with_hash_seed(memory: M, hash_seed: u64) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new_with_hash_seed(memory, hash_seed);
        }
        Self::open(memory)
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
        let control = control::read_for_open(&memory, header.control_offset, header.hash_seed)
            .map_err(|()| InitError::InvalidLayout)?;
        Self::validate_control(control)?;
        let end = header
            .buckets_offset
            .checked_add(
                control
                    .physical_buckets
                    .checked_mul(header.bucket_page_stride)
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        if allocated < end {
            return Err(InitError::InvalidLayout);
        }
        Ok(Self {
            memory,
            header,
            scrub_lineage: next_scrub_lineage(),
            hash_secrets: RefCell::new(CachedHashSecrets {
                seed: header.hash_seed,
                secrets: hash_secrets(header.hash_seed),
            }),
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

    /// Starts a serializable physical-slot scan at slot zero.
    ///
    /// The returned cursor survives exact reopen and canister upgrade while the map's immutable
    /// identity, incarnation, and physical bucket bound remain unchanged.
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

    /// Reads at most `physical_slot_budget` slots in deterministic physical order.
    ///
    /// Fixed-width keys and values make the slot budget an entry and encoded-byte bound as well:
    /// at most one entry and `key_size + value_size` payload bytes are read per examined slot.
    /// A short `entries` list does not mean EOF; only [`ScanPage::exhausted`] does. The mutation
    /// epoch fences this call only. Mutations between successful calls are permitted while a split
    /// or reset requires restarting from [`Self::scan_start`].
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
            .checked_mul(u64::from(BUCKET_SIZE))
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
        .map_err(|()| ScanError::RestartRequired)?;
        if after.incarnation != cursor.incarnation
            || after.physical_buckets != cursor.physical_buckets
        {
            return Err(ScanError::RestartRequired);
        }
        if !after.mutation_epoch.is_multiple_of(2) || after.mutation_epoch != observed_epoch {
            return Err(ScanError::InProgress);
        }
        Self::validate_control(after).map_err(|_| ScanError::RestartRequired)?;

        let mut next_cursor = cursor;
        next_cursor.next_slot = end;
        Ok(ScanPage {
            entries,
            next_cursor,
            examined_slots: end - cursor.next_slot,
            exhausted: end == total_slots,
        })
    }

    /// Destructively returns this map region to its initial empty geometry.
    ///
    /// This is an owner operation, not a general collection `clear`: the caller must present the
    /// currently persisted incarnation. Every rejection occurs before the first stable write.
    /// Payload bytes and pages beyond the initial eight buckets are deliberately left untouched.
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

        let successor = control
            .incarnation
            .checked_add(1)
            .ok_or(ResetError::IncarnationExhausted)?;
        let completed_epoch = control
            .mutation_epoch
            .checked_add(2)
            .ok_or(ResetError::EpochExhausted)?;
        let initial_end = self
            .header
            .buckets_offset
            .checked_add(
                INITIAL_BUCKETS
                    .checked_mul(self.header.bucket_page_stride)
                    .ok_or(ResetError::CapacityOverflow)?,
            )
            .ok_or(ResetError::CapacityOverflow)?;
        let allocated = self
            .memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .ok_or(ResetError::CapacityOverflow)?;
        if allocated < initial_end {
            return Err(ResetError::CapacityOverflow);
        }

        let mutation =
            self.begin_mutation_at(control.mutation_epoch)
                .map_err(|error| match error {
                    MutationError::InProgress => ResetError::InProgress,
                    MutationError::EpochExhausted => ResetError::EpochExhausted,
                    _ => unreachable!("reset acquisition returns only epoch errors"),
                })?;
        debug_assert_eq!(mutation.completed_epoch, completed_epoch);
        for bucket in 0..INITIAL_BUCKETS {
            self.memory.write(
                Self::bucket_base(self.header, bucket),
                &[0; BUCKET_HEADER_BYTES as usize],
            );
        }
        control::write(
            &self.memory,
            self.header.control_offset,
            ControlRegion {
                len: 0,
                physical_buckets: INITIAL_BUCKETS,
                mutation_epoch: completed_epoch,
                incarnation: successor,
                level: INITIAL_LEVEL,
                split_cursor: 0,
                hash_seed: self.header.hash_seed,
            },
        );
        Ok(successor)
    }

    /// Captures the exact immutable/control fence for an external bounded scrub cursor.
    pub fn scrub_snapshot(&self) -> Result<ScrubCursor, ScrubError> {
        let snapshot = self.current_scrub_snapshot()?;
        Ok(ScrubCursor {
            snapshot,
            next_primary_bucket: 0,
            occupied: 0,
            lineage: self.scrub_lineage,
        })
    }

    /// Validates at most `primary_bucket_budget` primary buckets against a captured fence.
    ///
    /// Candidate-bucket reads are bounded by the eight slots in each primary bucket. This method
    /// writes no map bytes and returning the input cursor to this method repeats the same work. A
    /// primary bucket performs one occupancy read and, per occupied slot, one key read, one value
    /// read, and at most two candidate occupancy plus sixteen candidate-key reads.
    /// A user-defined decode, encode, hash, or equality panic traps on wasm and is not a typed
    /// `ScrubError`; retry begins from the last cursor owned by the caller.
    pub fn scrub_step(
        &self,
        cursor: ScrubCursor,
        primary_bucket_budget: u64,
    ) -> Result<ScrubStep, ScrubError> {
        if primary_bucket_budget == 0 {
            return Err(ScrubError::ZeroBudget);
        }
        if cursor.lineage != self.scrub_lineage {
            return Err(ScrubError::InvalidCursor);
        }
        let scanned_capacity = cursor
            .next_primary_bucket
            .checked_mul(u64::from(BUCKET_SIZE))
            .ok_or(ScrubError::InvalidCursor)?;
        if cursor.next_primary_bucket > cursor.snapshot.physical_buckets
            || cursor.occupied > cursor.snapshot.len.min(scanned_capacity)
            || (cursor.next_primary_bucket == 0 && cursor.occupied != 0)
        {
            return Err(ScrubError::InvalidCursor);
        }
        self.ensure_scrub_fence(cursor.snapshot)?;

        let end = cursor
            .next_primary_bucket
            .saturating_add(primary_bucket_budget)
            .min(cursor.snapshot.physical_buckets);
        let scan = (|| {
            let mut occupied = cursor.occupied;
            let control = self.scrub_control(cursor.snapshot);
            for bucket in cursor.next_primary_bucket..end {
                occupied = occupied
                    .checked_add(self.scrub_bucket(bucket, control)?)
                    .ok_or(ScrubError::InvalidCursor)?;
                if occupied > cursor.snapshot.len {
                    return Err(ScrubError::LengthMismatch {
                        expected: cursor.snapshot.len,
                        actual: occupied,
                    });
                }
            }
            Ok(occupied)
        })();
        self.ensure_scrub_fence(cursor.snapshot)?;
        let occupied = scan?;
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
        self.read_consistent(|| {
            let hot = control::read_hot(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            self.get_with_hot(key, hot)
        })
    }

    pub fn contains_key(&self, key: &K) -> Result<bool, MutationError> {
        self.read_consistent(|| {
            let hot = control::read_hot(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            self.find_with_hot(key, hot).is_some()
        })
    }

    pub fn insert(&self, key: K, value: V) -> Result<Option<V>, MutationError> {
        let key_bytes = Self::checked_storable_bytes(
            &key,
            self.header.key_size,
            MutationError::InvalidKeyEncoding,
        )?
        .into_owned();
        let hash_input = key.stable_hash_bytes();
        let hash_bytes = hash_input.as_ref().to_vec();
        let value_bytes = Self::checked_storable_bytes(
            &value,
            self.header.value_size,
            MutationError::InvalidValueEncoding,
        )?
        .into_owned();
        let plan = self.plan_insert(&key, &hash_bytes, key_bytes, value_bytes)?;
        self.apply_insert(plan)
    }

    /// Plans a bounded relocation only after the requested key has no free current candidate slot.
    ///
    /// The scan order is the two target buckets in candidate order, then their occupied slots in
    /// ascending order. Every decode, route calculation, page image, and output byte range is
    /// resolved before [`Self::begin_mutation_at`] can publish an odd epoch.
    fn plan_one_hop_insert(
        &self,
        observed_epoch: u64,
        control: ControlRegion,
        candidates: (u64, u64),
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<InsertPlan<V>, MutationError> {
        let len = control
            .len
            .checked_add(1)
            .ok_or(MutationError::CapacityOverflow)?;
        let page_bytes = usize::try_from(self.header.bucket_page_stride)
            .map_err(|_| MutationError::CapacityOverflow)?;
        let secrets = self.secrets_for_seed(self.header.hash_seed);
        for (candidate_index, source_bucket) in [candidates.0, candidates.1].into_iter().enumerate()
        {
            if candidate_index == 1 && candidates.0 == candidates.1 {
                continue;
            }
            let mut source_page = vec![0; page_bytes];
            self.memory.read(
                Self::bucket_base(self.header, source_bucket),
                &mut source_page,
            );
            let occupancy = Self::page_occupancy(&source_page);
            for source_slot in 0..BUCKET_SIZE {
                if occupancy & (1 << source_slot) == 0 {
                    continue;
                }
                let resident_key_offset = Self::page_key_offset(self.header, source_slot);
                let resident_key_end = resident_key_offset
                    .checked_add(self.header.key_size as usize)
                    .ok_or(MutationError::CapacityOverflow)?;
                let resident_value_offset = Self::page_value_offset(self.header, source_slot);
                let resident_value_end = resident_value_offset
                    .checked_add(self.header.value_size as usize)
                    .ok_or(MutationError::CapacityOverflow)?;
                let resident_key = K::from_bytes(Cow::Borrowed(
                    &source_page[resident_key_offset..resident_key_end],
                ));
                let resident_hash = resident_key.stable_hash_bytes();
                let resident_candidates = Self::candidate_buckets_from_bytes_at(
                    resident_hash.as_ref(),
                    &secrets,
                    control.level,
                    control.split_cursor,
                );
                let destination_bucket = match resident_candidates {
                    (first, second) if first == source_bucket && second != source_bucket => second,
                    (first, second) if second == source_bucket && first != source_bucket => first,
                    _ => continue,
                };
                let mut destination_page = vec![0; page_bytes];
                self.memory.read(
                    Self::bucket_base(self.header, destination_bucket),
                    &mut destination_page,
                );
                let destination_occupancy = Self::page_occupancy(&destination_page);
                let Some(destination_slot) = Self::first_empty(destination_occupancy) else {
                    continue;
                };
                Self::write_page_entry(
                    &mut destination_page,
                    self.header,
                    destination_slot,
                    &source_page[resident_key_offset..resident_key_end],
                    &source_page[resident_value_offset..resident_value_end],
                );
                Self::set_page_occupancy(
                    &mut destination_page,
                    destination_occupancy | (1 << destination_slot),
                );
                Self::write_page_entry(&mut source_page, self.header, source_slot, &key, &value);
                return Ok(InsertPlan::OneHopInsert {
                    observed_epoch,
                    source_bucket,
                    source_page,
                    destination_bucket,
                    destination_page,
                    len,
                });
            }
        }
        Err(MutationError::TablePressure)
    }

    fn plan_insert(
        &self,
        key: &K,
        hash_bytes: &[u8],
        key_bytes: Vec<u8>,
        value_bytes: Vec<u8>,
    ) -> Result<InsertPlan<V>, MutationError> {
        let control = control::read(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(MutationError::InProgress);
        }
        let observed_epoch = control.mutation_epoch;
        let planned = (|| {
            let candidates =
                self.candidate_buckets_for_bytes_at(hash_bytes, self.header.hash_seed, control);
            let occupancies = self.candidate_occupancies(candidates);
            if let Some((bucket, slot, _)) = self.find_in_candidates(candidates, occupancies, key) {
                let previous = self.read_value(bucket, slot);
                return Ok(InsertPlan::Overwrite {
                    observed_epoch,
                    bucket,
                    slot,
                    value: value_bytes,
                    previous,
                });
            }
            let threshold = Self::split_threshold(control.physical_buckets)?;
            if control.len < threshold {
                if let Some((bucket, slot, occupancy)) =
                    Self::choose_placement(candidates, occupancies)
                {
                    return Ok(InsertPlan::DirectInsert {
                        observed_epoch,
                        bucket,
                        slot,
                        occupancy,
                        key: key_bytes,
                        value: value_bytes,
                        len: control
                            .len
                            .checked_add(1)
                            .ok_or(MutationError::CapacityOverflow)?,
                    });
                }
                return self.plan_one_hop_insert(
                    observed_epoch,
                    control,
                    candidates,
                    key_bytes,
                    value_bytes,
                );
            }

            match self.plan_split_insert(
                observed_epoch,
                control,
                hash_bytes,
                key_bytes.clone(),
                value_bytes.clone(),
            ) {
                Ok(plan) => Ok(plan),
                Err(MutationError::TablePressure) => {
                    if let Some((bucket, slot, occupancy)) =
                        Self::choose_placement(candidates, self.candidate_occupancies(candidates))
                    {
                        return Ok(InsertPlan::DirectInsert {
                            observed_epoch,
                            bucket,
                            slot,
                            occupancy,
                            key: key_bytes,
                            value: value_bytes,
                            len: control
                                .len
                                .checked_add(1)
                                .ok_or(MutationError::CapacityOverflow)?,
                        });
                    }
                    self.plan_one_hop_insert(
                        observed_epoch,
                        control,
                        candidates,
                        key_bytes,
                        value_bytes,
                    )
                }
                Err(error) => Err(error),
            }
        })();
        match planned {
            Ok(plan) => Ok(plan),
            Err(error) => {
                self.ensure_epoch(observed_epoch)?;
                Err(error)
            }
        }
    }

    fn plan_split_insert(
        &self,
        observed_epoch: u64,
        control: ControlRegion,
        hash_bytes: &[u8],
        key_bytes: Vec<u8>,
        value_bytes: Vec<u8>,
    ) -> Result<InsertPlan<V>, MutationError> {
        let (level, split_cursor, physical_buckets) = Self::next_geometry(control)?;
        let source_bucket = control.split_cursor;
        let new_bucket = source_bucket
            .checked_add(Self::base_buckets(control.level).ok_or(MutationError::CapacityOverflow)?)
            .ok_or(MutationError::CapacityOverflow)?;
        let page_bytes = usize::try_from(self.header.bucket_page_stride)
            .map_err(|_| MutationError::CapacityOverflow)?;
        let mut source_page = vec![0; page_bytes];
        self.memory.read(
            Self::bucket_base(self.header, source_bucket),
            &mut source_page,
        );
        let source_occupancy = Self::page_occupancy(&source_page);
        let mut new_page = vec![0; page_bytes];
        let secrets = self.secrets_for_seed(self.header.hash_seed);
        let mut retained = source_occupancy;
        let mut new_occupancy = 0u8;
        for slot in 0..BUCKET_SIZE {
            if source_occupancy & (1 << slot) == 0 {
                continue;
            }
            let key = Self::page_key_offset(self.header, slot);
            let key_end = key + self.header.key_size as usize;
            let stored_key = K::from_bytes(Cow::Borrowed(&source_page[key..key_end]));
            let hash_input = stored_key.stable_hash_bytes();
            let routes = Self::candidate_buckets_from_bytes_at(
                hash_input.as_ref(),
                &secrets,
                level,
                split_cursor,
            );
            if routes.0 == source_bucket || routes.1 == source_bucket {
                continue;
            }
            let destination_slot = Self::first_empty(new_occupancy)
                .expect("at most one full source bucket moves into an empty bucket");
            let destination_key = Self::page_key_offset(self.header, destination_slot);
            let destination_value = Self::page_value_offset(self.header, destination_slot);
            let value = Self::page_value_offset(self.header, slot);
            new_page[destination_key..destination_key + self.header.key_size as usize]
                .copy_from_slice(&source_page[key..key_end]);
            new_page[destination_value..destination_value + self.header.value_size as usize]
                .copy_from_slice(&source_page[value..value + self.header.value_size as usize]);
            retained &= !(1 << slot);
            new_occupancy |= 1 << destination_slot;
        }
        Self::set_page_occupancy(&mut source_page, retained);
        Self::set_page_occupancy(&mut new_page, new_occupancy);

        let candidates = self.candidate_buckets_for_bytes_at(
            hash_bytes,
            self.header.hash_seed,
            ControlRegion {
                level,
                split_cursor,
                physical_buckets,
                ..control
            },
        );
        let occupancies = Self::planned_occupancies(
            candidates,
            source_bucket,
            retained,
            new_bucket,
            new_occupancy,
            &self.memory,
            self.header,
        );
        let (bucket, slot, occupancy) =
            Self::choose_placement(candidates, occupancies).ok_or(MutationError::TablePressure)?;
        let target = (bucket != source_bucket && bucket != new_bucket).then_some(PreparedEntry {
            bucket,
            slot,
            occupancy,
            key: key_bytes.clone(),
            value: value_bytes.clone(),
        });
        if bucket == source_bucket {
            Self::write_page_entry(
                &mut source_page,
                self.header,
                slot,
                &key_bytes,
                &value_bytes,
            );
            Self::set_page_occupancy(&mut source_page, retained | (1 << slot));
        } else if bucket == new_bucket {
            Self::write_page_entry(&mut new_page, self.header, slot, &key_bytes, &value_bytes);
            Self::set_page_occupancy(&mut new_page, new_occupancy | (1 << slot));
        }
        let grow_to = self
            .bucket_end(physical_buckets)
            .ok_or(MutationError::CapacityOverflow)?;
        Ok(InsertPlan::SplitInsert {
            observed_epoch,
            source_bucket,
            source_page,
            new_bucket,
            new_page,
            target,
            level,
            split_cursor,
            physical_buckets,
            len: control
                .len
                .checked_add(1)
                .ok_or(MutationError::CapacityOverflow)?,
            grow_to,
        })
    }

    fn apply_insert(&self, plan: InsertPlan<V>) -> Result<Option<V>, MutationError> {
        match plan {
            InsertPlan::Overwrite {
                observed_epoch,
                bucket,
                slot,
                value,
                previous,
            } => {
                let mutation = self.begin_mutation_at(observed_epoch)?;
                self.write_value_bytes(bucket, slot, &value);
                mutation.finish();
                Ok(Some(previous))
            }
            InsertPlan::DirectInsert {
                observed_epoch,
                bucket,
                slot,
                occupancy,
                key,
                value,
                len,
            } => {
                let mutation = self.begin_mutation_at(observed_epoch)?;
                self.write_key_bytes(bucket, slot, &key);
                self.write_value_bytes(bucket, slot, &value);
                self.write_occupancy(bucket, occupancy | (1 << slot));
                control::write_len(&self.memory, self.header.control_offset, len);
                mutation.finish();
                Ok(None)
            }
            InsertPlan::OneHopInsert {
                observed_epoch,
                source_bucket,
                source_page,
                destination_bucket,
                destination_page,
                len,
            } => {
                let mutation = self.begin_mutation_at(observed_epoch)?;
                self.memory.write(
                    Self::bucket_base(self.header, destination_bucket),
                    &destination_page,
                );
                self.memory
                    .write(Self::bucket_base(self.header, source_bucket), &source_page);
                control::write_len(&self.memory, self.header.control_offset, len);
                mutation.finish();
                Ok(None)
            }
            InsertPlan::SplitInsert {
                observed_epoch,
                source_bucket,
                source_page,
                new_bucket,
                new_page,
                target,
                level,
                split_cursor,
                physical_buckets,
                len,
                grow_to,
            } => {
                grow_to_bytes(&self.memory, grow_to).map_err(Self::map_grow_error)?;
                let mutation = self.begin_mutation_at(observed_epoch)?;
                self.memory
                    .write(Self::bucket_base(self.header, source_bucket), &source_page);
                self.memory
                    .write(Self::bucket_base(self.header, new_bucket), &new_page);
                if let Some(target) = target {
                    self.write_key_bytes(target.bucket, target.slot, &target.key);
                    self.write_value_bytes(target.bucket, target.slot, &target.value);
                    self.write_occupancy(target.bucket, target.occupancy | (1 << target.slot));
                }
                control::publish_split(
                    &self.memory,
                    self.header.control_offset,
                    level,
                    split_cursor,
                    physical_buckets,
                    len,
                );
                mutation.finish();
                Ok(None)
            }
        }
    }

    pub fn remove(&self, key: &K) -> Result<Option<V>, MutationError> {
        let plan = self.plan_remove(key)?;
        self.apply_remove(plan)
    }

    fn plan_remove(&self, key: &K) -> Result<RemovePlan<V>, MutationError> {
        let control = control::read(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(MutationError::InProgress);
        }
        let observed_epoch = control.mutation_epoch;
        let planned = (|| {
            let hash_input = key.stable_hash_bytes();
            let candidates = self.candidate_buckets_for_bytes_at(
                hash_input.as_ref(),
                self.header.hash_seed,
                control,
            );
            let Some((bucket, slot, occupancy, previous)) =
                self.find_value_in_candidates(candidates, key)
            else {
                return Ok(RemovePlan::Absent);
            };
            Ok(RemovePlan::Remove {
                observed_epoch,
                bucket,
                slot,
                occupancy,
                len: control
                    .len
                    .checked_sub(1)
                    .ok_or(MutationError::CapacityOverflow)?,
                previous,
            })
        })();
        match planned {
            Ok(RemovePlan::Absent) => {
                self.ensure_epoch(observed_epoch)?;
                Ok(RemovePlan::Absent)
            }
            Ok(plan) => Ok(plan),
            Err(error) => {
                self.ensure_epoch(observed_epoch)?;
                Err(error)
            }
        }
    }

    fn apply_remove(&self, plan: RemovePlan<V>) -> Result<Option<V>, MutationError> {
        match plan {
            RemovePlan::Absent => Ok(None),
            RemovePlan::Remove {
                observed_epoch,
                bucket,
                slot,
                occupancy,
                len,
                previous,
            } => {
                let mutation = self.begin_mutation_at(observed_epoch)?;
                self.write_occupancy(bucket, occupancy & !(1 << slot));
                control::write_len(&self.memory, self.header.control_offset, len);
                mutation.finish();
                Ok(Some(previous))
            }
        }
    }

    fn expected_header(hash_seed: u64) -> Result<Header, InitError> {
        if !K::BOUND.is_fixed_size() || !V::BOUND.is_fixed_size() {
            return Err(InitError::IncompatibleElementType);
        }
        let key_size = K::BOUND.max_size();
        let value_size = V::BOUND.max_size();
        let value_slab_offset = BUCKET_HEADER_BYTES
            .checked_add(
                u64::from(BUCKET_SIZE)
                    .checked_mul(u64::from(key_size))
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        let control_offset = HEADER_SIZE;
        let buckets_offset = BUCKETS_OFFSET;
        let bucket_page_stride = value_slab_offset
            .checked_add(
                u64::from(BUCKET_SIZE)
                    .checked_mul(u64::from(value_size))
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        Ok(Header {
            key_size,
            value_size,
            key_storage_schema_id: K::KEY_STORAGE_ID,
            key_routing_schema_id: K::KEY_ROUTING_ID,
            value_storage_schema_id: V::VALUE_STORAGE_ID,
            hash_seed,
            bucket_size: BUCKET_SIZE,
            control_offset,
            control_bytes: CONTROL_BYTES,
            buckets_offset,
            value_slab_offset,
            bucket_page_stride,
        })
    }

    fn validate_control(control: ControlRegion) -> Result<(), InitError> {
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(InitError::RecoveryRequired);
        }
        let base = Self::base_buckets(control.level).ok_or(InitError::InvalidLayout)?;
        let expected_buckets = base
            .checked_add(control.split_cursor)
            .ok_or(InitError::InvalidLayout)?;
        let capacity = expected_buckets
            .checked_mul(u64::from(BUCKET_SIZE))
            .ok_or(InitError::InvalidLayout)?;
        if control.level < INITIAL_LEVEL
            || control.split_cursor >= base
            || control.physical_buckets != expected_buckets
            || control.len > capacity
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
        .map_err(|()| ScanError::RestartRequired)?;
        if !control.mutation_epoch.is_multiple_of(2) || control.mutation_epoch != epoch {
            return Err(ScanError::InProgress);
        }
        Self::validate_control(control).map_err(|_| ScanError::RestartRequired)?;
        Ok((epoch, control))
    }

    fn scan_physical_window(&self, start: u64, end: u64) -> Vec<(K, V)> {
        let mut entries = Vec::new();
        let mut physical_slot = start;
        while physical_slot < end {
            let bucket = physical_slot / u64::from(BUCKET_SIZE);
            let first_slot = (physical_slot % u64::from(BUCKET_SIZE)) as u32;
            let bucket_end = end.min((bucket + 1) * u64::from(BUCKET_SIZE));
            let last_slot = (bucket_end - bucket * u64::from(BUCKET_SIZE)) as u32;
            let occupancy = Self::occupancy_byte(&self.memory, self.header, bucket);
            for slot in first_slot..last_slot {
                if occupancy & (1 << slot) == 0 {
                    continue;
                }
                let mut key_bytes = vec![0; self.header.key_size as usize];
                self.memory
                    .read(self.key_offset(bucket, slot), &mut key_bytes);
                let mut value_bytes = vec![0; self.header.value_size as usize];
                self.memory
                    .read(self.value_offset(bucket, slot), &mut value_bytes);
                entries.push((
                    K::from_bytes(Cow::Owned(key_bytes)),
                    V::from_bytes(Cow::Owned(value_bytes)),
                ));
            }
            physical_slot = bucket_end;
        }
        entries
    }

    fn current_scrub_snapshot(&self) -> Result<ScrubSnapshot, ScrubError> {
        let persisted_header = header::read(&self.memory).map_err(|_| ScrubError::Stale)?;
        if persisted_header != self.header {
            return Err(ScrubError::Stale);
        }
        let control = control::read_for_open(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        )
        .map_err(|()| ScrubError::Stale)?;
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
            level,
            split_cursor: snapshot.physical_buckets - (1u64 << level),
            hash_seed: snapshot.hash_seed,
        }
    }

    fn scrub_bucket(&self, bucket: u64, control: ControlRegion) -> Result<u64, ScrubError> {
        let occupancy = self.scrub_occupancy(bucket)?;
        let mut occupied = occupancy;
        while occupied != 0 {
            let slot = occupied.trailing_zeros();
            occupied &= occupied - 1;
            let key = self.scrub_key(bucket, slot)?;
            self.scrub_value(bucket, slot)?;
            let hash_bytes = scrub_callback(
                || key.stable_hash_bytes(),
                ScrubError::InvalidKeyEncoding { bucket, slot },
            )?;
            let candidates = self.candidate_buckets_for_bytes_at(
                hash_bytes.as_ref(),
                control.hash_seed,
                control,
            );
            if candidates.0 != bucket && candidates.1 != bucket {
                return Err(ScrubError::UnreachablePlacement { bucket, slot });
            }
            self.ensure_unique_candidate_placement(bucket, slot, &key, candidates)?;
        }
        Ok(u64::from(occupancy.count_ones()))
    }

    fn scrub_occupancy(&self, bucket: u64) -> Result<u8, ScrubError> {
        let mut bytes = [0; BUCKET_HEADER_BYTES as usize];
        self.memory
            .read(Self::bucket_base(self.header, bucket), &mut bytes);
        if bytes[1..].iter().any(|byte| *byte != 0) {
            return Err(ScrubError::InvalidOccupancy { bucket });
        }
        Ok(bytes[0])
    }

    fn scrub_key(&self, bucket: u64, slot: u32) -> Result<K, ScrubError> {
        let mut bytes = vec![0; self.header.key_size as usize];
        self.memory.read(self.key_offset(bucket, slot), &mut bytes);
        let original = bytes.clone();
        let key = scrub_callback(
            || K::from_bytes(Cow::Owned(bytes)),
            ScrubError::InvalidKeyEncoding { bucket, slot },
        )?;
        let encoded = scrub_callback(
            || key.to_bytes(),
            ScrubError::InvalidKeyEncoding { bucket, slot },
        )?;
        if encoded.as_ref() != original {
            return Err(ScrubError::InvalidKeyEncoding { bucket, slot });
        }
        Ok(key)
    }

    fn scrub_value(&self, bucket: u64, slot: u32) -> Result<(), ScrubError> {
        let mut bytes = vec![0; self.header.value_size as usize];
        self.memory
            .read(self.value_offset(bucket, slot), &mut bytes);
        let original = bytes.clone();
        let value = scrub_callback(
            || V::from_bytes(Cow::Owned(bytes)),
            ScrubError::InvalidValueEncoding { bucket, slot },
        )?;
        let encoded = scrub_callback(
            || value.to_bytes(),
            ScrubError::InvalidValueEncoding { bucket, slot },
        )?;
        if encoded.as_ref() != original {
            return Err(ScrubError::InvalidValueEncoding { bucket, slot });
        }
        Ok(())
    }

    fn ensure_unique_candidate_placement(
        &self,
        bucket: u64,
        slot: u32,
        key: &K,
        candidates: (u64, u64),
    ) -> Result<(), ScrubError> {
        for (candidate_index, candidate) in [candidates.0, candidates.1].into_iter().enumerate() {
            if candidate_index == 1 && candidates.0 == candidates.1 {
                continue;
            }
            let occupancy = self.scrub_occupancy(candidate)?;
            let mut occupied = occupancy;
            while occupied != 0 {
                let candidate_slot = occupied.trailing_zeros();
                occupied &= occupied - 1;
                if candidate == bucket && candidate_slot == slot {
                    continue;
                }
                let candidate_key = self.scrub_key(candidate, candidate_slot)?;
                let equal = scrub_callback(
                    || candidate_key == *key,
                    ScrubError::InvalidKeyEncoding {
                        bucket: candidate,
                        slot: candidate_slot,
                    },
                )?;
                if equal {
                    return Err(ScrubError::DuplicateKey { bucket, slot });
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn candidate_buckets(&self, key: &K) -> (u64, u64) {
        let hot = control::read_hot(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        let bytes = key.stable_hash_bytes();
        Self::candidate_buckets_from_bytes_at(
            bytes.as_ref(),
            &self.secrets_for_seed(hot.hash_seed),
            hot.level,
            hot.split_cursor,
        )
    }

    #[cfg(any(test, all(feature = "canbench", target_family = "wasm")))]
    fn candidate_buckets_from_bytes(bytes: &[u8], secrets: &[RapidSecrets; 2]) -> (u64, u64) {
        Self::candidate_buckets_from_bytes_at(bytes, secrets, INITIAL_LEVEL, 0)
    }

    fn candidate_buckets_from_bytes_at(
        bytes: &[u8],
        secrets: &[RapidSecrets; 2],
        level: u8,
        split_cursor: u64,
    ) -> (u64, u64) {
        let first = linear_bucket(hash(bytes, &secrets[0]), level, split_cursor);
        let second = linear_bucket(hash(bytes, &secrets[1]), level, split_cursor);
        (first, second)
    }

    fn candidate_buckets_for_bytes_at(
        &self,
        bytes: &[u8],
        seed: u64,
        control: ControlRegion,
    ) -> (u64, u64) {
        let cached = self.hash_secrets.borrow();
        if cached.seed == seed {
            return Self::candidate_buckets_from_bytes_at(
                bytes,
                &cached.secrets,
                control.level,
                control.split_cursor,
            );
        }
        drop(cached);

        let mut cached = self.hash_secrets.borrow_mut();
        if cached.seed != seed {
            *cached = CachedHashSecrets {
                seed,
                secrets: hash_secrets(seed),
            };
        }
        Self::candidate_buckets_from_bytes_at(
            bytes,
            &cached.secrets,
            control.level,
            control.split_cursor,
        )
    }

    #[cfg(test)]
    fn find(&self, key: &K) -> Option<(u64, u32, u8)> {
        let hot = control::read_hot(
            &self.memory,
            self.header.control_offset,
            self.header.hash_seed,
        );
        self.find_with_hot(key, hot)
    }

    fn find_with_hot(&self, key: &K, hot: control::HotControl) -> Option<(u64, u32, u8)> {
        let candidates =
            self.candidate_buckets_for_key_at(key, hot.hash_seed, hot.level, hot.split_cursor);
        let occupancies = self.candidate_occupancies(candidates);
        self.find_in_candidates(candidates, occupancies, key)
    }

    #[inline(always)]
    fn get_with_hot(&self, key: &K, hot: control::HotControl) -> Option<V> {
        self.find_value_with_hot(key, hot)
            .map(|(_, _, _, value)| value)
    }

    fn find_value_with_hot(&self, key: &K, hot: control::HotControl) -> Option<(u64, u32, u8, V)> {
        let candidates =
            self.candidate_buckets_for_key_at(key, hot.hash_seed, hot.level, hot.split_cursor);
        self.find_value_in_candidates(candidates, key)
    }

    #[inline(always)]
    fn find_value_in_candidates(
        &self,
        candidates: (u64, u64),
        key: &K,
    ) -> Option<(u64, u32, u8, V)> {
        if self.header.bucket_page_stride <= BULK_SCAN_MAX_BYTES {
            return self.find_in_small_candidates(
                candidates,
                key,
                self.header.bucket_page_stride as usize,
            );
        }
        let occupancies = self.candidate_occupancies(candidates);
        self.find_in_candidates(candidates, occupancies, key)
            .map(|(bucket, slot, occupancy)| {
                (bucket, slot, occupancy, self.read_value(bucket, slot))
            })
    }

    fn candidate_buckets_for_key_at(
        &self,
        key: &K,
        seed: u64,
        level: u8,
        split_cursor: u64,
    ) -> (u64, u64) {
        let bytes = key.stable_hash_bytes();
        Self::candidate_buckets_from_bytes_at(
            bytes.as_ref(),
            &self.secrets_for_seed(seed),
            level,
            split_cursor,
        )
    }

    fn read_consistent<T>(&self, read: impl FnOnce() -> T) -> Result<T, MutationError> {
        let epoch = self.idle_epoch()?;
        let result = read();
        if control::read_mutation_epoch(&self.memory, self.header.control_offset) != epoch {
            return Err(MutationError::InProgress);
        }
        Ok(result)
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
        let in_progress = epoch + 1;
        control::write_mutation_epoch(&self.memory, self.header.control_offset, in_progress);
        Ok(MutationGuard {
            map: self,
            completed_epoch: completed,
        })
    }

    #[cfg(test)]
    fn begin_mutation(&self) -> Result<MutationGuard<'_, K, V, M>, MutationError> {
        let epoch = self.idle_epoch()?;
        self.begin_mutation_at(epoch)
    }

    fn ensure_epoch(&self, observed_epoch: u64) -> Result<(), MutationError> {
        (control::read_mutation_epoch(&self.memory, self.header.control_offset) == observed_epoch)
            .then_some(())
            .ok_or(MutationError::InProgress)
    }

    fn split_threshold(physical_buckets: u64) -> Result<u64, MutationError> {
        physical_buckets
            .checked_mul(u64::from(BUCKET_SIZE))
            .and_then(|capacity| capacity.checked_div(4))
            .and_then(|quarter| quarter.checked_mul(3))
            .ok_or(MutationError::CapacityOverflow)
    }

    fn base_buckets(level: u8) -> Option<u64> {
        (level < 63).then(|| 1u64 << level)
    }

    fn next_geometry(control: ControlRegion) -> Result<(u8, u64, u64), MutationError> {
        let base = Self::base_buckets(control.level).ok_or(MutationError::CapacityOverflow)?;
        let physical_buckets = control
            .physical_buckets
            .checked_add(1)
            .ok_or(MutationError::CapacityOverflow)?;
        if control.split_cursor.checked_add(1) == Some(base) {
            let level = control
                .level
                .checked_add(1)
                .filter(|level| *level < 63)
                .ok_or(MutationError::CapacityOverflow)?;
            Ok((level, 0, physical_buckets))
        } else {
            Ok((control.level, control.split_cursor + 1, physical_buckets))
        }
    }

    fn map_grow_error(error: GrowError) -> MutationError {
        match error {
            GrowError::OutOfMemory => MutationError::OutOfMemory,
            GrowError::CapacityOverflow => MutationError::CapacityOverflow,
        }
    }

    fn choose_placement(candidates: (u64, u64), occupancies: (u8, u8)) -> Option<(u64, u32, u8)> {
        let (preferred, preferred_occupancy, alternate, alternate_occupancy) =
            if occupancies.0.count_ones() <= occupancies.1.count_ones() {
                (candidates.0, occupancies.0, candidates.1, occupancies.1)
            } else {
                (candidates.1, occupancies.1, candidates.0, occupancies.0)
            };
        Self::first_empty(preferred_occupancy)
            .map(|slot| (preferred, slot, preferred_occupancy))
            .or_else(|| {
                Self::first_empty(alternate_occupancy)
                    .map(|slot| (alternate, slot, alternate_occupancy))
            })
    }

    fn planned_occupancies(
        candidates: (u64, u64),
        source_bucket: u64,
        source_occupancy: u8,
        new_bucket: u64,
        new_occupancy: u8,
        memory: &M,
        header: Header,
    ) -> (u8, u8) {
        let occupancy = |bucket| {
            if bucket == source_bucket {
                source_occupancy
            } else if bucket == new_bucket {
                new_occupancy
            } else {
                Self::occupancy_byte(memory, header, bucket)
            }
        };
        (occupancy(candidates.0), occupancy(candidates.1))
    }

    fn page_occupancy(page: &[u8]) -> u8 {
        u16::from_le_bytes([page[0], page[1]]) as u8
    }

    fn set_page_occupancy(page: &mut [u8], occupancy: u8) {
        page[..2].copy_from_slice(&u16::from(occupancy).to_le_bytes());
    }

    fn page_key_offset(header: Header, slot: u32) -> usize {
        BUCKET_HEADER_BYTES as usize + slot as usize * header.key_size as usize
    }

    fn page_value_offset(header: Header, slot: u32) -> usize {
        header.value_slab_offset as usize + slot as usize * header.value_size as usize
    }

    fn write_page_entry(page: &mut [u8], header: Header, slot: u32, key: &[u8], value: &[u8]) {
        let key_offset = Self::page_key_offset(header, slot);
        let value_offset = Self::page_value_offset(header, slot);
        page[key_offset..key_offset + header.key_size as usize].copy_from_slice(key);
        page[value_offset..value_offset + header.value_size as usize].copy_from_slice(value);
    }

    fn secrets_for_seed(&self, seed: u64) -> [RapidSecrets; 2] {
        let cached = self.hash_secrets.borrow();
        if cached.seed == seed {
            cached.secrets
        } else {
            hash_secrets(seed)
        }
    }

    fn bucket_end(&self, physical_buckets: u64) -> Option<u64> {
        self.header
            .buckets_offset
            .checked_add(physical_buckets.checked_mul(self.header.bucket_page_stride)?)
    }

    #[inline(always)]
    fn find_in_small_candidates(
        &self,
        candidates: (u64, u64),
        key: &K,
        page_bytes: usize,
    ) -> Option<(u64, u32, u8, V)> {
        let mut page = read_exact_to_vec_uninit(
            &self.memory,
            Self::bucket_base(self.header, candidates.0),
            page_bytes,
        );
        self.find_in_small_page(key, &page)
            .map(|(slot, occupancy, value)| (candidates.0, slot, occupancy, value))
            .or_else(|| {
                (candidates.1 != candidates.0)
                    .then(|| {
                        self.memory
                            .read(Self::bucket_base(self.header, candidates.1), &mut page);
                        self.find_in_small_page(key, &page)
                            .map(|(slot, occupancy, value)| (candidates.1, slot, occupancy, value))
                    })
                    .flatten()
            })
    }

    #[inline(always)]
    fn find_in_small_page(&self, key: &K, page: &[u8]) -> Option<(u32, u8, V)> {
        let occupancy = u16::from_le_bytes([page[0], page[1]]) as u8;
        let mut occupied = occupancy;
        while occupied != 0 {
            let slot = occupied.trailing_zeros();
            let key_offset = Self::page_key_offset(self.header, slot);
            let value_offset = Self::page_value_offset(self.header, slot);
            if K::from_bytes(Cow::Borrowed(
                &page[key_offset..key_offset + self.header.key_size as usize],
            )) == *key
            {
                return Some((
                    slot,
                    occupancy,
                    V::from_bytes(Cow::Borrowed(
                        &page[value_offset..value_offset + self.header.value_size as usize],
                    )),
                ));
            }
            occupied &= occupied - 1;
        }
        None
    }

    fn candidate_occupancies(&self, candidates: (u64, u64)) -> (u8, u8) {
        let first = Self::occupancy_byte(&self.memory, self.header, candidates.0);
        let second = if candidates.1 == candidates.0 {
            first
        } else {
            Self::occupancy_byte(&self.memory, self.header, candidates.1)
        };
        (first, second)
    }

    fn find_in_candidates(
        &self,
        candidates: (u64, u64),
        occupancies: (u8, u8),
        key: &K,
    ) -> Option<(u64, u32, u8)> {
        let (first, second) = candidates;
        if occupancies.0 == 0 && occupancies.1 == 0 {
            return None;
        }
        let mut buffer =
            vec![0; self.header.value_slab_offset as usize - BUCKET_HEADER_BYTES as usize];
        self.find_in_bucket(first, occupancies.0, key, &mut buffer)
            .map(|slot| (first, slot, occupancies.0))
            .or_else(|| {
                (second != first)
                    .then(|| {
                        self.find_in_bucket(second, occupancies.1, key, &mut buffer)
                            .map(|slot| (second, slot, occupancies.1))
                    })
                    .flatten()
            })
    }

    fn find_in_bucket(&self, bucket: u64, occupancy: u8, key: &K, keys: &mut [u8]) -> Option<u32> {
        if occupancy == 0 {
            return None;
        }
        let keys_base = Self::keys_base(self.header, bucket);
        self.memory.read(keys_base, keys);
        let mut occupied = occupancy;
        while occupied != 0 {
            let slot = occupied.trailing_zeros();
            let start = slot as usize * self.header.key_size as usize;
            let key_bytes = &keys[start..start + self.header.key_size as usize];
            if K::from_bytes(Cow::Borrowed(key_bytes)) == *key {
                return Some(slot);
            }
            occupied &= occupied - 1;
        }
        None
    }

    #[cfg(test)]
    fn bucket_load(&self, bucket: u64) -> u32 {
        Self::occupancy_byte(&self.memory, self.header, bucket).count_ones()
    }

    fn first_empty(occupancy: u8) -> Option<u32> {
        let slot = (!occupancy).trailing_zeros();
        (slot < BUCKET_SIZE).then_some(slot)
    }

    fn write_occupancy(&self, bucket: u64, occupancy: u8) {
        self.memory.write(
            Self::bucket_base(self.header, bucket),
            &u16::from(occupancy).to_le_bytes(),
        );
    }

    fn occupancy_byte(memory: &M, header: Header, bucket: u64) -> u8 {
        let mut occupancy = [0; 2];
        memory.read(Self::bucket_base(header, bucket), &mut occupancy);
        u16::from_le_bytes(occupancy) as u8
    }

    fn bucket_base(header: Header, bucket: u64) -> u64 {
        header.buckets_offset + bucket * header.bucket_page_stride
    }

    fn keys_base(header: Header, bucket: u64) -> u64 {
        Self::bucket_base(header, bucket) + BUCKET_HEADER_BYTES
    }

    fn key_offset(&self, bucket: u64, slot: u32) -> u64 {
        Self::keys_base(self.header, bucket) + u64::from(slot) * u64::from(self.header.key_size)
    }

    fn values_base(header: Header, bucket: u64) -> u64 {
        Self::bucket_base(header, bucket) + header.value_slab_offset
    }

    fn value_offset(&self, bucket: u64, slot: u32) -> u64 {
        Self::values_base(self.header, bucket) + u64::from(slot) * u64::from(self.header.value_size)
    }

    fn read_value(&self, bucket: u64, slot: u32) -> V {
        read_storable(
            &self.memory,
            self.value_offset(bucket, slot),
            self.header.value_size,
        )
    }

    fn write_key_bytes(&self, bucket: u64, slot: u32, key: &[u8]) {
        self.memory.write(self.key_offset(bucket, slot), key);
    }

    fn write_value_bytes(&self, bucket: u64, slot: u32, value: &[u8]) {
        self.memory.write(self.value_offset(bucket, slot), value);
    }

    fn checked_storable_bytes<'a, T: Storable>(
        value: &'a T,
        expected_size: u32,
        error: MutationError,
    ) -> Result<Cow<'a, [u8]>, MutationError> {
        let bytes = value.to_bytes();
        if bytes.len() != expected_size as usize {
            return Err(error);
        }
        Ok(bytes)
    }
}

/// Reads exactly `count` bytes into a newly allocated vector without first zero-filling it.
///
/// # Safety proof
///
/// `Vec::with_capacity(count)` allocates writable storage for at least `count` bytes, so its
/// pointer meets `Memory::read_unsafe`'s destination-size requirement. That allocation is heap
/// storage owned by this call, while `Memory` supplies stable/linear memory, so the source and
/// destination cannot overlap. `Memory::read_unsafe` promises to initialize all `count` bytes on
/// return; only then does this helper publish them with `set_len(count)`. The caller keeps the
/// resulting buffer operation-local and uses it only as initialized bytes.
#[inline(always)]
fn read_exact_to_vec_uninit<M: Memory>(memory: &M, offset: u64, count: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(count);
    // SAFETY: the documented proof above establishes the required writable, non-overlapping
    // destination and initialization-before-`set_len` conditions.
    unsafe {
        memory.read_unsafe(offset, bytes.as_mut_ptr(), count);
        bytes.set_len(count);
    }
    bytes
}

fn linear_bucket(hash: u64, level: u8, split_cursor: u64) -> u64 {
    // Valid persisted geometry keeps `level < 63`, so both power-of-two masks fit in `u64`.
    let base_mask = (1u64 << level) - 1;
    let bucket = hash & base_mask;
    if bucket < split_cursor {
        hash & ((base_mask << 1) | 1)
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

fn read_storable<T: Storable, M: Memory>(memory: &M, offset: u64, size: u32) -> T {
    let mut bytes = vec![0; size as usize];
    memory.read(offset, &mut bytes);
    T::from_bytes(Cow::Owned(bytes))
}

#[cfg(all(feature = "canbench", target_family = "wasm"))]
pub(crate) mod canbench_probe {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Route {
        candidates: (u64, u64),
    }

    #[derive(Clone, Copy)]
    pub(crate) struct PreparedRoute {
        bytes: [u8; 8],
        secrets: [RapidSecrets; 2],
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Mutation {
        bucket: u64,
        slot: u32,
        occupancy: u8,
        len: u64,
    }

    impl<V: StableMapValue, M: Memory> StableLinearHashMap<u64, V, M> {
        pub(crate) fn probe_one_hop_candidates(&self, key: u64) -> (u64, u64) {
            let hot = control::read_hot(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            self.candidate_buckets_for_key_at(&key, hot.hash_seed, hot.level, hot.split_cursor)
        }

        pub(crate) fn probe_one_hop_place(
            &self,
            bucket: u64,
            slot: u32,
            occupancy: u8,
            key: u64,
            value: V,
        ) {
            self.write_key_bytes(bucket, slot, key.to_bytes().as_ref());
            self.write_value_bytes(bucket, slot, value.to_bytes().as_ref());
            self.write_occupancy(bucket, occupancy | (1 << slot));
        }

        pub(crate) fn probe_one_hop_set_len(&self, len: u64) {
            control::write_len(&self.memory, self.header.control_offset, len);
        }

        pub(crate) fn probe_one_hop_resident_bucket(&self, key: u64) -> u64 {
            let hot = control::read_hot(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            self.find_with_hot(&key, hot)
                .expect("one-hop benchmark resident exists")
                .0
        }
    }

    impl<M: Memory> StableLinearHashMap<u64, u64, M> {
        pub(crate) fn probe_candidates(&self, key: u64) -> (u64, u64) {
            let hot = control::read_hot(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            self.candidate_buckets_for_key_at(&key, hot.hash_seed, hot.level, hot.split_cursor)
        }

        pub(crate) fn probe_bucket_occupancy(&self, bucket: u64) -> u8 {
            Self::occupancy_byte(&self.memory, self.header, bucket)
        }

        pub(crate) fn probe_resident_bucket(&self, key: u64) -> u64 {
            self.find_with_hot(
                &key,
                control::read_hot(
                    &self.memory,
                    self.header.control_offset,
                    self.header.hash_seed,
                ),
            )
            .expect("benchmark resident exists")
            .0
        }

        pub(crate) fn probe_seed(&self) -> u64 {
            self.header.hash_seed
        }

        pub(crate) fn probe_route_hash(&self, key: u64, seed: u64) -> Route {
            let bytes = key.stable_hash_bytes();
            Route {
                candidates: Self::candidate_buckets_from_bytes(
                    bytes.as_ref(),
                    &self.secrets_for_seed(seed),
                ),
            }
        }

        pub(crate) fn probe_key_encode(&self, key: u64) -> [u8; 8] {
            key.to_be_bytes()
        }

        pub(crate) fn prepare_route(&self, bytes: [u8; 8], seed: u64) -> PreparedRoute {
            let cached = self.hash_secrets.borrow();
            assert_eq!(cached.seed, seed, "diagnostic fixture uses the cached seed");
            PreparedRoute {
                bytes,
                secrets: cached.secrets,
            }
        }

        pub(crate) fn probe_secret_cache_hit(&self, seed: u64) -> u64 {
            let cached = self.hash_secrets.borrow();
            assert_eq!(cached.seed, seed, "diagnostic fixture uses the cached seed");
            cached.seed
        }

        pub(crate) fn probe_first_hash(&self, route: &PreparedRoute) -> u64 {
            hash(&route.bytes, &route.secrets[0])
        }

        pub(crate) fn probe_second_hash(&self, route: &PreparedRoute) -> u64 {
            hash(&route.bytes, &route.secrets[1])
        }

        pub(crate) fn probe_bucket_mapping(&self, first_hash: u64, second_hash: u64) -> Route {
            Route {
                candidates: (
                    linear_bucket(first_hash, INITIAL_LEVEL, 0),
                    linear_bucket(second_hash, INITIAL_LEVEL, 0),
                ),
            }
        }

        pub(crate) fn probe_bucket_value(&self, key: u64, route: Route) -> Option<u64> {
            self.find_in_small_candidates(
                route.candidates,
                &key,
                self.header.bucket_page_stride as usize,
            )
            .map(|(_, _, _, value)| value)
        }

        pub(crate) fn probe_insert_control_route_lookup(&self, key: u64) -> Mutation {
            let hot = control::read_hot(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            let route = self.probe_route_hash(key, hot.hash_seed);
            let occupancies = self.candidate_occupancies(route.candidates);
            let preferred = if occupancies.0.count_ones() <= occupancies.1.count_ones() {
                (route.candidates.0, occupancies.0)
            } else {
                (route.candidates.1, occupancies.1)
            };
            let alternate = if preferred.0 == route.candidates.0 {
                (route.candidates.1, occupancies.1)
            } else {
                (route.candidates.0, occupancies.0)
            };
            let (bucket, slot, occupancy) = Self::first_empty(preferred.1)
                .map(|slot| (preferred.0, slot, preferred.1))
                .or_else(|| {
                    Self::first_empty(alternate.1).map(|slot| (alternate.0, slot, alternate.1))
                })
                .expect("diagnostic fixture has a free candidate slot");
            Mutation {
                bucket,
                slot,
                occupancy,
                len: control::read_len(&self.memory, self.header.control_offset),
            }
        }

        pub(crate) fn probe_insert_payload_write(&self, key: u64, value: u64, mutation: Mutation) {
            self.write_key_bytes(mutation.bucket, mutation.slot, key.to_be_bytes().as_ref());
            self.write_value_bytes(mutation.bucket, mutation.slot, value.to_be_bytes().as_ref());
        }

        pub(crate) fn probe_payload_equals(
            &self,
            key: u64,
            value: u64,
            mutation: Mutation,
        ) -> bool {
            let mut stored_key = [0; 8];
            let mut stored_value = [0; 8];
            self.memory.read(
                self.key_offset(mutation.bucket, mutation.slot),
                &mut stored_key,
            );
            self.memory.read(
                self.value_offset(mutation.bucket, mutation.slot),
                &mut stored_value,
            );
            stored_key == key.to_be_bytes() && stored_value == value.to_be_bytes()
        }

        pub(crate) fn probe_insert_metadata_publish(&self, mutation: Mutation) {
            self.write_occupancy(mutation.bucket, mutation.occupancy | (1 << mutation.slot));
            control::write_len(&self.memory, self.header.control_offset, mutation.len + 1);
        }

        pub(crate) fn probe_remove_control_route_bucket_value(&self, key: u64) -> (Mutation, u64) {
            let hot = control::read_hot(
                &self.memory,
                self.header.control_offset,
                self.header.hash_seed,
            );
            let (bucket, slot, occupancy, value) = self
                .find_value_with_hot(&key, hot)
                .expect("diagnostic fixture contains key");
            (
                Mutation {
                    bucket,
                    slot,
                    occupancy,
                    len: control::read_len(&self.memory, self.header.control_offset),
                },
                value,
            )
        }

        pub(crate) fn probe_remove_metadata_publish(&self, mutation: Mutation) {
            self.write_occupancy(mutation.bucket, mutation.occupancy & !(1 << mutation.slot));
            control::write_len(&self.memory, self.header.control_offset, mutation.len - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;
    use std::cell::Cell;
    use std::rc::Rc;

    type Callback = Rc<dyn Fn()>;
    type ResidentHashCallback = (u64, Callback);

    thread_local! {
        static PROBED_HASH_KEYS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
        static RECORD_HASH_KEYS: Cell<bool> = const { Cell::new(false) };
        static RESIDENT_DECODE_CALLBACK: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
        static RESIDENT_HASH_CALLBACK: RefCell<Option<ResidentHashCallback>> = const { RefCell::new(None) };
    }

    type Map = StableLinearHashMap<u64, u64, VectorMemory>;
    type GrowCallback = Callback;

    #[derive(Clone, Default)]
    struct CountingMemory {
        inner: VectorMemory,
        read_calls: Rc<Cell<u64>>,
        read_bytes: Rc<Cell<u64>>,
        read_ranges: Rc<RefCell<Vec<(u64, usize)>>>,
        write_calls: Rc<Cell<u64>>,
        write_bytes: Rc<Cell<u64>>,
    }

    #[derive(Clone, Default)]
    struct FailpointMemory {
        inner: VectorMemory,
        fail_grow: Rc<Cell<bool>>,
        fail_write: Rc<Cell<Option<u64>>>,
        writes: Rc<Cell<u64>>,
        size_override: Rc<Cell<Option<u64>>>,
        after_grow: Rc<RefCell<Option<GrowCallback>>>,
        epoch_read_override: Rc<Cell<Option<u64>>>,
        after_write: Rc<RefCell<Option<GrowCallback>>>,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RoutingKey(u64);

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ProbeKey(u64);

    impl Storable for ProbeKey {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(self.0.to_be_bytes().to_vec())
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_be_bytes().to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            Self(u64::from_be_bytes(
                bytes.as_ref().try_into().expect("fixed-width ProbeKey"),
            ))
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 8,
                is_fixed_size: true,
            };
    }

    impl StableHashKey for ProbeKey {
        const KEY_STORAGE_ID: [u8; 16] = [4; 16];
        const KEY_ROUTING_ID: [u8; 16] = [14; 16];
        type HashBytes<'a> = [u8; 8];

        fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
            RECORD_HASH_KEYS.with(|record| {
                if record.get() {
                    PROBED_HASH_KEYS.with(|keys| keys.borrow_mut().push(self.0));
                }
            });
            self.0.to_be_bytes()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Width3Key([u8; 3]);

    impl Storable for Width3Key {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(&self.0)
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            Self(bytes.as_ref().try_into().expect("fixed-width Width3Key"))
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 3,
                is_fixed_size: true,
            };
    }

    impl StableHashKey for Width3Key {
        const KEY_STORAGE_ID: [u8; 16] = [5; 16];
        const KEY_ROUTING_ID: [u8; 16] = [15; 16];
        type HashBytes<'a> = [u8; 3];

        fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
            self.0
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Width5Value([u8; 5]);

    impl Storable for Width5Value {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(&self.0)
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            Self(bytes.as_ref().try_into().expect("fixed-width Width5Value"))
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 5,
                is_fixed_size: true,
            };
    }

    impl StableMapValue for Width5Value {
        const VALUE_STORAGE_ID: [u8; 16] = [25; 16];
    }

    impl Storable for RoutingKey {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(self.0.to_le_bytes().to_vec())
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_le_bytes().to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            Self(u64::from_le_bytes(
                bytes.as_ref().try_into().expect("fixed-width RoutingKey"),
            ))
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 8,
                is_fixed_size: true,
            };
    }

    impl StableHashKey for RoutingKey {
        const KEY_STORAGE_ID: [u8; 16] = [1; 16];
        const KEY_ROUTING_ID: [u8; 16] = [11; 16];
        type HashBytes<'a>
            = [u8; 8]
        where
            Self: 'a;

        fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
            self.0.to_be_bytes()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct BadKey(u64);

    impl Storable for BadKey {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(self.0.to_be_bytes()[..7].to_vec())
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_be_bytes()[..7].to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            let bytes: [u8; 8] = bytes.as_ref().try_into().expect("fixed-width BadKey");
            Self(u64::from_be_bytes(bytes))
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 8,
                is_fixed_size: true,
            };
    }

    impl StableHashKey for BadKey {
        const KEY_STORAGE_ID: [u8; 16] = [2; 16];
        const KEY_ROUTING_ID: [u8; 16] = [12; 16];
        type HashBytes<'a>
            = [u8; 8]
        where
            Self: 'a;

        fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
            self.0.to_be_bytes()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct BadValue(u64);

    impl Storable for BadValue {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(self.0.to_be_bytes()[..7].to_vec())
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_be_bytes()[..7].to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            let bytes: [u8; 8] = bytes.as_ref().try_into().expect("fixed-width BadValue");
            Self(u64::from_be_bytes(bytes))
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 8,
                is_fixed_size: true,
            };
    }

    impl StableMapValue for BadValue {
        const VALUE_STORAGE_ID: [u8; 16] = [22; 16];
    }

    #[derive(Clone)]
    struct CallbackKey {
        value: u64,
        on_encode: Option<Rc<dyn Fn()>>,
        on_hash: Option<Rc<dyn Fn()>>,
        on_eq: Option<Rc<dyn Fn()>>,
        invalid: bool,
    }

    impl CallbackKey {
        fn plain(value: u64) -> Self {
            Self {
                value,
                on_encode: None,
                on_hash: None,
                on_eq: None,
                invalid: false,
            }
        }
    }

    impl PartialEq for CallbackKey {
        fn eq(&self, other: &Self) -> bool {
            if let Some(callback) = self.on_eq.as_ref().or(other.on_eq.as_ref()) {
                callback();
            }
            self.value == other.value
        }
    }

    impl Eq for CallbackKey {}

    impl Storable for CallbackKey {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            if let Some(callback) = &self.on_encode {
                callback();
            }
            let bytes = self.value.to_be_bytes();
            Cow::Owned(if self.invalid {
                bytes[..7].to_vec()
            } else {
                bytes.to_vec()
            })
        }

        fn into_bytes(self) -> Vec<u8> {
            self.value.to_be_bytes().to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            RESIDENT_DECODE_CALLBACK.with(|callback| {
                if let Some(callback) = callback.borrow().as_ref() {
                    callback();
                }
            });
            let bytes: [u8; 8] = bytes.as_ref().try_into().expect("fixed-width CallbackKey");
            Self::plain(u64::from_be_bytes(bytes))
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 8,
                is_fixed_size: true,
            };
    }

    impl StableHashKey for CallbackKey {
        const KEY_STORAGE_ID: [u8; 16] = [3; 16];
        const KEY_ROUTING_ID: [u8; 16] = [13; 16];
        type HashBytes<'a>
            = [u8; 8]
        where
            Self: 'a;

        fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
            if let Some(callback) = &self.on_hash {
                callback();
            } else {
                RESIDENT_HASH_CALLBACK.with(|callback| {
                    if let Some((value, callback)) = callback.borrow().as_ref()
                        && *value == self.value
                    {
                        callback();
                    }
                });
            }
            self.value.to_be_bytes()
        }
    }

    #[derive(Clone)]
    struct CallbackValue {
        value: u64,
        on_encode: Option<Rc<dyn Fn()>>,
        invalid: bool,
    }

    impl Storable for CallbackValue {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            if let Some(callback) = &self.on_encode {
                callback();
            }
            let bytes = self.value.to_be_bytes();
            Cow::Owned(if self.invalid {
                bytes[..7].to_vec()
            } else {
                bytes.to_vec()
            })
        }

        fn into_bytes(self) -> Vec<u8> {
            self.to_bytes().into_owned()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            Self {
                value: u64::from_be_bytes(bytes.as_ref().try_into().expect("fixed-width value")),
                on_encode: None,
                invalid: false,
            }
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 8,
                is_fixed_size: true,
            };
    }

    impl StableMapValue for CallbackValue {
        const VALUE_STORAGE_ID: [u8; 16] = [23; 16];
    }

    impl Memory for CountingMemory {
        fn size(&self) -> u64 {
            self.inner.size()
        }

        fn grow(&self, pages: u64) -> i64 {
            self.inner.grow(pages)
        }

        fn read(&self, offset: u64, dst: &mut [u8]) {
            self.read_calls.set(self.read_calls.get() + 1);
            self.read_bytes
                .set(self.read_bytes.get() + dst.len() as u64);
            self.read_ranges.borrow_mut().push((offset, dst.len()));
            self.inner.read(offset, dst);
        }

        fn write(&self, offset: u64, src: &[u8]) {
            self.write_calls.set(self.write_calls.get() + 1);
            self.write_bytes
                .set(self.write_bytes.get() + src.len() as u64);
            self.inner.write(offset, src);
        }
    }

    impl Memory for FailpointMemory {
        fn size(&self) -> u64 {
            if let Some(size) = self.size_override.get() {
                size
            } else if self.fail_grow.get() {
                0
            } else {
                self.inner.size()
            }
        }

        fn grow(&self, pages: u64) -> i64 {
            if self.fail_grow.get() {
                -1
            } else {
                let previous = self.inner.grow(pages);
                if previous != -1
                    && let Some(callback) = self.after_grow.borrow().as_ref()
                {
                    callback();
                }
                previous
            }
        }

        fn read(&self, offset: u64, dst: &mut [u8]) {
            if offset == HEADER_SIZE + 16
                && dst.len() == 8
                && let Some(epoch) = self.epoch_read_override.take()
            {
                dst.copy_from_slice(&epoch.to_le_bytes());
                return;
            }
            self.inner.read(offset, dst);
        }

        fn write(&self, offset: u64, src: &[u8]) {
            let write = self.writes.get() + 1;
            self.writes.set(write);
            if self.fail_write.get() == Some(write) {
                panic!("injected stable-memory write failure");
            }
            self.inner.write(offset, src);
            let callback = self.after_write.borrow_mut().take();
            if let Some(callback) = callback {
                callback();
            }
        }
    }

    fn reset_counts(memory: &CountingMemory) {
        memory.read_calls.set(0);
        memory.read_bytes.set(0);
        memory.read_ranges.borrow_mut().clear();
        memory.write_calls.set(0);
        memory.write_bytes.set(0);
    }

    fn read_overlaps(range: (u64, usize), start: u64, len: u64) -> bool {
        let end = range.0 + range.1 as u64;
        range.0 < start + len && start < end
    }

    fn seed_large_second_candidate_fixture(
        map: &StableLinearHashMap<u64, [u8; 2048], CountingMemory>,
    ) -> (u64, [u8; 2048], u64) {
        let target = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct target candidates");
        let candidates = map.candidate_buckets(&target);
        let mut residents = Vec::new();
        for (bucket, marker) in [(candidates.0, 0x31), (candidates.1, 0x51)] {
            for slot in 0..2u32 {
                let key = (target + 1..)
                    .find(|key| {
                        !residents.contains(key) && *key != target && {
                            let routes = map.candidate_buckets(key);
                            routes.0 == bucket || routes.1 == bucket
                        }
                    })
                    .expect("candidate resident");
                map.write_key_bytes(bucket, slot, key.to_bytes().as_ref());
                map.write_value_bytes(bucket, slot, &[marker + slot as u8; 2048]);
                map.write_occupancy(bucket, (1 << (slot + 1)) - 1);
                residents.push(key);
            }
        }
        let target_slot = 2;
        let target_value = [0x91; 2048];
        map.write_key_bytes(candidates.1, target_slot, target.to_bytes().as_ref());
        map.write_value_bytes(candidates.1, target_slot, &target_value);
        map.write_occupancy(candidates.1, 0b111);
        control::write_len(&map.memory, map.header.control_offset, 5);

        let miss = (target + 1..)
            .find(|key| {
                *key != target
                    && !residents.contains(key)
                    && map.candidate_buckets(key) == candidates
            })
            .expect("miss with the same candidates");
        (target, target_value, miss)
    }

    fn read_bytes(memory: &VectorMemory, offset: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0; len];
        memory.read(offset, &mut bytes);
        bytes
    }

    fn allocated_bytes(memory: &VectorMemory) -> Vec<u8> {
        read_bytes(
            memory,
            0,
            usize::try_from(memory.size() * crate::memory::WASM_PAGE_SIZE)
                .expect("allocated byte length"),
        )
    }

    fn write_incarnation<M: Memory>(map: &StableLinearHashMap<u64, u64, M>, incarnation: u64) {
        map.memory
            .write(map.header.control_offset + 24, &incarnation.to_le_bytes());
    }

    #[derive(Debug, PartialEq, Eq)]
    struct StableSnapshot {
        pages: u64,
        bytes: Vec<u8>,
        control: ControlRegion,
    }

    fn stable_snapshot<K: StableHashKey, V: StableMapValue, M: Memory>(
        map: &StableLinearHashMap<K, V, M>,
        backing: &VectorMemory,
    ) -> StableSnapshot {
        StableSnapshot {
            pages: backing.size(),
            bytes: allocated_bytes(backing),
            control: control::read(backing, map.header.control_offset, map.header.hash_seed),
        }
    }

    fn assert_even_callback<M: Memory, K: StableHashKey, V: StableMapValue>(
        map: &StableLinearHashMap<K, V, M>,
        called: &Cell<bool>,
    ) {
        called.set(true);
        assert!(
            control::read_mutation_epoch(&map.memory, map.header.control_offset).is_multiple_of(2)
        );
    }

    fn seed_callback_pressure_fixture(
        map: &StableLinearHashMap<CallbackKey, u64, VectorMemory>,
    ) -> (Vec<(u64, u64)>, u64) {
        let target = (0u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(&CallbackKey::plain(*key));
                candidates.0 == candidates.1
            })
            .expect("duplicate candidate callback target");
        let bucket = map.candidate_buckets(&CallbackKey::plain(target)).0;
        let residents = (target + 1..)
            .filter(|key| map.candidate_buckets(&CallbackKey::plain(*key)) == (bucket, bucket))
            .take(BUCKET_SIZE as usize)
            .map(|key| (key, key ^ 0xa5a5))
            .collect::<Vec<_>>();
        for (slot, &(key, value)) in residents.iter().enumerate() {
            map.write_key_bytes(bucket, slot as u32, &key.to_be_bytes());
            map.write_value_bytes(bucket, slot as u32, &value.to_be_bytes());
        }
        map.write_occupancy(bucket, u8::MAX);
        control::write_len(
            &map.memory,
            map.header.control_offset,
            u64::from(BUCKET_SIZE),
        );
        (residents, target)
    }

    #[derive(Clone, Copy)]
    enum SplitTarget {
        Source,
        New,
        Unaffected,
    }

    fn place_fixture_entry<M: Memory>(
        map: &StableLinearHashMap<u64, u64, M>,
        occupancies: &mut [u8],
        residents: &mut Vec<(u64, u64)>,
        used: &mut Vec<u64>,
        bucket: u64,
        key: u64,
    ) {
        let slot = StableLinearHashMap::<u64, u64, M>::first_empty(occupancies[bucket as usize])
            .expect("fixture bucket capacity");
        let value = key ^ 0xa5a5_a5a5_a5a5_a5a5;
        let key_bytes = key.to_bytes();
        let value_bytes = value.to_bytes();
        map.write_key_bytes(bucket, slot, key_bytes.as_ref());
        map.write_value_bytes(bucket, slot, value_bytes.as_ref());
        occupancies[bucket as usize] |= 1 << slot;
        map.write_occupancy(bucket, occupancies[bucket as usize]);
        used.push(key);
        residents.push((key, value));
    }

    fn seed_threshold_split_fixture<M: Memory>(
        map: &StableLinearHashMap<u64, u64, M>,
        level: u8,
        split_cursor: u64,
        physical_buckets: u64,
        move_count: usize,
        target_kind: SplitTarget,
    ) -> (Vec<(u64, u64)>, Vec<u64>, u64) {
        assert!(move_count <= BUCKET_SIZE as usize);
        let _ = (level, split_cursor);
        map.memory.write(
            map.header.control_offset + 8,
            &physical_buckets.to_le_bytes(),
        );
        let seed = map.header.hash_seed;
        let secrets = hash_secrets(seed);
        let (next_level, next_cursor, _) = StableLinearHashMap::<u64, u64, M>::next_geometry(
            control::read(&map.memory, map.header.control_offset, map.header.hash_seed),
        )
        .expect("next fixture geometry");
        let source = split_cursor;
        let new_bucket = source + (1u64 << level);
        let mut used = Vec::new();
        let mut residents = Vec::new();
        let mut source_keys = Vec::new();
        let mut occupancies = vec![0u8; physical_buckets as usize];

        for should_move in [true, false] {
            let count = if should_move {
                move_count
            } else {
                BUCKET_SIZE as usize - move_count
            };
            for _ in 0..count {
                let key = (1u64..1 << 20)
                    .find(|key| {
                        if used.contains(key) {
                            return false;
                        }
                        let before =
                            StableLinearHashMap::<u64, u64, M>::candidate_buckets_from_bytes_at(
                                &key.stable_hash_bytes(),
                                &secrets,
                                level,
                                split_cursor,
                            );
                        let after =
                            StableLinearHashMap::<u64, u64, M>::candidate_buckets_from_bytes_at(
                                &key.stable_hash_bytes(),
                                &secrets,
                                next_level,
                                next_cursor,
                            );
                        (before.0 == source || before.1 == source)
                            && ((after.0 != source && after.1 != source) == should_move)
                            && (!should_move || after.0 == new_bucket || after.1 == new_bucket)
                    })
                    .expect("bounded source-key search");
                source_keys.push(key);
                place_fixture_entry(
                    map,
                    &mut occupancies,
                    &mut residents,
                    &mut used,
                    source,
                    key,
                );
            }
        }

        let threshold = StableLinearHashMap::<u64, u64, M>::split_threshold(physical_buckets)
            .expect("fixture threshold");
        let mut bucket = 0;
        while residents.len() < threshold as usize {
            bucket = (bucket + 1) % physical_buckets;
            if bucket == source || occupancies[bucket as usize] == u8::MAX {
                continue;
            }
            let key = (1u64..1 << 20)
                .find(|key| {
                    if used.contains(key) {
                        return false;
                    }
                    let routes =
                        StableLinearHashMap::<u64, u64, M>::candidate_buckets_from_bytes_at(
                            &key.stable_hash_bytes(),
                            &secrets,
                            level,
                            split_cursor,
                        );
                    routes.0 == bucket || routes.1 == bucket
                })
                .expect("bounded filler-key search");
            place_fixture_entry(
                map,
                &mut occupancies,
                &mut residents,
                &mut used,
                bucket,
                key,
            );
        }
        control::write_len(&map.memory, map.header.control_offset, threshold);

        let target = (1u64..1 << 20)
            .find(|key| {
                if used.contains(key) {
                    return false;
                }
                let routes = StableLinearHashMap::<u64, u64, M>::candidate_buckets_from_bytes_at(
                    &key.stable_hash_bytes(),
                    &secrets,
                    next_level,
                    next_cursor,
                );
                match target_kind {
                    SplitTarget::Source => routes == (source, source),
                    SplitTarget::New => routes == (new_bucket, new_bucket),
                    SplitTarget::Unaffected => {
                        routes.0 == routes.1
                            && routes.0 != source
                            && routes.0 != new_bucket
                            && occupancies[routes.0 as usize] != u8::MAX
                    }
                }
            })
            .expect("bounded target-key search");
        (residents, source_keys, target)
    }

    fn seed_one_hop_fixture_at_geometry<M: Memory>(
        map: &StableLinearHashMap<u64, u64, M>,
        level: u8,
        split_cursor: u64,
        physical_buckets: u64,
        reference_geometry: Option<(u8, u64)>,
    ) -> (Vec<(u64, u64)>, u64, u64, u64) {
        let _ = (level, split_cursor);
        map.memory.write(
            map.header.control_offset + 8,
            &physical_buckets.to_le_bytes(),
        );
        let target = (1u64..1 << 20)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct target candidates");
        let candidates = map.candidate_buckets(&target);
        let movable = (1u64..1 << 20)
            .find_map(|key| {
                if key == target {
                    return None;
                }
                let routes = map.candidate_buckets(&key);
                let destination = if routes.0 == candidates.0 && routes.1 != candidates.0 {
                    routes.1
                } else if routes.1 == candidates.0 && routes.0 != candidates.0 {
                    routes.0
                } else {
                    return None;
                };
                let current_only =
                    reference_geometry.is_none_or(|(reference_level, reference_cursor)| {
                        let secrets = hash_secrets(map.header.hash_seed);
                        let reference =
                            StableLinearHashMap::<u64, u64, M>::candidate_buckets_from_bytes_at(
                                &key.stable_hash_bytes(),
                                &secrets,
                                reference_level,
                                reference_cursor,
                            );
                        reference.0 != destination && reference.1 != destination
                    });
                (destination != candidates.0 && destination != candidates.1 && current_only)
                    .then_some((key, destination))
            })
            .expect("movable first-source resident");
        let mut residents = Vec::new();
        let mut used = vec![target];
        let mut occupancies = vec![0u8; physical_buckets as usize];
        place_fixture_entry(
            map,
            &mut occupancies,
            &mut residents,
            &mut used,
            candidates.0,
            movable.0,
        );
        for bucket in [candidates.0, candidates.1] {
            while occupancies[bucket as usize] != u8::MAX {
                let key = (1u64..1 << 20)
                    .find(|key| {
                        !used.contains(key) && {
                            let routes = map.candidate_buckets(key);
                            routes.0 == bucket || routes.1 == bucket
                        }
                    })
                    .expect("candidate filler");
                place_fixture_entry(
                    map,
                    &mut occupancies,
                    &mut residents,
                    &mut used,
                    bucket,
                    key,
                );
            }
        }
        control::write_len(
            &map.memory,
            map.header.control_offset,
            residents.len() as u64,
        );
        (residents, target, movable.0, movable.1)
    }

    fn seed_one_hop_fixture<M: Memory>(
        map: &StableLinearHashMap<u64, u64, M>,
    ) -> (Vec<(u64, u64)>, u64, u64, u64) {
        seed_one_hop_fixture_at_geometry(map, INITIAL_LEVEL, 0, INITIAL_BUCKETS, None)
    }

    #[test]
    fn exact_layout_and_idle_control_are_persisted() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 11).expect("new");
        assert_eq!(
            map.header(),
            Header {
                key_size: 8,
                value_size: 8,
                key_storage_schema_id: u64::KEY_STORAGE_ID,
                key_routing_schema_id: u64::KEY_ROUTING_ID,
                value_storage_schema_id: u64::VALUE_STORAGE_ID,
                hash_seed: 11,
                bucket_size: 8,
                control_offset: 128,
                control_bytes: 64,
                buckets_offset: 192,
                value_slab_offset: 72,
                bucket_page_stride: 136,
            }
        );

        let mut expected_header = [0; 128];
        expected_header[..3].copy_from_slice(b"LHM");
        expected_header[3] = 1;
        expected_header[4..8].copy_from_slice(&8u32.to_le_bytes());
        expected_header[8..12].copy_from_slice(&8u32.to_le_bytes());
        expected_header[16..32].copy_from_slice(&u64::KEY_STORAGE_ID);
        expected_header[32..48].copy_from_slice(&u64::KEY_ROUTING_ID);
        expected_header[48..64].copy_from_slice(&u64::VALUE_STORAGE_ID);
        expected_header[64..72].copy_from_slice(&11u64.to_le_bytes());
        assert_eq!(read_bytes(&memory, 0, 128), expected_header);

        let mut expected_control = [0; 64];
        expected_control[8..16].copy_from_slice(&8u64.to_le_bytes());
        expected_control[24..32].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(read_bytes(&memory, 128, 64), expected_control);
        assert_eq!(read_bytes(&memory, 192, 8 * 136), vec![0; 8 * 136]);
    }

    #[test]
    fn strict_create_rejects_nonempty_memory_without_writes() {
        let memory = VectorMemory::default();
        assert_eq!(memory.grow(1), 0);
        memory.write(0, b"existing bytes");
        let before = allocated_bytes(&memory);
        assert!(matches!(
            Map::new_with_hash_seed(memory.clone(), 11),
            Err(InitError::NonEmptyMemory)
        ));
        assert_eq!(allocated_bytes(&memory), before);
    }

    #[test]
    fn old_v1_like_nonempty_bytes_are_rejected_without_fallback_or_writes() {
        let memory = VectorMemory::default();
        assert_eq!(memory.grow(1), 0);
        let mut old_header = [0; 64];
        old_header[..4].copy_from_slice(b"LHM\x01");
        old_header[4..8].copy_from_slice(&8u32.to_le_bytes());
        old_header[8..12].copy_from_slice(&8u32.to_le_bytes());
        memory.write(0, &old_header);
        let before = allocated_bytes(&memory);

        assert!(Map::open_or_create(memory.clone()).is_err());
        assert_eq!(allocated_bytes(&memory), before);
    }

    #[test]
    fn header_reserved_and_each_same_width_schema_mismatch_are_rejected() {
        for (offset, expected) in [
            (16, InitError::IncompatibleKeyStorageSchema),
            (32, InitError::IncompatibleKeyRoutingSchema),
            (48, InitError::IncompatibleValueStorageSchema),
        ] {
            let memory = VectorMemory::default();
            let map = Map::new(memory.clone()).expect("new schema fixture");
            drop(map);
            memory.write(offset, &[0xff; 16]);
            assert!(matches!(Map::open(memory), Err(error) if error == expected));
        }

        for offset in [12, 72] {
            let memory = VectorMemory::default();
            let map = Map::new(memory.clone()).expect("new reserved fixture");
            drop(map);
            memory.write(offset, &[1]);
            assert!(matches!(Map::open(memory), Err(InitError::InvalidLayout)));
        }
    }

    #[test]
    fn exact_open_rejects_empty_memory_without_creating() {
        let memory = VectorMemory::default();
        assert!(matches!(
            Map::open(memory.clone()),
            Err(InitError::InvalidLayout)
        ));
        assert_eq!(memory.size(), 0);
    }

    #[test]
    fn nonempty_open_reads_exactly_header_and_control() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 17).expect("strict create");
        for key in 0..49 {
            map.insert(key, key + 1).expect("populate split geometry");
        }
        let buckets = map.control_region().expect("idle control").physical_buckets;
        assert!(buckets > INITIAL_BUCKETS);
        drop(map);
        reset_counts(&memory);

        let reopened = CountingMap::open(memory.clone()).expect("exact open");
        assert_eq!(memory.read_calls.get(), 2);
        assert_eq!(memory.read_bytes.get(), HEADER_SIZE + CONTROL_BYTES);
        assert_eq!(&*memory.read_ranges.borrow(), &[(0, 128), (128, 64)]);
        assert_eq!(reopened.len(), Ok(49));
    }

    #[test]
    fn asymmetric_width_soa_layout_and_reopen_are_exact() {
        type AsymmetricMap = StableLinearHashMap<Width3Key, Width5Value, VectorMemory>;

        let memory = VectorMemory::default();
        let map = AsymmetricMap::new_with_hash_seed(memory.clone(), 313).expect("new asymmetric");
        let header = map.header();
        assert_eq!(header.key_size, 3);
        assert_eq!(header.value_size, 5);
        assert_eq!(header.value_slab_offset, 8 + 8 * 3);
        assert_eq!(header.value_slab_offset, 32);
        assert_eq!(header.bucket_page_stride, 32 + 8 * 5);
        assert_eq!(header.bucket_page_stride, 72);

        let (key_a, forced_bucket) = (0u32..=0x00ff_ffff)
            .map(|candidate| {
                Width3Key([
                    (candidate >> 16) as u8,
                    (candidate >> 8) as u8,
                    candidate as u8,
                ])
            })
            .find_map(|candidate| {
                let routes = map.candidate_buckets(&candidate);
                (routes.0 == routes.1).then_some((candidate, routes.0))
            })
            .expect("single-bucket asymmetric key");
        let key_b = (0u32..=0x00ff_ffff)
            .map(|candidate| {
                Width3Key([
                    (candidate >> 16) as u8,
                    (candidate >> 8) as u8,
                    candidate as u8,
                ])
            })
            .find(|candidate| {
                *candidate != key_a
                    && map.candidate_buckets(candidate) == (forced_bucket, forced_bucket)
            })
            .expect("same single-bucket asymmetric key");
        let value_a = Width5Value([0xa1, 0xa2, 0xa3, 0xa4, 0xa5]);
        let value_b = Width5Value([0xb1, 0xb2, 0xb3, 0xb4, 0xb5]);
        assert_eq!(map.insert(key_a.clone(), value_a.clone()), Ok(None));
        assert_eq!(map.insert(key_b.clone(), value_b.clone()), Ok(None));
        let (bucket_a, slot_a, _) = map.find(&key_a).expect("first stored key");
        let (bucket_b, slot_b, _) = map.find(&key_b).expect("second stored key");
        assert_eq!(bucket_a, bucket_b);
        assert_ne!(slot_a, slot_b);

        let base = header.buckets_offset + bucket_a * header.bucket_page_stride;
        let key_a_offset = base + 8 + u64::from(slot_a) * 3;
        let key_b_offset = base + 8 + u64::from(slot_b) * 3;
        let value_a_offset = base + 32 + u64::from(slot_a) * 5;
        let value_b_offset = base + 32 + u64::from(slot_b) * 5;
        assert_eq!(read_bytes(&memory, key_a_offset, 3), key_a.0);
        assert_eq!(read_bytes(&memory, key_b_offset, 3), key_b.0);
        assert_eq!(read_bytes(&memory, value_a_offset, 5), value_a.0);
        assert_eq!(read_bytes(&memory, value_b_offset, 5), value_b.0);
        assert_ne!(
            read_bytes(&memory, key_a_offset, 3),
            read_bytes(&memory, key_b_offset, 3)
        );
        assert_ne!(
            read_bytes(&memory, value_a_offset, 5),
            read_bytes(&memory, value_b_offset, 5)
        );
        assert!(key_a_offset + 3 <= base + 32);
        assert!(key_b_offset + 3 <= base + 32);
        assert!(value_a_offset >= base + 32);
        assert!(value_b_offset >= base + 32);
        assert_eq!(map.get(&key_a), Ok(Some(value_a.clone())));
        assert_eq!(map.get(&key_b), Ok(Some(value_b.clone())));

        let reopened = AsymmetricMap::init(memory).expect("reopen asymmetric");
        assert_eq!(reopened.header(), header);
        assert_eq!(reopened.get(&key_a), Ok(Some(value_a)));
        assert_eq!(reopened.get(&key_b), Ok(Some(value_b)));
    }

    #[test]
    fn asymmetric_width_split_preserves_pairing_and_reopens() {
        type AsymmetricMap = StableLinearHashMap<Width3Key, Width5Value, VectorMemory>;

        let memory = VectorMemory::default();
        let map = AsymmetricMap::new_with_hash_seed(memory.clone(), 313).expect("new asymmetric");
        let control = map.control_region().expect("initial control");
        let secrets = hash_secrets(map.header.hash_seed);
        let next = AsymmetricMap::next_geometry(control).expect("next geometry");
        let source = control.split_cursor;
        let new_bucket = source + (1u64 << control.level);
        let mut occupied = vec![0u8; control.physical_buckets as usize];
        let mut residents = Vec::new();

        for should_move in [true, false] {
            for ordinal in 0..4u8 {
                let key = (1u32..=0x00ff_ffff)
                    .map(|candidate| {
                        Width3Key([
                            (candidate >> 16) as u8,
                            (candidate >> 8) as u8,
                            candidate as u8,
                        ])
                    })
                    .find(|candidate| {
                        !residents.iter().any(|(key, _)| key == candidate) && {
                            let before = AsymmetricMap::candidate_buckets_from_bytes_at(
                                &candidate.stable_hash_bytes(),
                                &secrets,
                                control.level,
                                control.split_cursor,
                            );
                            let after = AsymmetricMap::candidate_buckets_from_bytes_at(
                                &candidate.stable_hash_bytes(),
                                &secrets,
                                next.0,
                                next.1,
                            );
                            (before.0 == source || before.1 == source)
                                && ((after.0 != source && after.1 != source) == should_move)
                                && (!should_move || after.0 == new_bucket || after.1 == new_bucket)
                        }
                    })
                    .expect("asymmetric source resident");
                let value = Width5Value([
                    if should_move { 0xa0 } else { 0xb0 } | ordinal,
                    key.0[0] ^ 0x11,
                    key.0[1] ^ 0x22,
                    key.0[2] ^ 0x44,
                    0xf0 | ordinal,
                ]);
                let slot =
                    AsymmetricMap::first_empty(occupied[source as usize]).expect("source capacity");
                map.write_key_bytes(source, slot, key.to_bytes().as_ref());
                map.write_value_bytes(source, slot, value.to_bytes().as_ref());
                occupied[source as usize] |= 1 << slot;
                map.write_occupancy(source, occupied[source as usize]);
                residents.push((key, value));
            }
        }

        for (bucket, count) in [
            (1u64, 6usize),
            (2, 6),
            (3, 6),
            (4, 6),
            (5, 6),
            (6, 5),
            (7, 5),
        ] {
            for ordinal in 0..count {
                let key = (1u32..=0x00ff_ffff)
                    .map(|candidate| {
                        Width3Key([
                            (candidate >> 16) as u8,
                            (candidate >> 8) as u8,
                            candidate as u8,
                        ])
                    })
                    .find(|candidate| {
                        !residents.iter().any(|(key, _)| key == candidate) && {
                            let routes = AsymmetricMap::candidate_buckets_from_bytes_at(
                                &candidate.stable_hash_bytes(),
                                &secrets,
                                control.level,
                                control.split_cursor,
                            );
                            routes.0 == bucket || routes.1 == bucket
                        }
                    })
                    .expect("asymmetric threshold resident");
                let value = Width5Value([
                    0xc0 | bucket as u8,
                    ordinal as u8,
                    key.0[0] ^ 0x33,
                    key.0[1] ^ 0x55,
                    key.0[2] ^ 0xaa,
                ]);
                let slot = AsymmetricMap::first_empty(occupied[bucket as usize])
                    .expect("threshold bucket capacity");
                map.write_key_bytes(bucket, slot, key.to_bytes().as_ref());
                map.write_value_bytes(bucket, slot, value.to_bytes().as_ref());
                occupied[bucket as usize] |= 1 << slot;
                map.write_occupancy(bucket, occupied[bucket as usize]);
                residents.push((key, value));
            }
        }
        assert_eq!(residents.len(), 48);

        let threshold =
            AsymmetricMap::split_threshold(control.physical_buckets).expect("split threshold");
        control::write_len(&memory, map.header.control_offset, threshold);
        let target = (1u32..=0x00ff_ffff)
            .map(|candidate| {
                Width3Key([
                    (candidate >> 16) as u8,
                    (candidate >> 8) as u8,
                    candidate as u8,
                ])
            })
            .find(|candidate| {
                !residents.iter().any(|(key, _)| key == candidate)
                    && AsymmetricMap::candidate_buckets_from_bytes_at(
                        &candidate.stable_hash_bytes(),
                        &secrets,
                        next.0,
                        next.1,
                    ) == (new_bucket, new_bucket)
            })
            .expect("new-bucket target");
        let target_value = Width5Value([0xcc, 0x01, 0x23, 0x45, 0x67]);

        assert_eq!(map.insert(target.clone(), target_value.clone()), Ok(None));
        assert_eq!(
            map.control_region()
                .expect("post-split control")
                .physical_buckets,
            9
        );
        for (key, value) in &residents {
            assert_eq!(map.get(key), Ok(Some(value.clone())));
        }
        assert_eq!(map.get(&target), Ok(Some(target_value.clone())));

        let reopened = AsymmetricMap::init(memory).expect("reopen asymmetric split");
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Ok(Some(value)));
        }
        assert_eq!(reopened.get(&target), Ok(Some(target_value)));
    }

    #[test]
    fn stable_hash_key_routes_canonical_bytes_but_persists_storable_bytes() {
        type RoutingMap = StableLinearHashMap<RoutingKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let map = RoutingMap::new_with_hash_seed(memory.clone(), 0x6a09_e667_f3bc_c909)
            .expect("new routing map");
        let key = RoutingKey(0x0123_4567_89ab_cdef);
        let expected = RoutingMap::candidate_buckets_from_bytes(
            &key.0.to_be_bytes(),
            &hash_secrets(0x6a09_e667_f3bc_c909),
        );
        assert_ne!(key.stable_hash_bytes().as_ref(), key.to_bytes().as_ref());
        assert_eq!(map.candidate_buckets(&key), expected);
        assert_eq!(map.insert(key.clone(), 41), Ok(None));

        let (bucket, slot, _) = map.find(&key).expect("stored key");
        assert_eq!(
            read_bytes(&memory, map.key_offset(bucket, slot), 8),
            key.0.to_le_bytes()
        );
        drop(map);

        let reopened = RoutingMap::init(memory).expect("reopen routing map");
        assert_eq!(reopened.get(&key), Ok(Some(41)));
    }

    #[test]
    fn key_schema_ids_are_persisted_and_reopen_rejects_a_different_key_contract() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new");
        assert_eq!(map.header().key_routing_schema_id, u64::KEY_ROUTING_ID);
        drop(map);

        assert!(matches!(
            StableLinearHashMap::<RoutingKey, u64, _>::init(memory),
            Err(InitError::IncompatibleKeyStorageSchema)
        ));
    }

    #[test]
    fn odd_epoch_rejects_live_calls_and_reopen_until_recovery_exists() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new");
        control::write_mutation_epoch(&memory, map.header.control_offset, 1);

        assert_eq!(map.control_region(), Err(MutationError::InProgress));
        assert_eq!(map.get(&7), Err(MutationError::InProgress));
        assert_eq!(map.contains_key(&7), Err(MutationError::InProgress));
        assert_eq!(map.len(), Err(MutationError::InProgress));
        assert_eq!(map.is_empty(), Err(MutationError::InProgress));
        assert_eq!(map.hash_seed(), Err(MutationError::InProgress));
        assert_eq!(map.insert(7, 70), Err(MutationError::InProgress));
        assert_eq!(map.remove(&7), Err(MutationError::InProgress));
        assert!(matches!(
            Map::init(memory),
            Err(InitError::RecoveryRequired)
        ));
    }

    #[test]
    fn completed_nested_mutation_invalidates_a_read_snapshot() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new");
        assert_eq!(
            map.read_consistent(|| {
                let nested = map.begin_mutation().expect("nested mutation fixture");
                nested.finish();
                70
            }),
            Err(MutationError::InProgress),
            "the post-read epoch check rejects a completed nested mutation"
        );
        assert_eq!(map.get(&7), Ok(None));
    }

    #[test]
    fn alias_mutation_during_insert_planning_invalidates_plan_without_outer_write() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let nested = Rc::new(CallbackMap::new(memory.clone()).expect("new nested handle"));
        assert_eq!(nested.insert(CallbackKey::plain(7), 70), Ok(None));
        let outer = CallbackMap::init(memory).expect("open outer handle");
        let attempted = Rc::new(Cell::new(false));
        let nested_for_callback = nested.clone();
        let attempted_for_callback = attempted.clone();
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            if attempted_for_callback.replace(true) {
                return;
            }
            assert_eq!(
                nested_for_callback.insert(CallbackKey::plain(8), 80),
                Ok(None)
            );
        });
        let reentrant = CallbackKey {
            value: 7,
            on_encode: None,
            on_hash: None,
            on_eq: Some(callback),
            invalid: false,
        };

        let epoch_before = outer.control_region().expect("idle control").mutation_epoch;
        assert_eq!(outer.insert(reentrant, 71), Err(MutationError::InProgress));
        assert!(attempted.get());
        assert_eq!(outer.get(&CallbackKey::plain(7)), Ok(Some(70)));
        assert_eq!(outer.get(&CallbackKey::plain(8)), Ok(Some(80)));
        let control = outer
            .control_region()
            .expect("idle control after alias write");
        assert_eq!(control.len, 2);
        assert_eq!(control.mutation_epoch, epoch_before + 2);
    }

    #[test]
    fn alias_mutation_supersedes_direct_pressure_error_without_outer_write() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let nested = Rc::new(
            CallbackMap::new_with_hash_seed(memory.clone(), 59).expect("new nested handle"),
        );
        let target_value = 0;
        let pair = nested.candidate_buckets(&CallbackKey::plain(target_value));
        let required = if pair.0 == pair.1 { 8 } else { 16 };
        let colliders = (1u64..)
            .filter(|candidate| nested.candidate_buckets(&CallbackKey::plain(*candidate)) == pair)
            .take(required)
            .collect::<Vec<_>>();
        for &key in &colliders {
            assert_eq!(nested.insert(CallbackKey::plain(key), key), Ok(None));
        }
        let outer = CallbackMap::init(memory).expect("open outer handle");
        let nested_key = colliders[0];
        let attempted = Rc::new(Cell::new(false));
        let attempted_for_callback = attempted.clone();
        let nested_for_callback = nested.clone();
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            if !attempted_for_callback.replace(true) {
                assert_eq!(
                    nested_for_callback.insert(CallbackKey::plain(nested_key), 99_001),
                    Ok(Some(nested_key))
                );
            }
        });
        let target = CallbackKey {
            value: target_value,
            on_encode: None,
            on_hash: None,
            on_eq: Some(callback),
            invalid: false,
        };
        let before = outer.control_region().expect("pre-pressure control");

        assert_eq!(outer.insert(target, 99_002), Err(MutationError::InProgress));
        assert!(attempted.get());
        assert_eq!(outer.get(&CallbackKey::plain(target_value)), Ok(None));
        assert_eq!(outer.get(&CallbackKey::plain(nested_key)), Ok(Some(99_001)));
        let after = outer.control_region().expect("post-pressure control");
        assert_eq!(after.len, before.len);
        assert_eq!(after.physical_buckets, before.physical_buckets);
        assert_eq!(after.split_cursor, before.split_cursor);
        assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
    }

    #[test]
    fn alias_mutation_supersedes_split_pressure_error_without_outer_write() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        const RESIDENTS: [u64; 48] = [
            215, 265, 887, 1017, 29, 60, 118, 162, 84, 114, 197, 206, 262, 339, 107, 122, 376, 416,
            605, 622, 61, 71, 130, 198, 381, 384, 39, 42, 91, 110, 132, 246, 7, 28, 69, 83, 101,
            108, 98, 131, 217, 235, 281, 38, 173, 223, 237, 240,
        ];
        let memory = VectorMemory::default();
        let nested = Rc::new(
            CallbackMap::new_with_hash_seed(memory.clone(), 211).expect("new nested handle"),
        );
        for key in RESIDENTS {
            assert_eq!(nested.insert(CallbackKey::plain(key), key), Ok(None));
        }
        let secrets = hash_secrets(211);
        let pressure_bucket = 1;
        let mut occupancy = CallbackMap::occupancy_byte(&memory, nested.header, pressure_bucket);
        let mut used = RESIDENTS.to_vec();
        while occupancy != u8::MAX {
            let key = (1u64..1 << 20)
                .find(|key| {
                    !used.contains(key)
                        && CallbackMap::candidate_buckets_from_bytes_at(
                            &key.to_be_bytes(),
                            &secrets,
                            3,
                            0,
                        ) == (pressure_bucket, pressure_bucket)
                })
                .expect("pressure filler");
            let slot = CallbackMap::first_empty(occupancy).expect("free pressure slot");
            nested.write_key_bytes(
                pressure_bucket,
                slot,
                CallbackKey::plain(key).to_bytes().as_ref(),
            );
            nested.write_value_bytes(pressure_bucket, slot, key.to_bytes().as_ref());
            occupancy |= 1 << slot;
            nested.write_occupancy(pressure_bucket, occupancy);
            used.push(key);
        }
        control::write_len(&memory, nested.header.control_offset, used.len() as u64);
        let target_value = (1u64..1 << 20)
            .find(|key| {
                !used.contains(key)
                    && CallbackMap::candidate_buckets_from_bytes_at(
                        &key.to_be_bytes(),
                        &secrets,
                        3,
                        1,
                    ) == (pressure_bucket, pressure_bucket)
            })
            .expect("pressured target");
        let outer = CallbackMap::init(memory).expect("open outer handle");
        let nested_key = RESIDENTS[0];
        let attempted = Rc::new(Cell::new(false));
        let attempted_for_callback = attempted.clone();
        let nested_for_callback = nested.clone();
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            if !attempted_for_callback.replace(true) {
                assert_eq!(
                    nested_for_callback.insert(CallbackKey::plain(nested_key), 99_101),
                    Ok(Some(nested_key))
                );
            }
        });
        let target = CallbackKey {
            value: target_value,
            on_encode: None,
            on_hash: None,
            on_eq: Some(callback),
            invalid: false,
        };
        let before = outer.control_region().expect("pre-split-pressure control");

        assert_eq!(outer.insert(target, 99_102), Err(MutationError::InProgress));
        assert!(attempted.get());
        assert_eq!(outer.get(&CallbackKey::plain(target_value)), Ok(None));
        assert_eq!(outer.get(&CallbackKey::plain(nested_key)), Ok(Some(99_101)));
        let after = outer.control_region().expect("post-split-pressure control");
        assert_eq!(after.len, before.len);
        assert_eq!(after.physical_buckets, before.physical_buckets);
        assert_eq!(after.split_cursor, before.split_cursor);
        assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
    }

    #[test]
    fn get_rejects_a_completed_nested_mutation_from_hash_callback() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let nested = Rc::new(CallbackMap::new(memory.clone()).expect("new nested handle"));
        assert_eq!(nested.insert(CallbackKey::plain(7), 70), Ok(None));
        let reader = CallbackMap::init(memory).expect("open reader handle");
        let nested_for_callback = nested.clone();
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            assert_eq!(
                nested_for_callback.insert(CallbackKey::plain(8), 80),
                Ok(None)
            );
        });
        let reentrant = CallbackKey {
            value: 7,
            on_encode: None,
            on_hash: Some(callback),
            on_eq: None,
            invalid: false,
        };

        assert_eq!(reader.get(&reentrant), Err(MutationError::InProgress));
        assert_eq!(reader.get(&CallbackKey::plain(7)), Ok(Some(70)));
        assert_eq!(reader.get(&CallbackKey::plain(8)), Ok(Some(80)));
    }

    #[test]
    fn remove_key_callback_panics_before_the_epoch_becomes_odd() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let map = CallbackMap::new(memory.clone()).expect("new callback map");
        assert_eq!(map.insert(CallbackKey::plain(7), 70), Ok(None));
        let bytes_before = allocated_bytes(&memory);
        let epoch_before = map.control_region().expect("idle control").mutation_epoch;
        let panicking = CallbackKey {
            value: 7,
            on_encode: None,
            on_hash: Some(Rc::new(|| panic!("remove key callback"))),
            on_eq: None,
            invalid: false,
        };

        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = map.remove(&panicking);
            }))
            .is_err()
        );
        assert_eq!(allocated_bytes(&memory), bytes_before);
        assert_eq!(
            map.control_region().expect("still idle").mutation_epoch,
            epoch_before
        );
        assert_eq!(map.get(&CallbackKey::plain(7)), Ok(Some(70)));
        drop(map);
        let reopened = CallbackMap::init(memory).expect("reopen after callback panic");
        assert_eq!(reopened.get(&CallbackKey::plain(7)), Ok(Some(70)));
    }

    #[test]
    fn remove_equality_callback_panics_before_the_epoch_becomes_odd() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let map = CallbackMap::new(memory.clone()).expect("new callback map");
        assert_eq!(map.insert(CallbackKey::plain(7), 70), Ok(None));
        let bytes_before = allocated_bytes(&memory);
        let epoch_before = map.control_region().expect("idle control").mutation_epoch;
        let panicking = CallbackKey {
            value: 7,
            on_encode: None,
            on_hash: None,
            on_eq: Some(Rc::new(|| panic!("remove equality callback"))),
            invalid: false,
        };

        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.remove(&panicking)))
                .is_err()
        );
        assert_eq!(allocated_bytes(&memory), bytes_before);
        assert_eq!(
            map.control_region().expect("still idle").mutation_epoch,
            epoch_before
        );
        assert_eq!(map.get(&CallbackKey::plain(7)), Ok(Some(70)));
        let reopened = CallbackMap::init(memory).expect("reopen after equality callback panic");
        assert_eq!(reopened.get(&CallbackKey::plain(7)), Ok(Some(70)));
    }

    #[test]
    fn completed_alias_mutation_supersedes_remove_without_outer_write() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let nested = Rc::new(CallbackMap::new(memory.clone()).expect("new nested handle"));
        assert_eq!(nested.insert(CallbackKey::plain(7), 70), Ok(None));
        let outer = CallbackMap::init(memory).expect("open outer handle");
        let nested_for_callback = nested.clone();
        let attempted = Rc::new(Cell::new(false));
        let attempted_for_callback = attempted.clone();
        let reentrant = CallbackKey {
            value: 7,
            on_encode: None,
            on_hash: Some(Rc::new(move || {
                if !attempted_for_callback.replace(true) {
                    assert_eq!(
                        nested_for_callback.insert(CallbackKey::plain(8), 80),
                        Ok(None)
                    );
                }
            })),
            on_eq: None,
            invalid: false,
        };

        assert_eq!(outer.remove(&reentrant), Err(MutationError::InProgress));
        assert!(attempted.get());
        assert_eq!(outer.get(&CallbackKey::plain(7)), Ok(Some(70)));
        assert_eq!(outer.get(&CallbackKey::plain(8)), Ok(Some(80)));
        assert_eq!(outer.len(), Ok(2));
    }

    #[test]
    fn malformed_fixed_width_encodings_leave_bytes_and_epoch_unchanged() {
        type BadKeyMap = StableLinearHashMap<BadKey, u64, VectorMemory>;
        type BadValueMap = StableLinearHashMap<u64, BadValue, VectorMemory>;

        let key_memory = VectorMemory::default();
        let bad_key_map = BadKeyMap::new(key_memory.clone()).expect("new bad-key map");
        let key_bytes_before = allocated_bytes(&key_memory);
        let key_epoch_before =
            control::read_mutation_epoch(&key_memory, bad_key_map.header.control_offset);
        assert_eq!(
            bad_key_map.insert(BadKey(7), 70),
            Err(MutationError::InvalidKeyEncoding)
        );
        assert_eq!(allocated_bytes(&key_memory), key_bytes_before);
        assert_eq!(
            control::read_mutation_epoch(&key_memory, bad_key_map.header.control_offset),
            key_epoch_before
        );
        assert_eq!(bad_key_map.len(), Ok(0));

        let value_memory = VectorMemory::default();
        let bad_value_map = BadValueMap::new(value_memory.clone()).expect("new bad-value map");
        let value_bytes_before = allocated_bytes(&value_memory);
        let value_epoch_before =
            control::read_mutation_epoch(&value_memory, bad_value_map.header.control_offset);
        assert_eq!(
            bad_value_map.insert(7, BadValue(70)),
            Err(MutationError::InvalidValueEncoding)
        );
        assert_eq!(allocated_bytes(&value_memory), value_bytes_before);
        assert_eq!(
            control::read_mutation_epoch(&value_memory, bad_value_map.header.control_offset),
            value_epoch_before
        );
        assert_eq!(bad_value_map.len(), Ok(0));
    }

    #[test]
    fn exhausted_epoch_rejects_mutation_without_changing_control() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new");
        let exhausted = u64::MAX - 1;
        control::write_mutation_epoch(&memory, map.header.control_offset, exhausted);
        let before = allocated_bytes(&memory);

        assert_eq!(map.insert(7, 70), Err(MutationError::EpochExhausted));
        assert_eq!(allocated_bytes(&memory), before);
        assert_eq!(
            control::read_mutation_epoch(&memory, map.header.control_offset),
            exhausted
        );
    }

    #[test]
    fn odd_epoch_left_by_an_aborted_mutation_reopen_fails_closed() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new");
        control::write_mutation_epoch(&memory, map.header.control_offset, 1);

        assert_eq!(map.get(&7), Err(MutationError::InProgress));
        assert!(matches!(
            Map::init(memory),
            Err(InitError::RecoveryRequired)
        ));
    }

    #[test]
    fn crud_and_reopen_preserve_entries_and_seed() {
        let memory = VectorMemory::default();
        {
            let map = Map::new_with_hash_seed(memory.clone(), 29).expect("new");
            for key in 0..40 {
                assert_eq!(map.insert(key, key * 3), Ok(None));
            }
            assert_eq!(map.insert(7, 700), Ok(Some(21)));
            assert_eq!(map.remove(&8), Ok(Some(24)));
        }
        let reopened = Map::init_with_hash_seed(memory, 999).expect("reopen persisted seed");
        assert_eq!(reopened.hash_seed(), Ok(29));
        assert_eq!(reopened.len(), Ok(39));
        assert_eq!(reopened.get(&7), Ok(Some(700)));
        assert_eq!(reopened.get(&8), Ok(None));
        for key in (0..40).filter(|key| *key != 7 && *key != 8) {
            assert_eq!(reopened.get(&key), Ok(Some(key * 3)));
        }
    }

    #[test]
    fn owner_reset_clears_initial_occupancy_and_publishes_successor_last() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 0x51).expect("new reset fixture");
        let header_before = read_bytes(&memory, 0, HEADER_SIZE as usize);
        for key in 1..=12 {
            assert_eq!(map.insert(key, key ^ 0xaa), Ok(None));
        }
        let before = map.control_region().expect("reset control");
        let bytes_before = allocated_bytes(&memory);

        assert_eq!(map.reset(before.incarnation), Ok(before.incarnation + 1));

        let after = map.control_region().expect("settled reset control");
        assert_eq!(after.len, 0);
        assert_eq!(after.physical_buckets, INITIAL_BUCKETS);
        assert_eq!(after.level, INITIAL_LEVEL);
        assert_eq!(after.split_cursor, 0);
        assert_eq!(after.incarnation, before.incarnation + 1);
        assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
        assert_eq!(after.hash_seed, before.hash_seed);
        assert_eq!(read_bytes(&memory, 0, HEADER_SIZE as usize), header_before);
        for bucket in 0..INITIAL_BUCKETS {
            assert_eq!(
                read_bytes(
                    &memory,
                    Map::bucket_base(map.header, bucket),
                    BUCKET_HEADER_BYTES as usize,
                ),
                vec![0; BUCKET_HEADER_BYTES as usize]
            );
        }
        let bytes_after = allocated_bytes(&memory);
        let is_reset_mutation = |offset: usize| {
            let control = HEADER_SIZE as usize..(HEADER_SIZE + CONTROL_BYTES) as usize;
            let occupancy = (0..INITIAL_BUCKETS).any(|bucket| {
                let start = Map::bucket_base(map.header, bucket) as usize;
                (start..start + BUCKET_HEADER_BYTES as usize).contains(&offset)
            });
            control.contains(&offset) || occupancy
        };
        for (offset, (before_byte, after_byte)) in bytes_before.iter().zip(&bytes_after).enumerate()
        {
            if !is_reset_mutation(offset) {
                assert_eq!(
                    after_byte, before_byte,
                    "reset changed non-owned byte at offset {offset}"
                );
            }
        }
        assert_eq!(map.hash_seed(), Ok(0x51));
    }

    #[test]
    fn reset_preflight_errors_write_nothing() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        let memory = FailpointMemory::default();
        let map = FailMap::new(memory.clone()).expect("new reset rejection fixture");
        let current = map.control_region().expect("current control");

        for expected in [current.incarnation + 1, current.incarnation - 1] {
            let before = allocated_bytes(&memory.inner);
            memory.writes.set(0);
            assert_eq!(
                map.reset(expected),
                Err(ResetError::IncarnationMismatch {
                    current: current.incarnation
                })
            );
            assert_eq!(memory.writes.get(), 0);
            assert_eq!(allocated_bytes(&memory.inner), before);
        }

        write_incarnation(&map, u64::MAX);
        let before = allocated_bytes(&memory.inner);
        memory.writes.set(0);
        assert_eq!(map.reset(u64::MAX), Err(ResetError::IncarnationExhausted));
        assert_eq!(memory.writes.get(), 0);
        assert_eq!(allocated_bytes(&memory.inner), before);

        write_incarnation(&map, current.incarnation);
        control::write_mutation_epoch(&memory, map.header.control_offset, 1);
        let before = allocated_bytes(&memory.inner);
        memory.writes.set(0);
        assert_eq!(map.reset(current.incarnation), Err(ResetError::InProgress));
        assert_eq!(memory.writes.get(), 0);
        assert_eq!(allocated_bytes(&memory.inner), before);

        control::write_mutation_epoch(&memory, map.header.control_offset, u64::MAX - 1);
        let before = allocated_bytes(&memory.inner);
        memory.writes.set(0);
        assert_eq!(
            map.reset(current.incarnation),
            Err(ResetError::EpochExhausted)
        );
        assert_eq!(memory.writes.get(), 0);
        assert_eq!(allocated_bytes(&memory.inner), before);
    }

    #[test]
    fn reset_alias_epoch_change_rejects_before_outer_write() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        let memory = FailpointMemory::default();
        let map = FailMap::new(memory.clone()).expect("new reset alias fixture");
        let current = map.control_region().expect("current control");
        let before = allocated_bytes(&memory.inner);
        memory.writes.set(0);
        memory
            .epoch_read_override
            .set(Some(current.mutation_epoch + 2));

        assert_eq!(map.reset(current.incarnation), Err(ResetError::InProgress));
        assert_eq!(memory.writes.get(), 0);
        assert_eq!(allocated_bytes(&memory.inner), before);
    }

    #[test]
    fn reset_reentrancy_observes_odd_epoch_and_cannot_write() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        let memory = FailpointMemory::default();
        let map = Rc::new(FailMap::new(memory.clone()).expect("new reset reentrancy fixture"));
        let current = map.control_region().expect("current control");
        let nested_map = Rc::clone(&map);
        let nested_result = Rc::new(RefCell::new(None));
        let callback_result = Rc::clone(&nested_result);
        *memory.after_write.borrow_mut() = Some(Rc::new(move || {
            *callback_result.borrow_mut() = Some(nested_map.reset(current.incarnation));
        }));

        assert_eq!(map.reset(current.incarnation), Ok(current.incarnation + 1));
        assert_eq!(
            nested_result.borrow_mut().take(),
            Some(Err(ResetError::InProgress))
        );
        assert_eq!(
            map.control_region().expect("settled reset").mutation_epoch,
            current.mutation_epoch + 2
        );
    }

    #[test]
    fn split_after_reset_fully_overwrites_reused_trailing_page_and_reopens() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 0x71).expect("new reuse fixture");
        let (_, _, first_target) = seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        assert_eq!(map.insert(first_target, 0x1111), Ok(None));
        assert_eq!(
            map.control_region().expect("first split").physical_buckets,
            9
        );
        let current = map.control_region().expect("pre-reset control");
        assert_eq!(map.reset(current.incarnation), Ok(current.incarnation + 1));

        let trailing_base = Map::bucket_base(map.header, 8);
        memory.write(
            trailing_base,
            &vec![0xa5; map.header.bucket_page_stride as usize],
        );
        let (residents, _, target) =
            seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        assert_eq!(map.insert(target, 0x2222), Ok(None));
        let reused = read_bytes(
            &memory,
            trailing_base,
            map.header.bucket_page_stride as usize,
        );
        assert!(
            reused[2..BUCKET_HEADER_BYTES as usize]
                .iter()
                .all(|byte| *byte == 0)
        );

        let second_control = map.control_region().expect("second pre-reset control");
        assert_eq!(
            map.reset(second_control.incarnation),
            Ok(second_control.incarnation + 1)
        );
        memory.write(
            trailing_base,
            &vec![0x5a; map.header.bucket_page_stride as usize],
        );
        let (_, _, replay_target) =
            seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        assert_eq!(replay_target, target);
        assert_eq!(map.insert(replay_target, 0x2222), Ok(None));
        assert_eq!(
            read_bytes(
                &memory,
                trailing_base,
                map.header.bucket_page_stride as usize
            ),
            reused,
            "the complete reused page must be independent of trailing bytes"
        );

        let reopened = Map::open(memory).expect("reopen reused trailing page");
        assert_eq!(reopened.get(&target), Ok(Some(0x2222)));
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Ok(Some(value)));
        }
    }

    #[test]
    fn init_with_hash_seed_only_uses_argument_for_empty_memory() {
        let memory = VectorMemory::default();
        let map = Map::init_with_hash_seed(memory.clone(), 41).expect("initialize");
        assert_eq!(map.hash_seed(), Ok(41));
        drop(map);
        assert_eq!(
            Map::init_with_hash_seed(memory, 43)
                .expect("reopen")
                .hash_seed(),
            Ok(41)
        );
    }

    #[test]
    fn cloned_memory_handles_share_canonical_len() {
        let memory = VectorMemory::default();
        let first = Map::new_with_hash_seed(memory.clone(), 101).expect("new first handle");
        let second = Map::init(memory).expect("open second handle");

        assert_eq!(first.insert(7, 70), Ok(None));
        assert_eq!(second.len(), Ok(1));
        assert_eq!(first.get(&7), Ok(Some(70)));
        assert_eq!(second.get(&7), Ok(Some(70)));
    }

    #[test]
    fn cloned_memory_handle_uses_the_immutable_seed() {
        let old_seed = 107;
        let memory = VectorMemory::default();
        let first = Map::new_with_hash_seed(memory.clone(), old_seed).expect("new first handle");
        let second = Map::init(memory.clone()).expect("open second handle");
        let (key, old_candidates) = (0u64..)
            .map(|key| (key, first.candidate_buckets(&key)))
            .find(|(_, candidates)| candidates.0 != candidates.1)
            .expect("distinct candidates");

        assert_eq!(second.insert(key, 110), Ok(None));
        let actual_bucket = second.find(&key).expect("find through refreshed seed").0;
        assert!(actual_bucket == old_candidates.0 || actual_bucket == old_candidates.1);
        assert_eq!(first.get(&key), Ok(Some(110)));
        drop(first);
        drop(second);

        let reopened = Map::init(memory).expect("reopen new seed routing");
        assert_eq!(reopened.hash_seed(), Ok(old_seed));
        assert_eq!(reopened.len(), Ok(1));
        assert_eq!(reopened.get(&key), Ok(Some(110)));
    }

    #[test]
    fn candidate_pressure_is_failure_atomic_with_other_free_buckets() {
        let map = Map::new_with_hash_seed(VectorMemory::default(), 59).expect("new");
        let target = 0;
        let pair = map.candidate_buckets(&target);
        let required = if pair.0 == pair.1 { 8 } else { 16 };
        let mut colliders = Vec::new();
        for candidate in 1u64.. {
            if map.candidate_buckets(&candidate) == pair {
                colliders.push(candidate);
                if colliders.len() == required {
                    break;
                }
            }
        }
        for key in &colliders {
            map.insert(*key, *key + 1000).expect("fill candidates");
        }
        let before = map.len().expect("idle len");
        let bytes_before = allocated_bytes(&map.memory);
        assert!(before < 8 * u64::from(BUCKET_SIZE));
        assert_eq!(map.insert(target, 999), Err(MutationError::TablePressure));
        assert_eq!(allocated_bytes(&map.memory), bytes_before);
        assert_eq!(map.len(), Ok(before));
        assert_eq!(map.get(&target), Ok(None));
        for key in colliders {
            assert_eq!(map.get(&key), Ok(Some(key + 1000)));
        }
    }

    #[test]
    fn one_hop_admits_below_threshold_pressure_without_geometry_growth_and_preserves_lifecycle() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 401).expect("new one-hop fixture");
        let (residents, target, moved, destination) = seed_one_hop_fixture(&map);
        let before = map.control_region().expect("pre-one-hop control");
        let source = map.find(&moved).expect("movable resident").0;
        let target_candidates = map.candidate_buckets(&target);
        assert!(
            before.len < Map::split_threshold(before.physical_buckets).expect("split threshold")
        );
        assert_eq!(
            Map::occupancy_byte(&memory, map.header, target_candidates.0),
            u8::MAX
        );
        assert_eq!(
            Map::occupancy_byte(&memory, map.header, target_candidates.1),
            u8::MAX
        );
        assert_eq!(map.insert(target, 90_001), Ok(None));
        let after = map.control_region().expect("post-one-hop control");
        assert_eq!(after.len, before.len + 1);
        assert_eq!(after.physical_buckets, before.physical_buckets);
        assert_eq!(after.split_cursor, before.split_cursor);
        assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
        assert_eq!(map.find(&moved).expect("relocated resident").0, destination);
        assert_ne!(source, destination);
        assert_eq!(map.get(&target), Ok(Some(90_001)));
        assert_eq!(map.insert(target, 90_002), Ok(Some(90_001)));
        assert_eq!(map.remove(&target), Ok(Some(90_002)));
        assert_eq!(map.insert(target, 90_003), Ok(None));
        for &(key, value) in &residents {
            assert_eq!(map.get(&key), Ok(Some(value)));
        }

        let reopened = Map::init(memory).expect("reopen one-hop fixture");
        assert_eq!(reopened.get(&target), Ok(Some(90_003)));
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Ok(Some(value)));
        }
        let cursor = reopened.scrub_snapshot().expect("post-one-hop snapshot");
        assert!(matches!(
            reopened.scrub_step(cursor, u64::MAX),
            Ok(ScrubStep::Complete(_))
        ));
    }

    fn assert_one_hop_uses_current_geometry(
        level: u8,
        split_cursor: u64,
        physical_buckets: u64,
        preceding_geometry: (u8, u64),
    ) {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 401).expect("new geometry fixture");
        let (residents, target, moved, destination) = seed_one_hop_fixture_at_geometry(
            &map,
            level,
            split_cursor,
            physical_buckets,
            Some(preceding_geometry),
        );
        let before = map.control_region().expect("valid pre-relocation geometry");
        assert_eq!(
            (before.level, before.split_cursor, before.physical_buckets),
            (level, split_cursor, physical_buckets)
        );
        let target_candidates = map.candidate_buckets(&target);
        assert_ne!(target_candidates.0, target_candidates.1);
        assert_eq!(
            Map::occupancy_byte(&memory, map.header, target_candidates.0),
            u8::MAX
        );
        assert_eq!(
            Map::occupancy_byte(&memory, map.header, target_candidates.1),
            u8::MAX
        );
        let source = map.find(&moved).expect("movable resident").0;
        let secrets = hash_secrets(before.hash_seed);
        let current_routes = Map::candidate_buckets_from_bytes_at(
            &moved.stable_hash_bytes(),
            &secrets,
            level,
            split_cursor,
        );
        assert!(current_routes.0 == source || current_routes.1 == source);
        assert!(current_routes.0 == destination || current_routes.1 == destination);
        let preceding_routes = Map::candidate_buckets_from_bytes_at(
            &moved.stable_hash_bytes(),
            &secrets,
            preceding_geometry.0,
            preceding_geometry.1,
        );
        assert_ne!(preceding_routes.0, destination);
        assert_ne!(preceding_routes.1, destination);

        assert_eq!(map.insert(target, 90_051), Ok(None));
        assert_eq!(map.get(&target), Ok(Some(90_051)));
        assert_eq!(map.find(&moved).expect("relocated resident").0, destination);
        for &(key, value) in &residents {
            assert_eq!(map.get(&key), Ok(Some(value)));
        }
        let after = map.control_region().expect("post-relocation geometry");
        assert_eq!(after.level, before.level);
        assert_eq!(after.split_cursor, before.split_cursor);
        assert_eq!(after.physical_buckets, before.physical_buckets);

        let reopened = Map::init(memory).expect("reopen current geometry");
        let reopened_control = reopened.control_region().expect("reopened geometry");
        assert_eq!(reopened_control.level, before.level);
        assert_eq!(reopened_control.split_cursor, before.split_cursor);
        assert_eq!(reopened_control.physical_buckets, before.physical_buckets);
        assert_eq!(reopened.get(&target), Ok(Some(90_051)));
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Ok(Some(value)));
        }
    }

    #[test]
    fn one_hop_uses_current_nonzero_split_geometry_and_reopens() {
        assert_one_hop_uses_current_geometry(3, 3, 11, (3, 2));
    }

    #[test]
    fn one_hop_uses_current_rollover_geometry_and_reopens() {
        assert_one_hop_uses_current_geometry(4, 0, 16, (3, 7));
    }

    #[test]
    fn one_hop_no_movable_resident_is_exactly_failure_atomic() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 59).expect("new exhausted fixture");
        let target = 0;
        let candidates = map.candidate_buckets(&target);
        let required = if candidates.0 == candidates.1 { 8 } else { 16 };
        let colliders = (1u64..)
            .filter(|key| map.candidate_buckets(key) == candidates)
            .take(required)
            .collect::<Vec<_>>();
        for &key in &colliders {
            assert_eq!(map.insert(key, key ^ 0xa5a5), Ok(None));
        }
        let before = allocated_bytes(&memory);
        let control_before = map.control_region().expect("pre-exhausted control");
        assert_eq!(
            map.insert(target, 90_101),
            Err(MutationError::TablePressure)
        );
        assert_eq!(allocated_bytes(&memory), before);
        assert_eq!(map.control_region(), Ok(control_before));
        assert_eq!(map.get(&target), Ok(None));
        for key in colliders {
            assert_eq!(map.get(&key), Ok(Some(key ^ 0xa5a5)));
        }
    }

    #[test]
    fn one_hop_duplicate_candidates_probe_only_one_bucket() {
        let memory = CountingMemory::default();
        let map = StableLinearHashMap::<u64, u64, _>::new_with_hash_seed(memory.clone(), 409)
            .expect("new duplicate-candidate fixture");
        let target = (0u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 == candidates.1
            })
            .expect("duplicate candidate target");
        let bucket = map.candidate_buckets(&target).0;
        let mut occupancy = 0u8;
        let mut residents = Vec::new();
        for slot in 0..BUCKET_SIZE {
            let key = (target + 1..)
                .find(|key| {
                    !residents.contains(key) && map.candidate_buckets(key) == (bucket, bucket)
                })
                .expect("duplicate-candidate resident");
            map.write_key_bytes(bucket, slot, key.to_bytes().as_ref());
            map.write_value_bytes(bucket, slot, &(key ^ 0xa5a5).to_bytes());
            occupancy |= 1 << slot;
            residents.push(key);
        }
        map.write_occupancy(bucket, occupancy);
        control::write_len(&memory, map.header.control_offset, BUCKET_SIZE.into());
        let before = stable_snapshot(&map, &memory.inner);
        reset_counts(&memory);
        assert_eq!(
            map.insert(target, 90_201),
            Err(MutationError::TablePressure)
        );
        assert_eq!(stable_snapshot(&map, &memory.inner), before);
        assert_eq!(memory.write_calls.get(), 0);
        let source_page_reads = memory
            .read_ranges
            .borrow()
            .iter()
            .filter(|&&(offset, len)| {
                offset == Map::bucket_base(map.header, bucket)
                    && len == map.header.bucket_page_stride as usize
            })
            .count();
        assert_eq!(source_page_reads, 1);
        assert_eq!(map.get(&target), Ok(None));
        for &key in &residents {
            assert_eq!(map.get(&key), Ok(Some(key ^ 0xa5a5)));
        }
        assert_eq!(map.len(), Ok(BUCKET_SIZE.into()));
    }

    #[test]
    fn one_hop_prewrite_callback_and_error_matrix_is_atomic() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;
        type ValueMap = StableLinearHashMap<CallbackKey, CallbackValue, VectorMemory>;

        for callback_kind in [
            "target_hash",
            "target_key",
            "resident_decode",
            "resident_hash",
        ] {
            let memory = VectorMemory::default();
            let map = CallbackMap::new_with_hash_seed(memory.clone(), 409).expect("callback map");
            let (residents, target_value) = seed_callback_pressure_fixture(&map);
            let called = Rc::new(Cell::new(false));
            let callback_map = Rc::new(map);
            let called_for_callback = called.clone();
            let map_for_callback = callback_map.clone();
            let callback: Rc<dyn Fn()> = Rc::new(move || {
                assert_even_callback(&*map_for_callback, &called_for_callback);
            });
            let mut target = CallbackKey::plain(target_value);
            let expected = match callback_kind {
                "target_hash" => {
                    target.on_hash = Some(callback);
                    MutationError::TablePressure
                }
                "target_key" => {
                    target.on_encode = Some(callback);
                    target.invalid = true;
                    MutationError::InvalidKeyEncoding
                }
                "resident_decode" => {
                    RESIDENT_DECODE_CALLBACK.with(|slot| *slot.borrow_mut() = Some(callback));
                    MutationError::TablePressure
                }
                "resident_hash" => {
                    RESIDENT_HASH_CALLBACK.with(|slot| {
                        *slot.borrow_mut() = Some((residents[0].0, callback));
                    });
                    MutationError::TablePressure
                }
                _ => unreachable!(),
            };
            let before = stable_snapshot(&*callback_map, &memory);
            assert_eq!(
                callback_map.insert(target, 90_211),
                Err(expected),
                "{callback_kind}"
            );
            RESIDENT_DECODE_CALLBACK.with(|slot| slot.borrow_mut().take());
            RESIDENT_HASH_CALLBACK.with(|slot| slot.borrow_mut().take());
            assert!(called.get(), "{callback_kind}");
            assert_eq!(
                stable_snapshot(&*callback_map, &memory),
                before,
                "{callback_kind}"
            );
            assert_eq!(
                callback_map.get(&CallbackKey::plain(target_value)),
                Ok(None)
            );
            for (key, value) in residents {
                assert_eq!(callback_map.get(&CallbackKey::plain(key)), Ok(Some(value)));
            }
        }

        let memory = VectorMemory::default();
        let map = Rc::new(ValueMap::new_with_hash_seed(memory.clone(), 409).expect("value map"));
        let called = Rc::new(Cell::new(false));
        let map_for_callback = map.clone();
        let called_for_callback = called.clone();
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            assert_even_callback(&*map_for_callback, &called_for_callback);
        });
        let before = stable_snapshot(&*map, &memory);
        assert!(matches!(
            map.insert(
                CallbackKey::plain(7),
                CallbackValue {
                    value: 70,
                    on_encode: Some(callback),
                    invalid: true,
                },
            ),
            Err(MutationError::InvalidValueEncoding)
        ));
        assert!(called.get());
        assert_eq!(stable_snapshot(&*map, &memory), before);

        for (epoch, expected) in [
            (1, MutationError::InProgress),
            (u64::MAX - 1, MutationError::EpochExhausted),
        ] {
            let memory = VectorMemory::default();
            let map = CallbackMap::new_with_hash_seed(memory.clone(), 409).expect("epoch map");
            control::write_mutation_epoch(&memory, map.header.control_offset, epoch);
            let before = stable_snapshot(&map, &memory);
            assert_eq!(map.insert(CallbackKey::plain(7), 70), Err(expected));
            assert_eq!(stable_snapshot(&map, &memory), before);
        }
    }

    #[test]
    fn one_hop_exhausted_scan_is_bounded_and_deterministic() {
        type ProbeMap = StableLinearHashMap<ProbeKey, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = ProbeMap::new_with_hash_seed(memory.clone(), 419).expect("new probe fixture");
        let target = (0u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(&ProbeKey(*key));
                candidates.0 != candidates.1
            })
            .expect("distinct target candidates");
        let candidates = map.candidate_buckets(&ProbeKey(target));
        let colliders = (target + 1..)
            .filter(|key| map.candidate_buckets(&ProbeKey(*key)) == candidates)
            .take(16)
            .collect::<Vec<_>>();
        for &key in &colliders {
            assert_eq!(map.insert(ProbeKey(key), key ^ 0xa5a5), Ok(None));
        }
        let mut expected = colliders
            .iter()
            .map(|&key| {
                let (bucket, slot, _) = map.find(&ProbeKey(key)).expect("resident placement");
                let candidate_order = usize::from(bucket == candidates.1);
                (candidate_order, slot, key)
            })
            .collect::<Vec<_>>();
        expected.sort_unstable();
        let expected = expected
            .into_iter()
            .map(|(_, _, key)| key)
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 16);
        assert!(
            map.len().expect("probe len")
                < ProbeMap::split_threshold(INITIAL_BUCKETS).expect("threshold")
        );
        reset_counts(&memory);
        PROBED_HASH_KEYS.with(|keys| keys.borrow_mut().clear());
        RECORD_HASH_KEYS.with(|record| record.set(true));
        let result = map.insert(ProbeKey(target), 90_301);
        RECORD_HASH_KEYS.with(|record| record.set(false));
        assert_eq!(result, Err(MutationError::TablePressure));
        let probed = PROBED_HASH_KEYS.with(|keys| keys.borrow().clone());
        assert_eq!(probed.first(), Some(&target));
        assert_eq!(&probed[1..], expected);
        assert_eq!(probed.len() - 1, 16);
        assert_eq!(memory.write_calls.get(), 0);
    }

    #[test]
    fn one_hop_success_probes_at_most_sixteen_and_relocates_exactly_one() {
        type ProbeMap = StableLinearHashMap<ProbeKey, u64, CountingMemory>;

        let probe = Map::new_with_hash_seed(VectorMemory::default(), 421).expect("new seed probe");
        let (residents, target, moved, destination) = seed_one_hop_fixture(&probe);
        let memory = CountingMemory::default();
        let map = ProbeMap::new_with_hash_seed(memory.clone(), 421).expect("new counted fixture");
        let mut occupancies = [0u8; INITIAL_BUCKETS as usize];
        for &(key, value) in &residents {
            let bucket = probe.find(&key).expect("probe resident").0;
            let slot = ProbeMap::first_empty(occupancies[bucket as usize]).expect("fixture slot");
            map.write_key_bytes(bucket, slot, &key.to_be_bytes());
            map.write_value_bytes(bucket, slot, &value.to_be_bytes());
            occupancies[bucket as usize] |= 1 << slot;
            map.write_occupancy(bucket, occupancies[bucket as usize]);
        }
        control::write_len(&memory, map.header.control_offset, residents.len() as u64);
        let before_buckets = residents
            .iter()
            .map(|&(key, _)| (key, map.find(&ProbeKey(key)).expect("before resident").0))
            .collect::<Vec<_>>();
        reset_counts(&memory);
        PROBED_HASH_KEYS.with(|keys| keys.borrow_mut().clear());
        RECORD_HASH_KEYS.with(|record| record.set(true));
        let result = map.insert(ProbeKey(target), 90_351);
        RECORD_HASH_KEYS.with(|record| record.set(false));
        assert_eq!(result, Ok(None));
        let probed = PROBED_HASH_KEYS.with(|keys| keys.borrow().clone());
        assert_eq!(probed.first(), Some(&target));
        assert!(probed.len() - 1 <= 16);
        let moved_residents = before_buckets
            .iter()
            .filter(|&&(key, before_bucket)| {
                map.find(&ProbeKey(key)).expect("after resident").0 != before_bucket
            })
            .count();
        assert_eq!(moved_residents, 1);
        assert_eq!(
            map.find(&ProbeKey(moved)).expect("moved resident").0,
            destination
        );
        assert_eq!(memory.write_calls.get(), 5);
    }

    #[test]
    fn one_hop_alias_mutation_invalidates_planning() {
        type CallbackMap = StableLinearHashMap<CallbackKey, u64, VectorMemory>;

        let probe = Map::new_with_hash_seed(VectorMemory::default(), 421).expect("new probe");
        let (residents, target_value, _, _) = seed_one_hop_fixture(&probe);
        let memory = VectorMemory::default();
        let nested = Rc::new(
            CallbackMap::new_with_hash_seed(memory.clone(), 421).expect("new callback fixture"),
        );
        let mut occupancies = [0u8; INITIAL_BUCKETS as usize];
        for &(key, value) in &residents {
            let bucket = probe.find(&key).expect("probe resident").0;
            let slot =
                CallbackMap::first_empty(occupancies[bucket as usize]).expect("fixture slot");
            nested.write_key_bytes(bucket, slot, &key.to_be_bytes());
            nested.write_value_bytes(bucket, slot, &value.to_be_bytes());
            occupancies[bucket as usize] |= 1 << slot;
            nested.write_occupancy(bucket, occupancies[bucket as usize]);
        }
        control::write_len(
            &memory,
            nested.header.control_offset,
            residents.len() as u64,
        );
        let outer = CallbackMap::init(memory).expect("open outer callback handle");
        let nested_key = residents[0].0;
        let attempted = Rc::new(Cell::new(false));
        let attempted_for_callback = attempted.clone();
        let nested_for_callback = nested.clone();
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            if !attempted_for_callback.replace(true) {
                assert_eq!(
                    nested_for_callback.insert(CallbackKey::plain(nested_key), 90_401,),
                    Ok(Some(nested_key ^ 0xa5a5_a5a5_a5a5_a5a5))
                );
            }
        });
        let target = CallbackKey {
            value: target_value,
            on_encode: None,
            on_hash: None,
            on_eq: Some(callback),
            invalid: false,
        };
        let before = outer.control_region().expect("pre-alias control");
        assert_eq!(outer.insert(target, 90_402), Err(MutationError::InProgress));
        assert!(attempted.get());
        assert_eq!(outer.get(&CallbackKey::plain(target_value)), Ok(None));
        assert_eq!(outer.get(&CallbackKey::plain(nested_key)), Ok(Some(90_401)));
        let after = outer.control_region().expect("post-alias control");
        assert_eq!(after.len, before.len);
        assert_eq!(after.physical_buckets, before.physical_buckets);
        assert_eq!(after.split_cursor, before.split_cursor);
        assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
    }

    #[test]
    fn every_one_hop_apply_write_boundary_fails_closed() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        for fail_write in 2..=5 {
            let memory = FailpointMemory::default();
            let map = FailMap::new_with_hash_seed(memory.clone(), 431).expect("new fail fixture");
            let (_, target, _, _) = seed_one_hop_fixture(&map);
            memory.writes.set(0);
            memory.fail_write.set(Some(fail_write));
            let trapped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = map.insert(target, 90_501);
            }));
            assert!(trapped.is_err(), "write boundary {fail_write}");
            memory.fail_write.set(None);
            assert_eq!(
                control::read_mutation_epoch(&memory, map.header.control_offset) & 1,
                1,
                "write boundary {fail_write}"
            );
            assert!(matches!(
                FailMap::init(memory),
                Err(InitError::RecoveryRequired)
            ));
        }
    }

    fn assert_split_fixture(move_count: usize, target_kind: SplitTarget) {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 211).expect("new split fixture");
        let (residents, source_keys, target) =
            seed_threshold_split_fixture(&map, 3, 0, 8, move_count, target_kind);
        let epoch_before = map
            .control_region()
            .expect("pre-split control")
            .mutation_epoch;
        assert_eq!(map.insert(target, 9_999), Ok(None));
        let control = map.control_region().expect("post-split control");
        assert_eq!(
            (
                control.level,
                control.split_cursor,
                control.physical_buckets
            ),
            (3, 1, 9)
        );
        assert_eq!(control.len, 49);
        assert_eq!(control.mutation_epoch, epoch_before + 2);
        let moved = source_keys
            .iter()
            .filter(|key| map.find(key).expect("resident after split").0 == 8)
            .count();
        assert_eq!(moved, move_count);
        for &(key, value) in &residents {
            assert_eq!(map.get(&key), Ok(Some(value)));
        }
        assert_eq!(map.get(&target), Ok(Some(9_999)));

        let reopened = Map::init(memory).expect("reopen split fixture");
        assert_eq!(reopened.get(&target), Ok(Some(9_999)));
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Ok(Some(value)));
        }
    }

    #[test]
    fn split_redistributes_zero_source_entries_and_places_target_in_new_bucket() {
        assert_split_fixture(0, SplitTarget::New);
    }

    #[test]
    fn split_redistributes_four_source_entries_and_places_target_in_new_bucket() {
        assert_split_fixture(4, SplitTarget::New);
    }

    #[test]
    fn split_redistributes_all_source_entries_and_reopens() {
        assert_split_fixture(8, SplitTarget::Source);
    }

    #[test]
    fn split_places_target_in_unaffected_bucket_and_reopens() {
        assert_split_fixture(4, SplitTarget::Unaffected);
    }

    #[test]
    fn split_threshold_boundaries_and_overwrite_do_not_split_early() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory, 211).expect("new threshold fixture");
        let (mut residents, _, projected_over_target) =
            seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        let (removed_key, removed_value) = residents.pop().expect("resident below threshold");
        assert_eq!(map.remove(&removed_key), Ok(Some(removed_value)));
        assert_eq!(map.len(), Ok(47));
        assert_eq!(map.insert(removed_key, removed_value), Ok(None));
        let equal = map.control_region().expect("exact threshold");
        assert_eq!(
            (equal.split_cursor, equal.physical_buckets, equal.len),
            (0, 8, 48)
        );
        assert_eq!(
            map.insert(removed_key, removed_value + 1),
            Ok(Some(removed_value))
        );
        let overwritten = map.control_region().expect("overwrite at threshold");
        assert_eq!(
            (
                overwritten.split_cursor,
                overwritten.physical_buckets,
                overwritten.len
            ),
            (0, 8, 48)
        );
        assert_eq!(map.insert(projected_over_target, 30_001), Ok(None));
        let split = map.control_region().expect("projected-over split");
        assert_eq!(
            (split.split_cursor, split.physical_buckets, split.len),
            (1, 9, 49)
        );
    }

    #[test]
    fn post_split_pressure_uses_one_hop_without_geometry_growth() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 239).expect("new pressure fixture");
        let (mut residents, _, _) =
            seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        let secrets = hash_secrets(239);
        let bucket = 1;
        let mut occupancy = Map::occupancy_byte(&memory, map.header, bucket);
        let mut used = residents.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        while occupancy != u8::MAX {
            let key = (1u64..1 << 20)
                .find(|key| {
                    !used.contains(key)
                        && Map::candidate_buckets_from_bytes_at(
                            &key.stable_hash_bytes(),
                            &secrets,
                            3,
                            0,
                        ) == (bucket, bucket)
                })
                .expect("pressure filler");
            let mut occupancies = vec![0; 8];
            occupancies[bucket as usize] = occupancy;
            place_fixture_entry(
                &map,
                &mut occupancies,
                &mut residents,
                &mut used,
                bucket,
                key,
            );
            occupancy = occupancies[bucket as usize];
        }
        control::write_len(&memory, map.header.control_offset, residents.len() as u64);
        let target = (1u64..1 << 20)
            .find(|key| {
                !used.contains(key)
                    && Map::candidate_buckets_from_bytes_at(
                        &key.stable_hash_bytes(),
                        &secrets,
                        3,
                        1,
                    ) == (bucket, bucket)
            })
            .expect("pressured target");
        assert_eq!(map.candidate_buckets(&target), (bucket, bucket));
        assert_eq!(Map::occupancy_byte(&memory, map.header, bucket), u8::MAX);
        let before = map.control_region().expect("pre-one-hop control");
        let before_buckets = residents
            .iter()
            .map(|&(key, _)| (key, map.find(&key).expect("pre-one-hop resident").0))
            .collect::<Vec<_>>();
        let size_before = memory.size();
        assert_eq!(map.insert(target, 30_101), Ok(None));
        let after = map.control_region().expect("post-one-hop control");
        assert_eq!(memory.size(), size_before);
        assert_eq!(after.len, before.len + 1);
        assert_eq!(after.physical_buckets, before.physical_buckets);
        assert_eq!(after.split_cursor, before.split_cursor);
        assert_eq!(after.mutation_epoch, before.mutation_epoch + 2);
        assert_eq!(map.get(&target), Ok(Some(30_101)));
        assert_eq!(
            before_buckets
                .iter()
                .filter(
                    |&&(key, bucket)| map.find(&key).expect("post-one-hop resident").0 != bucket
                )
                .count(),
            1
        );
        for &(key, value) in &residents {
            assert_eq!(map.get(&key), Ok(Some(value)));
        }
        let reopened = Map::init(memory).expect("reopen post-split one-hop fixture");
        assert_eq!(reopened.get(&target), Ok(Some(30_101)));
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Ok(Some(value)));
        }
    }

    #[test]
    fn split_redistribution_decodes_stored_keys_for_canonical_hash_bytes() {
        type RoutingMap = StableLinearHashMap<RoutingKey, u64, VectorMemory>;

        let memory = VectorMemory::default();
        let map = RoutingMap::new_with_hash_seed(memory.clone(), 211).expect("new routing fixture");
        let secrets = hash_secrets(211);
        let mut keys = Vec::new();
        for bucket in 0..8 {
            let count = if bucket == 0 {
                8
            } else if bucket <= 5 {
                6
            } else {
                5
            };
            for _ in 0..count {
                let key = (1u64..1 << 20)
                    .find(|key| {
                        !keys.contains(key)
                            && RoutingMap::candidate_buckets_from_bytes_at(
                                &key.to_be_bytes(),
                                &secrets,
                                3,
                                0,
                            ) == (bucket, bucket)
                            && (bucket != 0
                                || RoutingMap::candidate_buckets_from_bytes_at(
                                    &key.to_be_bytes(),
                                    &secrets,
                                    3,
                                    1,
                                ) == (8, 8))
                    })
                    .expect("bounded routing fixture search");
                assert_eq!(map.insert(RoutingKey(key), key ^ 0x55aa), Ok(None));
                keys.push(key);
            }
        }
        assert_eq!(keys.len(), 48);
        let target = (1u64..1 << 20)
            .find(|key| {
                !keys.contains(key)
                    && RoutingMap::candidate_buckets_from_bytes_at(
                        &key.to_be_bytes(),
                        &secrets,
                        3,
                        1,
                    ) == (0, 0)
            })
            .expect("bounded target search");
        assert_eq!(map.insert(RoutingKey(target), target ^ 0x55aa), Ok(None));
        for &key in &keys[..8] {
            assert_eq!(map.find(&RoutingKey(key)).expect("moved key").0, 8);
        }
        let reopened = RoutingMap::init(memory).expect("reopen routing fixture");
        for key in keys.into_iter().chain([target]) {
            assert_eq!(reopened.get(&RoutingKey(key)), Ok(Some(key ^ 0x55aa)));
        }
    }

    #[test]
    fn split_rollover_promotes_level_and_resets_cursor() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 223).expect("new rollover fixture");
        let (residents, _, target) =
            seed_threshold_split_fixture(&map, 3, 7, 15, 4, SplitTarget::New);
        assert_eq!(map.insert(target, 22_300), Ok(None));
        let control = map.control_region().expect("rollover control");
        assert_eq!(
            (
                control.level,
                control.split_cursor,
                control.physical_buckets
            ),
            (4, 0, 16)
        );
        assert_eq!(control.len, 91);
        assert_eq!(map.get(&target), Ok(Some(22_300)));
        let reopened = Map::init(memory).expect("reopen rollover");
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Ok(Some(value)));
        }
        assert_eq!(reopened.get(&target), Ok(Some(22_300)));
    }

    #[test]
    fn split_grow_failure_leaves_logical_bytes_and_epoch_unchanged() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        let memory = FailpointMemory::default();
        let map = FailMap::new_with_hash_seed(memory.clone(), 227).expect("new OOM fixture");
        let (residents, _, target) =
            seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        let bytes_before = allocated_bytes(&memory.inner);
        let control_before = map.control_region().expect("pre-OOM control");
        memory.fail_grow.set(true);
        assert_eq!(map.insert(target, 22_700), Err(MutationError::OutOfMemory));
        memory.fail_grow.set(false);
        assert_eq!(allocated_bytes(&memory.inner), bytes_before);
        assert_eq!(map.control_region(), Ok(control_before));
        assert_eq!(map.get(&target), Ok(None));
        for (key, value) in residents {
            assert_eq!(map.get(&key), Ok(Some(value)));
        }
    }

    #[test]
    fn every_split_apply_write_boundary_leaves_odd_epoch_and_reopen_fails_closed() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        for fail_write in 2..=9 {
            let memory = FailpointMemory::default();
            let map = FailMap::new_with_hash_seed(memory.clone(), 229).expect("new panic fixture");
            let (_, _, target) =
                seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::Unaffected);
            memory.writes.set(0);
            memory.fail_write.set(Some(fail_write));
            let trapped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = map.insert(target, 22_900);
            }));
            assert!(trapped.is_err(), "write boundary {fail_write}");
            memory.fail_write.set(None);
            assert_eq!(
                control::read_mutation_epoch(&memory, map.header.control_offset) & 1,
                1,
                "write boundary {fail_write}"
            );
            assert!(matches!(
                FailMap::init(memory),
                Err(InitError::RecoveryRequired)
            ));
        }
    }

    #[test]
    fn split_capacity_overflow_is_prewrite_atomic() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        let memory = FailpointMemory::default();
        let map = FailMap::new_with_hash_seed(memory.clone(), 241).expect("new overflow fixture");
        let (_, _, target) = seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        let bytes_before = allocated_bytes(&memory.inner);
        let control_before = map.control_region().expect("pre-overflow control");
        memory.size_override.set(Some(u64::MAX));
        assert_eq!(
            map.insert(target, 30_201),
            Err(MutationError::CapacityOverflow)
        );
        memory.size_override.set(None);
        assert_eq!(allocated_bytes(&memory.inner), bytes_before);
        assert_eq!(map.control_region(), Ok(control_before));
    }

    #[test]
    fn alias_mutation_after_split_grow_invalidates_guard_without_outer_write() {
        type FailMap = StableLinearHashMap<u64, u64, FailpointMemory>;

        let memory = FailpointMemory::default();
        let map = FailMap::new_with_hash_seed(memory.clone(), 243).expect("new alias fixture");
        let (residents, _, target) =
            seed_threshold_split_fixture(&map, 3, 0, 8, 4, SplitTarget::New);
        let control_before = map.control_region().expect("pre-alias control");
        let original_size = memory.inner.size();
        let callback_memory = memory.clone();
        *memory.after_grow.borrow_mut() = Some(Rc::new(move || {
            callback_memory
                .epoch_read_override
                .set(Some(control_before.mutation_epoch + 2));
        }));
        memory.size_override.set(Some(0));
        assert_eq!(map.insert(target, 30_301), Err(MutationError::InProgress));
        memory.size_override.set(None);
        memory.after_grow.borrow_mut().take();
        assert_eq!(memory.inner.size(), original_size + 1);
        assert_eq!(map.control_region(), Ok(control_before));
        assert_eq!(map.get(&target), Ok(None));
        for (key, value) in residents {
            assert_eq!(map.get(&key), Ok(Some(value)));
        }
        let appended = read_bytes(
            &memory.inner,
            original_size * crate::memory::WASM_PAGE_SIZE,
            crate::memory::WASM_PAGE_SIZE as usize,
        );
        assert!(appended.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn nonzero_control_reserved_bytes_fail_closed() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new reserved fixture");
        memory.write(map.header.control_offset + 32, &[1]);
        assert!(matches!(Map::init(memory), Err(InitError::InvalidLayout)));
    }

    #[test]
    fn reopen_defers_bucket_corruption_but_rejects_impossible_len() {
        let occupancy_memory = VectorMemory::default();
        let occupancy = Map::new(occupancy_memory.clone()).expect("new occupancy fixture");
        let middle_bucket =
            occupancy.header.buckets_offset + 3 * occupancy.header.bucket_page_stride;
        occupancy_memory.write(middle_bucket, &0x0100u16.to_le_bytes());
        assert!(Map::init(occupancy_memory).is_ok());

        let reserved_memory = VectorMemory::default();
        let reserved = Map::new(reserved_memory.clone()).expect("new reserved fixture");
        let last_bucket = reserved.header.buckets_offset + 7 * reserved.header.bucket_page_stride;
        reserved_memory.write(last_bucket + 2, &[1]);
        assert!(Map::init(reserved_memory).is_ok());

        let len_memory = VectorMemory::default();
        let len_map = Map::new(len_memory.clone()).expect("new len fixture");
        control::write_len(&len_memory, len_map.header.control_offset, 65);
        assert!(matches!(
            Map::init(len_memory),
            Err(InitError::InvalidLayout)
        ));
    }

    #[test]
    fn reopen_accepts_midround_geometry_and_rejects_invalid_geometry() {
        let smaller_memory = VectorMemory::default();
        let smaller = Map::new(smaller_memory.clone()).expect("new smaller fixture");
        smaller_memory.write(smaller.header.control_offset + 8, &4u64.to_le_bytes());
        assert!(matches!(
            Map::init(smaller_memory),
            Err(InitError::InvalidLayout)
        ));

        let split_memory = VectorMemory::default();
        let split = Map::new(split_memory.clone()).expect("new split fixture");
        split_memory.write(split.header.control_offset + 8, &9u64.to_le_bytes());
        let reopened = Map::init(split_memory).expect("reopen mid-round geometry");
        let control = reopened.control_region().expect("idle mid-round control");
        assert_eq!(control.level, 3);
        assert_eq!(control.split_cursor, 1);
        assert_eq!(control.physical_buckets, 9);
        assert_eq!(control.len, 0);

        let invalid_cursor_memory = VectorMemory::default();
        let invalid_cursor = Map::new(invalid_cursor_memory.clone()).expect("new cursor fixture");
        invalid_cursor_memory.write(
            invalid_cursor.header.control_offset + 8,
            &7u64.to_le_bytes(),
        );
        assert!(matches!(
            Map::init(invalid_cursor_memory),
            Err(InitError::InvalidLayout)
        ));

        let mismatch_memory = VectorMemory::default();
        let mismatch = Map::new(mismatch_memory.clone()).expect("new mismatch fixture");
        mismatch_memory.write(mismatch.header.control_offset + 24, &0u64.to_le_bytes());
        assert!(matches!(
            Map::init(mismatch_memory),
            Err(InitError::InvalidLayout)
        ));

        let high_memory = VectorMemory::default();
        let high = Map::new(high_memory.clone()).expect("new high fixture");
        high_memory.write(high.header.control_offset + 8, &u64::MAX.to_le_bytes());
        assert!(matches!(
            Map::init(high_memory),
            Err(InitError::InvalidLayout)
        ));

        let rollover_memory = VectorMemory::default();
        let rollover = Map::new(rollover_memory.clone()).expect("new rollover fixture");
        rollover_memory.write(rollover.header.control_offset + 8, &16u64.to_le_bytes());
        let reopened = Map::init(rollover_memory).expect("valid settled rollover");
        assert_eq!(
            reopened
                .control_region()
                .expect("rollover control")
                .physical_buckets,
            16
        );

        let extent_memory = FailpointMemory::default();
        let extent = StableLinearHashMap::<u64, u64, _>::new(extent_memory.clone())
            .expect("new extent fixture");
        extent_memory.write(extent.header.control_offset + 8, &512u64.to_le_bytes());
        extent_memory.size_override.set(Some(1));
        assert!(matches!(
            StableLinearHashMap::<u64, u64, _>::init(extent_memory),
            Err(InitError::InvalidLayout)
        ));
    }

    #[test]
    fn linear_bucket_uses_doubled_base_for_split_buckets() {
        assert_eq!(linear_bucket(9, 3, 3), 9);
        assert_eq!(linear_bucket(12, 3, 3), 4);
    }

    #[test]
    fn linear_bucket_matches_reference_at_geometry_boundaries_and_samples() {
        fn modulo_bucket(hash: u64, level: u8, split_cursor: u64) -> u64 {
            let base = 1u64 << level;
            let bucket = hash % base;
            if bucket < split_cursor {
                hash % (base * 2)
            } else {
                bucket
            }
        }

        for level in INITIAL_LEVEL..=62 {
            let base = 1u64 << level;
            let doubled = base * 2;
            for split_cursor in [0, 1, base / 2, base - 1] {
                let boundary_hashes = [
                    0,
                    1,
                    split_cursor.saturating_sub(1),
                    split_cursor,
                    split_cursor + 1,
                    base - 1,
                    base,
                    base + 1,
                    doubled - 1,
                    doubled,
                    0x0123_4567_89ab_cdef,
                    0xfedc_ba98_7654_3210,
                    u64::MAX - 1,
                    u64::MAX,
                ];
                for hash in boundary_hashes {
                    assert_eq!(
                        linear_bucket(hash, level, split_cursor),
                        modulo_bucket(hash, level, split_cursor),
                        "hash={hash:#018x}, level={level}, split_cursor={split_cursor}"
                    );
                }
                let mut sample = u64::from(level) << 56 | split_cursor;
                for _ in 0..64 {
                    sample = sample
                        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                        .wrapping_add(0xbf58_476d_1ce4_e5b9);
                    assert_eq!(
                        linear_bucket(sample, level, split_cursor),
                        modulo_bucket(sample, level, split_cursor),
                        "hash={sample:#018x}, level={level}, split_cursor={split_cursor}"
                    );
                }
            }
        }
    }

    #[test]
    fn routing_preserves_exact_placement_and_reopen_bytes() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 0x6a09_e667_f3bc_c909).expect("new");
        let expected = [
            (0, (1, 2)),
            (1, (2, 1)),
            (7, (5, 7)),
            (8, (6, 0)),
            (9, (6, 4)),
            (15, (5, 2)),
            (16, (3, 7)),
            (17, (7, 6)),
            (0x0123_4567_89ab_cdef, (3, 3)),
            (u64::MAX, (5, 0)),
        ];

        for &(key, candidates) in &expected {
            assert_eq!(map.candidate_buckets(&key), candidates);
            assert_eq!(map.insert(key, key ^ 0xa5a5), Ok(None));
        }
        let bytes_before_reopen = allocated_bytes(&memory);
        drop(map);

        let reopened = Map::init(memory.clone()).expect("reopen");
        assert_eq!(allocated_bytes(&memory), bytes_before_reopen);
        for &(key, candidates) in &expected {
            assert_eq!(reopened.candidate_buckets(&key), candidates);
            assert_eq!(reopened.get(&key), Ok(Some(key ^ 0xa5a5)));
        }
    }

    #[test]
    fn equal_candidate_loads_choose_first_bucket() {
        let map = Map::new_with_hash_seed(VectorMemory::default(), 67).expect("new");
        let key = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct candidates");
        let candidates = map.candidate_buckets(&key);

        assert_eq!(map.insert(key, 700), Ok(None));
        assert_eq!(map.find(&key), Some((candidates.0, 0, 1)));
        assert_eq!(map.bucket_load(candidates.0), 1);
        assert_eq!(map.bucket_load(candidates.1), 0);
    }

    #[test]
    fn lower_loaded_second_candidate_is_preferred() {
        let map = Map::new_with_hash_seed(VectorMemory::default(), 71).expect("new");
        let (key, filler) = (1u64..)
            .find_map(|key| {
                let target_candidates = map.candidate_buckets(&key);
                if target_candidates.0 == target_candidates.1 {
                    return None;
                }
                (1u64..).find_map(|filler| {
                    (filler != key && map.candidate_buckets(&filler).0 == target_candidates.0)
                        .then_some((key, filler))
                })
            })
            .expect("target and filler placement");
        let candidates = map.candidate_buckets(&key);
        assert_eq!(map.insert(filler, 711), Ok(None));
        assert_eq!(map.find(&filler).map(|entry| entry.0), Some(candidates.0));

        assert_eq!(map.insert(key, 710), Ok(None));
        assert_eq!(map.find(&key), Some((candidates.1, 0, 1)));
        assert_eq!(map.bucket_load(candidates.0), 1);
        assert_eq!(map.bucket_load(candidates.1), 1);
        assert_eq!(map.get(&filler), Ok(Some(711)));
        assert_eq!(map.get(&key), Ok(Some(710)));
        let memory = map.into_memory();
        let reopened = Map::init(memory).expect("reopen valid placement");
        assert_eq!(reopened.len(), Ok(2));
        assert_eq!(reopened.get(&filler), Ok(Some(711)));
        assert_eq!(reopened.get(&key), Ok(Some(710)));
    }

    #[test]
    fn physical_scan_empty_and_sparse_pages_use_explicit_eof() {
        let empty = Map::new_with_hash_seed(VectorMemory::default(), 191).expect("empty map");
        let start = empty.scan_start().expect("empty scan start");
        let empty_page = empty.scan_step(start, 2).expect("empty bounded page");
        assert!(empty_page.entries().is_empty());
        assert_eq!(empty_page.examined_slots(), 2);
        assert!(!empty_page.exhausted());
        assert_eq!(empty_page.next_cursor().next_slot(), 2);

        let mut final_slot = start;
        final_slot.next_slot = u64::from(BUCKET_SIZE) * INITIAL_BUCKETS - 1;
        let eof = empty.scan_step(final_slot, 8).expect("explicit eof page");
        assert!(eof.entries().is_empty());
        assert_eq!(eof.examined_slots(), 1);
        assert!(eof.exhausted());

        let sparse = Map::new_with_hash_seed(VectorMemory::default(), 193).expect("sparse map");
        let key = (0u64..)
            .find(|key| sparse.candidate_buckets(key).0 < INITIAL_BUCKETS - 1)
            .expect("nonfinal sparse bucket");
        sparse.insert(key, 1_930).expect("sparse entry");
        let (bucket, slot, _) = sparse.find(&key).expect("sparse physical slot");
        let mut cursor = sparse.scan_start().expect("sparse scan start");
        cursor.next_slot = bucket * u64::from(BUCKET_SIZE) + u64::from(slot);
        let short = sparse.scan_step(cursor, 2).expect("short non-eof page");
        assert_eq!(short.entries(), &[(key, 1_930)]);
        assert_eq!(short.examined_slots(), 2);
        assert!(!short.exhausted());
    }

    #[test]
    fn physical_scan_respects_exact_bucket_boundaries() {
        let map = Map::new_with_hash_seed(VectorMemory::default(), 197).expect("map");
        map.write_key_bytes(0, 7, 7u64.to_bytes().as_ref());
        map.write_value_bytes(0, 7, 70u64.to_bytes().as_ref());
        map.write_occupancy(0, 1 << 7);
        map.write_key_bytes(1, 0, 8u64.to_bytes().as_ref());
        map.write_value_bytes(1, 0, 80u64.to_bytes().as_ref());
        map.write_occupancy(1, 1);

        let start = map.scan_start().expect("scan start");
        let first = map.scan_step(start, 8).expect("first bucket");
        assert_eq!(first.entries(), &[(7, 70)]);
        assert_eq!(first.examined_slots(), 8);
        assert_eq!(first.next_cursor().next_slot(), 8);
        assert!(!first.exhausted());

        let second = map
            .scan_step(first.next_cursor(), 1)
            .expect("first slot of second bucket");
        assert_eq!(second.entries(), &[(8, 80)]);
        assert_eq!(second.examined_slots(), 1);
        assert_eq!(second.next_cursor().next_slot(), 9);
        assert!(!second.exhausted());
    }

    #[test]
    fn physical_scan_is_exactly_once_on_an_unchanged_map_and_replayable() {
        let map = Map::new_with_hash_seed(VectorMemory::default(), 199).expect("map");
        let mut expected = Vec::new();
        for key in 0..20 {
            let entry = (key, key + 1_990);
            map.insert(entry.0, entry.1).expect("seed entry");
            expected.push(entry);
        }

        let initial = map.scan_start().expect("scan start");
        let first = map.scan_step(initial, 7).expect("first page");
        assert_eq!(map.scan_step(initial, 7), Ok(first));
        let first = map.scan_step(initial, 7).expect("replayed first page");
        let mut actual = first.entries().to_vec();
        let mut cursor = first.next_cursor();
        let mut exhausted = first.exhausted();
        while !exhausted {
            let page = map.scan_step(cursor, 7).expect("next page");
            actual.extend_from_slice(page.entries());
            cursor = page.next_cursor();
            exhausted = page.exhausted();
        }
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);

        let eof = map.scan_step(cursor, 7).expect("replayed eof");
        assert!(eof.entries().is_empty());
        assert_eq!(eof.examined_slots(), 0);
        assert!(eof.exhausted());
    }

    #[test]
    fn physical_scan_does_not_fence_mutations_between_steps() {
        let map = Map::new_with_hash_seed(VectorMemory::default(), 209).expect("map");
        map.insert(1, 10).expect("initial entry");
        let first = map
            .scan_step(map.scan_start().expect("scan start"), 1)
            .expect("first step");
        map.insert(2, 20).expect("between-step mutation");

        let second = map
            .scan_step(first.next_cursor(), 1)
            .expect("cursor remains usable at unchanged geometry");
        assert_eq!(second.examined_slots(), 1);
        assert_eq!(second.next_cursor().next_slot(), 2);
    }

    #[test]
    fn physical_scan_discards_output_when_alias_mutates_mid_step() {
        type CallbackMap = StableLinearHashMap<CallbackKey, CallbackValue, VectorMemory>;

        let memory = VectorMemory::default();
        let map = CallbackMap::new_with_hash_seed(memory.clone(), 211).expect("map");
        map.insert(
            CallbackKey::plain(1),
            CallbackValue {
                value: 10,
                on_encode: None,
                invalid: false,
            },
        )
        .expect("seed entry");
        let alias = Rc::new(CallbackMap::open(memory).expect("alias"));
        let cursor = map.scan_start().expect("scan start");
        let fired = Rc::new(Cell::new(false));
        let alias_for_callback = Rc::clone(&alias);
        let fired_for_callback = Rc::clone(&fired);
        RESIDENT_DECODE_CALLBACK.with(|callback| {
            *callback.borrow_mut() = Some(Rc::new(move || {
                if fired_for_callback.replace(true) {
                    return;
                }
                alias_for_callback
                    .insert(
                        CallbackKey::plain(2),
                        CallbackValue {
                            value: 20,
                            on_encode: None,
                            invalid: false,
                        },
                    )
                    .expect("nested alias mutation");
            }));
        });

        assert!(matches!(
            map.scan_step(cursor, u64::MAX),
            Err(ScanError::InProgress)
        ));
        assert!(fired.get());
        RESIDENT_DECODE_CALLBACK.with(|callback| callback.borrow_mut().take());
    }

    #[test]
    fn physical_scan_requires_restart_after_split_or_reset() {
        let split_map = Map::new_with_hash_seed(VectorMemory::default(), 223).expect("split map");
        let split_cursor = split_map.scan_start().expect("pre-split cursor");
        let (_, _, target) = seed_threshold_split_fixture(&split_map, 3, 0, 8, 4, SplitTarget::New);
        split_map.insert(target, 2_230).expect("split insertion");
        assert_eq!(
            split_map
                .control_region()
                .expect("split control")
                .physical_buckets,
            INITIAL_BUCKETS + 1
        );
        assert!(matches!(
            split_map.scan_step(split_cursor, 1),
            Err(ScanError::RestartRequired)
        ));

        let reset_map = Map::new_with_hash_seed(VectorMemory::default(), 227).expect("reset map");
        let reset_cursor = reset_map.scan_start().expect("pre-reset cursor");
        let incarnation = reset_map.control_region().expect("control").incarnation;
        reset_map.reset(incarnation).expect("owner reset");
        assert!(matches!(
            reset_map.scan_step(reset_cursor, 1),
            Err(ScanError::RestartRequired)
        ));
    }

    #[test]
    fn serialized_physical_scan_cursor_survives_exact_reopen() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 229).expect("map");
        let mut expected = Vec::new();
        for key in 0..10 {
            let entry = (key, key + 2_290);
            map.insert(entry.0, entry.1).expect("seed entry");
            expected.push(entry);
        }
        let first = map
            .scan_step(map.scan_start().expect("scan start"), 9)
            .expect("first page");
        let mut actual = first.entries().to_vec();
        let encoded = first.next_cursor().encode();
        assert_eq!(encoded.len(), ScanCursor::ENCODED_SIZE);
        drop(map);

        let reopened = Map::open(memory).expect("exact reopen");
        let mut cursor = ScanCursor::decode(&encoded).expect("decode persisted cursor");
        assert_eq!(cursor.hash_seed(), 229);
        assert_eq!(cursor.physical_buckets(), INITIAL_BUCKETS);
        loop {
            let page = reopened.scan_step(cursor, 9).expect("reopened page");
            actual.extend_from_slice(page.entries());
            cursor = page.next_cursor();
            if page.exhausted() {
                break;
            }
        }
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn physical_scan_rejects_zero_budget_and_malformed_cursor() {
        let map = Map::new_with_hash_seed(VectorMemory::default(), 233).expect("map");
        let cursor = map.scan_start().expect("scan start");
        assert!(matches!(
            map.scan_step(cursor, 0),
            Err(ScanError::ZeroBudget)
        ));

        let encoded = cursor.encode();
        assert_eq!(
            ScanCursor::decode(&encoded[..encoded.len() - 1]),
            Err(ScanError::InvalidCursor)
        );
        let mut bad_magic = encoded;
        bad_magic[0] ^= 0xff;
        assert_eq!(
            ScanCursor::decode(&bad_magic),
            Err(ScanError::InvalidCursor)
        );
        let mut bad_reserved = encoded;
        bad_reserved[4] = 1;
        assert_eq!(
            ScanCursor::decode(&bad_reserved),
            Err(ScanError::InvalidCursor)
        );

        let mut out_of_bounds = cursor;
        out_of_bounds.next_slot = INITIAL_BUCKETS * u64::from(BUCKET_SIZE) + 1;
        assert!(matches!(
            map.scan_step(out_of_bounds, 1),
            Err(ScanError::InvalidCursor)
        ));
        let other = Map::new_with_hash_seed(VectorMemory::default(), 239).expect("other map");
        assert!(matches!(
            other.scan_step(cursor, 1),
            Err(ScanError::InvalidCursor)
        ));
    }

    #[test]
    fn physical_scan_read_work_is_bounded_by_examined_slots() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 241).expect("map");
        for slot in 0..BUCKET_SIZE {
            let key = u64::from(slot);
            map.write_key_bytes(0, slot, key.to_bytes().as_ref());
            map.write_value_bytes(0, slot, (key + 100).to_bytes().as_ref());
        }
        map.write_occupancy(0, u8::MAX);
        map.write_key_bytes(1, 0, 8u64.to_bytes().as_ref());
        map.write_value_bytes(1, 0, 108u64.to_bytes().as_ref());
        map.write_occupancy(1, 1);
        let cursor = map.scan_start().expect("scan start");
        reset_counts(&memory);

        let page = map.scan_step(cursor, 9).expect("bounded page");
        assert_eq!(page.entries().len(), 9);
        assert_eq!(page.examined_slots(), 9);
        assert_eq!(page.next_cursor().next_slot(), 9);
        assert!(!page.exhausted());
        assert_eq!(memory.read_calls.get(), 23);
        assert_eq!(memory.read_bytes.get(), 284);
        let bucket_reads = memory
            .read_ranges
            .borrow()
            .iter()
            .filter(|(offset, _)| *offset >= BUCKETS_OFFSET)
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(bucket_reads.len(), 20);
        assert!(bucket_reads.iter().all(|(offset, _)| {
            let bucket = (offset - BUCKETS_OFFSET) / map.header.bucket_page_stride;
            bucket <= 1
        }));
        assert_eq!(memory.write_calls.get(), 0);
    }

    #[test]
    fn bounded_scrub_replays_cursor_and_completes_without_writes() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 107).expect("new");
        for key in 0..12 {
            assert_eq!(map.insert(key, key + 1_000), Ok(None));
        }
        let initial = map.scrub_snapshot().expect("snapshot");

        reset_counts(&memory);
        let first = map.scrub_step(initial, 2).expect("first step");
        let replay = map.scrub_step(initial, 2).expect("replayed first step");
        assert_eq!(first, replay);
        assert!(
            matches!(first, ScrubStep::InProgress(cursor) if cursor.next_primary_bucket() == 2)
        );
        assert_eq!(memory.write_calls.get(), 0);

        let mut cursor = match first {
            ScrubStep::InProgress(cursor) => cursor,
            ScrubStep::Complete(_) => panic!("two buckets cannot complete this fixture"),
        };
        loop {
            match map.scrub_step(cursor, 3).expect("bounded step") {
                ScrubStep::InProgress(next) => cursor = next,
                ScrubStep::Complete(snapshot) => {
                    assert_eq!(snapshot, initial.snapshot());
                    break;
                }
            }
        }
        assert_eq!(memory.write_calls.get(), 0);
    }

    #[test]
    fn bounded_scrub_reads_only_budgeted_primary_buckets_and_candidates() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 109).expect("new");
        let primary = 0;
        let mut keys = Vec::new();
        let mut legal_candidates = std::collections::BTreeSet::from([primary]);
        for key in 0u64.. {
            let candidates = map.candidate_buckets(&key);
            if candidates.0 != candidates.1
                && (candidates.0 == primary || candidates.1 == primary)
                && !keys.iter().any(|(existing, _)| *existing == key)
            {
                legal_candidates.extend([candidates.0, candidates.1]);
                keys.push((key, candidates));
                if keys.len() == BUCKET_SIZE as usize {
                    break;
                }
            }
        }
        assert_eq!(keys.len(), BUCKET_SIZE as usize);
        for (slot, (key, _)) in keys.iter().enumerate() {
            map.write_key_bytes(primary, slot as u32, &key.to_bytes());
            map.write_value_bytes(primary, slot as u32, &key.to_bytes());
        }
        map.write_occupancy(primary, u8::MAX);
        for candidate in legal_candidates
            .iter()
            .copied()
            .filter(|bucket| *bucket != primary)
        {
            let mut occupancy = 0;
            for slot in 0..BUCKET_SIZE {
                let filler = (0u64..)
                    .find(|key| {
                        !keys.iter().any(|(existing, _)| existing == key) && {
                            let routes = map.candidate_buckets(key);
                            routes.0 == candidate || routes.1 == candidate
                        }
                    })
                    .expect("candidate filler");
                keys.push((filler, map.candidate_buckets(&filler)));
                map.write_key_bytes(candidate, slot, &filler.to_bytes());
                map.write_value_bytes(candidate, slot, &filler.to_bytes());
                occupancy |= 1 << slot;
            }
            map.write_occupancy(candidate, occupancy);
        }
        let total_entries = legal_candidates.len() as u64 * u64::from(BUCKET_SIZE);
        control::write_len(&memory, map.header.control_offset, total_entries);
        let cursor = map.scrub_snapshot().expect("snapshot");
        reset_counts(&memory);

        let step = map.scrub_step(cursor, 1).expect("one primary bucket");
        assert!(matches!(step, ScrubStep::InProgress(_)));
        let bucket_ranges = memory
            .read_ranges
            .borrow()
            .iter()
            .filter(|(offset, _)| *offset >= BUCKETS_OFFSET)
            .count();
        let per_occupied_slot = 2 + 2 * (1 + BUCKET_SIZE as usize);
        let maximum_bucket_reads = 1 + BUCKET_SIZE as usize * per_occupied_slot;
        assert!(bucket_ranges <= maximum_bucket_reads);
        for &(offset, _) in memory
            .read_ranges
            .borrow()
            .iter()
            .filter(|(offset, _)| *offset >= BUCKETS_OFFSET)
        {
            let bucket = (offset - BUCKETS_OFFSET) / map.header.bucket_page_stride;
            assert!(legal_candidates.contains(&bucket));
        }
        assert_eq!(memory.write_calls.get(), 0);
    }

    #[test]
    fn bounded_scrub_rejects_zero_budget_and_stale_alias_cursor() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 113).expect("new");
        let alias = Map::open(memory).expect("alias");
        let cursor = map.scrub_snapshot().expect("snapshot");

        assert_eq!(map.scrub_step(cursor, 0), Err(ScrubError::ZeroBudget));
        alias.insert(1, 10).expect("alias mutation");
        assert_eq!(map.scrub_step(cursor, 1), Err(ScrubError::Stale));
    }

    #[test]
    fn bounded_scrub_rejects_cursor_from_another_map_or_handle() {
        let first = Map::new_with_hash_seed(VectorMemory::default(), 179).expect("first map");
        let second = Map::new_with_hash_seed(VectorMemory::default(), 179).expect("second map");
        for key in 0..4 {
            first.insert(key, key).expect("first seed");
            second.insert(key, key).expect("second seed");
        }
        let cursor = first.scrub_snapshot().expect("first snapshot");
        let advanced = match first.scrub_step(cursor, 1).expect("first step") {
            ScrubStep::InProgress(cursor) => cursor,
            ScrubStep::Complete(_) => panic!("one bucket cannot complete"),
        };
        assert_eq!(
            second.scrub_step(advanced, u64::MAX),
            Err(ScrubError::InvalidCursor)
        );

        let memory = VectorMemory::default();
        let original = Map::new_with_hash_seed(memory.clone(), 181).expect("original");
        let alias = Map::open(memory).expect("alias");
        let cursor = original.scrub_snapshot().expect("original snapshot");
        assert_eq!(alias.scrub_step(cursor, 1), Err(ScrubError::InvalidCursor));
    }

    #[test]
    fn bounded_scrub_rejects_reserved_occupancy_and_length_mismatch() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 127).expect("new");
        let cursor = map.scrub_snapshot().expect("snapshot");
        memory.write(Map::bucket_base(map.header, 0), &[0, 1]);
        assert_eq!(
            map.scrub_step(cursor, 1),
            Err(ScrubError::InvalidOccupancy { bucket: 0 })
        );

        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 131).expect("new");
        control::write_len(&memory, map.header.control_offset, 1);
        let cursor = map.scrub_snapshot().expect("snapshot");
        assert_eq!(
            map.scrub_step(cursor, u64::MAX),
            Err(ScrubError::LengthMismatch {
                expected: 1,
                actual: 0
            })
        );
    }

    #[test]
    fn bounded_scrub_rejects_unreachable_and_duplicate_placements() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 137).expect("new");
        let key = 7;
        let candidates = map.candidate_buckets(&key);
        let unreachable = (0..INITIAL_BUCKETS)
            .find(|bucket| *bucket != candidates.0 && *bucket != candidates.1)
            .expect("unreachable bucket");
        map.write_key_bytes(unreachable, 0, &key.to_bytes());
        map.write_value_bytes(unreachable, 0, &70u64.to_bytes());
        map.write_occupancy(unreachable, 1);
        control::write_len(&memory, map.header.control_offset, 1);
        let cursor = map.scrub_snapshot().expect("snapshot");
        assert_eq!(
            map.scrub_step(cursor, u64::MAX),
            Err(ScrubError::UnreachablePlacement {
                bucket: unreachable,
                slot: 0
            })
        );

        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 139).expect("new");
        let key = (0u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("two candidates");
        let candidates = map.candidate_buckets(&key);
        for bucket in [candidates.0, candidates.1] {
            map.write_key_bytes(bucket, 0, &key.to_bytes());
            map.write_value_bytes(bucket, 0, &key.to_bytes());
            map.write_occupancy(bucket, 1);
        }
        control::write_len(&memory, map.header.control_offset, 2);
        let cursor = map.scrub_snapshot().expect("snapshot");
        assert!(matches!(
            map.scrub_step(cursor, u64::MAX),
            Err(ScrubError::DuplicateKey { .. })
        ));

        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 149).expect("new");
        let key = (0u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 == candidates.1
            })
            .expect("one candidate");
        let bucket = map.candidate_buckets(&key).0;
        for slot in 0..2 {
            map.write_key_bytes(bucket, slot, &key.to_bytes());
            map.write_value_bytes(bucket, slot, &key.to_bytes());
        }
        map.write_occupancy(bucket, 0b11);
        control::write_len(&memory, map.header.control_offset, 2);
        let cursor = map.scrub_snapshot().expect("snapshot");
        assert!(matches!(
            map.scrub_step(cursor, u64::MAX),
            Err(ScrubError::DuplicateKey { .. })
        ));
    }

    #[test]
    fn bounded_scrub_rejects_noncanonical_fixed_encodings_and_forged_cursor() {
        let memory = VectorMemory::default();
        let map = StableLinearHashMap::<BadKey, u64, _>::new_with_hash_seed(memory.clone(), 151)
            .expect("new bad-key map");
        let bucket = map.candidate_buckets(&BadKey(1)).0;
        map.write_key_bytes(bucket, 0, &1u64.to_be_bytes());
        map.write_value_bytes(bucket, 0, &10u64.to_bytes());
        map.write_occupancy(bucket, 1);
        control::write_len(&memory, map.header.control_offset, 1);
        let cursor = map.scrub_snapshot().expect("snapshot");
        assert_eq!(
            map.scrub_step(cursor, u64::MAX),
            Err(ScrubError::InvalidKeyEncoding { bucket, slot: 0 })
        );

        let memory = VectorMemory::default();
        let map = StableLinearHashMap::<u64, BadValue, _>::new_with_hash_seed(memory.clone(), 157)
            .expect("new bad-value map");
        let key = 2u64;
        let bucket = map.candidate_buckets(&key).0;
        map.write_key_bytes(bucket, 0, &key.to_bytes());
        map.write_value_bytes(bucket, 0, &20u64.to_be_bytes());
        map.write_occupancy(bucket, 1);
        control::write_len(&memory, map.header.control_offset, 1);
        let cursor = map.scrub_snapshot().expect("snapshot");
        assert_eq!(
            map.scrub_step(cursor, u64::MAX),
            Err(ScrubError::InvalidValueEncoding { bucket, slot: 0 })
        );

        let map = Map::new_with_hash_seed(VectorMemory::default(), 163).expect("new");
        let mut forged = map.scrub_snapshot().expect("snapshot");
        forged.next_primary_bucket = forged.snapshot.physical_buckets + 1;
        assert_eq!(map.scrub_step(forged, 1), Err(ScrubError::InvalidCursor));
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn bounded_scrub_maps_user_decode_panic_only_on_unwind_enabled_host() {
        type CallbackMap = StableLinearHashMap<CallbackKey, CallbackValue, VectorMemory>;

        let memory = VectorMemory::default();
        let map = CallbackMap::new_with_hash_seed(memory, 165).expect("new");
        map.insert(
            CallbackKey::plain(1),
            CallbackValue {
                value: 10,
                on_encode: None,
                invalid: false,
            },
        )
        .expect("seed");
        let bucket = map.find(&CallbackKey::plain(1)).expect("resident").0;
        let cursor = map.scrub_snapshot().expect("snapshot");
        RESIDENT_DECODE_CALLBACK.with(|callback| {
            *callback.borrow_mut() = Some(Rc::new(|| panic!("host-only decode panic")));
        });

        assert_eq!(
            map.scrub_step(cursor, u64::MAX),
            Err(ScrubError::InvalidKeyEncoding { bucket, slot: 0 })
        );
        RESIDENT_DECODE_CALLBACK.with(|callback| callback.borrow_mut().take());
    }

    #[test]
    fn bounded_scrub_final_fence_supersedes_reentrant_scan_error() {
        type CallbackMap = StableLinearHashMap<CallbackKey, CallbackValue, VectorMemory>;

        let memory = VectorMemory::default();
        let map = CallbackMap::new_with_hash_seed(memory.clone(), 167).expect("new");
        map.insert(
            CallbackKey::plain(1),
            CallbackValue {
                value: 10,
                on_encode: None,
                invalid: false,
            },
        )
        .expect("seed");
        let alias = Rc::new(CallbackMap::open(memory).expect("alias"));
        let cursor = map.scrub_snapshot().expect("snapshot");
        let alias_for_callback = Rc::clone(&alias);
        let fired = Rc::new(Cell::new(false));
        let fired_for_callback = Rc::clone(&fired);
        RESIDENT_HASH_CALLBACK.with(|callback| {
            *callback.borrow_mut() = Some((
                1,
                Rc::new(move || {
                    if fired_for_callback.replace(true) {
                        return;
                    }
                    alias_for_callback
                        .insert(
                            CallbackKey::plain(2),
                            CallbackValue {
                                value: 20,
                                on_encode: None,
                                invalid: false,
                            },
                        )
                        .expect("nested alias mutation");
                }),
            ));
        });

        assert_eq!(map.scrub_step(cursor, u64::MAX), Err(ScrubError::Stale));
        RESIDENT_HASH_CALLBACK.with(|callback| callback.borrow_mut().take());
    }

    #[test]
    fn large_values_are_not_read_during_key_search() {
        type LargeValueMap = StableLinearHashMap<u64, [u8; 2048], CountingMemory>;

        let memory = CountingMemory::default();
        let map = LargeValueMap::new_with_hash_seed(memory.clone(), 73).expect("new");
        let (key, _, miss) = seed_large_second_candidate_fixture(&map);
        let header = map.header();
        let candidates = map.candidate_buckets(&key);
        let value_slabs = [candidates.0, candidates.1].map(|bucket| {
            (
                StableLinearHashMap::<u64, [u8; 2048], CountingMemory>::values_base(header, bucket),
                u64::from(BUCKET_SIZE) * u64::from(header.value_size),
            )
        });

        for (probe, expected) in [(key, true), (miss, false)] {
            reset_counts(&memory);
            assert_eq!(map.contains_key(&probe), Ok(expected));
            assert!(memory.read_ranges.borrow().iter().all(|range| {
                value_slabs
                    .iter()
                    .all(|&(start, len)| !read_overlaps(*range, start, len))
            }));
            assert_eq!(memory.write_calls.get(), 0);
        }
    }

    #[test]
    fn direct_insert_reads_one_authoritative_control_snapshot_then_one_epoch_recheck() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 73).expect("new");

        reset_counts(&memory);
        assert_eq!(map.insert(7, 70), Ok(None));
        assert_eq!(memory.read_calls.get(), 4);
        assert_eq!(memory.read_bytes.get(), 64 + 2 + 2 + 8);
        assert_eq!(memory.write_calls.get(), 6);
        assert_eq!(memory.write_bytes.get(), 8 + 8 + 8 + 2 + 8 + 8);
        assert_eq!(map.get(&7), Ok(Some(70)));
    }

    #[test]
    fn overwrite_reads_one_authoritative_control_snapshot_then_one_epoch_recheck() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 79).expect("new");
        assert_eq!(map.insert(7, 70), Ok(None));

        reset_counts(&memory);
        assert_eq!(map.insert(7, 71), Ok(Some(70)));
        assert_eq!(memory.read_calls.get(), 6);
        assert_eq!(memory.read_bytes.get(), 64 + 2 + 2 + 64 + 8 + 8);
        assert_eq!(memory.write_calls.get(), 3);
        assert_eq!(memory.write_bytes.get(), 8 + 8 + 8);
        assert_eq!(map.get(&7), Ok(Some(71)));
    }

    #[test]
    fn small_get_reads_one_fused_page_for_a_first_candidate_hit() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 79).expect("new");
        let key = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct candidates");
        assert_eq!(map.insert(key, 790), Ok(None));
        assert_eq!(
            map.find(&key).map(|entry| entry.0),
            Some(map.candidate_buckets(&key).0)
        );

        reset_counts(&memory);
        assert_eq!(map.get(&key), Ok(Some(790)));
        assert_eq!(memory.read_calls.get(), 4);
        assert_eq!(memory.read_bytes.get(), 16 + 8 + 136);
        assert_eq!(memory.write_calls.get(), 0);
        assert_eq!(memory.write_bytes.get(), 0);
    }

    #[test]
    fn small_get_reads_two_fused_pages_for_a_second_candidate_hit() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 83).expect("new");
        let (key, filler) = (1u64..)
            .find_map(|key| {
                let candidates = map.candidate_buckets(&key);
                (candidates.0 != candidates.1).then(|| {
                    (1u64..)
                        .find(|filler| {
                            *filler != key && map.candidate_buckets(filler).0 == candidates.0
                        })
                        .map(|filler| (key, filler))
                })
            })
            .flatten()
            .expect("second-candidate placement");
        let candidates = map.candidate_buckets(&key);
        assert_eq!(map.insert(filler, 831), Ok(None));
        assert_eq!(map.insert(key, 830), Ok(None));
        assert_eq!(map.find(&key).map(|entry| entry.0), Some(candidates.1));

        reset_counts(&memory);
        assert_eq!(map.get(&key), Ok(Some(830)));
        assert_eq!(memory.read_calls.get(), 5);
        assert_eq!(memory.read_bytes.get(), 16 + 8 + 2 * 136);
        assert_eq!(memory.write_calls.get(), 0);
        assert_eq!(memory.write_bytes.get(), 0);
    }

    #[test]
    fn small_get_miss_reads_both_distinct_candidate_pages() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 91).expect("new");
        let key = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct candidates");

        reset_counts(&memory);
        assert_eq!(map.get(&key), Ok(None));
        assert_eq!(memory.read_calls.get(), 5);
        assert_eq!(memory.read_bytes.get(), 16 + 8 + 2 * 136);
        assert_eq!(memory.write_calls.get(), 0);
        assert_eq!(memory.write_bytes.get(), 0);
    }

    #[test]
    fn small_remove_reads_one_fused_page_and_publishes_metadata() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 97).expect("new");
        let key = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct candidates");
        assert_eq!(map.insert(key, 970), Ok(None));
        assert_eq!(
            map.find(&key).map(|entry| entry.0),
            Some(map.candidate_buckets(&key).0)
        );

        reset_counts(&memory);
        assert_eq!(map.remove(&key), Ok(Some(970)));
        assert_eq!(memory.read_calls.get(), 3);
        assert_eq!(memory.read_bytes.get(), 64 + 136 + 8);
        assert_eq!(memory.write_calls.get(), 4);
        assert_eq!(memory.write_bytes.get(), 8 + 2 + 8 + 8);
        assert_eq!(map.len(), Ok(0));
    }

    #[test]
    fn small_remove_reads_two_fused_pages_for_a_second_candidate_hit() {
        type CountingMap = StableLinearHashMap<u64, u64, CountingMemory>;

        let memory = CountingMemory::default();
        let map = CountingMap::new_with_hash_seed(memory.clone(), 101).expect("new");
        let (key, filler) = (1u64..)
            .find_map(|key| {
                let candidates = map.candidate_buckets(&key);
                (candidates.0 != candidates.1).then(|| {
                    (1u64..)
                        .find(|filler| {
                            *filler != key && map.candidate_buckets(filler).0 == candidates.0
                        })
                        .map(|filler| (key, filler))
                })
            })
            .flatten()
            .expect("second-candidate placement");
        assert_eq!(map.insert(filler, 1_011), Ok(None));
        assert_eq!(map.insert(key, 1_010), Ok(None));
        assert_eq!(
            map.find(&key).map(|entry| entry.0),
            Some(map.candidate_buckets(&key).1)
        );

        reset_counts(&memory);
        assert_eq!(map.remove(&key), Ok(Some(1_010)));
        assert_eq!(memory.read_calls.get(), 4);
        assert_eq!(memory.read_bytes.get(), 64 + 2 * 136 + 8);
        assert_eq!(memory.write_calls.get(), 4);
        assert_eq!(memory.write_bytes.get(), 8 + 2 + 8 + 8);
        assert_eq!(map.get(&filler), Ok(Some(1_011)));
    }

    #[test]
    fn large_remove_reads_only_candidate_headers_matched_key_and_value() {
        type LargeValueMap = StableLinearHashMap<u64, [u8; 2048], CountingMemory>;

        let memory = CountingMemory::default();
        let map = LargeValueMap::new_with_hash_seed(memory.clone(), 103).expect("new");
        let (key, value, _) = seed_large_second_candidate_fixture(&map);
        let (bucket, slot, _) = map.find(&key).expect("second-candidate target");
        let candidates = map.candidate_buckets(&key);
        assert_eq!(bucket, candidates.1);
        let matched_value = map.value_offset(bucket, slot);
        let value_slabs = [candidates.0, candidates.1].map(|candidate| {
            (
                LargeValueMap::values_base(map.header, candidate),
                u64::from(BUCKET_SIZE) * u64::from(map.header.value_size),
            )
        });

        reset_counts(&memory);
        assert_eq!(map.remove(&key), Ok(Some(value)));
        let value_reads = memory
            .read_ranges
            .borrow()
            .iter()
            .copied()
            .filter(|range| {
                value_slabs
                    .iter()
                    .any(|&(start, len)| read_overlaps(*range, start, len))
            })
            .collect::<Vec<_>>();
        assert_eq!(value_reads, vec![(matched_value, 2048)]);
        assert_eq!(memory.write_calls.get(), 4);
        assert_eq!(memory.write_bytes.get(), 8 + 2 + 8 + 8);
    }

    #[test]
    fn large_get_reads_only_the_matched_key_and_value() {
        type LargeValueMap = StableLinearHashMap<u64, [u8; 2048], CountingMemory>;

        let memory = CountingMemory::default();
        let map = LargeValueMap::new_with_hash_seed(memory.clone(), 89).expect("new");
        let (key, value, miss) = seed_large_second_candidate_fixture(&map);
        let (bucket, slot, _) = map.find(&key).expect("second-candidate target");
        let candidates = map.candidate_buckets(&key);
        assert_eq!(bucket, candidates.1);
        let matched_value = map.value_offset(bucket, slot);
        let value_slabs = [candidates.0, candidates.1].map(|candidate| {
            (
                LargeValueMap::values_base(map.header, candidate),
                u64::from(BUCKET_SIZE) * u64::from(map.header.value_size),
            )
        });

        reset_counts(&memory);
        assert_eq!(map.get(&key), Ok(Some(value)));
        let value_reads = memory
            .read_ranges
            .borrow()
            .iter()
            .copied()
            .filter(|range| {
                value_slabs
                    .iter()
                    .any(|&(start, len)| read_overlaps(*range, start, len))
            })
            .collect::<Vec<_>>();
        assert_eq!(value_reads, vec![(matched_value, 2048)]);
        assert_eq!(memory.write_calls.get(), 0);

        reset_counts(&memory);
        assert_eq!(map.get(&miss), Ok(None));
        assert!(memory.read_ranges.borrow().iter().all(|range| {
            value_slabs
                .iter()
                .all(|&(start, len)| !read_overlaps(*range, start, len))
        }));
        assert_eq!(memory.write_calls.get(), 0);
    }

    #[test]
    fn reopen_rejects_wrong_type_magic_and_version() {
        let typed_memory = VectorMemory::default();
        Map::new(typed_memory.clone()).expect("new typed fixture");
        assert!(matches!(
            StableLinearHashMap::<u32, u64, _>::init(typed_memory),
            Err(InitError::IncompatibleElementType)
        ));

        let magic_memory = VectorMemory::default();
        Map::new(magic_memory.clone()).expect("new magic fixture");
        magic_memory.write(0, b"BAD");
        assert!(matches!(
            Map::init(magic_memory),
            Err(InitError::BadMagic { .. })
        ));

        let version_memory = VectorMemory::default();
        Map::new(version_memory.clone()).expect("new version fixture");
        version_memory.write(3, &[2]);
        assert!(matches!(
            Map::init(version_memory),
            Err(InitError::IncompatibleVersion(2))
        ));
    }

    #[test]
    fn remove_then_reinsert_reuses_bucket_slot() {
        let map = Map::new(VectorMemory::default()).expect("new");
        map.insert(9, 90).expect("insert");
        assert_eq!(map.remove(&9), Ok(Some(90)));
        assert_eq!(map.insert(9, 91), Ok(None));
        assert_eq!(map.get(&9), Ok(Some(91)));
        assert_eq!(map.len(), Ok(1));
    }
}
