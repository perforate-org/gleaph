//! Isolated ADR 0048 mate persistence fixtures.
//!
//! This module is a storage/codec foundation only. It is deliberately not connected to
//! canonical adjacency, mutation maintenance, candidate selection, or ordinary reads.
//!
//! The production-shaped boundary is [`MateLocatorRecord`], [`MateLocatorStore`], and
//! [`MatePayloadRegion`]. [`MateEnvelope`] and [`MateEnvelopeStore`] are retained only as a
//! self-contained validation fixture for the earlier envelope shape; they are not a second
//! production storage owner and must not be wired into `MateStorage` callers.

use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

pub(crate) const MAX_MATE_BUCKET_ENTRIES: u32 = 65_535;
pub(crate) const MAX_MATE_BUCKET_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const REGION_MAGIC: [u8; 3] = *b"MEV";
const REGION_VERSION: u8 = 1;
const REGION_HEADER_BYTES: usize = 32;
const WASM_PAGE_BYTES: u64 = 65_536;
const HEADER_BYTES: usize = 35;
const MAX_OFFSET_40: u64 = (1 << 40) - 1;
const LOCATOR_BYTES: usize = 22;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MateEnvelopeCandidate {
    RankedPacked = 1,
    Sampled = 2,
}

impl MateEnvelopeCandidate {
    fn decode(value: u8) -> Result<Self, MateEnvelopeError> {
        match value {
            1 => Ok(Self::RankedPacked),
            2 => Ok(Self::Sampled),
            other => Err(MateEnvelopeError::UnsupportedCandidate(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MateEnvelopeLifecycle {
    Empty = 0,
    Rebuilding = 1,
    Published = 2,
    Stale = 3,
}

impl MateEnvelopeLifecycle {
    fn decode(value: u8) -> Result<Self, MateEnvelopeError> {
        match value {
            0 => Ok(Self::Empty),
            1 => Ok(Self::Rebuilding),
            2 => Ok(Self::Published),
            3 => Ok(Self::Stale),
            other => Err(MateEnvelopeError::UnsupportedLifecycle(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MateEnvelopeKey {
    pub orientation: u8,
    pub leaf: u32,
    pub owner_vertex_id: u32,
    pub bucket_label_key: u16,
}

impl MateEnvelopeKey {
    fn validate(&self) -> Result<(), MateEnvelopeError> {
        if self.orientation > 1 {
            return Err(MateEnvelopeError::InvalidTopology);
        }
        Ok(())
    }
}

impl Storable for MateEnvelopeKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 11,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned((*self).into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(11);
        bytes.push(self.orientation);
        bytes.extend_from_slice(&self.leaf.to_be_bytes());
        bytes.extend_from_slice(&self.owner_vertex_id.to_be_bytes());
        bytes.extend_from_slice(&self.bucket_label_key.to_be_bytes());
        bytes
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 11, "invalid mate envelope key");
        Self {
            orientation: bytes[0],
            leaf: u32::from_be_bytes(bytes[1..5].try_into().expect("leaf")),
            owner_vertex_id: u32::from_be_bytes(bytes[5..9].try_into().expect("owner")),
            bucket_label_key: u16::from_be_bytes(bytes[9..11].try_into().expect("label")),
        }
    }
}

/// Isolated self-contained codec fixture; production storage uses locator/payload separation below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MateEnvelope {
    pub key: MateEnvelopeKey,
    pub candidate: MateEnvelopeCandidate,
    pub lifecycle: MateEnvelopeLifecycle,
    pub generation: u64,
    pub topology_digest: u64,
    pub cardinality: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MateEnvelopeError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    InvalidHeaderLength(u16),
    UnsupportedCandidate(u8),
    UnsupportedLifecycle(u8),
    InvalidTopology,
    EmptyPayload,
    TooManyEntries,
    PayloadTooLarge,
    LengthMismatch,
    TrailingBytes,
    ArithmeticOverflow,
    NotPublished,
    RegionMemory,
    RegionReservedBytesNonZero,
    OffsetOverflow,
    InvalidLocatorLength,
    PayloadRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MateEnvelopeRegionHeader {
    pub version: u8,
}

impl MateEnvelopeRegionHeader {
    pub(crate) fn encode(self) -> [u8; REGION_HEADER_BYTES] {
        let mut bytes = [0u8; REGION_HEADER_BYTES];
        bytes[..3].copy_from_slice(&REGION_MAGIC);
        bytes[3] = self.version;
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, MateEnvelopeError> {
        if bytes.len() < REGION_HEADER_BYTES {
            return Err(MateEnvelopeError::Truncated);
        }
        if bytes[..3] != REGION_MAGIC {
            return Err(MateEnvelopeError::BadMagic);
        }
        if bytes[3] != REGION_VERSION {
            return Err(MateEnvelopeError::UnsupportedVersion(bytes[3]));
        }
        if bytes[4..].iter().any(|byte| *byte != 0) {
            return Err(MateEnvelopeError::RegionReservedBytesNonZero);
        }
        Ok(Self { version: bytes[3] })
    }
}

/// Locator-owned metadata for a payload stored in the separate mate byte region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MateLocatorRecord {
    pub candidate: MateEnvelopeCandidate,
    pub lifecycle: MateEnvelopeLifecycle,
    pub generation: u64,
    pub payload_offset: u64,
    pub payload_length: u32,
    pub cardinality: u32,
}

impl MateLocatorRecord {
    pub(crate) fn encode(self) -> Result<[u8; LOCATOR_BYTES], MateEnvelopeError> {
        if self.payload_offset > MAX_OFFSET_40 {
            return Err(MateEnvelopeError::OffsetOverflow);
        }
        if self.payload_length == 0 {
            return Err(MateEnvelopeError::InvalidLocatorLength);
        }
        if self.cardinality == 0 || self.cardinality > MAX_MATE_BUCKET_ENTRIES {
            return Err(MateEnvelopeError::TooManyEntries);
        }
        let mut bytes = [0u8; LOCATOR_BYTES];
        bytes[0] = (self.candidate as u8) | ((self.lifecycle as u8) << 2);
        bytes[1..9].copy_from_slice(&self.generation.to_be_bytes());
        let offset = self.payload_offset.to_be_bytes();
        bytes[9..14].copy_from_slice(&offset[3..]);
        bytes[14..18].copy_from_slice(&self.payload_length.to_be_bytes());
        bytes[18..22].copy_from_slice(&self.cardinality.to_be_bytes());
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, MateEnvelopeError> {
        if bytes.len() != LOCATOR_BYTES {
            return Err(MateEnvelopeError::InvalidLocatorLength);
        }
        let flags = bytes[0];
        if flags & 0xf0 != 0 {
            return Err(MateEnvelopeError::InvalidTopology);
        }
        let candidate = MateEnvelopeCandidate::decode(flags & 0x03)?;
        let lifecycle = MateEnvelopeLifecycle::decode((flags >> 2) & 0x03)?;
        let mut offset = [0u8; 8];
        offset[3..].copy_from_slice(&bytes[9..14]);
        let record = Self {
            candidate,
            lifecycle,
            generation: u64::from_be_bytes(bytes[1..9].try_into().expect("generation")),
            payload_offset: u64::from_be_bytes(offset),
            payload_length: u32::from_be_bytes(bytes[14..18].try_into().expect("length")),
            cardinality: u32::from_be_bytes(bytes[18..22].try_into().expect("cardinality")),
        };
        record.encode()?;
        Ok(record)
    }
}

impl Storable for MateLocatorRecord {
    const BOUND: Bound = Bound::Bounded {
        max_size: LOCATOR_BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode().expect("validated locator record").to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode().expect("validated locator record").to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(&bytes).expect("invalid persisted mate locator record")
    }
}

fn read<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], MateEnvelopeError> {
    let end = offset
        .checked_add(N)
        .ok_or(MateEnvelopeError::ArithmeticOverflow)?;
    let value = bytes
        .get(*offset..end)
        .ok_or(MateEnvelopeError::Truncated)?;
    *offset = end;
    value.try_into().map_err(|_| MateEnvelopeError::Truncated)
}

impl MateEnvelope {
    fn encode_flags(&self) -> u8 {
        (self.candidate as u8) | ((self.lifecycle as u8) << 2) | (self.key.orientation << 4)
    }

    fn decode_flags(
        flags: u8,
    ) -> Result<(MateEnvelopeCandidate, MateEnvelopeLifecycle, u8), MateEnvelopeError> {
        if flags & 0xe0 != 0 {
            return Err(MateEnvelopeError::InvalidTopology);
        }
        let candidate = MateEnvelopeCandidate::decode(flags & 0x03)?;
        let lifecycle = MateEnvelopeLifecycle::decode((flags >> 2) & 0x03)?;
        let orientation = (flags >> 4) & 0x01;
        Ok((candidate, lifecycle, orientation))
    }

    fn validate(&self) -> Result<(), MateEnvelopeError> {
        self.key.validate()?;
        if self.cardinality == 0 {
            return Err(MateEnvelopeError::EmptyPayload);
        }
        if self.cardinality > MAX_MATE_BUCKET_ENTRIES {
            return Err(MateEnvelopeError::TooManyEntries);
        }
        if self.payload.is_empty() {
            return Err(MateEnvelopeError::EmptyPayload);
        }
        if self.payload.len() > MAX_MATE_BUCKET_PAYLOAD_BYTES {
            return Err(MateEnvelopeError::PayloadTooLarge);
        }
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, MateEnvelopeError> {
        self.validate()?;
        let total = HEADER_BYTES
            .checked_add(self.payload.len())
            .ok_or(MateEnvelopeError::ArithmeticOverflow)?;
        let total_u32 = u32::try_from(total).map_err(|_| MateEnvelopeError::PayloadTooLarge)?;
        let mut bytes = Vec::with_capacity(total);
        bytes.push(self.encode_flags());
        bytes.extend_from_slice(&self.key.leaf.to_be_bytes());
        bytes.extend_from_slice(&self.key.owner_vertex_id.to_be_bytes());
        bytes.extend_from_slice(&self.key.bucket_label_key.to_be_bytes());
        bytes.extend_from_slice(&self.generation.to_be_bytes());
        bytes.extend_from_slice(&self.topology_digest.to_be_bytes());
        bytes.extend_from_slice(&self.cardinality.to_be_bytes());
        bytes.extend_from_slice(&total_u32.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, MateEnvelopeError> {
        if bytes.len() < HEADER_BYTES {
            return Err(MateEnvelopeError::Truncated);
        }
        let mut offset = 0;
        let (candidate, lifecycle, orientation) =
            Self::decode_flags(read::<1>(bytes, &mut offset)?[0])?;
        let leaf = u32::from_be_bytes(read::<4>(bytes, &mut offset)?);
        let owner_vertex_id = u32::from_be_bytes(read::<4>(bytes, &mut offset)?);
        let bucket_label_key = u16::from_be_bytes(read::<2>(bytes, &mut offset)?);
        let generation = u64::from_be_bytes(read::<8>(bytes, &mut offset)?);
        let topology_digest = u64::from_be_bytes(read::<8>(bytes, &mut offset)?);
        let cardinality = u32::from_be_bytes(read::<4>(bytes, &mut offset)?);
        let total_len = usize::try_from(u32::from_be_bytes(read::<4>(bytes, &mut offset)?))
            .map_err(|_| MateEnvelopeError::ArithmeticOverflow)?;
        if total_len != bytes.len() {
            return Err(if total_len < bytes.len() {
                MateEnvelopeError::TrailingBytes
            } else {
                MateEnvelopeError::LengthMismatch
            });
        }
        if cardinality > MAX_MATE_BUCKET_ENTRIES {
            return Err(MateEnvelopeError::TooManyEntries);
        }
        let envelope = Self {
            key: MateEnvelopeKey {
                orientation,
                leaf,
                owner_vertex_id,
                bucket_label_key,
            },
            candidate,
            lifecycle,
            generation,
            topology_digest,
            cardinality,
            payload: bytes[HEADER_BYTES..].to_vec(),
        };
        envelope.validate()?;
        Ok(envelope)
    }

    pub(crate) fn published(self) -> Result<Self, MateEnvelopeError> {
        if self.lifecycle != MateEnvelopeLifecycle::Published {
            return Err(MateEnvelopeError::NotPublished);
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvelopeBytes(Vec<u8>);

impl Storable for EnvelopeBytes {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }
}

pub(crate) struct MateLocatorStore<M: Memory> {
    map: StableBTreeMap<MateEnvelopeKey, MateLocatorRecord, M>,
}

impl<M: Memory> MateLocatorStore<M> {
    pub(crate) fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub(crate) fn insert(
        &mut self,
        key: MateEnvelopeKey,
        record: MateLocatorRecord,
    ) -> Result<(), MateEnvelopeError> {
        key.validate()?;
        record.encode()?;
        self.map.insert(key, record);
        Ok(())
    }

    pub(crate) fn get(
        &self,
        key: &MateEnvelopeKey,
    ) -> Result<Option<MateLocatorRecord>, MateEnvelopeError> {
        Ok(self.map.get(key))
    }

    pub(crate) fn into_memory(self) -> M {
        self.map.into_memory()
    }
}

const PAYLOAD_MAGIC: [u8; 3] = *b"MEP";

pub(crate) struct MatePayloadRegion<M: Memory> {
    memory: M,
    tail: u64,
}

impl<M: Memory> MatePayloadRegion<M> {
    pub(crate) fn init(memory: M) -> Result<Self, MateEnvelopeError> {
        if memory.size() == 0 {
            memory.grow(1);
            let mut header = [0u8; REGION_HEADER_BYTES];
            header[..3].copy_from_slice(&PAYLOAD_MAGIC);
            header[3] = REGION_VERSION;
            crate::safe_write(&memory, 0, &header).map_err(|_| MateEnvelopeError::RegionMemory)?;
            return Ok(Self { memory, tail: 0 });
        }
        if memory.size().saturating_mul(WASM_PAGE_BYTES) < REGION_HEADER_BYTES as u64 {
            return Err(MateEnvelopeError::Truncated);
        }
        let mut header = [0u8; REGION_HEADER_BYTES];
        memory.read(0, &mut header);
        if header[..3] != PAYLOAD_MAGIC {
            return Err(MateEnvelopeError::BadMagic);
        }
        if header[3] != REGION_VERSION {
            return Err(MateEnvelopeError::UnsupportedVersion(header[3]));
        }
        let tail = u64::from_be_bytes(header[4..12].try_into().expect("payload tail"));
        if tail > memory.size().saturating_mul(WASM_PAGE_BYTES) - REGION_HEADER_BYTES as u64 {
            return Err(MateEnvelopeError::PayloadRange);
        }
        Ok(Self { memory, tail })
    }

    pub(crate) fn append(&mut self, payload: &[u8]) -> Result<(u64, u32), MateEnvelopeError> {
        if payload.is_empty() || payload.len() > MAX_MATE_BUCKET_PAYLOAD_BYTES {
            return Err(MateEnvelopeError::PayloadTooLarge);
        }
        let length =
            u32::try_from(payload.len()).map_err(|_| MateEnvelopeError::PayloadTooLarge)?;
        let offset = self.tail;
        let absolute = (REGION_HEADER_BYTES as u64)
            .checked_add(offset)
            .ok_or(MateEnvelopeError::ArithmeticOverflow)?;
        crate::safe_write(&self.memory, absolute, payload)
            .map_err(|_| MateEnvelopeError::RegionMemory)?;
        self.tail = self
            .tail
            .checked_add(u64::from(length))
            .ok_or(MateEnvelopeError::ArithmeticOverflow)?;
        self.persist_tail()?;
        Ok((offset, length))
    }

    pub(crate) fn read(&self, offset: u64, length: u32) -> Result<Vec<u8>, MateEnvelopeError> {
        let end = offset
            .checked_add(u64::from(length))
            .ok_or(MateEnvelopeError::ArithmeticOverflow)?;
        if end > self.tail {
            return Err(MateEnvelopeError::PayloadRange);
        }
        let mut bytes =
            vec![0u8; usize::try_from(length).map_err(|_| MateEnvelopeError::PayloadRange)?];
        self.memory
            .read(REGION_HEADER_BYTES as u64 + offset, &mut bytes);
        Ok(bytes)
    }

    fn persist_tail(&self) -> Result<(), MateEnvelopeError> {
        crate::safe_write(&self.memory, 4, &self.tail.to_be_bytes())
            .map_err(|_| MateEnvelopeError::RegionMemory)
    }

    pub(crate) fn into_memory(self) -> M {
        self.memory
    }
}

/// A fixture/pre-production-only map. No canonical region is passed to this type.
pub(crate) struct MateEnvelopeStore<M: Memory> {
    map: StableBTreeMap<MateEnvelopeKey, EnvelopeBytes, M>,
}

/// Region-level owner that stores the format header separately from entry values.
/// Isolated map-backed envelope fixture; production storage uses `MateLocatorStore` and
/// `MatePayloadRegion` instead.
pub(crate) struct MateEnvelopeRegion<M: Memory> {
    header: M,
    entries: MateEnvelopeStore<M>,
}

impl<M: Memory> MateEnvelopeRegion<M> {
    pub(crate) fn init(header: M, entries: M) -> Result<Self, MateEnvelopeError> {
        match header.size() {
            0 => {
                header.grow(1);
                crate::safe_write(
                    &header,
                    0,
                    &MateEnvelopeRegionHeader {
                        version: REGION_VERSION,
                    }
                    .encode(),
                )
                .map_err(|_| MateEnvelopeError::RegionMemory)?;
            }
            _ => {
                if header.size().saturating_mul(WASM_PAGE_BYTES) < REGION_HEADER_BYTES as u64 {
                    return Err(MateEnvelopeError::Truncated);
                }
                let mut bytes = [0u8; REGION_HEADER_BYTES];
                header.read(0, &mut bytes);
                MateEnvelopeRegionHeader::decode(&bytes)?;
            }
        }
        Ok(Self {
            header,
            entries: MateEnvelopeStore::init(entries),
        })
    }

    pub(crate) fn put(&mut self, envelope: &MateEnvelope) -> Result<(), MateEnvelopeError> {
        self.entries.put(envelope)
    }

    pub(crate) fn get_published(
        &self,
        key: &MateEnvelopeKey,
    ) -> Result<Option<MateEnvelope>, MateEnvelopeError> {
        self.entries.get_published(key)
    }

    pub(crate) fn into_memories(self) -> (M, M) {
        (self.header, self.entries.map.into_memory())
    }
}

impl<M: Memory> MateEnvelopeStore<M> {
    pub(crate) fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub(crate) fn put(&mut self, envelope: &MateEnvelope) -> Result<(), MateEnvelopeError> {
        let bytes = envelope.encode()?;
        self.map.insert(envelope.key, EnvelopeBytes(bytes));
        Ok(())
    }

    pub(crate) fn get(
        &self,
        key: &MateEnvelopeKey,
    ) -> Result<Option<MateEnvelope>, MateEnvelopeError> {
        let Some(bytes) = self.map.get(key) else {
            return Ok(None);
        };
        let envelope = MateEnvelope::decode(&bytes.0)?;
        if envelope.key != *key {
            return Err(MateEnvelopeError::InvalidTopology);
        }
        Ok(Some(envelope))
    }

    pub(crate) fn get_published(
        &self,
        key: &MateEnvelopeKey,
    ) -> Result<Option<MateEnvelope>, MateEnvelopeError> {
        self.get(key)?.map(MateEnvelope::published).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;

    fn envelope(lifecycle: MateEnvelopeLifecycle) -> MateEnvelope {
        MateEnvelope {
            key: MateEnvelopeKey {
                orientation: 1,
                leaf: 7,
                owner_vertex_id: 42,
                bucket_label_key: 3,
            },
            candidate: MateEnvelopeCandidate::RankedPacked,
            lifecycle,
            generation: 9,
            topology_digest: 0x1234,
            cardinality: 4,
            payload: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn published_round_trip_and_reopen() {
        let bytes = envelope(MateEnvelopeLifecycle::Published)
            .encode()
            .expect("encode");
        assert_eq!(
            MateEnvelope::decode(&bytes).expect("decode"),
            envelope(MateEnvelopeLifecycle::Published)
        );
        let memory = VectorMemory::default();
        let mut store = MateEnvelopeStore::init(memory.clone());
        store
            .put(&envelope(MateEnvelopeLifecycle::Published))
            .expect("put");
        let reopened = MateEnvelopeStore::init(memory);
        assert_eq!(
            reopened
                .get(&envelope(MateEnvelopeLifecycle::Published).key)
                .expect("get"),
            Some(envelope(MateEnvelopeLifecycle::Published))
        );
    }

    #[test]
    fn region_header_reopens_before_entries_and_rejects_partial_header() {
        let header = VectorMemory::default();
        let entries = VectorMemory::default();
        let mut region = MateEnvelopeRegion::init(header, entries).expect("fresh region");
        region
            .put(&envelope(MateEnvelopeLifecycle::Published))
            .expect("put");
        let (header, entries) = region.into_memories();
        let reopened = MateEnvelopeRegion::init(header, entries).expect("reopen region");
        assert_eq!(
            reopened
                .get_published(&envelope(MateEnvelopeLifecycle::Published).key)
                .expect("get"),
            Some(envelope(MateEnvelopeLifecycle::Published))
        );

        let partial = VectorMemory::default();
        partial.grow(1);
        assert!(matches!(
            MateEnvelopeRegion::init(partial, VectorMemory::default()),
            Err(MateEnvelopeError::BadMagic)
        ));
    }

    #[test]
    fn locator_and_payload_are_separate_and_reopenable() {
        let key = envelope(MateEnvelopeLifecycle::Published).key;
        let record = MateLocatorRecord {
            candidate: MateEnvelopeCandidate::RankedPacked,
            lifecycle: MateEnvelopeLifecycle::Published,
            generation: 9,
            payload_offset: 0,
            payload_length: 4,
            cardinality: 4,
        };
        assert_eq!(
            MateLocatorRecord::decode(&record.encode().expect("locator")).expect("decode"),
            record
        );
        let locator_memory = VectorMemory::default();
        let payload_memory = VectorMemory::default();
        let mut locators = MateLocatorStore::init(locator_memory);
        let mut payloads = MatePayloadRegion::init(payload_memory).expect("payload region");
        let (offset, length) = payloads.append(&[1, 2, 3, 4]).expect("payload");
        assert_eq!(offset, 0);
        assert_eq!(
            payloads.read(offset, length).expect("read"),
            vec![1, 2, 3, 4]
        );
        locators
            .insert(
                key,
                MateLocatorRecord {
                    payload_offset: offset,
                    payload_length: length,
                    ..record
                },
            )
            .expect("insert");
        let (locator_memory, payload_memory) = (locators.into_memory(), payloads.into_memory());
        let reopened_locators = MateLocatorStore::init(locator_memory);
        let reopened_payloads = MatePayloadRegion::init(payload_memory).expect("reopen payload");
        assert_eq!(reopened_locators.get(&key).expect("locator"), Some(record));
        assert_eq!(
            reopened_payloads.read(offset, length).expect("payload"),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn stale_and_rebuilding_are_not_published() {
        for lifecycle in [
            MateEnvelopeLifecycle::Empty,
            MateEnvelopeLifecycle::Rebuilding,
            MateEnvelopeLifecycle::Stale,
        ] {
            assert!(envelope(lifecycle).published().is_err());
        }
        let memory = VectorMemory::default();
        let mut store = MateEnvelopeStore::init(memory);
        store
            .put(&envelope(MateEnvelopeLifecycle::Stale))
            .expect("put");
        assert_eq!(
            store
                .get_published(&envelope(MateEnvelopeLifecycle::Stale).key)
                .expect_err("stale must be rejected"),
            MateEnvelopeError::NotPublished
        );
    }

    #[test]
    fn malformed_region_version_truncation_and_trailing_are_rejected() {
        let encoded = envelope(MateEnvelopeLifecycle::Published)
            .encode()
            .expect("encode");
        let region = MateEnvelopeRegionHeader { version: 1 }.encode();
        assert_eq!(
            MateEnvelopeRegionHeader::decode(&region)
                .expect("region header")
                .version,
            1
        );
        let mut version = region;
        version[3] = 9;
        assert_eq!(
            MateEnvelopeRegionHeader::decode(&version),
            Err(MateEnvelopeError::UnsupportedVersion(9))
        );
        let mut magic = region;
        magic[0] ^= 1;
        assert_eq!(
            MateEnvelopeRegionHeader::decode(&magic),
            Err(MateEnvelopeError::BadMagic)
        );
        let mut reserved = region;
        reserved[4] = 1;
        assert_eq!(
            MateEnvelopeRegionHeader::decode(&reserved),
            Err(MateEnvelopeError::RegionReservedBytesNonZero)
        );
        assert_eq!(
            MateEnvelope::decode(&encoded[..encoded.len() - 1]),
            Err(MateEnvelopeError::LengthMismatch)
        );
        let mut oversized = encoded.clone();
        oversized[27..31].copy_from_slice(
            &u32::try_from(MAX_MATE_BUCKET_PAYLOAD_BYTES + 1)
                .expect("bound fits u32")
                .to_be_bytes(),
        );
        assert_eq!(
            MateEnvelope::decode(&oversized),
            Err(MateEnvelopeError::TooManyEntries)
        );
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            MateEnvelope::decode(&trailing),
            Err(MateEnvelopeError::TrailingBytes)
        );
    }

    #[test]
    fn bounds_and_topology_are_fail_closed() {
        let mut too_many = envelope(MateEnvelopeLifecycle::Published);
        too_many.cardinality = MAX_MATE_BUCKET_ENTRIES + 1;
        assert_eq!(too_many.encode(), Err(MateEnvelopeError::TooManyEntries));
        let mut too_large = envelope(MateEnvelopeLifecycle::Published);
        too_large.payload = vec![0; MAX_MATE_BUCKET_PAYLOAD_BYTES + 1];
        assert_eq!(too_large.encode(), Err(MateEnvelopeError::PayloadTooLarge));
        let mut bad_key = envelope(MateEnvelopeLifecycle::Published);
        bad_key.key.orientation = 2;
        assert_eq!(bad_key.encode(), Err(MateEnvelopeError::InvalidTopology));
    }
}
