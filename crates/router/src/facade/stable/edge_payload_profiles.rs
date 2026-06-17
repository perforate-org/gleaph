//! Router SSOT for `(GraphId, EdgeLabelId) → EdgePayloadProfile` (ADR 0008, ADR 0018).

use std::fmt;

use gleaph_graph_kernel::entry::{
    EdgeLabelId, EdgePayloadProfile, EdgePayloadProfileError, GraphId,
};
use gleaph_graph_kernel::scoped_name_catalog::GraphScopedIdKey;
use ic_stable_structures::{Memory, StableBTreeMap};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgePayloadProfileStoreError {
    InvalidCatalogLabel(EdgeLabelId),
    InvalidProfile(EdgePayloadProfileError),
}

impl fmt::Display for EdgePayloadProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCatalogLabel(id) => {
                write!(
                    f,
                    "edge payload profiles require catalog edge label id {}",
                    id.raw()
                )
            }
            Self::InvalidProfile(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EdgePayloadProfileStoreError {}

pub struct EdgePayloadProfileStore<M: Memory> {
    inner: StableBTreeMap<GraphScopedIdKey<EdgeLabelId>, EdgePayloadProfile, M>,
}

impl<M: Memory> EdgePayloadProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, graph_id: GraphId, label: EdgeLabelId) -> Option<EdgePayloadProfile> {
        self.inner.get(&GraphScopedIdKey {
            graph_id,
            id: label,
        })
    }

    pub fn insert(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgePayloadProfileStoreError::InvalidCatalogLabel(label));
        }
        profile
            .validate()
            .map_err(EdgePayloadProfileStoreError::InvalidProfile)?;
        self.inner.insert(
            GraphScopedIdKey {
                graph_id,
                id: label,
            },
            profile,
        );
        Ok(())
    }

    pub fn insert_if_absent(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        if self.get(graph_id, label).is_some() {
            return Ok(());
        }
        self.insert(graph_id, label, profile)
    }

    pub fn label_ids_with_nonzero_payload(&self, graph_id: GraphId) -> Vec<EdgeLabelId> {
        self.inner
            .iter()
            .filter_map(|entry| {
                let key = entry.key();
                (key.graph_id == graph_id && entry.value().required_byte_width() > 0)
                    .then_some(key.id)
            })
            .collect()
    }

    pub fn remove_graph(&mut self, graph_id: GraphId) {
        let mut keys = Vec::new();
        for entry in self.inner.iter() {
            if entry.key().graph_id == graph_id {
                keys.push(*entry.key());
            }
        }
        for key in keys {
            self.inner.remove(&key);
        }
    }
}
