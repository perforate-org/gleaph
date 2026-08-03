//! Composite posting key: `(physical_index_id, property_id, payload_bytes, shard_id, vertex_id)`
//! ordered for prefix scans.

use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::PhysicalIndexId;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use std::borrow::Cow;
use std::cmp::Ordering;

const POSTING_KEY_MAGIC: u8 = 4;

/// Lexicographic order: physical namespace, property, value, shard, then vertex.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostingKey {
    pub physical_index_id: PhysicalIndexId,
    pub property_id: u32,
    pub value: Vec<u8>,
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

impl PartialOrd for PostingKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PostingKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.physical_index_id
            .cmp(&other.physical_index_id)
            .then_with(|| self.property_id.cmp(&other.property_id))
            .then_with(|| self.value.cmp(&other.value))
            .then_with(|| self.shard_id.cmp(&other.shard_id))
            .then_with(|| self.vertex_id.cmp(&other.vertex_id))
    }
}

impl Storable for PostingKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref()).expect("PostingKey decode")
    }
}

impl PostingKey {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 8 + 4 + 4 + self.value.len() + 4 + 4);
        out.push(POSTING_KEY_MAGIC);
        out.extend_from_slice(&self.physical_index_id.to_le_bytes());
        out.extend_from_slice(&self.property_id.to_le_bytes());
        let len_u32: u32 = self
            .value
            .len()
            .try_into()
            .expect("value length must fit u32");
        out.extend_from_slice(&len_u32.to_le_bytes());
        out.extend_from_slice(&self.value);
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.vertex_id.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.first().copied()? != POSTING_KEY_MAGIC {
            return None;
        }
        let physical_index_id = PhysicalIndexId::from_le_bytes(bytes.get(1..9)?.try_into().ok()?)?;
        let pid = u32::from_le_bytes(bytes.get(9..13)?.try_into().ok()?);
        let vlen = u32::from_le_bytes(bytes.get(13..17)?.try_into().ok()?);
        let usize_len = usize::try_from(vlen).ok()?;
        let val_start: usize = 17;
        let val_end = val_start.checked_add(usize_len)?;
        let value = bytes.get(val_start..val_end)?.to_vec();
        let shard_off = val_end;
        let shard_id =
            ShardId::from_le_bytes(bytes.get(shard_off..shard_off + 4)?.try_into().ok()?);
        let vid_off = shard_off + 4;
        let vertex_id = u32::from_le_bytes(bytes.get(vid_off..vid_off + 4)?.try_into().ok()?);
        Some(Self {
            physical_index_id,
            property_id: pid,
            value,
            shard_id,
            vertex_id,
        })
    }

    /// Lower bound for postings matching `(physical_index_id, property_id, value)`.
    pub fn prefix_lower(
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: &[u8],
    ) -> Self {
        Self {
            physical_index_id,
            property_id,
            value: value.to_vec(),
            shard_id: ShardId::new(0),
            vertex_id: 0,
        }
    }

    /// Upper bound for postings matching `(physical_index_id, property_id, value)`.
    pub fn prefix_upper(
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: &[u8],
    ) -> Self {
        Self {
            physical_index_id,
            property_id,
            value: value.to_vec(),
            shard_id: ShardId::new(u32::MAX),
            vertex_id: u32::MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posting_key_roundtrip() {
        let k = PostingKey {
            physical_index_id: PhysicalIndexId::new(11).unwrap(),
            property_id: 7,
            value: vec![1, 2, 3],
            shard_id: ShardId::new(1),
            vertex_id: 42,
        };
        let bytes = k.encode();
        assert_eq!(PostingKey::decode(&bytes).unwrap(), k);
    }

    #[test]
    fn posting_key_orders_physical_namespace_first() {
        let lower = PostingKey::prefix_lower(PhysicalIndexId::new(1).unwrap(), u32::MAX, &[255]);
        let higher = PostingKey::prefix_lower(PhysicalIndexId::new(2).unwrap(), 0, &[]);
        assert!(lower < higher);
    }

    #[test]
    fn posting_key_rejects_reserved_zero_namespace() {
        let key = PostingKey::prefix_lower(PhysicalIndexId::new(1).unwrap(), 7, b"v");
        let mut bytes = key.encode();
        bytes[1..9].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(PostingKey::decode(&bytes), None);
    }
}
