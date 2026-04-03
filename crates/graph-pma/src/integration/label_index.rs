use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Cursor, Read};

use gleaph_graph_kernel::LabelId;
use roaring::RoaringBitmap;

pub const VERTEX_LABEL_PROMOTION_THRESHOLD_BASE: usize = 256;
const VERTEX_LABEL_PROMOTION_THRESHOLD_MIN: usize = 64;
const VERTEX_LABEL_PROMOTION_THRESHOLD_MAX: usize = 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum LabelMembership {
    SmallVec(Vec<u32>),
    Roaring(RoaringBitmap),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VertexLabelIndex {
    pub(crate) by_label: BTreeMap<LabelId, LabelMembership>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VertexGcState {
    pub(crate) tombstone_ordinals: BTreeSet<usize>,
    pub(crate) reclaim_queue: VecDeque<usize>,
    pub(crate) free_list: Vec<usize>,
    pub(crate) queue_cursor: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VacuumStats {
    pub tombstones: usize,
    pub queue_len: usize,
    pub free_list_len: usize,
    pub queue_cursor: u64,
}

impl VertexLabelIndex {
    pub(crate) fn insert(&mut self, label_id: LabelId, ordinal: usize, threshold: usize) {
        let Ok(ordinal_u32) = u32::try_from(ordinal) else {
            return;
        };
        let entry = self
            .by_label
            .entry(label_id)
            .or_insert_with(|| LabelMembership::SmallVec(Vec::new()));
        match entry {
            LabelMembership::SmallVec(values) => {
                if values.binary_search(&ordinal_u32).is_ok() {
                    return;
                }
                let pos = values.partition_point(|x| *x < ordinal_u32);
                values.insert(pos, ordinal_u32);
                if values.len() >= threshold {
                    let mut bitmap = RoaringBitmap::new();
                    for &v in values.iter() {
                        bitmap.insert(v);
                    }
                    *entry = LabelMembership::Roaring(bitmap);
                }
            }
            LabelMembership::Roaring(bitmap) => {
                bitmap.insert(ordinal_u32);
            }
        }
    }

    pub(crate) fn remove(&mut self, label_id: LabelId, ordinal: usize) {
        let Some(entry) = self.by_label.get_mut(&label_id) else {
            return;
        };
        let Ok(ordinal_u32) = u32::try_from(ordinal) else {
            return;
        };
        match entry {
            LabelMembership::SmallVec(values) => {
                if let Ok(pos) = values.binary_search(&ordinal_u32) {
                    values.remove(pos);
                }
                if values.is_empty() {
                    self.by_label.remove(&label_id);
                }
            }
            LabelMembership::Roaring(bitmap) => {
                bitmap.remove(ordinal_u32);
                if bitmap.is_empty() {
                    self.by_label.remove(&label_id);
                }
            }
        }
    }

    pub(crate) fn ordinals_for(&self, label_id: LabelId) -> Vec<usize> {
        let Some(entry) = self.by_label.get(&label_id) else {
            return Vec::new();
        };
        match entry {
            LabelMembership::SmallVec(values) => values
                .iter()
                .filter_map(|v| usize::try_from(*v).ok())
                .collect(),
            LabelMembership::Roaring(bitmap) => bitmap
                .iter()
                .filter_map(|v| usize::try_from(v).ok())
                .collect(),
        }
    }

    pub(crate) fn cardinality(&self, label_id: LabelId) -> usize {
        let Some(entry) = self.by_label.get(&label_id) else {
            return 0;
        };
        match entry {
            LabelMembership::SmallVec(values) => values.len(),
            LabelMembership::Roaring(bitmap) => bitmap.len() as usize,
        }
    }
}

pub(crate) fn vertex_label_promotion_threshold(card: usize) -> usize {
    if card > 10_000 {
        VERTEX_LABEL_PROMOTION_THRESHOLD_MIN
    } else if card > 1_000 {
        VERTEX_LABEL_PROMOTION_THRESHOLD_BASE / 2
    } else if card < 10 {
        VERTEX_LABEL_PROMOTION_THRESHOLD_MAX
    } else {
        VERTEX_LABEL_PROMOTION_THRESHOLD_BASE
    }
}

pub fn encode_vertex_label_catalog(
    labels: &BTreeMap<String, LabelId>,
    next_label_id: u16,
    index: &VertexLabelIndex,
    gc_state: &VertexGcState,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"VLBL");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&next_label_id.to_le_bytes());
    out.extend_from_slice(&(labels.len() as u32).to_le_bytes());
    for (label, label_id) in labels {
        out.extend_from_slice(&(label.len() as u16).to_le_bytes());
        out.extend_from_slice(label.as_bytes());
        out.extend_from_slice(&label_id.to_le_bytes());
    }
    out.extend_from_slice(&(index.by_label.len() as u32).to_le_bytes());
    for (label_id, membership) in &index.by_label {
        out.extend_from_slice(&label_id.to_le_bytes());
        match membership {
            LabelMembership::SmallVec(values) => {
                out.push(0);
                out.extend_from_slice(&(values.len() as u32).to_le_bytes());
                for value in values {
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
            LabelMembership::Roaring(bitmap) => {
                out.push(1);
                let mut bytes = Vec::new();
                bitmap
                    .serialize_into(&mut bytes)
                    .expect("serialize roaring bitmap");
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(&bytes);
            }
        }
    }
    out.extend_from_slice(&(gc_state.tombstone_ordinals.len() as u32).to_le_bytes());
    for ordinal in &gc_state.tombstone_ordinals {
        out.extend_from_slice(&(*ordinal as u32).to_le_bytes());
    }
    out.extend_from_slice(&(gc_state.reclaim_queue.len() as u32).to_le_bytes());
    for ordinal in &gc_state.reclaim_queue {
        out.extend_from_slice(&(*ordinal as u32).to_le_bytes());
    }
    out.extend_from_slice(&(gc_state.free_list.len() as u32).to_le_bytes());
    for ordinal in &gc_state.free_list {
        out.extend_from_slice(&(*ordinal as u32).to_le_bytes());
    }
    out.extend_from_slice(&gc_state.queue_cursor.to_le_bytes());
    out
}

pub fn decode_vertex_label_catalog(
    bytes: &[u8],
) -> Option<(
    BTreeMap<String, LabelId>,
    LabelId,
    VertexLabelIndex,
    VertexGcState,
)> {
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic).ok()?;
    if &magic != b"VLBL" {
        return None;
    }
    let mut version = [0u8; 2];
    cursor.read_exact(&mut version).ok()?;
    if u16::from_le_bytes(version) != 1 {
        return None;
    }
    let mut next_label_id = [0u8; 2];
    cursor.read_exact(&mut next_label_id).ok()?;
    let next_label_id = u16::from_le_bytes(next_label_id);

    let read_u32 = |cursor: &mut Cursor<&[u8]>| -> Option<u32> {
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).ok()?;
        Some(u32::from_le_bytes(buf))
    };

    let label_count = read_u32(&mut cursor)? as usize;
    let mut labels = BTreeMap::new();
    for _ in 0..label_count {
        let mut len = [0u8; 2];
        cursor.read_exact(&mut len).ok()?;
        let len = u16::from_le_bytes(len) as usize;
        let mut label = vec![0u8; len];
        cursor.read_exact(&mut label).ok()?;
        let mut label_id = [0u8; 2];
        cursor.read_exact(&mut label_id).ok()?;
        labels.insert(String::from_utf8(label).ok()?, u16::from_le_bytes(label_id));
    }

    let membership_count = read_u32(&mut cursor)? as usize;
    let mut by_label = BTreeMap::new();
    for _ in 0..membership_count {
        let mut label_id = [0u8; 2];
        cursor.read_exact(&mut label_id).ok()?;
        let label_id = u16::from_le_bytes(label_id);
        let mut kind = [0u8; 1];
        cursor.read_exact(&mut kind).ok()?;
        let byte_len = read_u32(&mut cursor)? as usize;
        let membership = match kind[0] {
            0 => {
                let count = byte_len;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let mut value = [0u8; 4];
                    cursor.read_exact(&mut value).ok()?;
                    values.push(u32::from_le_bytes(value));
                }
                LabelMembership::SmallVec(values)
            }
            1 => {
                let mut data = vec![0u8; byte_len];
                cursor.read_exact(&mut data).ok()?;
                let bitmap = RoaringBitmap::deserialize_from(&mut Cursor::new(data)).ok()?;
                LabelMembership::Roaring(bitmap)
            }
            _ => return None,
        };
        by_label.insert(label_id, membership);
    }

    let tombstone_count = read_u32(&mut cursor)? as usize;
    let mut tombstone_ordinals = BTreeSet::new();
    for _ in 0..tombstone_count {
        let mut value = [0u8; 4];
        cursor.read_exact(&mut value).ok()?;
        tombstone_ordinals.insert(u32::from_le_bytes(value) as usize);
    }

    let queue_count = read_u32(&mut cursor)? as usize;
    let mut reclaim_queue = VecDeque::with_capacity(queue_count);
    for _ in 0..queue_count {
        let mut value = [0u8; 4];
        cursor.read_exact(&mut value).ok()?;
        reclaim_queue.push_back(u32::from_le_bytes(value) as usize);
    }

    let free_count = read_u32(&mut cursor)? as usize;
    let mut free_list = Vec::with_capacity(free_count);
    for _ in 0..free_count {
        let mut value = [0u8; 4];
        cursor.read_exact(&mut value).ok()?;
        free_list.push(u32::from_le_bytes(value) as usize);
    }

    let mut queue_cursor = [0u8; 8];
    cursor.read_exact(&mut queue_cursor).ok()?;

    Some((
        labels,
        next_label_id,
        VertexLabelIndex { by_label },
        VertexGcState {
            tombstone_ordinals,
            reclaim_queue,
            free_list,
            queue_cursor: u64::from_le_bytes(queue_cursor),
        },
    ))
}
