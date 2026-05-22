//! Optional [`gleaph_graph_kernel::entry::EdgeValueProfile`] per catalog [`EdgeLabelId`].

use gleaph_graph_kernel::entry::{
    EdgeLabelId, EdgeValueProfile, EdgeValueProfileError, EdgeWeightProfile,
};
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeValueProfileStoreError {
    InvalidCatalogLabel(EdgeLabelId),
    InvalidProfile(EdgeValueProfileError),
}

impl fmt::Display for EdgeValueProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCatalogLabel(id) => {
                write!(
                    f,
                    "edge value profiles require catalog edge label id {}",
                    id.raw()
                )
            }
            Self::InvalidProfile(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EdgeValueProfileStoreError {}

pub struct EdgeValueProfileStore<M: Memory> {
    inner: StableBTreeMap<EdgeLabelId, EdgeValueProfile, M>,
}

impl<M: Memory> EdgeValueProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, label: EdgeLabelId) -> Option<EdgeValueProfile> {
        self.inner.get(&label)
    }

    pub fn insert(
        &mut self,
        label: EdgeLabelId,
        profile: EdgeValueProfile,
    ) -> Result<(), EdgeValueProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgeValueProfileStoreError::InvalidCatalogLabel(label));
        }
        profile
            .validate()
            .map_err(EdgeValueProfileStoreError::InvalidProfile)?;
        self.inner.insert(label, profile);
        Ok(())
    }

    pub fn insert_from_weight_profile(
        &mut self,
        label: EdgeLabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), EdgeValueProfileStoreError> {
        self.insert(label, EdgeValueProfile::from(profile))
    }

    pub fn remove(&mut self, label: EdgeLabelId) {
        self.inner.remove(&label);
    }

    pub fn into_memory(self) -> M {
        self.inner.into_memory()
    }
}
