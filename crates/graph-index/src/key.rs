//! Composite posting key: `(property_id, payload_bytes, shard_id, vertex_id)` ordered for prefix scans.

use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use std::borrow::Cow;
use std::cmp::Ordering;

const POSTING_KEY_MAGIC: u8 = 2;

/// Lexicographic order: `property_id`, then `value` (memcmp), then `shard_id`, then `vertex_id`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostingKey {
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
        self.property_id
            .cmp(&other.property_id)
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
        let mut out = Vec::with_capacity(1 + 4 + 4 + self.value.len() + 4 + 4);
        out.push(POSTING_KEY_MAGIC);
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
        let pid = u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?);
        let vlen = u32::from_le_bytes(bytes.get(5..9)?.try_into().ok()?);
        let usize_len = usize::try_from(vlen).ok()?;
        let val_start: usize = 9;
        let val_end = val_start.checked_add(usize_len)?;
        let value = bytes.get(val_start..val_end)?.to_vec();
        let shard_off = val_end;
        let shard_id = u32::from_le_bytes(bytes.get(shard_off..shard_off + 4)?.try_into().ok()?);
        let vid_off = shard_off + 4;
        let vertex_id = u32::from_le_bytes(bytes.get(vid_off..vid_off + 4)?.try_into().ok()?);
        Some(Self {
            property_id: pid,
            value,
            shard_id,
            vertex_id,
        })
    }

    /// Lower bound for `range` scans over all postings matching `(property_id, value)`.
    pub fn prefix_lower(property_id: u32, value: &[u8]) -> Self {
        Self {
            property_id,
            value: value.to_vec(),
            shard_id: 0,
            vertex_id: 0,
        }
    }

    /// Upper bound for `range` scans over all postings matching `(property_id, value)`.
    pub fn prefix_upper(property_id: u32, value: &[u8]) -> Self {
        Self {
            property_id,
            value: value.to_vec(),
            shard_id: u32::MAX,
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
            property_id: 7,
            value: vec![1, 2, 3],
            shard_id: 99,
            vertex_id: 42,
        };
        let bytes = k.encode();
        assert_eq!(PostingKey::decode(&bytes).unwrap(), k);
    }
}
