//! Optional [`gleaph_graph_kernel::entry::EdgeWeightProfile`] per catalog [`EdgeLabelId`].

use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeWeightProfile, WeightProfilePrepareError};
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeWeightProfileStoreError {
    InvalidCatalogLabel(EdgeLabelId),
    InvalidProfile(WeightProfilePrepareError),
}

impl fmt::Display for EdgeWeightProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCatalogLabel(id) => {
                write!(
                    f,
                    "weight profiles require catalog edge label id {}",
                    id.raw()
                )
            }
            Self::InvalidProfile(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EdgeWeightProfileStoreError {}

pub struct EdgeWeightProfileStore<M: Memory> {
    inner: StableBTreeMap<EdgeLabelId, EdgeWeightProfile, M>,
}

impl<M: Memory> EdgeWeightProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, label: EdgeLabelId) -> Option<EdgeWeightProfile> {
        self.inner.get(&label)
    }

    pub fn insert(
        &mut self,
        label: EdgeLabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), EdgeWeightProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgeWeightProfileStoreError::InvalidCatalogLabel(label));
        }
        profile
            .validate()
            .map_err(EdgeWeightProfileStoreError::InvalidProfile)?;
        self.inner.insert(label, profile);
        Ok(())
    }

    pub fn remove(&mut self, label: EdgeLabelId) {
        self.inner.remove(&label);
    }

    pub fn into_memory(self) -> M {
        self.inner.into_memory()
    }
}
