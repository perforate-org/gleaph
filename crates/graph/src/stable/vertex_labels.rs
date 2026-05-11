use gleaph_graph_kernel::entry::{LabelId, Vertex};
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::{borrow::Cow, fmt};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VertexLabelSetBlob(Vec<LabelId>);

impl VertexLabelSetBlob {
    pub fn new(labels: impl IntoIterator<Item = LabelId>) -> Result<Self, VertexLabelStoreError> {
        let labels = normalize_labels(labels)?;
        Ok(Self(labels))
    }

    pub fn labels(&self) -> &[LabelId] {
        &self.0
    }
}

impl Storable for VertexLabelSetBlob {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len() * 2);
        for label in self.0 {
            out.extend_from_slice(&label.to_le_bytes());
        }
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        assert!(
            bytes.len().is_multiple_of(2),
            "VertexLabelSetBlob expects an even number of bytes"
        );
        let mut labels = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks_exact(2) {
            labels.push(LabelId::from_le_bytes([chunk[0], chunk[1]]));
        }
        let labels = normalize_labels(labels).expect("VertexLabelSetBlob contains label id 0");
        Self(labels)
    }
}

pub struct VertexLabelStore<M: Memory> {
    sidecars: StableBTreeMap<u32, VertexLabelSetBlob, M>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexLabelStoreError {
    ReservedLabelId(LabelId),
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

    pub fn labels_for(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<LabelId> {
        if vertex.has_label_sidecar()
            && let Some(blob) = self.sidecars.get(&vertex_key(vertex_id))
        {
            return blob.labels().to_vec();
        }
        vertex.primary_label_id().into_iter().collect()
    }

    pub fn set_labels(
        &mut self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = LabelId>,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let labels = normalize_labels(labels)?;
        let key = vertex_key(vertex_id);
        match labels.as_slice() {
            [] => {
                self.sidecars.remove(&key);
                Ok(vertex.with_primary_label_id(None).with_label_sidecar(false))
            }
            [label] => {
                self.sidecars.remove(&key);
                Ok(vertex
                    .with_primary_label_id(Some(*label))
                    .with_label_sidecar(false))
            }
            [primary, ..] => {
                let primary = *primary;
                self.sidecars.insert(key, VertexLabelSetBlob(labels));
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
        label: LabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let mut labels = self.labels_for(vertex_id, vertex);
        labels.push(label);
        self.set_labels(vertex_id, vertex, labels)
    }

    pub fn remove_label(&mut self, vertex_id: VertexId, vertex: Vertex, label: LabelId) -> Vertex {
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
    labels: impl IntoIterator<Item = LabelId>,
) -> Result<Vec<LabelId>, VertexLabelStoreError> {
    let mut labels: Vec<_> = labels.into_iter().collect();
    if let Some(id) = labels.iter().copied().find(|id| id.raw() == 0) {
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
        Vertex {
            base_slot_start: 1,
            live_edge_count: 2,
            metadata: 0,
        }
    }

    #[test]
    fn zero_labels_clear_inline_and_sidecar() {
        let mut store = store();
        let vid = VertexId::from(7);
        let v = store
            .set_labels(vid, vertex(), [LabelId::from_raw(2), LabelId::from_raw(3)])
            .unwrap();

        let v = store.set_labels(vid, v, []).unwrap();

        assert_eq!(v.primary_label_id(), None);
        assert!(!v.has_label_sidecar());
        assert!(store.labels_for(vid, v).is_empty());
    }

    #[test]
    fn one_label_stays_inline_only() {
        let mut store = store();
        let vid = VertexId::from(7);

        let v = store
            .set_labels(vid, vertex(), [LabelId::from_raw(12)])
            .unwrap();

        assert_eq!(v.primary_label_id(), Some(LabelId::from_raw(12)));
        assert!(!v.has_label_sidecar());
        assert_eq!(store.labels_for(vid, v), vec![LabelId::from_raw(12)]);
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
                    LabelId::from_raw(30),
                    LabelId::from_raw(10),
                    LabelId::from_raw(30),
                    LabelId::from_raw(20),
                ],
            )
            .unwrap();

        assert_eq!(v.primary_label_id(), Some(LabelId::from_raw(10)));
        assert!(v.has_label_sidecar());
        assert_eq!(
            store.labels_for(vid, v),
            vec![
                LabelId::from_raw(10),
                LabelId::from_raw(20),
                LabelId::from_raw(30)
            ]
        );
    }

    #[test]
    fn add_and_remove_promote_and_demote_sidecar() {
        let mut store = store();
        let vid = VertexId::from(7);

        let v = store
            .add_label(vid, vertex(), LabelId::from_raw(2))
            .unwrap();
        assert!(!v.has_label_sidecar());

        let v = store.add_label(vid, v, LabelId::from_raw(1)).unwrap();
        assert!(v.has_label_sidecar());
        assert_eq!(
            store.labels_for(vid, v),
            vec![LabelId::from_raw(1), LabelId::from_raw(2)]
        );

        let v = store.remove_label(vid, v, LabelId::from_raw(1));
        assert_eq!(v.primary_label_id(), Some(LabelId::from_raw(2)));
        assert!(!v.has_label_sidecar());
        assert_eq!(store.labels_for(vid, v), vec![LabelId::from_raw(2)]);
    }

    #[test]
    fn persists_sidecars_across_reopen() {
        let mut store = store();
        let vid = VertexId::from(7);
        let v = store
            .set_labels(vid, vertex(), [LabelId::from_raw(1), LabelId::from_raw(2)])
            .unwrap();
        let memory = store.into_memory();

        let reopened = VertexLabelStore::init(memory);

        assert_eq!(
            reopened.labels_for(vid, v),
            vec![LabelId::from_raw(1), LabelId::from_raw(2)]
        );
    }

    #[test]
    fn rejects_reserved_label_id() {
        let mut store = store();

        assert!(matches!(
            store.set_labels(VertexId::from(7), vertex(), [LabelId::default()]),
            Err(VertexLabelStoreError::ReservedLabelId(id)) if id.raw() == 0
        ));
    }
}
