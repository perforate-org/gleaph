use gleaph_graph_kernel::entry::{Vertex, VertexLabelId};
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

pub struct VertexLabelStore<M: Memory> {
    sidecars: StableBTreeMap<u32, VertexLabelSetBlob, M>,
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
    pub fn init(memory: M) -> Self {
        Self {
            sidecars: StableBTreeMap::init(memory),
        }
    }

    pub fn labels_for(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<VertexLabelId> {
        if let Some(blob) = self.sidecars.get(&vertex_key(vertex_id)) {
            return blob.labels().to_vec();
        }
        vertex.primary_label_id().into_iter().collect()
    }

    /// Runs `f` on the resolved label-id slice without allocating a `Vec<VertexLabelId>` for the
    /// common sidecar path.
    pub(crate) fn with_label_ids<R>(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        f: impl FnOnce(&[VertexLabelId]) -> R,
    ) -> R {
        if let Some(blob) = self.sidecars.get(&vertex_key(vertex_id)) {
            f(blob.labels())
        } else {
            match vertex.primary_label_id() {
                Some(id) => {
                    let buf = [id];
                    f(&buf)
                }
                None => f(&[]),
            }
        }
    }

    pub fn set_labels(
        &mut self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let labels = normalize_labels(labels)?;
        let key = vertex_key(vertex_id);
        match labels.as_slice() {
            [] => {
                self.sidecars.remove(&key);
                Ok(vertex.with_primary_label_id(None).with_label_sidecar(false))
            }
            slice => {
                let primary = slice[0];
                self.sidecars
                    .insert(key, VertexLabelSetBlob::new(slice.iter().copied())?);
                Ok(vertex
                    .with_primary_label_id(Some(primary))
                    .with_label_sidecar(true))
            }
        }
    }

    pub fn add_label(
        &mut self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let mut labels = self.labels_for(vertex_id, vertex);
        labels.push(label);
        self.set_labels(vertex_id, vertex, labels)
    }

    pub fn remove_label(
        &mut self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Vertex {
        let labels = self
            .labels_for(vertex_id, vertex)
            .into_iter()
            .filter(|current| *current != label);
        self.set_labels(vertex_id, vertex, labels)
            .expect("removing a label cannot introduce reserved label id 0")
    }

    pub fn into_memory(self) -> M {
        self.sidecars.into_memory()
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
        VertexLabelStore::init(VectorMemory::default())
    }

    fn vertex() -> Vertex {
        Vertex::default()
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

    #[test]
    fn zero_labels_clear_inline_and_sidecar() {
        let mut store = store();
        let vid = VertexId::from(7);
        let v = store
            .set_labels(
                vid,
                vertex(),
                [
                    VertexLabelId::from_raw(1 + 2),
                    VertexLabelId::from_raw(1 + 3),
                ],
            )
            .unwrap();

        let v = store.set_labels(vid, v, []).unwrap();

        assert_eq!(v.primary_label_id(), None);
        assert!(!v.has_label_sidecar());
        assert!(store.labels_for(vid, v).is_empty());
    }

    #[test]
    fn one_label_persists_in_sidecar() {
        let mut store = store();
        let vid = VertexId::from(7);

        let v = store
            .set_labels(vid, vertex(), [VertexLabelId::from_raw(1 + 12)])
            .unwrap();

        assert_eq!(
            store.labels_for(vid, v),
            vec![VertexLabelId::from_raw(1 + 12)]
        );
    }

    #[test]
    fn multiple_labels_use_sorted_sidecar_and_primary_hint() {
        let mut store = store();
        let vid = VertexId::from(7);

        let v = store
            .set_labels(
                vid,
                vertex(),
                [
                    VertexLabelId::from_raw(1 + 30),
                    VertexLabelId::from_raw(1 + 10),
                    VertexLabelId::from_raw(1 + 30),
                    VertexLabelId::from_raw(1 + 20),
                ],
            )
            .unwrap();

        assert_eq!(
            store.labels_for(vid, v),
            vec![
                VertexLabelId::from_raw(1 + 10),
                VertexLabelId::from_raw(1 + 20),
                VertexLabelId::from_raw(1 + 30)
            ]
        );
    }

    #[test]
    fn add_and_remove_promote_and_demote_sidecar() {
        let mut store = store();
        let vid = VertexId::from(7);

        let v = store
            .add_label(vid, vertex(), VertexLabelId::from_raw(1 + 2))
            .unwrap();
        assert_eq!(
            store.labels_for(vid, v),
            vec![VertexLabelId::from_raw(1 + 2)]
        );

        let v = store
            .add_label(vid, v, VertexLabelId::from_raw(1 + 1))
            .unwrap();
        assert_eq!(
            store.labels_for(vid, v),
            vec![
                VertexLabelId::from_raw(1 + 1),
                VertexLabelId::from_raw(1 + 2)
            ]
        );

        let v = store.remove_label(vid, v, VertexLabelId::from_raw(1 + 1));
        assert_eq!(
            store.labels_for(vid, v),
            vec![VertexLabelId::from_raw(1 + 2)]
        );
    }

    #[test]
    fn persists_sidecars_across_reopen() {
        let mut store = store();
        let vid = VertexId::from(7);
        let v = store
            .set_labels(
                vid,
                vertex(),
                [
                    VertexLabelId::from_raw(1 + 1),
                    VertexLabelId::from_raw(1 + 2),
                ],
            )
            .unwrap();
        let memory = store.into_memory();

        let reopened = VertexLabelStore::init(memory);

        assert_eq!(
            reopened.labels_for(vid, v),
            vec![
                VertexLabelId::from_raw(1 + 1),
                VertexLabelId::from_raw(1 + 2)
            ]
        );
    }

    #[test]
    fn rejects_reserved_label_id() {
        let mut store = store();

        assert!(matches!(
            store.set_labels(VertexId::from(7), vertex(), [VertexLabelId::default()]),
            Err(VertexLabelStoreError::ReservedLabelId(id)) if id.raw() == 0
        ));
    }
}
