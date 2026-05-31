//! External blob storage for value overflow log entries wider than 8 bytes.

use super::blob_id::EdgeValueBlobId;
use std::fmt;

/// Errors returned by edge-value blob storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobStoreError {
    /// The requested blob payload exceeds the blob store's representable size.
    ValueTooLarge,
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueTooLarge => write!(f, "edge value blob is too large"),
        }
    }
}

impl std::error::Error for BlobStoreError {}

/// Store for edge value bytes keyed by overflow log site.
pub trait EdgeValueBlobStore {
    fn put_blob(&mut self, id: EdgeValueBlobId, bytes: &[u8]) -> Result<(), BlobStoreError>;
    fn get_blob(&self, id: EdgeValueBlobId, out: &mut Vec<u8>) -> bool;
    fn drop_blob(&mut self, id: EdgeValueBlobId);

    #[inline]
    fn drop_log_site(&mut self, leaf: u32, entry_idx: u32) {
        self.drop_blob(EdgeValueBlobId::from_log_site(leaf, entry_idx));
    }

    fn drain_leaf_segment(&mut self, leaf: u32, high_water_entry_idx: u32) {
        for entry_idx in 0..high_water_entry_idx {
            self.drop_log_site(leaf, entry_idx);
        }
    }
}

/// No-op blob store for graphs/tests without external value blobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopEdgeValueBlobStore;

impl EdgeValueBlobStore for NoopEdgeValueBlobStore {
    fn put_blob(&mut self, _: EdgeValueBlobId, _: &[u8]) -> Result<(), BlobStoreError> {
        Ok(())
    }

    fn get_blob(&self, _: EdgeValueBlobId, _: &mut Vec<u8>) -> bool {
        false
    }

    fn drop_blob(&mut self, _: EdgeValueBlobId) {}
}
