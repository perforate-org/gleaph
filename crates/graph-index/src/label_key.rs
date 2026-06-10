//! Composite label posting key: `(vertex_label_id, shard_id, vertex_id)` ordered for prefix scans.

use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use std::borrow::Cow;
use std::cmp::Ordering;

const LABEL_POSTING_KEY_MAGIC: u8 = 3;

/// Lexicographic order: `vertex_label_id`, then `shard_id`, then `vertex_id`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelPostingKey {
    pub vertex_label_id: u32,
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

impl PartialOrd for LabelPostingKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LabelPostingKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.vertex_label_id
            .cmp(&other.vertex_label_id)
            .then_with(|| self.shard_id.cmp(&other.shard_id))
            .then_with(|| self.vertex_id.cmp(&other.vertex_id))
    }
}

impl Storable for LabelPostingKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref()).expect("LabelPostingKey decode")
    }
}

impl LabelPostingKey {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + 4 + 4);
        out.push(LABEL_POSTING_KEY_MAGIC);
        out.extend_from_slice(&self.vertex_label_id.to_le_bytes());
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.vertex_id.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.first().copied()? != LABEL_POSTING_KEY_MAGIC {
            return None;
        }
        let label_id = u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?);
        let shard_id = u32::from_le_bytes(bytes.get(5..9)?.try_into().ok()?);
        let vertex_id = u32::from_le_bytes(bytes.get(9..13)?.try_into().ok()?);
        Some(Self {
            vertex_label_id: label_id,
            shard_id,
            vertex_id,
        })
    }

    pub fn prefix_lower(vertex_label_id: u32) -> Self {
        Self {
            vertex_label_id,
            shard_id: 0,
            vertex_id: 0,
        }
    }

    pub fn prefix_upper(vertex_label_id: u32) -> Self {
        Self {
            vertex_label_id,
            shard_id: u32::MAX,
            vertex_id: u32::MAX,
        }
    }

    /// Smallest key strictly greater than `self`, within the same label bucket order.
    pub fn successor(self) -> Option<Self> {
        if self.vertex_id < u32::MAX {
            return Some(Self {
                vertex_id: self.vertex_id + 1,
                ..self
            });
        }
        if self.shard_id < u32::MAX {
            return Some(Self {
                shard_id: self.shard_id + 1,
                vertex_id: 0,
                vertex_label_id: self.vertex_label_id,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_posting_key_successor() {
        let k = LabelPostingKey {
            vertex_label_id: 1,
            shard_id: 7,
            vertex_id: 42,
        };
        assert_eq!(
            k.successor(),
            Some(LabelPostingKey {
                vertex_label_id: 1,
                shard_id: 7,
                vertex_id: 43,
            })
        );
    }

    #[test]
    fn label_posting_key_roundtrip() {
        let k = LabelPostingKey {
            vertex_label_id: 7,
            shard_id: 99,
            vertex_id: 42,
        };
        let bytes = k.encode();
        assert_eq!(LabelPostingKey::decode(&bytes).unwrap(), k);
    }
}
