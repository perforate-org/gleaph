use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Cursor, Read};

use gleaph_graph_kernel::LabelId;
use ic_stable_structures::storable::Bound;
use ic_stable_structures::{Memory, StableBTreeMap, StableCell, Storable};
use roaring::RoaringBitmap;

use crate::adjacency::GraphAdjacencyMemory;
use crate::GraphStoreMemorySlots;

pub const VERTEX_LABEL_PROMOTION_THRESHOLD_BASE: usize = 256;
const VERTEX_LABEL_PROMOTION_THRESHOLD_MIN: usize = 64;
const VERTEX_LABEL_PROMOTION_THRESHOLD_MAX: usize = 1024;
const LABEL_CATALOG_METADATA_MAGIC: [u8; 4] = *b"VLM1";
const LABEL_GC_STATE_MAGIC: [u8; 4] = *b"VLG1";
const VERTEX_LABEL_INDEX_MAGIC: [u8; 4] = *b"VLI1";

const LABEL_CATALOG_SLOT_LABELS: u8 = 0;
const LABEL_CATALOG_SLOT_NEXT_LABEL_ID: u8 = 1;

const GC_STATE_SLOT_QUEUE_CURSOR: u8 = 0;
const GC_STATE_SLOT_TOMBSTONES: u8 = 1;
const GC_STATE_SLOT_RECLAIM_QUEUE: u8 = 2;
const GC_STATE_SLOT_RECLAIM_FRONT: u8 = 3;
const GC_STATE_SLOT_RECLAIM_BACK: u8 = 4;
const GC_STATE_SLOT_FREE_LIST: u8 = 5;
const GC_STATE_SLOT_FREE_LIST_LEN: u8 = 6;
const GC_STATE_SLOT_VERTEX_LABEL_INDEX: u8 = 7;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LabelMembershipBlob(Vec<u8>);

impl Storable for LabelMembershipBlob {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }

    const BOUND: Bound = Bound::Unbounded;
}

type LabelCatalogRootMemory<M> = GraphAdjacencyMemory<M>;
type LabelCatalogSubMemory<M> = GraphAdjacencyMemory<LabelCatalogRootMemory<M>>;
type LabelCatalogMap<M> = StableBTreeMap<String, LabelId, LabelCatalogSubMemory<M>>;
type LabelCatalogNextLabelCell<M> = StableCell<LabelId, LabelCatalogSubMemory<M>>;
type GcStateRootMemory<M> = GraphAdjacencyMemory<M>;
type GcStateSubMemory<M> = GraphAdjacencyMemory<GcStateRootMemory<M>>;
type VertexLabelIndexMap<M> = StableBTreeMap<LabelId, LabelMembershipBlob, GcStateSubMemory<M>>;
type GcStateU64Cell<M> = StableCell<u64, GcStateSubMemory<M>>;
type TombstoneMap<M> = StableBTreeMap<u64, u8, GcStateSubMemory<M>>;
type SequenceMap<M> = StableBTreeMap<u64, u32, GcStateSubMemory<M>>;

pub struct LabelCatalogStore<M: Memory + Clone> {
    labels: LabelCatalogMap<M>,
    next_label_id: LabelCatalogNextLabelCell<M>,
}

pub struct VertexLabelStateStore<M: Memory + Clone> {
    index_entries: VertexLabelIndexMap<M>,
    queue_cursor: GcStateU64Cell<M>,
    tombstone_ordinals: TombstoneMap<M>,
    reclaim_queue: SequenceMap<M>,
    reclaim_front: GcStateU64Cell<M>,
    reclaim_back: GcStateU64Cell<M>,
    free_list: SequenceMap<M>,
    free_list_len: GcStateU64Cell<M>,
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

pub fn encode_vertex_label_gc_state(index: &VertexLabelIndex, gc_state: &VertexGcState) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&LABEL_GC_STATE_MAGIC);
    out.extend_from_slice(&1u16.to_le_bytes());
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

pub fn encode_vertex_label_index_blob(index: &VertexLabelIndex) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&VERTEX_LABEL_INDEX_MAGIC);
    out.extend_from_slice(&1u16.to_le_bytes());
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
    out
}

fn encode_label_membership_blob(membership: &LabelMembership) -> Vec<u8> {
    let mut out = Vec::new();
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
    out
}

pub fn encode_label_catalog_metadata(
    next_label_id: LabelId,
    index: &VertexLabelIndex,
    gc_state: &VertexGcState,
) -> Vec<u8> {
    let gc_bytes = encode_vertex_label_gc_state(index, gc_state);
    let mut out = Vec::with_capacity(12 + gc_bytes.len());
    out.extend_from_slice(&LABEL_CATALOG_METADATA_MAGIC);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&next_label_id.to_le_bytes());
    out.extend_from_slice(&(gc_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&gc_bytes);
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

pub fn decode_vertex_label_gc_state(bytes: &[u8]) -> Option<(VertexLabelIndex, VertexGcState)> {
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic).ok()?;
    if magic != LABEL_GC_STATE_MAGIC {
        return None;
    }
    let mut version = [0u8; 2];
    cursor.read_exact(&mut version).ok()?;
    if u16::from_le_bytes(version) != 1 {
        return None;
    }

    let read_u32 = |cursor: &mut Cursor<&[u8]>| -> Option<u32> {
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).ok()?;
        Some(u32::from_le_bytes(buf))
    };

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
                let mut values = Vec::with_capacity(byte_len);
                for _ in 0..byte_len {
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
        VertexLabelIndex { by_label },
        VertexGcState {
            tombstone_ordinals,
            reclaim_queue,
            free_list,
            queue_cursor: u64::from_le_bytes(queue_cursor),
        },
    ))
}

pub fn decode_vertex_label_index_blob(bytes: &[u8]) -> Option<VertexLabelIndex> {
    if bytes.is_empty() {
        return Some(VertexLabelIndex::default());
    }
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic).ok()?;
    if magic != VERTEX_LABEL_INDEX_MAGIC {
        return None;
    }
    let mut version = [0u8; 2];
    cursor.read_exact(&mut version).ok()?;
    if u16::from_le_bytes(version) != 1 {
        return None;
    }

    let read_u32 = |cursor: &mut Cursor<&[u8]>| -> Option<u32> {
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).ok()?;
        Some(u32::from_le_bytes(buf))
    };

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
                let mut values = Vec::with_capacity(byte_len);
                for _ in 0..byte_len {
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
    Some(VertexLabelIndex { by_label })
}

fn decode_label_membership_blob(bytes: &[u8]) -> Option<LabelMembership> {
    let mut cursor = Cursor::new(bytes);
    let mut kind = [0u8; 1];
    cursor.read_exact(&mut kind).ok()?;
    let mut len = [0u8; 4];
    cursor.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len) as usize;
    match kind[0] {
        0 => {
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                let mut value = [0u8; 4];
                cursor.read_exact(&mut value).ok()?;
                values.push(u32::from_le_bytes(value));
            }
            Some(LabelMembership::SmallVec(values))
        }
        1 => {
            let mut data = vec![0u8; len];
            cursor.read_exact(&mut data).ok()?;
            let bitmap = RoaringBitmap::deserialize_from(&mut Cursor::new(data)).ok()?;
            Some(LabelMembership::Roaring(bitmap))
        }
        _ => None,
    }
}

pub fn decode_label_catalog_metadata(
    bytes: &[u8],
) -> Option<(LabelId, VertexLabelIndex, VertexGcState)> {
    if bytes.is_empty() {
        return Some((1, VertexLabelIndex::default(), VertexGcState::default()));
    }
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic).ok()?;
    if magic != LABEL_CATALOG_METADATA_MAGIC {
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
    let mut gc_len = [0u8; 4];
    cursor.read_exact(&mut gc_len).ok()?;
    let gc_len = u32::from_le_bytes(gc_len) as usize;
    let mut gc_bytes = vec![0u8; gc_len];
    cursor.read_exact(&mut gc_bytes).ok()?;
    let (index, gc_state) = decode_vertex_label_gc_state(&gc_bytes)?;
    Some((next_label_id, index, gc_state))
}

impl<M: Memory + Clone> LabelCatalogStore<M> {
    pub fn open(slots: &GraphStoreMemorySlots<M>) -> Self {
        let memory_manager = ic_stable_structures::memory_manager::MemoryManager::init_with_bucket_size(
            slots.label_catalog(),
            1,
        );
        Self {
            labels: StableBTreeMap::init(memory_manager.get(
                ic_stable_structures::memory_manager::MemoryId::new(LABEL_CATALOG_SLOT_LABELS),
            )),
            next_label_id: StableCell::init(
                memory_manager.get(ic_stable_structures::memory_manager::MemoryId::new(
                    LABEL_CATALOG_SLOT_NEXT_LABEL_ID,
                )),
                1,
            ),
        }
    }

    pub fn snapshot_labels_and_next_id(&self) -> (BTreeMap<String, LabelId>, LabelId) {
        let labels = self
            .labels
            .iter()
            .map(|entry| (entry.key().clone(), entry.value()))
            .collect();
        (labels, *self.next_label_id.get())
    }

    pub fn lookup_label_id(&self, name: &str) -> Option<LabelId> {
        self.labels.get(&name.to_owned())
    }

    pub fn next_label_id(&self) -> LabelId {
        *self.next_label_id.get()
    }

    pub fn set_next_label_id(&mut self, next_label_id: LabelId) {
        let _ = self.next_label_id.set(next_label_id);
    }

    pub fn ensure_label_id(&mut self, name: &str) -> LabelId {
        if let Some(existing) = self.lookup_label_id(name) {
            return existing;
        }
        let next_label_id = *self.next_label_id.get();
        let label_id = next_label_id;
        self.labels.insert(name.to_owned(), label_id);
        let _ = self.next_label_id.set(next_label_id.saturating_add(1));
        label_id
    }
}

impl<M: Memory + Clone> VertexLabelStateStore<M> {
    pub fn open(slots: &GraphStoreMemorySlots<M>) -> Self {
        let memory_manager = ic_stable_structures::memory_manager::MemoryManager::init_with_bucket_size(
            slots.gc_state(),
            1,
        );
        Self {
            index_entries: StableBTreeMap::init(
                memory_manager.get(ic_stable_structures::memory_manager::MemoryId::new(
                    GC_STATE_SLOT_VERTEX_LABEL_INDEX,
                )),
            ),
            queue_cursor: StableCell::init(
                memory_manager.get(ic_stable_structures::memory_manager::MemoryId::new(
                    GC_STATE_SLOT_QUEUE_CURSOR,
                )),
                0,
            ),
            tombstone_ordinals: StableBTreeMap::init(memory_manager.get(
                ic_stable_structures::memory_manager::MemoryId::new(GC_STATE_SLOT_TOMBSTONES),
            )),
            reclaim_queue: StableBTreeMap::init(memory_manager.get(
                ic_stable_structures::memory_manager::MemoryId::new(GC_STATE_SLOT_RECLAIM_QUEUE),
            )),
            reclaim_front: StableCell::init(
                memory_manager.get(ic_stable_structures::memory_manager::MemoryId::new(
                    GC_STATE_SLOT_RECLAIM_FRONT,
                )),
                0,
            ),
            reclaim_back: StableCell::init(
                memory_manager.get(ic_stable_structures::memory_manager::MemoryId::new(
                    GC_STATE_SLOT_RECLAIM_BACK,
                )),
                0,
            ),
            free_list: StableBTreeMap::init(memory_manager.get(
                ic_stable_structures::memory_manager::MemoryId::new(GC_STATE_SLOT_FREE_LIST),
            )),
            free_list_len: StableCell::init(
                memory_manager.get(ic_stable_structures::memory_manager::MemoryId::new(
                    GC_STATE_SLOT_FREE_LIST_LEN,
                )),
                0,
            ),
        }
    }

    pub fn load_index_blob(&self) -> VertexLabelIndex {
        let by_label = self
            .index_entries
            .iter()
            .filter_map(|entry| {
                let membership = decode_label_membership_blob(entry.value().0.as_slice())?;
                Some((*entry.key(), membership))
            })
            .collect();
        VertexLabelIndex { by_label }
    }

    pub fn store_index_blob(&mut self, index: &VertexLabelIndex) {
        let keys: Vec<LabelId> = self.index_entries.iter().map(|entry| *entry.key()).collect();
        for key in keys {
            self.index_entries.remove(&key);
        }
        for (label_id, membership) in &index.by_label {
            self.index_entries
                .insert(*label_id, LabelMembershipBlob(encode_label_membership_blob(membership)));
        }
    }

    pub fn store_label_membership(&mut self, label_id: LabelId, membership: &LabelMembership) {
        self.index_entries.insert(
            label_id,
            LabelMembershipBlob(encode_label_membership_blob(membership)),
        );
    }

    pub fn remove_label_membership(&mut self, label_id: LabelId) {
        self.index_entries.remove(&label_id);
    }

    pub fn load_gc_state(&self) -> VertexGcState {
        let tombstone_ordinals = self
            .tombstone_ordinals
            .iter()
            .filter_map(|entry| usize::try_from(*entry.key()).ok())
            .collect();
        let reclaim_queue = self
            .iter_sequence(&self.reclaim_queue, *self.reclaim_front.get(), *self.reclaim_back.get())
            .into_iter()
            .collect();
        let free_list = self
            .iter_sequence(&self.free_list, 0, *self.free_list_len.get());
        VertexGcState {
            tombstone_ordinals,
            reclaim_queue,
            free_list,
            queue_cursor: *self.queue_cursor.get(),
        }
    }

    pub fn store_gc_state(&mut self, gc_state: &VertexGcState) {
        self.replace_tombstones(&gc_state.tombstone_ordinals);
        self.replace_queue(&gc_state.reclaim_queue.iter().copied().collect::<Vec<_>>());
        self.replace_free_list(&gc_state.free_list);
        let _ = self.free_list_len.set(gc_state.free_list.len() as u64);
        let _ = self.queue_cursor.set(gc_state.queue_cursor);
    }

    pub fn enqueue_reclaim(&mut self, ordinal: usize) {
        let Ok(ordinal_u32) = u32::try_from(ordinal) else {
            return;
        };
        let key = ordinal as u64;
        if self.tombstone_ordinals.contains_key(&key) {
            return;
        }
        self.tombstone_ordinals.insert(key, 1);
        let back = *self.reclaim_back.get();
        self.reclaim_queue.insert(back, ordinal_u32);
        let _ = self.reclaim_back.set(back.saturating_add(1));
    }

    pub fn vacuum_step(&mut self, max_ops: usize) -> usize {
        let mut ops = 0usize;
        while ops < max_ops {
            let front = *self.reclaim_front.get();
            let back = *self.reclaim_back.get();
            if front >= back {
                break;
            }
            let Some(ordinal) = self.reclaim_queue.get(&front) else {
                let _ = self.reclaim_front.set(front.saturating_add(1));
                continue;
            };
            self.reclaim_queue.remove(&front);
            let _ = self.reclaim_front.set(front.saturating_add(1));
            let cursor = *self.queue_cursor.get();
            let _ = self.queue_cursor.set(cursor.saturating_add(1));
            self.tombstone_ordinals.remove(&(ordinal as u64));
            let len = *self.free_list_len.get();
            self.free_list.insert(len, ordinal);
            let _ = self.free_list_len.set(len.saturating_add(1));
            ops += 1;
        }
        ops
    }

    pub fn vacuum_stats(&self) -> VacuumStats {
        VacuumStats {
            tombstones: self.tombstone_ordinals.len() as usize,
            queue_len: (*self.reclaim_back.get()).saturating_sub(*self.reclaim_front.get()) as usize,
            free_list_len: *self.free_list_len.get() as usize,
            queue_cursor: *self.queue_cursor.get(),
        }
    }

    fn replace_tombstones(&mut self, tombstones: &BTreeSet<usize>) {
        let keys: Vec<u64> = self
            .tombstone_ordinals
            .iter()
            .map(|entry| *entry.key())
            .collect();
        for key in keys {
            self.tombstone_ordinals.remove(&key);
        }
        for ordinal in tombstones {
            self.tombstone_ordinals.insert(*ordinal as u64, 1);
        }
    }

    fn replace_queue(&mut self, values: &[usize]) {
        let keys: Vec<u64> = self.reclaim_queue.iter().map(|entry| *entry.key()).collect();
        for key in keys {
            self.reclaim_queue.remove(&key);
        }
        let _ = self.reclaim_front.set(0);
        let _ = self.reclaim_back.set(0);
        for (idx, value) in values.iter().enumerate() {
            if let Ok(value) = u32::try_from(*value) {
                self.reclaim_queue.insert(idx as u64, value);
            }
        }
        let _ = self.reclaim_back.set(values.len() as u64);
    }

    fn replace_free_list(&mut self, values: &[usize]) {
        let keys: Vec<u64> = self.free_list.iter().map(|entry| *entry.key()).collect();
        for key in keys {
            self.free_list.remove(&key);
        }
        let _ = self.free_list_len.set(0);
        for (idx, value) in values.iter().enumerate() {
            if let Ok(value) = u32::try_from(*value) {
                self.free_list.insert(idx as u64, value);
            }
        }
    }

    fn iter_sequence(
        &self,
        entries: &SequenceMap<M>,
        start: u64,
        end: u64,
    ) -> Vec<usize> {
        let mut out = Vec::with_capacity(end.saturating_sub(start) as usize);
        for idx in start..end {
            if let Some(value) = entries.get(&idx) {
                out.push(value as usize);
            }
        }
        out
    }
}

pub fn load_vertex_label_catalog_from_slots<M: ic_stable_structures::Memory + Clone>(
    slots: &GraphStoreMemorySlots<M>,
) -> Option<(
    BTreeMap<String, LabelId>,
    LabelId,
    VertexLabelIndex,
    VertexGcState,
)> {
    let store: LabelCatalogStore<M> = LabelCatalogStore::open(slots);
    let state_store: VertexLabelStateStore<M> = VertexLabelStateStore::open(slots);
    let (labels, next_label_id) = store.snapshot_labels_and_next_id();
    Some((
        labels,
        next_label_id,
        state_store.load_index_blob(),
        state_store.load_gc_state(),
    ))
}

pub fn store_vertex_label_catalog_to_slots<M: ic_stable_structures::Memory + Clone>(
    slots: &GraphStoreMemorySlots<M>,
    labels: &BTreeMap<String, LabelId>,
    next_label_id: u16,
    index: &VertexLabelIndex,
    gc_state: &VertexGcState,
) {
    let mut store = LabelCatalogStore::open(slots);
    let mut state_store = VertexLabelStateStore::open(slots);
    let existing_keys: Vec<String> = store.labels.iter().map(|entry| entry.key().clone()).collect();
    for key in existing_keys {
        store.labels.remove(&key);
    }
    for (label, label_id) in labels {
        store.labels.insert(label.clone(), *label_id);
    }
    store.set_next_label_id(next_label_id);
    state_store.store_index_blob(index);
    state_store.store_gc_state(gc_state);
}

#[cfg(test)]
mod tests {
    use super::{
        LabelCatalogStore, LabelMembership, VertexGcState, VertexLabelIndex, VertexLabelStateStore,
        decode_label_catalog_metadata, decode_vertex_label_catalog, decode_vertex_label_gc_state,
        encode_label_catalog_metadata, encode_vertex_label_catalog, encode_vertex_label_gc_state,
        load_vertex_label_catalog_from_slots, store_vertex_label_catalog_to_slots,
    };
    use crate::GraphStoreMemorySlots;
    use gleaph_graph_kernel::LabelId;
    use ic_stable_structures::VectorMemory;
    use roaring::RoaringBitmap;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    #[test]
    fn vertex_label_catalog_round_trips_via_fixed_slot() {
        let slots = GraphStoreMemorySlots::new(VectorMemory::default());
        let labels = BTreeMap::from([
            ("Person".to_owned(), 1 as LabelId),
            ("Knows".to_owned(), 2 as LabelId),
        ]);
        let mut index = VertexLabelIndex::default();
        index.by_label.insert(1, LabelMembership::SmallVec(vec![1, 3, 8]));
        let mut bm = RoaringBitmap::new();
        bm.insert(5);
        bm.insert(7);
        index.by_label.insert(2, LabelMembership::Roaring(bm));
        let gc_state = VertexGcState {
            tombstone_ordinals: BTreeSet::from([9usize, 12usize]),
            reclaim_queue: VecDeque::from([4usize, 6usize]),
            free_list: vec![2usize],
            queue_cursor: 42,
        };

        store_vertex_label_catalog_to_slots(&slots, &labels, 3, &index, &gc_state);
        let decoded = load_vertex_label_catalog_from_slots(&slots).expect("decode from slots");
        assert_eq!(decoded, (labels.clone(), 3, index.clone(), gc_state.clone()));

        let reopened = LabelCatalogStore::open(&slots);
        assert_eq!(reopened.snapshot_labels_and_next_id(), (labels.clone(), 3));
        let reopened_state = VertexLabelStateStore::open(&slots);
        assert_eq!(reopened_state.load_index_blob(), index.clone());
        assert_eq!(reopened_state.load_gc_state(), gc_state.clone());

        let legacy_bytes = encode_vertex_label_catalog(&labels, 3, &index, &gc_state);
        let decoded_direct = decode_vertex_label_catalog(&legacy_bytes).expect("decode direct");
        assert_eq!(decoded_direct, (labels, 3, index, gc_state));
    }

    #[test]
    fn vertex_label_state_store_round_trips_queue_and_free_list() {
        let slots = GraphStoreMemorySlots::new(VectorMemory::default());
        let mut store = VertexLabelStateStore::open(&slots);
        store.enqueue_reclaim(4);
        store.enqueue_reclaim(9);
        let before = store.load_gc_state();
        assert_eq!(before.tombstone_ordinals, BTreeSet::from([4usize, 9usize]));
        assert_eq!(before.reclaim_queue, VecDeque::from([4usize, 9usize]));
        assert!(before.free_list.is_empty());
        assert_eq!(before.queue_cursor, 0);

        assert_eq!(store.vacuum_step(1), 1);
        let after = store.load_gc_state();
        assert_eq!(after.tombstone_ordinals, BTreeSet::from([9usize]));
        assert_eq!(after.reclaim_queue, VecDeque::from([9usize]));
        assert_eq!(after.free_list, vec![4usize]);
        assert_eq!(after.queue_cursor, 1);

        let reopened = VertexLabelStateStore::open(&slots);
        assert_eq!(reopened.load_gc_state(), after);
    }

    #[test]
    fn vertex_label_gc_and_metadata_codecs_round_trip() {
        let mut index = VertexLabelIndex::default();
        index.by_label.insert(7, LabelMembership::SmallVec(vec![1, 2, 9]));
        let gc_state = VertexGcState {
            tombstone_ordinals: BTreeSet::from([3usize]),
            reclaim_queue: VecDeque::from([4usize, 5usize]),
            free_list: vec![8usize],
            queue_cursor: 99,
        };

        let gc_bytes = encode_vertex_label_gc_state(&index, &gc_state);
        let decoded_gc = decode_vertex_label_gc_state(&gc_bytes).expect("decode gc");
        assert_eq!(decoded_gc, (index.clone(), gc_state.clone()));

        let metadata_bytes = encode_label_catalog_metadata(12, &index, &gc_state);
        let decoded_metadata =
            decode_label_catalog_metadata(&metadata_bytes).expect("decode metadata");
        assert_eq!(decoded_metadata, (12, index, gc_state));
    }
}
