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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeValueEncoding, EdgeValueProfile};
    use ic_stable_structures::VectorMemory;
    use std::{cell::RefCell, rc::Rc};

    fn mem() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn insert_rejects_invalid_profile_encoding() {
        let mut store = EdgeValueProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(1);
        let profile = EdgeValueProfile {
            byte_width: 4,
            encoding: EdgeValueEncoding::WeightRawU16,
        };
        assert!(matches!(
            store.insert(label, profile),
            Err(EdgeValueProfileStoreError::InvalidProfile(
                EdgeValueProfileError::WidthEncodingMismatch
            ))
        ));
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut store = EdgeValueProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(2);
        let profile = EdgeValueProfile {
            byte_width: 2,
            encoding: EdgeValueEncoding::WeightRawU16,
        };
        store.insert(label, profile.clone()).expect("insert");
        assert_eq!(store.get(label), Some(profile));
    }
}
