//! Edge property equality posting key (ADR 0009 §1).
//!
//! Lexicographic order: `(property_id, value, label_id, shard_id, owner_vertex_id, slot_index)`.

use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use std::borrow::Cow;
use std::cmp::Ordering;

const EDGE_POSTING_KEY_MAGIC: u8 = 3;

/// Global edge equality posting key on graph-index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EdgePostingKey {
    pub property_id: u32,
    pub value: Vec<u8>,
    pub label_id: u16,
    pub shard_id: ShardId,
    pub owner_vertex_id: u32,
    pub slot_index: u32,
}

impl PartialOrd for EdgePostingKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EdgePostingKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.property_id
            .cmp(&other.property_id)
            .then_with(|| self.value.cmp(&other.value))
            .then_with(|| self.label_id.cmp(&other.label_id))
            .then_with(|| self.shard_id.cmp(&other.shard_id))
            .then_with(|| self.owner_vertex_id.cmp(&other.owner_vertex_id))
            .then_with(|| self.slot_index.cmp(&other.slot_index))
    }
}

impl Storable for EdgePostingKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref()).expect("EdgePostingKey decode")
    }
}

impl EdgePostingKey {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + 4 + self.value.len() + 2 + 4 + 4 + 4);
        out.push(EDGE_POSTING_KEY_MAGIC);
        out.extend_from_slice(&self.property_id.to_le_bytes());
        let len_u32: u32 = self
            .value
            .len()
            .try_into()
            .expect("value length must fit u32");
        out.extend_from_slice(&len_u32.to_le_bytes());
        out.extend_from_slice(&self.value);
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.owner_vertex_id.to_le_bytes());
        out.extend_from_slice(&self.slot_index.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.first().copied()? != EDGE_POSTING_KEY_MAGIC {
            return None;
        }
        let property_id = u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?);
        let vlen = u32::from_le_bytes(bytes.get(5..9)?.try_into().ok()?);
        let usize_len = usize::try_from(vlen).ok()?;
        let val_start = 9usize;
        let val_end = val_start.checked_add(usize_len)?;
        let value = bytes.get(val_start..val_end)?.to_vec();
        let label_off = val_end;
        let label_id = u16::from_le_bytes(bytes.get(label_off..label_off + 2)?.try_into().ok()?);
        let shard_off = label_off + 2;
        let shard_id =
            ShardId::from_le_bytes(bytes.get(shard_off..shard_off + 4)?.try_into().ok()?);
        let owner_off = shard_off + 4;
        let owner_vertex_id =
            u32::from_le_bytes(bytes.get(owner_off..owner_off + 4)?.try_into().ok()?);
        let slot_off = owner_off + 4;
        let slot_index = u32::from_le_bytes(bytes.get(slot_off..slot_off + 4)?.try_into().ok()?);
        Some(Self {
            property_id,
            value,
            label_id,
            shard_id,
            owner_vertex_id,
            slot_index,
        })
    }

    pub fn prefix_lower(property_id: u32, value: &[u8]) -> Self {
        Self {
            property_id,
            value: value.to_vec(),
            label_id: 0,
            shard_id: ShardId::new(0),
            owner_vertex_id: 0,
            slot_index: 0,
        }
    }

    pub fn prefix_upper(property_id: u32, value: &[u8]) -> Self {
        Self {
            property_id,
            value: value.to_vec(),
            label_id: u16::MAX,
            shard_id: ShardId::new(u32::MAX),
            owner_vertex_id: u32::MAX,
            slot_index: u32::MAX,
        }
    }

    pub fn prefix_lower_labeled(property_id: u32, value: &[u8], label_id: u16) -> Self {
        Self {
            property_id,
            value: value.to_vec(),
            label_id,
            shard_id: ShardId::new(0),
            owner_vertex_id: 0,
            slot_index: 0,
        }
    }

    pub fn prefix_upper_labeled(property_id: u32, value: &[u8], label_id: u16) -> Self {
        Self {
            property_id,
            value: value.to_vec(),
            label_id,
            shard_id: ShardId::new(u32::MAX),
            owner_vertex_id: u32::MAX,
            slot_index: u32::MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_posting_key_roundtrip() {
        let k = EdgePostingKey {
            property_id: 9,
            value: vec![4, 5],
            label_id: 7,
            shard_id: ShardId::new(2),
            owner_vertex_id: 11,
            slot_index: 3,
        };
        let bytes = k.encode();
        assert_eq!(EdgePostingKey::decode(&bytes).unwrap(), k);
    }

    #[test]
    fn edge_posting_key_orders_label_before_shard() {
        let a = EdgePostingKey {
            property_id: 1,
            value: vec![1],
            label_id: 1,
            shard_id: ShardId::new(99),
            owner_vertex_id: 0,
            slot_index: 0,
        };
        let b = EdgePostingKey {
            property_id: 1,
            value: vec![1],
            label_id: 2,
            shard_id: ShardId::new(0),
            owner_vertex_id: 0,
            slot_index: 0,
        };
        assert!(a < b);
    }
}
