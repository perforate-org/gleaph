//! Optional [`gleaph_graph_kernel::entry::EdgePayloadProfile`] per catalog [`EdgeLabelId`].

use gleaph_graph_kernel::entry::{
    EdgeLabelId, EdgePayloadProfile, EdgePayloadProfileError, EdgeWeightProfile,
};
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

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
    inner: StableBTreeMap<EdgeLabelId, EdgePayloadProfile, M>,
}

impl<M: Memory> EdgePayloadProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, label: EdgeLabelId) -> Option<EdgePayloadProfile> {
        self.inner.get(&label)
    }

    pub fn insert(
        &mut self,
        label: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgePayloadProfileStoreError::InvalidCatalogLabel(label));
        }
        profile
            .validate()
            .map_err(EdgePayloadProfileStoreError::InvalidProfile)?;
        self.inner.insert(label, profile);
        Ok(())
    }

    pub fn insert_from_weight_profile(
        &mut self,
        label: EdgeLabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        self.insert(label, EdgePayloadProfile::from(profile))
    }

    pub fn remove(&mut self, label: EdgeLabelId) {
        self.inner.remove(&label);
    }

    pub fn into_memory(self) -> M {
        self.inner.into_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::{EdgeLabelId, EdgePayloadEncoding, EdgePayloadProfile};
    use ic_stable_structures::VectorMemory;
    use std::{cell::RefCell, rc::Rc};

    fn mem() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn insert_rejects_invalid_profile_encoding() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(1);
        let profile = EdgePayloadProfile {
            byte_width: 4,
            encoding: EdgePayloadEncoding::WeightRawU16,
        };
        assert!(matches!(
            store.insert(label, profile),
            Err(EdgePayloadProfileStoreError::InvalidProfile(
                EdgePayloadProfileError::WidthEncodingMismatch
            ))
        ));
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(2);
        let profile = EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightRawU16,
        };
        store.insert(label, profile.clone()).expect("insert");
        assert_eq!(store.get(label), Some(profile));
    }
}
