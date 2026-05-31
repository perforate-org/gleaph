//! Stable map for edge payload overflow payloads wider than 8 bytes.

use super::blob_id::EdgePayloadBlobId;
use super::blob_store::BlobStoreError;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::cell::RefCell;

#[derive(Clone, Debug, PartialEq, Eq)]
struct BlobBytes(Vec<u8>);

impl Storable for BlobBytes {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.0.len());
        let len = u16::try_from(self.0.len()).expect("blob length fits u16");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.0);
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let len = u16::from_le_bytes(bytes[0..2].try_into().unwrap()) as usize;
        Self(bytes[2..2 + len].to_vec())
    }
}

impl Storable for EdgePayloadBlobId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.raw().to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        EdgePayloadBlobId::from_raw(u64::from_le_bytes(bytes.as_ref().try_into().unwrap()))
    }
}

/// Stable btree backing large overflow-log edge payloads.
pub struct EdgePayloadBlobMap<M: Memory> {
    inner: RefCell<StableBTreeMap<EdgePayloadBlobId, BlobBytes, M>>,
}

impl<M: Memory> EdgePayloadBlobMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: RefCell::new(StableBTreeMap::init(memory)),
        }
    }

    pub fn into_memory(self) -> M {
        self.inner.into_inner().into_memory()
    }
}

impl<M: Memory> EdgePayloadBlobMap<M> {
    pub fn put_blob(&self, id: EdgePayloadBlobId, bytes: &[u8]) -> Result<(), BlobStoreError> {
        if bytes.len() > usize::from(u16::MAX) {
            return Err(BlobStoreError::ValueTooLarge);
        }
        self.inner
            .borrow_mut()
            .insert(id, BlobBytes(bytes.to_vec()));
        Ok(())
    }

    pub fn get_blob(&self, id: EdgePayloadBlobId, out: &mut Vec<u8>) -> bool {
        let Some(blob) = self.inner.borrow().get(&id) else {
            return false;
        };
        out.clear();
        out.extend_from_slice(&blob.0);
        true
    }

    pub fn drop_blob(&self, id: EdgePayloadBlobId) {
        self.inner.borrow_mut().remove(&id);
    }

    pub fn drop_log_site(&self, leaf: u32, entry_idx: u32) {
        self.drop_blob(EdgePayloadBlobId::from_log_site(leaf, entry_idx));
    }

    pub fn drain_leaf_segment(&self, leaf: u32, high_water_entry_idx: u32) {
        for entry_idx in 0..high_water_entry_idx {
            self.drop_log_site(leaf, entry_idx);
        }
    }
}
