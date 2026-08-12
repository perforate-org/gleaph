use crate::StableHashKey;
use crate::control;
use crate::header::{self, CONTROL_BYTES, ControlRegion, HEADER_SIZE, Header, InitError};
use crate::memory::{GrowError, grow_to_bytes};
use ic_stable_structures::{Memory, Storable};
use rapidhash::v3::{RapidSecrets, rapidhash_v3_inline};
use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;
use std::marker::PhantomData;

pub const BUCKET_SIZE: u32 = 8;
const INITIAL_LEVEL: u8 = 3;
const INITIAL_BUCKETS: u64 = 1 << INITIAL_LEVEL;
const BUCKET_HEADER_BYTES: u64 = 8;
const BULK_SCAN_MAX_BYTES: u64 = 1024;
const DEFAULT_HASH_SEED: u64 = 0x243f_6a88_85a3_08d3;
const HASH_DOMAIN_0: u64 = 0x1319_8a2e_0370_7344;
const HASH_DOMAIN_1: u64 = 0xa409_3822_299f_31d0;

/// An operation could not run because a mutation is in progress, a seed change is invalid, or a
/// new key has no free candidate slot.
#[derive(Debug, PartialEq, Eq)]
pub enum MutationError {
    /// Both candidate buckets are full.
    TablePressure,
    /// A hash-seed change was attempted after the first entry was inserted.
    HashSeedNonEmpty,
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

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TablePressure => write!(f, "both candidate buckets are full"),
            Self::HashSeedNonEmpty => write!(f, "hash seed can only change while the map is empty"),
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

/// Fixed-geometry two-choice stable-memory map.
///
/// Calls that read or mutate live state return [`MutationError::InProgress`] when another handle
/// has an incomplete mutation. Mutation uses a persisted odd/even epoch so an alias cannot start
/// nested mutation and a read detects a completed nested mutation before it returns a snapshot.
/// An odd epoch on reopen fails closed until a future journal/recovery design owns recovery.
pub struct StableLinearHashMap<K: StableHashKey, V: Storable, M: Memory> {
    memory: M,
    header: Header,
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
struct MutationGuard<'a, K: StableHashKey, V: Storable, M: Memory> {
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

struct PreparedEntry {
    bucket: u64,
    slot: u32,
    occupancy: u8,
    key: Vec<u8>,
    value: Vec<u8>,
}

impl<K: StableHashKey, V: Storable, M: Memory> MutationGuard<'_, K, V, M> {
    fn finish(self) {
        control::write_mutation_epoch(
            &self.map.memory,
            self.map.header.control_offset,
            self.completed_epoch,
        );
    }
}

impl<K: StableHashKey, V: Storable, M: Memory> StableLinearHashMap<K, V, M> {
    pub fn new(memory: M) -> Result<Self, InitError> {
        Self::new_with_hash_seed(memory, DEFAULT_HASH_SEED)
    }

    pub fn new_with_hash_seed(memory: M, hash_seed: u64) -> Result<Self, InitError> {
        let header = Self::expected_header()?;
        let end = header
            .buckets_offset
            .checked_add(
                INITIAL_BUCKETS
                    .checked_mul(header.bucket_page_stride)
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        grow_to_bytes(&memory, end).map_err(|_| InitError::OutOfMemory)?;
        header::write(&memory, header);
        control::write(
            &memory,
            header.control_offset,
            ControlRegion {
                len: 0,
                level: INITIAL_LEVEL,
                split_cursor: 0,
                physical_buckets: INITIAL_BUCKETS,
                hash_seed,
                split_state: 0,
                split_work_cursor: 0,
                journal_state: 0,
                mutation_epoch: control::INITIAL_MUTATION_EPOCH,
                hash_encoding_id: K::HASH_ENCODING_ID,
            },
        );
        let journal_bytes = header.journal_bytes().ok_or(InitError::InvalidLayout)?;
        memory.write(header.journal_offset, &vec![0; journal_bytes as usize]);
        for bucket in 0..INITIAL_BUCKETS {
            memory.write(
                Self::bucket_offset(header, bucket),
                &[0; BUCKET_HEADER_BYTES as usize],
            );
        }
        Ok(Self {
            memory,
            header,
            hash_secrets: RefCell::new(CachedHashSecrets {
                seed: hash_seed,
                secrets: hash_secrets(hash_seed),
            }),
            _marker: PhantomData,
        })
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        Self::init_with_hash_seed(memory, DEFAULT_HASH_SEED)
    }

    pub fn init_with_hash_seed(memory: M, hash_seed: u64) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new_with_hash_seed(memory, hash_seed);
        }
        let header = header::read(&memory)?;
        let expected = Self::expected_header()?;
        if header.key_size != expected.key_size || header.value_size != expected.value_size {
            return Err(InitError::IncompatibleElementType);
        }
        if header != expected {
            return Err(InitError::InvalidLayout);
        }
        let control = control::read(&memory, header.control_offset);
        Self::validate_control(control)?;
        if control.hash_encoding_id != K::HASH_ENCODING_ID {
            return Err(InitError::IncompatibleHashEncoding);
        }
        let end = header
            .buckets_offset
            .checked_add(
                control
                    .physical_buckets
                    .checked_mul(header.bucket_page_stride)
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        let allocated = memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .ok_or(InitError::InvalidLayout)?;
        if allocated < end {
            return Err(InitError::InvalidLayout);
        }
        let mut occupied = 0u64;
        for bucket in 0..control.physical_buckets {
            occupied += Self::occupancy_byte(&memory, header, bucket).count_ones() as u64;
        }
        if occupied != control.len {
            return Err(InitError::InvalidLayout);
        }
        Self::validate_bucket_headers(&memory, header, control.physical_buckets)?;
        Ok(Self {
            memory,
            header,
            hash_secrets: RefCell::new(CachedHashSecrets {
                seed: control.hash_seed,
                secrets: hash_secrets(control.hash_seed),
            }),
            _marker: PhantomData,
        })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    pub fn control_region(&self) -> Result<ControlRegion, MutationError> {
        self.read_consistent(|| control::read(&self.memory, self.header.control_offset))
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
        self.read_consistent(|| control::read_hash_seed(&self.memory, self.header.control_offset))
    }

    pub fn set_hash_seed(&self, hash_seed: u64) -> Result<(), MutationError> {
        let mutation = self.begin_mutation()?;
        if control::read_len(&self.memory, self.header.control_offset) != 0 {
            mutation.finish();
            return Err(MutationError::HashSeedNonEmpty);
        }
        control::write_hash_seed(&self.memory, self.header.control_offset, hash_seed);
        *self.hash_secrets.borrow_mut() = CachedHashSecrets {
            seed: hash_seed,
            secrets: hash_secrets(hash_seed),
        };
        mutation.finish();
        Ok(())
    }

    pub fn get(&self, key: &K) -> Result<Option<V>, MutationError> {
        self.read_consistent(|| {
            let hot = control::read_hot(&self.memory, self.header.control_offset);
            self.get_with_hot(key, hot)
        })
    }

    pub fn contains_key(&self, key: &K) -> Result<bool, MutationError> {
        self.read_consistent(|| {
            let hot = control::read_hot(&self.memory, self.header.control_offset);
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

    fn plan_insert(
        &self,
        key: &K,
        hash_bytes: &[u8],
        key_bytes: Vec<u8>,
        value_bytes: Vec<u8>,
    ) -> Result<InsertPlan<V>, MutationError> {
        let control = control::read(&self.memory, self.header.control_offset);
        if !control.mutation_epoch.is_multiple_of(2) {
            return Err(MutationError::InProgress);
        }
        let observed_epoch = control.mutation_epoch;
        let planned = (|| {
            let candidates =
                self.candidate_buckets_for_bytes_at(hash_bytes, control.hash_seed, control);
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
                let (bucket, slot, occupancy) = Self::choose_placement(candidates, occupancies)
                    .ok_or(MutationError::TablePressure)?;
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

            self.plan_split_insert(observed_epoch, control, hash_bytes, key_bytes, value_bytes)
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
            Self::bucket_offset(self.header, source_bucket),
            &mut source_page,
        );
        let source_occupancy = Self::page_occupancy(&source_page);
        let mut new_page = vec![0; page_bytes];
        let secrets = self.secrets_for_seed(control.hash_seed);
        let mut retained = source_occupancy;
        let mut new_occupancy = 0u8;
        for slot in 0..BUCKET_SIZE {
            if source_occupancy & (1 << slot) == 0 {
                continue;
            }
            let entry = Self::page_entry_offset(self.header, slot);
            let key_end = entry + self.header.key_size as usize;
            let stored_key = K::from_bytes(Cow::Borrowed(&source_page[entry..key_end]));
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
            let destination = Self::page_entry_offset(self.header, destination_slot);
            let end = destination + self.header.entry_stride as usize;
            new_page[destination..end]
                .copy_from_slice(&source_page[entry..entry + self.header.entry_stride as usize]);
            retained &= !(1 << slot);
            new_occupancy |= 1 << destination_slot;
        }
        Self::set_page_occupancy(&mut source_page, retained);
        Self::set_page_occupancy(&mut new_page, new_occupancy);

        let candidates = self.candidate_buckets_for_bytes_at(
            hash_bytes,
            control.hash_seed,
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
                self.memory.write(
                    Self::bucket_offset(self.header, source_bucket),
                    &source_page,
                );
                self.memory
                    .write(Self::bucket_offset(self.header, new_bucket), &new_page);
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
        let hash_input = key.stable_hash_bytes();
        let hash_bytes = hash_input.as_ref();
        let mutation = self.begin_mutation()?;
        let hot = control::read_hot(&self.memory, self.header.control_offset);
        let candidates = self.candidate_buckets_for_bytes_with_hot(hash_bytes, hot);
        let Some((bucket, slot, occupancy, previous)) =
            self.find_value_in_candidates(candidates, key)
        else {
            mutation.finish();
            return Ok(None);
        };
        self.write_occupancy(bucket, occupancy & !(1 << slot));
        let len = hot.len - 1;
        control::write_len(&self.memory, self.header.control_offset, len);
        mutation.finish();
        Ok(Some(previous))
    }

    fn expected_header() -> Result<Header, InitError> {
        if !K::BOUND.is_fixed_size() || !V::BOUND.is_fixed_size() {
            return Err(InitError::IncompatibleElementType);
        }
        let key_size = K::BOUND.max_size();
        let value_size = V::BOUND.max_size();
        let entry_stride = u64::from(key_size)
            .checked_add(u64::from(value_size))
            .ok_or(InitError::InvalidLayout)?;
        let control_offset = HEADER_SIZE;
        let journal_offset = control_offset
            .checked_add(CONTROL_BYTES)
            .ok_or(InitError::InvalidLayout)?;
        let buckets_offset = journal_offset
            .checked_add(
                8u64.checked_add(entry_stride)
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        let bucket_page_stride = BUCKET_HEADER_BYTES
            .checked_add(
                u64::from(BUCKET_SIZE)
                    .checked_mul(entry_stride)
                    .ok_or(InitError::InvalidLayout)?,
            )
            .ok_or(InitError::InvalidLayout)?;
        Ok(Header {
            key_size,
            value_size,
            bucket_size: BUCKET_SIZE,
            control_offset,
            control_bytes: CONTROL_BYTES,
            journal_offset,
            buckets_offset,
            entry_stride,
            bucket_page_stride,
        })
    }

    fn validate_control(control: ControlRegion) -> Result<(), InitError> {
        if control.split_state != 0
            || control.journal_state != 0
            || !control.mutation_epoch.is_multiple_of(2)
        {
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
            || control.split_work_cursor != 0
        {
            return Err(InitError::InvalidLayout);
        }
        Ok(())
    }

    #[cfg(test)]
    fn candidate_buckets(&self, key: &K) -> (u64, u64) {
        let hot = control::read_hot(&self.memory, self.header.control_offset);
        let bytes = key.stable_hash_bytes();
        self.candidate_buckets_for_bytes_with_hot(bytes.as_ref(), hot)
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

    fn candidate_buckets_for_bytes_with_hot(
        &self,
        bytes: &[u8],
        hot: control::HotControl,
    ) -> (u64, u64) {
        Self::candidate_buckets_from_bytes_at(
            bytes,
            &self.secrets_for_seed(hot.hash_seed),
            hot.level,
            hot.split_cursor,
        )
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
        let hot = control::read_hot(&self.memory, self.header.control_offset);
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
        let bucket_entries_bytes = u64::from(BUCKET_SIZE) * self.header.entry_stride;
        if bucket_entries_bytes <= BULK_SCAN_MAX_BYTES {
            return self.find_in_small_candidates(candidates, key, bucket_entries_bytes as usize);
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

    fn begin_mutation(&self) -> Result<MutationGuard<'_, K, V, M>, MutationError> {
        let epoch = self.idle_epoch()?;
        let completed = epoch.checked_add(2).ok_or(MutationError::EpochExhausted)?;
        control::write_mutation_epoch(&self.memory, self.header.control_offset, epoch + 1);
        Ok(MutationGuard {
            map: self,
            completed_epoch: completed,
        })
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

    fn page_entry_offset(header: Header, slot: u32) -> usize {
        BUCKET_HEADER_BYTES as usize + slot as usize * header.entry_stride as usize
    }

    fn write_page_entry(page: &mut [u8], header: Header, slot: u32, key: &[u8], value: &[u8]) {
        let entry = Self::page_entry_offset(header, slot);
        let key_end = entry + header.key_size as usize;
        page[entry..key_end].copy_from_slice(key);
        page[key_end..key_end + header.value_size as usize].copy_from_slice(value);
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
        bucket_entries_bytes: usize,
    ) -> Option<(u64, u32, u8, V)> {
        let page_bytes = BUCKET_HEADER_BYTES as usize + bucket_entries_bytes;
        let mut page = read_exact_to_vec_uninit(
            &self.memory,
            Self::bucket_offset(self.header, candidates.0),
            page_bytes,
        );
        self.find_in_small_page(key, &page)
            .map(|(slot, occupancy, value)| (candidates.0, slot, occupancy, value))
            .or_else(|| {
                (candidates.1 != candidates.0)
                    .then(|| {
                        self.memory
                            .read(Self::bucket_offset(self.header, candidates.1), &mut page);
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
            let entry =
                BUCKET_HEADER_BYTES as usize + slot as usize * self.header.entry_stride as usize;
            let key_end = entry + self.header.key_size as usize;
            if K::from_bytes(Cow::Borrowed(&page[entry..key_end])) == *key {
                let value_end = key_end + self.header.value_size as usize;
                return Some((
                    slot,
                    occupancy,
                    V::from_bytes(Cow::Borrowed(&page[key_end..value_end])),
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
        let bucket_bytes = u64::from(BUCKET_SIZE) * self.header.entry_stride;
        let mut buffer = vec![
            0;
            if bucket_bytes <= BULK_SCAN_MAX_BYTES {
                bucket_bytes
            } else {
                u64::from(self.header.key_size)
            } as usize
        ];
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

    fn find_in_bucket(
        &self,
        bucket: u64,
        occupancy: u8,
        key: &K,
        entries: &mut [u8],
    ) -> Option<u32> {
        if occupancy == 0 {
            return None;
        }
        let bucket_entries = Self::bucket_offset(self.header, bucket) + BUCKET_HEADER_BYTES;
        let bulk_scan = entries.len() > self.header.key_size as usize;
        if bulk_scan {
            self.memory.read(bucket_entries, entries);
        }
        let mut occupied = occupancy;
        while occupied != 0 {
            let slot = occupied.trailing_zeros();
            let key_bytes = if bulk_scan {
                let start = slot as usize * self.header.entry_stride as usize;
                &entries[start..start + self.header.key_size as usize]
            } else {
                self.memory.read(
                    bucket_entries + u64::from(slot) * self.header.entry_stride,
                    &mut *entries,
                );
                &*entries
            };
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
            Self::bucket_offset(self.header, bucket),
            &u16::from(occupancy).to_le_bytes(),
        );
    }

    fn occupancy_byte(memory: &M, header: Header, bucket: u64) -> u8 {
        let mut occupancy = [0; 2];
        memory.read(Self::bucket_offset(header, bucket), &mut occupancy);
        u16::from_le_bytes(occupancy) as u8
    }

    fn validate_bucket_headers(
        memory: &M,
        header: Header,
        physical_buckets: u64,
    ) -> Result<(), InitError> {
        for bucket in 0..physical_buckets {
            let mut bucket_header = [0; BUCKET_HEADER_BYTES as usize];
            memory.read(Self::bucket_offset(header, bucket), &mut bucket_header);
            let occupancy = u16::from_le_bytes([bucket_header[0], bucket_header[1]]);
            if occupancy & !0x00ff != 0 || bucket_header[2..].iter().any(|byte| *byte != 0) {
                return Err(InitError::InvalidLayout);
            }
        }
        Ok(())
    }

    fn bucket_offset(header: Header, bucket: u64) -> u64 {
        header.buckets_offset + bucket * header.bucket_page_stride
    }

    fn entry_offset(&self, bucket: u64, slot: u32) -> u64 {
        Self::bucket_offset(self.header, bucket)
            + BUCKET_HEADER_BYTES
            + u64::from(slot) * self.header.entry_stride
    }

    fn read_value(&self, bucket: u64, slot: u32) -> V {
        read_storable(
            &self.memory,
            self.entry_offset(bucket, slot) + u64::from(self.header.key_size),
            self.header.value_size,
        )
    }

    fn write_key_bytes(&self, bucket: u64, slot: u32, key: &[u8]) {
        self.memory.write(self.entry_offset(bucket, slot), key);
    }

    fn write_value_bytes(&self, bucket: u64, slot: u32, value: &[u8]) {
        self.memory.write(
            self.entry_offset(bucket, slot) + u64::from(self.header.key_size),
            value,
        );
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

    impl<M: Memory> StableLinearHashMap<u64, u64, M> {
        pub(crate) fn probe_candidates(&self, key: u64) -> (u64, u64) {
            let hot = control::read_hot(&self.memory, self.header.control_offset);
            self.candidate_buckets_for_key_at(&key, hot.hash_seed, hot.level, hot.split_cursor)
        }

        pub(crate) fn probe_bucket_occupancy(&self, bucket: u64) -> u8 {
            Self::occupancy_byte(&self.memory, self.header, bucket)
        }

        pub(crate) fn probe_resident_bucket(&self, key: u64) -> u64 {
            self.find_with_hot(
                &key,
                control::read_hot(&self.memory, self.header.control_offset),
            )
            .expect("benchmark resident exists")
            .0
        }

        pub(crate) fn probe_seed(&self) -> u64 {
            control::read_hash_seed(&self.memory, self.header.control_offset)
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
            let bucket_entries_bytes = u64::from(BUCKET_SIZE) * self.header.entry_stride;
            self.find_in_small_candidates(route.candidates, &key, bucket_entries_bytes as usize)
                .map(|(_, _, _, value)| value)
        }

        pub(crate) fn probe_insert_control_route_lookup(&self, key: u64) -> Mutation {
            let hot = control::read_hot(&self.memory, self.header.control_offset);
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
                len: hot.len,
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
            let mut bytes = [0; 16];
            self.memory.read(
                self.entry_offset(mutation.bucket, mutation.slot),
                &mut bytes,
            );
            bytes[..8] == key.to_be_bytes() && bytes[8..] == value.to_be_bytes()
        }

        pub(crate) fn probe_insert_metadata_publish(&self, mutation: Mutation) {
            self.write_occupancy(mutation.bucket, mutation.occupancy | (1 << mutation.slot));
            control::write_len(&self.memory, self.header.control_offset, mutation.len + 1);
        }

        pub(crate) fn probe_remove_control_route_bucket_value(&self, key: u64) -> (Mutation, u64) {
            let hot = control::read_hot(&self.memory, self.header.control_offset);
            let (bucket, slot, occupancy, value) = self
                .find_value_with_hot(&key, hot)
                .expect("diagnostic fixture contains key");
            (
                Mutation {
                    bucket,
                    slot,
                    occupancy,
                    len: hot.len,
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

    type Map = StableLinearHashMap<u64, u64, VectorMemory>;
    type GrowCallback = Rc<dyn Fn()>;

    #[derive(Clone, Default)]
    struct CountingMemory {
        inner: VectorMemory,
        read_calls: Rc<Cell<u64>>,
        read_bytes: Rc<Cell<u64>>,
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
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RoutingKey(u64);

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
        const HASH_ENCODING_ID: u64 = 0x4c48_4d00_0000_1001;
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
        const HASH_ENCODING_ID: u64 = 0x4c48_4d00_0000_1002;
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

    #[derive(Clone)]
    struct CallbackKey {
        value: u64,
        on_hash: Option<Rc<dyn Fn()>>,
        on_eq: Option<Rc<dyn Fn()>>,
    }

    impl CallbackKey {
        fn plain(value: u64) -> Self {
            Self {
                value,
                on_hash: None,
                on_eq: None,
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
            Cow::Owned(self.value.to_be_bytes().to_vec())
        }

        fn into_bytes(self) -> Vec<u8> {
            self.value.to_be_bytes().to_vec()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
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
        const HASH_ENCODING_ID: u64 = 0x4c48_4d00_0000_1003;
        type HashBytes<'a>
            = [u8; 8]
        where
            Self: 'a;

        fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
            if let Some(callback) = &self.on_hash {
                callback();
            }
            self.value.to_be_bytes()
        }
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
            if offset == 64 + 48
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
        }
    }

    fn reset_counts(memory: &CountingMemory) {
        memory.read_calls.set(0);
        memory.read_bytes.set(0);
        memory.write_calls.set(0);
        memory.write_bytes.set(0);
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
        map.memory.write(map.header.control_offset + 8, &[level]);
        map.memory
            .write(map.header.control_offset + 16, &split_cursor.to_le_bytes());
        map.memory.write(
            map.header.control_offset + 24,
            &physical_buckets.to_le_bytes(),
        );
        let seed = control::read_hash_seed(&map.memory, map.header.control_offset);
        let secrets = hash_secrets(seed);
        let (next_level, next_cursor, _) = StableLinearHashMap::<u64, u64, M>::next_geometry(
            control::read(&map.memory, map.header.control_offset),
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

    #[test]
    fn exact_layout_and_idle_control_are_persisted() {
        let memory = VectorMemory::default();
        let map = Map::new_with_hash_seed(memory.clone(), 11).expect("new");
        assert_eq!(
            map.header(),
            Header {
                key_size: 8,
                value_size: 8,
                bucket_size: 8,
                control_offset: 64,
                control_bytes: 64,
                journal_offset: 128,
                buckets_offset: 152,
                entry_stride: 16,
                bucket_page_stride: 136,
            }
        );

        let expected_header = [
            b'L', b'H', b'M', 1, 8, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 64,
            0, 0, 0, 0, 0, 0, 0, 128, 0, 0, 0, 0, 0, 0, 0, 152, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0,
            0, 0, 0, 0, 136, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(read_bytes(&memory, 0, 64), expected_header);

        let mut expected_control = [0; 64];
        expected_control[8] = 3;
        expected_control[24..32].copy_from_slice(&8u64.to_le_bytes());
        expected_control[32..40].copy_from_slice(&11u64.to_le_bytes());
        expected_control[56..64].copy_from_slice(&u64::HASH_ENCODING_ID.to_le_bytes());
        assert_eq!(read_bytes(&memory, 64, 64), expected_control);
        assert_eq!(read_bytes(&memory, 128, 24), vec![0; 24]);
        assert_eq!(read_bytes(&memory, 152, 8 * 136), vec![0; 8 * 136]);
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
            read_bytes(&memory, map.entry_offset(bucket, slot), 8),
            key.0.to_le_bytes()
        );
        drop(map);

        let reopened = RoutingMap::init(memory).expect("reopen routing map");
        assert_eq!(reopened.get(&key), Ok(Some(41)));
    }

    #[test]
    fn hash_encoding_id_is_persisted_and_reopen_rejects_a_different_key_contract() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new");
        assert_eq!(
            map.control_region().expect("idle control").hash_encoding_id,
            u64::HASH_ENCODING_ID
        );
        drop(map);

        assert!(matches!(
            StableLinearHashMap::<RoutingKey, u64, _>::init(memory),
            Err(InitError::IncompatibleHashEncoding)
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
        assert_eq!(map.set_hash_seed(13), Err(MutationError::InProgress));
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
            on_hash: None,
            on_eq: Some(callback),
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
            on_hash: None,
            on_eq: Some(callback),
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
            on_hash: None,
            on_eq: Some(callback),
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
            on_hash: Some(callback),
            on_eq: None,
        };

        assert_eq!(reader.get(&reentrant), Err(MutationError::InProgress));
        assert_eq!(reader.get(&CallbackKey::plain(7)), Ok(Some(70)));
        assert_eq!(reader.get(&CallbackKey::plain(8)), Ok(Some(80)));
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
    fn hash_seed_changes_only_while_empty() {
        let memory = VectorMemory::default();
        let map = Map::new(memory.clone()).expect("new");
        let header_before = read_bytes(&memory, 0, 64);
        let control_before = read_bytes(&memory, 64, 64);
        let journal_before = read_bytes(&memory, 128, 24);
        let buckets_before = read_bytes(&memory, 152, 8 * 136);

        map.set_hash_seed(51).expect("empty seed change");
        assert_eq!(map.hash_seed(), Ok(51));
        assert_eq!(read_bytes(&memory, 0, 64), header_before);
        assert_eq!(read_bytes(&memory, 128, 24), journal_before);
        assert_eq!(read_bytes(&memory, 152, 8 * 136), buckets_before);
        let mut expected_control = control_before;
        expected_control[32..40].copy_from_slice(&51u64.to_le_bytes());
        expected_control[48..56].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(read_bytes(&memory, 64, 64), expected_control);

        drop(map);
        let map = Map::init(memory).expect("reopen changed seed");
        assert_eq!(map.hash_seed(), Ok(51));
        map.insert(1, 10).expect("insert");
        assert_eq!(map.set_hash_seed(53), Err(MutationError::HashSeedNonEmpty));
        assert_eq!(map.hash_seed(), Ok(51));
        assert_eq!(map.get(&1), Ok(Some(10)));
    }

    #[test]
    fn cloned_memory_handles_share_canonical_len_and_nonempty_seed_guard() {
        let memory = VectorMemory::default();
        let first = Map::new_with_hash_seed(memory.clone(), 101).expect("new first handle");
        let second = Map::init(memory).expect("open second handle");

        assert_eq!(first.insert(7, 70), Ok(None));
        assert_eq!(second.len(), Ok(1));
        assert_eq!(
            second.set_hash_seed(103),
            Err(MutationError::HashSeedNonEmpty)
        );
        assert_eq!(first.get(&7), Ok(Some(70)));
        assert_eq!(second.get(&7), Ok(Some(70)));
    }

    #[test]
    fn cloned_memory_handle_refreshes_seed_for_routing_and_reopen() {
        let old_seed = 107;
        let new_seed = 109;
        let memory = VectorMemory::default();
        let first = Map::new_with_hash_seed(memory.clone(), old_seed).expect("new first handle");
        let second = Map::init(memory.clone()).expect("open second handle");
        first
            .set_hash_seed(new_seed)
            .expect("change seed while empty");

        let old_secrets = hash_secrets(old_seed);
        let new_secrets = hash_secrets(new_seed);
        let (key, new_candidates) = (0u64..)
            .find_map(|key| {
                let bytes = key.to_bytes();
                let old = Map::candidate_buckets_from_bytes(bytes.as_ref(), &old_secrets);
                let new = Map::candidate_buckets_from_bytes(bytes.as_ref(), &new_secrets);
                (old.0 != new.0 && old.0 != new.1 && old.1 != new.0 && old.1 != new.1)
                    .then_some((key, new))
            })
            .expect("key with disjoint old and new candidates");

        assert_eq!(second.insert(key, 110), Ok(None));
        let actual_bucket = second.find(&key).expect("find through refreshed seed").0;
        assert!(actual_bucket == new_candidates.0 || actual_bucket == new_candidates.1);
        assert_eq!(first.get(&key), Ok(Some(110)));
        drop(first);
        drop(second);

        let reopened = Map::init(memory).expect("reopen new seed routing");
        assert_eq!(reopened.hash_seed(), Ok(new_seed));
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
    fn post_split_pressure_rejects_before_grow_or_epoch_change() {
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
        let bytes_before = allocated_bytes(&memory);
        let size_before = memory.size();
        assert_eq!(
            map.insert(target, 30_101),
            Err(MutationError::TablePressure)
        );
        assert_eq!(memory.size(), size_before);
        assert_eq!(allocated_bytes(&memory), bytes_before);
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

        for fail_write in 2..=11 {
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
    fn non_idle_split_control_and_journal_fail_closed() {
        let split_memory = VectorMemory::default();
        let split = Map::new(split_memory.clone()).expect("new split fixture");
        split_memory.write(split.header.control_offset + 9, &[1]);
        assert!(matches!(
            Map::init(split_memory),
            Err(InitError::RecoveryRequired)
        ));

        let journal_memory = VectorMemory::default();
        let journal = Map::new(journal_memory.clone()).expect("new journal fixture");
        journal_memory.write(journal.header.control_offset + 10, &[1]);
        assert!(matches!(
            Map::init(journal_memory),
            Err(InitError::RecoveryRequired)
        ));
    }

    #[test]
    fn reopen_rejects_invalid_occupancy_and_len() {
        let occupancy_memory = VectorMemory::default();
        let occupancy = Map::new(occupancy_memory.clone()).expect("new occupancy fixture");
        let middle_bucket =
            occupancy.header.buckets_offset + 3 * occupancy.header.bucket_page_stride;
        occupancy_memory.write(middle_bucket, &0x0100u16.to_le_bytes());
        assert!(matches!(
            Map::init(occupancy_memory),
            Err(InitError::InvalidLayout)
        ));

        let reserved_memory = VectorMemory::default();
        let reserved = Map::new(reserved_memory.clone()).expect("new reserved fixture");
        let last_bucket = reserved.header.buckets_offset + 7 * reserved.header.bucket_page_stride;
        reserved_memory.write(last_bucket + 2, &[1]);
        assert!(matches!(
            Map::init(reserved_memory),
            Err(InitError::InvalidLayout)
        ));

        let len_memory = VectorMemory::default();
        let len_map = Map::new(len_memory.clone()).expect("new len fixture");
        control::write_len(&len_memory, len_map.header.control_offset, 1);
        assert!(matches!(
            Map::init(len_memory),
            Err(InitError::InvalidLayout)
        ));
    }

    #[test]
    fn reopen_accepts_midround_geometry_and_rejects_invalid_geometry() {
        let smaller_memory = VectorMemory::default();
        let smaller = Map::new(smaller_memory.clone()).expect("new smaller fixture");
        smaller_memory.write(smaller.header.control_offset + 8, &[2]);
        smaller_memory.write(smaller.header.control_offset + 24, &4u64.to_le_bytes());
        assert!(matches!(
            Map::init(smaller_memory),
            Err(InitError::InvalidLayout)
        ));

        let split_memory = VectorMemory::default();
        let split = Map::new(split_memory.clone()).expect("new split fixture");
        split_memory.write(split.header.control_offset + 16, &1u64.to_le_bytes());
        split_memory.write(split.header.control_offset + 24, &9u64.to_le_bytes());
        let reopened = Map::init(split_memory).expect("reopen mid-round geometry");
        let control = reopened.control_region().expect("idle mid-round control");
        assert_eq!(control.level, 3);
        assert_eq!(control.split_cursor, 1);
        assert_eq!(control.physical_buckets, 9);
        assert_eq!(control.len, 0);

        let invalid_cursor_memory = VectorMemory::default();
        let invalid_cursor = Map::new(invalid_cursor_memory.clone()).expect("new cursor fixture");
        invalid_cursor_memory.write(
            invalid_cursor.header.control_offset + 16,
            &8u64.to_le_bytes(),
        );
        invalid_cursor_memory.write(
            invalid_cursor.header.control_offset + 24,
            &16u64.to_le_bytes(),
        );
        assert!(matches!(
            Map::init(invalid_cursor_memory),
            Err(InitError::InvalidLayout)
        ));

        let mismatch_memory = VectorMemory::default();
        let mismatch = Map::new(mismatch_memory.clone()).expect("new mismatch fixture");
        mismatch_memory.write(mismatch.header.control_offset + 16, &1u64.to_le_bytes());
        assert!(matches!(
            Map::init(mismatch_memory),
            Err(InitError::InvalidLayout)
        ));

        let high_memory = VectorMemory::default();
        let high = Map::new(high_memory.clone()).expect("new high fixture");
        high_memory.write(high.header.control_offset + 8, &[63]);
        assert!(matches!(
            Map::init(high_memory),
            Err(InitError::InvalidLayout)
        ));

        let rollover_memory = VectorMemory::default();
        let rollover = Map::new(rollover_memory.clone()).expect("new rollover fixture");
        rollover_memory.write(rollover.header.control_offset + 8, &[4]);
        rollover_memory.write(rollover.header.control_offset + 24, &16u64.to_le_bytes());
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
        extent_memory.write(extent.header.control_offset + 8, &[9]);
        extent_memory.write(extent.header.control_offset + 24, &512u64.to_le_bytes());
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
    fn large_values_are_not_read_during_key_search() {
        type LargeValueMap = StableLinearHashMap<u64, [u8; 2048], CountingMemory>;

        let memory = CountingMemory::default();
        let map = LargeValueMap::new_with_hash_seed(memory.clone(), 73).expect("new");
        let key = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct candidates");
        assert_eq!(map.insert(key, [7; 2048]), Ok(None));

        memory.read_bytes.set(0);
        assert_eq!(map.contains_key(&key), Ok(true));
        assert_eq!(memory.read_bytes.get(), 68);
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
        assert_eq!(memory.read_bytes.get(), 64 + 2 + 2 + 128 + 8 + 8);
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
        assert_eq!(memory.read_bytes.get(), 16 + 40 + 136);
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
        assert_eq!(memory.read_bytes.get(), 16 + 40 + 2 * 136);
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
        assert_eq!(memory.read_bytes.get(), 16 + 40 + 2 * 136);
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
        assert_eq!(memory.read_bytes.get(), 8 + 40 + 136);
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
        assert_eq!(memory.read_bytes.get(), 8 + 40 + 2 * 136);
        assert_eq!(memory.write_calls.get(), 4);
        assert_eq!(memory.write_bytes.get(), 8 + 2 + 8 + 8);
        assert_eq!(map.get(&filler), Ok(Some(1_011)));
    }

    #[test]
    fn large_remove_reads_only_candidate_headers_matched_key_and_value() {
        type LargeValueMap = StableLinearHashMap<u64, [u8; 2048], CountingMemory>;

        let memory = CountingMemory::default();
        let map = LargeValueMap::new_with_hash_seed(memory.clone(), 103).expect("new");
        let key = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct candidates");
        assert_eq!(map.insert(key, [3; 2048]), Ok(None));

        reset_counts(&memory);
        assert_eq!(map.remove(&key), Ok(Some([3; 2048])));
        assert_eq!(memory.read_calls.get(), 6);
        assert_eq!(memory.read_bytes.get(), 8 + 40 + 2 + 2 + 8 + 2048);
        assert_eq!(memory.write_calls.get(), 4);
        assert_eq!(memory.write_bytes.get(), 8 + 2 + 8 + 8);
    }

    #[test]
    fn large_get_reads_only_the_matched_key_and_value() {
        type LargeValueMap = StableLinearHashMap<u64, [u8; 2048], CountingMemory>;

        let memory = CountingMemory::default();
        let map = LargeValueMap::new_with_hash_seed(memory.clone(), 89).expect("new");
        let key = (1u64..)
            .find(|key| {
                let candidates = map.candidate_buckets(key);
                candidates.0 != candidates.1
            })
            .expect("distinct candidates");
        assert_eq!(map.insert(key, [9; 2048]), Ok(None));

        memory.read_bytes.set(0);
        assert_eq!(map.get(&key), Ok(Some([9; 2048])));
        assert_eq!(memory.read_bytes.get(), 16 + 40 + 2 + 2 + 8 + 2048);
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
