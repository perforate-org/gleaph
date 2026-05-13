//! Optional [`gleaph_graph_kernel::entry::EdgeWeightProfile`] per edge-capable [`LabelId`].

use gleaph_graph_kernel::entry::{EdgeWeightProfile, LabelId};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;

/// Encoded [`EdgeWeightProfile`] bytes (`candid`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileBlob(pub Vec<u8>);

impl Storable for ProfileBlob {
    const BOUND: Bound = Bound::Bounded {
        max_size: 256,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeWeightProfileStoreError {
    LabelNotEdgeInline(LabelId),
    EncodeFailed(String),
    DecodeFailed(String),
}

impl fmt::Display for EdgeWeightProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LabelNotEdgeInline(id) => {
                write!(f, "weight profiles require edge label id {}", id.raw())
            }
            Self::EncodeFailed(e) => write!(f, "failed to encode edge weight profile: {e}"),
            Self::DecodeFailed(e) => write!(f, "failed to decode edge weight profile: {e}"),
        }
    }
}

impl std::error::Error for EdgeWeightProfileStoreError {}

pub struct EdgeWeightProfileStore<M: Memory> {
    inner: StableBTreeMap<LabelId, ProfileBlob, M>,
}

impl<M: Memory> EdgeWeightProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, label: LabelId) -> Option<EdgeWeightProfile> {
        let bytes = self.inner.get(&label)?;
        candid::decode_one(&bytes.0).ok()
    }

    pub fn insert(
        &mut self,
        label: LabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), EdgeWeightProfileStoreError> {
        if !label.is_edge_inline_capable() {
            return Err(EdgeWeightProfileStoreError::LabelNotEdgeInline(label));
        }
        let bytes = candid::encode_one(&profile)
            .map_err(|e| EdgeWeightProfileStoreError::EncodeFailed(e.to_string()))?;
        self.inner.insert(label, ProfileBlob(bytes));
        Ok(())
    }

    pub fn remove(&mut self, label: LabelId) {
        self.inner.remove(&label);
    }

    pub fn into_memory(self) -> M {
        self.inner.into_memory()
    }
}
