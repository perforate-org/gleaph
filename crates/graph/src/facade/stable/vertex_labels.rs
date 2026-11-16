use gleaph_graph_kernel::entry::VertexLabelId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::{borrow::Cow, fmt};

/// Wire-layout version of the [`VertexLabelSetBlob`] envelope (region 32).
/// Writers prepend this byte; readers reject every other first byte so future
/// schema revisions stay additive instead of silently misparsing.
const LAYOUT_VERSION_V1: u8 = 1;

/// Versioned stable label sidecar value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexLabelSetBlob {
    V1(Vec<VertexLabelId>),
}

impl Default for VertexLabelSetBlob {
    fn default() -> Self {
        Self::V1(Vec::new())
    }
}

impl VertexLabelSetBlob {
    pub fn new(
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<Self, VertexLabelStoreError> {
        Ok(Self::V1(normalize_labels(labels)?))
    }

    pub fn labels(&self) -> &[VertexLabelId] {
        match self {
            VertexLabelSetBlob::V1(labels) => labels,
        }
    }
}

impl Storable for VertexLabelSetBlob {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let VertexLabelSetBlob::V1(labels) = self;
        let mut out = Vec::with_capacity(1 + labels.len() * 2);
        out.push(LAYOUT_VERSION_V1);
        for label in labels {
            out.extend_from_slice(&label.to_le_bytes());
        }
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let Some((version, label_bytes)) = bytes.split_first() else {
            panic!("vertex label set truncated: missing layout version byte");
        };
        assert_eq!(
            *version, LAYOUT_VERSION_V1,
            "unknown vertex label set layout version {version}"
        );
        assert!(
            label_bytes.len().is_multiple_of(2),
            "VertexLabelSetBlob expects an even number of bytes"
        );
        let mut labels = Vec::with_capacity(label_bytes.len() / 2);
        for chunk in label_bytes.as_chunks::<2>().0.iter() {
            labels.push(VertexLabelId::from_le_bytes([chunk[0], chunk[1]]));
        }
        let labels = normalize_labels(labels).expect("VertexLabelSetBlob contains label id 0");
        Self::V1(labels)
    }
}

/// One label-membership posting: vertex `vertex` carries label `label`.
///
/// Ordered by `(label, vertex)` so every vertex of one label is a contiguous ascending key range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LabelPosting {
    label: VertexLabelId,
    vertex: u32,
}

impl LabelPosting {
    const BYTES: usize = 6;
}

impl Storable for LabelPosting {
    const BOUND: Bound = Bound::Bounded {
        max_size: Self::BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::BYTES);
        out.extend_from_slice(&self.label.raw().to_be_bytes());
        out.extend_from_slice(&self.vertex.to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), Self::BYTES, "label posting key is 6 bytes");
        Self {
            label: VertexLabelId::from_raw(u16::from_be_bytes([bytes[0], bytes[1]])),
            vertex: u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]),
        }
    }
}

/// Vertex label membership in both directions (ADR 0123).
///
/// `sets` (vertex -> label set) is the canonical record; `postings` ((label, vertex) -> ()) is its
/// inverse, so a labeled scan reads only the vertices that carry the label. [`Self::set_labels`] is
/// the only writer of either map and writes them together, so the two cannot disagree.
pub struct VertexLabelStore<M: Memory> {
    sets: StableBTreeMap<u32, VertexLabelSetBlob, M>,
    postings: StableBTreeMap<LabelPosting, (), M>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexLabelStoreError {
    ReservedLabelId(VertexLabelId),
}

impl fmt::Display for VertexLabelStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedLabelId(id) => write!(f, "label id {} is reserved", id.raw()),
        }
    }
}

impl std::error::Error for VertexLabelStoreError {}

impl<M: Memory> VertexLabelStore<M> {
    pub fn init(sets_memory: M, postings_memory: M) -> Self {
        Self {
            sets: StableBTreeMap::init(sets_memory),
            postings: StableBTreeMap::init(postings_memory),
        }
    }

    pub fn labels_for(&self, vertex_id: VertexId) -> Vec<VertexLabelId> {
        self.with_label_ids(vertex_id, <[VertexLabelId]>::to_vec)
    }

    /// Runs `f` on the vertex's sorted label-id slice (empty for an unlabeled vertex).
    pub(crate) fn with_label_ids<R>(
        &self,
        vertex_id: VertexId,
        f: impl FnOnce(&[VertexLabelId]) -> R,
    ) -> R {
        match self.sets.get(&vertex_key(vertex_id)) {
            Some(blob) => f(blob.labels()),
            None => f(&[]),
        }
    }

    /// Appends to `out`, in ascending id order, up to `limit` vertices that carry `label` and whose
    /// id is at least `from`. Returns the id to resume from, or `None` once the label is exhausted.
    pub fn vertices_with_label(
        &self,
        label: VertexLabelId,
        from: u32,
        limit: usize,
        out: &mut Vec<VertexId>,
    ) -> Option<u32> {
        let range = LabelPosting {
            label,
            vertex: from,
        }..=LabelPosting {
            label,
            vertex: u32::MAX,
        };
        let start = out.len();
        out.extend(
            self.postings
                .keys_range(range)
                .take(limit)
                .map(|posting| VertexId::from(posting.vertex)),
        );
        if out.len() - start < limit {
            return None;
        }
        out.last().and_then(|last| u32::from(*last).checked_add(1))
    }

    /// Replaces the vertex's label set, keeping `postings` the exact inverse of `sets`.
    pub fn set_labels(
        &mut self,
        vertex_id: VertexId,
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<(), VertexLabelStoreError> {
        let next = normalize_labels(labels)?;
        let key = vertex_key(vertex_id);
        // The set write hands back the replaced set, so the posting diff costs no extra read.
        let prev = if next.is_empty() {
            self.sets.remove(&key)
        } else {
            self.sets.insert(key, VertexLabelSetBlob::V1(next.clone()))
        }
        .unwrap_or_default();
        for &label in prev.labels().iter().filter(|label| !next.contains(label)) {
            self.postings.remove(&LabelPosting { label, vertex: key });
        }
        for &label in next.iter().filter(|label| !prev.labels().contains(label)) {
            self.postings
                .insert(LabelPosting { label, vertex: key }, ());
        }
        Ok(())
    }

    pub fn add_label(
        &mut self,
        vertex_id: VertexId,
        label: VertexLabelId,
    ) -> Result<(), VertexLabelStoreError> {
        let mut labels = self.labels_for(vertex_id);
        labels.push(label);
        self.set_labels(vertex_id, labels)
    }

    pub fn remove_label(&mut self, vertex_id: VertexId, label: VertexLabelId) {
        let labels = self
            .labels_for(vertex_id)
            .into_iter()
            .filter(|current| *current != label);
        self.set_labels(vertex_id, labels)
            .expect("removing a label cannot introduce reserved label id 0")
    }

    pub fn into_memory(self) -> (M, M) {
        (self.sets.into_memory(), self.postings.into_memory())
    }
}

fn vertex_key(vertex_id: VertexId) -> u32 {
    vertex_id.into()
}

fn normalize_labels(
    labels: impl IntoIterator<Item = VertexLabelId>,
) -> Result<Vec<VertexLabelId>, VertexLabelStoreError> {
    let mut labels: Vec<_> = labels.into_iter().collect();
    if let Some(id) = labels.iter().copied().find(|id| id.is_reserved()) {
        return Err(VertexLabelStoreError::ReservedLabelId(id));
    }
    labels.sort_unstable();
    labels.dedup();
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn store() -> VertexLabelStore<VectorMemory> {
        VertexLabelStore::init(VectorMemory::default(), VectorMemory::default())
    }

    #[test]
    fn roundtrips_empty_label_set_with_version_prefix() {
        let blob = VertexLabelSetBlob::default();
        assert!(blob.labels().is_empty());

        let bytes = blob.clone().into_bytes();
        assert_eq!(
            bytes,
            vec![LAYOUT_VERSION_V1],
            "an empty label set still carries the v1 layout-version prefix"
        );
        assert_eq!(VertexLabelSetBlob::from_bytes(Cow::Owned(bytes)), blob);
    }

    #[test]
    fn roundtrips_sorted_deduped_label_sets() {
        let blob = VertexLabelSetBlob::new([
            VertexLabelId::from_raw(1 + 30),
            VertexLabelId::from_raw(1 + 10),
            VertexLabelId::from_raw(1 + 20),
            VertexLabelId::from_raw(1 + 10),
        ])
        .expect("non-reserved label ids");
        let expected = vec![
            VertexLabelId::from_raw(1 + 10),
            VertexLabelId::from_raw(1 + 20),
            VertexLabelId::from_raw(1 + 30),
        ];
        assert_eq!(blob.labels(), expected);

        let bytes = blob.clone().into_bytes();
        assert_eq!(bytes[0], LAYOUT_VERSION_V1);
        assert_eq!(bytes.len(), 1 + expected.len() * 2);
        assert_eq!(
            VertexLabelSetBlob::from_bytes(Cow::Borrowed(&bytes)),
            blob,
            "v1 wire bytes must round-trip"
        );
    }

    #[test]
    fn construction_rejects_reserved_label_id_zero() {
        assert!(matches!(
            VertexLabelSetBlob::new([VertexLabelId::from_raw(0)]),
            Err(VertexLabelStoreError::ReservedLabelId(id)) if id.raw() == 0
        ));
    }

    #[test]
    #[should_panic(expected = "VertexLabelSetBlob contains label id 0")]
    fn decode_rejects_reserved_label_id_zero() {
        VertexLabelSetBlob::from_bytes(Cow::Borrowed(&[LAYOUT_VERSION_V1, 0x00, 0x00]));
    }

    #[test]
    #[should_panic(expected = "missing layout version byte")]
    fn empty_blob_payload_is_rejected() {
        VertexLabelSetBlob::from_bytes(Cow::Borrowed(&[]));
    }

    #[test]
    #[should_panic(expected = "unknown vertex label set layout version")]
    fn blob_rejects_unknown_layout_version_2() {
        let mut bytes = VertexLabelSetBlob::new([VertexLabelId::from_raw(1 + 12)])
            .expect("non-reserved label id")
            .into_bytes();
        bytes[0] = 0x02;
        VertexLabelSetBlob::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "unknown vertex label set layout version")]
    fn blob_rejects_unknown_layout_version_255() {
        let mut bytes = VertexLabelSetBlob::new([VertexLabelId::from_raw(1 + 12)])
            .expect("non-reserved label id")
            .into_bytes();
        bytes[0] = 0xFF;
        VertexLabelSetBlob::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "even number of bytes")]
    fn truncated_blob_v1_payload_is_rejected() {
        // Version byte followed by one byte of a u16 label id.
        VertexLabelSetBlob::from_bytes(Cow::Borrowed(&[LAYOUT_VERSION_V1, 0x0D]));
    }

    fn l(raw: u16) -> VertexLabelId {
        VertexLabelId::from_raw(raw)
    }

    /// Every vertex of `label` through the paged posting read, with a page size that forces resumes.
    fn postings(store: &VertexLabelStore<VectorMemory>, label: VertexLabelId) -> Vec<u32> {
        let mut out = Vec::new();
        let mut from = Some(0);
        while let Some(start) = from {
            from = store.vertices_with_label(label, start, 2, &mut out);
        }
        out.into_iter().map(u32::from).collect()
    }

    /// `postings` is the exact inverse of `sets` over `vertices` and `labels`.
    fn assert_inverse(store: &VertexLabelStore<VectorMemory>, vertices: &[u32], labels: &[u16]) {
        for &raw in labels {
            let expected: Vec<u32> = vertices
                .iter()
                .copied()
                .filter(|&v| store.labels_for(VertexId::from(v)).contains(&l(raw)))
                .collect();
            assert_eq!(postings(store, l(raw)), expected, "postings of label {raw}");
        }
    }

    #[test]
    fn posting_key_roundtrips_and_orders_by_label_then_vertex() {
        let a = LabelPosting {
            label: l(2),
            vertex: u32::MAX,
        };
        let b = LabelPosting {
            label: l(3),
            vertex: 0,
        };
        assert!(a < b);
        assert_eq!(LabelPosting::from_bytes(Cow::Owned(a.into_bytes())), a);
        assert_eq!(a.into_bytes().len(), LabelPosting::BYTES);
    }

    #[test]
    fn insert_publishes_one_posting_per_label() {
        let mut store = store();
        store
            .set_labels(VertexId::from(7), [l(30), l(10), l(30)])
            .unwrap();
        store.set_labels(VertexId::from(3), [l(10)]).unwrap();

        assert_eq!(store.labels_for(VertexId::from(7)), vec![l(10), l(30)]);
        assert_eq!(postings(&store, l(10)), vec![3, 7]);
        assert_eq!(postings(&store, l(30)), vec![7]);
        assert_eq!(postings(&store, l(20)), Vec::<u32>::new());
        assert_inverse(&store, &[3, 7], &[10, 20, 30]);
    }

    #[test]
    fn label_change_moves_only_the_changed_postings() {
        let mut store = store();
        let vid = VertexId::from(7);
        store.set_labels(vid, [l(1), l(2)]).unwrap();
        store.set_labels(VertexId::from(8), [l(2)]).unwrap();

        store.set_labels(vid, [l(2), l(3)]).unwrap();
        assert_eq!(postings(&store, l(1)), Vec::<u32>::new());
        assert_eq!(postings(&store, l(2)), vec![7, 8]);
        assert_eq!(postings(&store, l(3)), vec![7]);

        store.add_label(vid, l(1)).unwrap();
        store.remove_label(vid, l(2));
        assert_eq!(store.labels_for(vid), vec![l(1), l(3)]);
        assert_eq!(postings(&store, l(2)), vec![8]);
        assert_inverse(&store, &[7, 8], &[1, 2, 3]);
    }

    #[test]
    fn clearing_labels_on_delete_drops_set_and_postings() {
        let mut store = store();
        let vid = VertexId::from(7);
        store.set_labels(vid, [l(2), l(3)]).unwrap();
        store.set_labels(VertexId::from(9), [l(3)]).unwrap();

        store.set_labels(vid, []).unwrap();

        assert!(store.labels_for(vid).is_empty());
        assert!(
            !store.sets.contains_key(&7),
            "an unlabeled vertex keeps no set row"
        );
        assert_eq!(postings(&store, l(2)), Vec::<u32>::new());
        assert_eq!(postings(&store, l(3)), vec![9]);
        assert_inverse(&store, &[7, 9], &[2, 3]);
    }

    #[test]
    fn rejected_label_set_writes_nothing() {
        let mut store = store();
        let vid = VertexId::from(7);
        store.set_labels(vid, [l(2)]).unwrap();

        assert!(matches!(
            store.set_labels(vid, [l(3), VertexLabelId::default()]),
            Err(VertexLabelStoreError::ReservedLabelId(id)) if id.raw() == 0
        ));
        assert_eq!(store.labels_for(vid), vec![l(2)]);
        assert_inverse(&store, &[7], &[2, 3]);
    }

    #[test]
    fn paged_reads_resume_and_stop_at_the_label_boundary() {
        let mut store = store();
        for v in 0..5 {
            store.set_labels(VertexId::from(v), [l(4)]).unwrap();
        }
        store.set_labels(VertexId::from(2), [l(4), l(5)]).unwrap();

        let mut out = Vec::new();
        assert_eq!(store.vertices_with_label(l(4), 0, 2, &mut out), Some(2));
        assert_eq!(store.vertices_with_label(l(4), 2, 2, &mut out), Some(4));
        assert_eq!(store.vertices_with_label(l(4), 4, 2, &mut out), None);
        assert_eq!(
            out.into_iter().map(u32::from).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );

        let mut out = Vec::new();
        assert_eq!(
            store.vertices_with_label(l(5), 0, 8, &mut out),
            None,
            "a short page ends the label"
        );
        assert_eq!(out, vec![VertexId::from(2)]);
    }

    #[test]
    fn persists_sets_and_postings_across_reopen() {
        let mut store = store();
        let vid = VertexId::from(7);
        store.set_labels(vid, [l(1), l(2)]).unwrap();
        let (sets, postings_memory) = store.into_memory();

        let reopened = VertexLabelStore::init(sets, postings_memory);

        assert_eq!(reopened.labels_for(vid), vec![l(1), l(2)]);
        assert_eq!(postings(&reopened, l(2)), vec![7]);
    }
}
