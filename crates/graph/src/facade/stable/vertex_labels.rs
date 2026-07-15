use gleaph_graph_kernel::entry::{Vertex, VertexLabelId};
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::{borrow::Cow, fmt};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VertexLabelSetBlob(Vec<VertexLabelId>);

impl VertexLabelSetBlob {
    pub fn new(
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<Self, VertexLabelStoreError> {
        let labels = normalize_labels(labels)?;
        Ok(Self(labels))
    }

    pub fn labels(&self) -> &[VertexLabelId] {
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
        for chunk in bytes.as_chunks::<2>().0.iter() {
            labels.push(VertexLabelId::from_le_bytes([chunk[0], chunk[1]]));
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
