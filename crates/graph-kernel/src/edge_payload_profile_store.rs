//! Stable `EdgeLabelId → EdgePayloadProfile` map (router SSOT per ADR 0008).

use crate::entry::{EdgeLabelId, EdgePayloadProfile, EdgePayloadProfileError, EdgeWeightProfile};
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgePayloadProfileStoreError {
    InvalidCatalogLabel(EdgeLabelId),
    InvalidProfile(EdgePayloadProfileError),
    ProfileAlreadyInstalled(EdgeLabelId),
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
            Self::ProfileAlreadyInstalled(id) => write!(
                f,
                "edge label {} payload profile is already installed",
                id.raw()
            ),
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

    pub fn insert_if_absent(
        &mut self,
        label: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        if self.inner.get(&label).is_some() {
            return Ok(());
        }
        self.insert(label, profile)
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

    pub fn catalog_label_ids(&self) -> Vec<EdgeLabelId> {
        self.inner.iter().map(|entry| *entry.key()).collect()
    }

    pub fn label_ids_with_nonzero_payload(&self) -> Vec<EdgeLabelId> {
        self.inner
            .iter()
            .filter_map(|entry| {
                if entry.value().required_byte_width() > 0 {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn into_memory(self) -> M {
        self.inner.into_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{EdgeLabelId, EdgePayloadEncoding, EdgePayloadProfile};
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
