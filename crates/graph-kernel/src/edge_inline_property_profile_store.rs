//! Stable `EdgeLabelId → EdgeInlinePropertyProfile` map (router SSOT per ADR 0008).

use crate::entry::{EdgeInlinePropertyProfile, EdgeInlinePropertyProfileError, EdgeLabelId};
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeInlinePropertyProfileStoreError {
    InvalidCatalogLabel(EdgeLabelId),
    InvalidProfile(EdgeInlinePropertyProfileError),
    ProfileAlreadyInstalled(EdgeLabelId),
}

impl fmt::Display for EdgeInlinePropertyProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCatalogLabel(id) => {
                write!(
                    f,
                    "edge inline value profiles require catalog edge label id {}",
                    id.raw()
                )
            }
            Self::InvalidProfile(e) => write!(f, "{e}"),
            Self::ProfileAlreadyInstalled(id) => write!(
                f,
                "edge label {} inline property profile is already installed",
                id.raw()
            ),
        }
    }
}

impl std::error::Error for EdgeInlinePropertyProfileStoreError {}

pub struct EdgeInlinePropertyProfileStore<M: Memory> {
    inner: StableBTreeMap<EdgeLabelId, EdgeInlinePropertyProfile, M>,
}

impl<M: Memory> EdgeInlinePropertyProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, label: EdgeLabelId) -> Option<EdgeInlinePropertyProfile> {
        self.inner.get(&label)
    }

    pub fn insert(
        &mut self,
        label: EdgeLabelId,
        profile: EdgeInlinePropertyProfile,
    ) -> Result<(), EdgeInlinePropertyProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgeInlinePropertyProfileStoreError::InvalidCatalogLabel(
                label,
            ));
        }
        profile
            .validate()
            .map_err(EdgeInlinePropertyProfileStoreError::InvalidProfile)?;
        self.inner.insert(label, profile);
        Ok(())
    }

    pub fn insert_if_absent(
        &mut self,
        label: EdgeLabelId,
        profile: EdgeInlinePropertyProfile,
    ) -> Result<(), EdgeInlinePropertyProfileStoreError> {
        if self.inner.get(&label).is_some() {
            return Ok(());
        }
        self.insert(label, profile)
    }

    pub fn remove(&mut self, label: EdgeLabelId) {
        self.inner.remove(&label);
    }

    pub fn catalog_label_ids(&self) -> Vec<EdgeLabelId> {
        self.inner.iter().map(|entry| *entry.key()).collect()
    }

    pub fn label_ids_with_nonzero_inline_property_bytes(&self) -> Vec<EdgeLabelId> {
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
    use crate::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId};
    use ic_stable_structures::VectorMemory;
    use std::{cell::RefCell, rc::Rc};

    fn mem() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn insert_rejects_invalid_profile_encoding() {
        let mut store = EdgeInlinePropertyProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(1);
        let profile = EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::RawU16,
        };
        assert!(matches!(
            store.insert(label, profile),
            Err(EdgeInlinePropertyProfileStoreError::InvalidProfile(
                EdgeInlinePropertyProfileError::WidthEncodingMismatch
            ))
        ));
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut store = EdgeInlinePropertyProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(2);
        let profile = EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: EdgeInlinePropertyEncoding::RawU16,
        };
        store.insert(label, profile.clone()).expect("insert");
        assert_eq!(store.get(label), Some(profile));
    }
}
