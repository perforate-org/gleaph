//! External blob storage for payload overflow log entries wider than 8 bytes.

use super::blob_id::EdgePayloadBlobId;
use std::fmt;

/// Errors returned by edge-payload blob storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobStoreError {
    /// The requested blob payload exceeds the blob store's representable size.
    ValueTooLarge,
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueTooLarge => write!(f, "edge payload blob is too large"),
        }
    }
}

impl std::error::Error for BlobStoreError {}

/// Store for edge payload bytes keyed by overflow log site.
pub trait EdgePayloadBlobStore {
    fn put_blob(&mut self, id: EdgePayloadBlobId, bytes: &[u8]) -> Result<(), BlobStoreError>;
    fn get_blob(&self, id: EdgePayloadBlobId, out: &mut Vec<u8>) -> bool;
    fn drop_blob(&mut self, id: EdgePayloadBlobId);

    #[inline]
    fn drop_log_site(&mut self, leaf: u32, entry_idx: u32) {
        self.drop_blob(EdgePayloadBlobId::from_log_site(leaf, entry_idx));
    }

    fn drain_leaf_segment(&mut self, leaf: u32, high_water_entry_idx: u32) {
        for entry_idx in 0..high_water_entry_idx {
            self.drop_log_site(leaf, entry_idx);
        }
    }
}

/// No-op blob store for graphs/tests without external value blobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopEdgePayloadBlobStore;

impl EdgePayloadBlobStore for NoopEdgePayloadBlobStore {
    fn put_blob(&mut self, _: EdgePayloadBlobId, _: &[u8]) -> Result<(), BlobStoreError> {
        Ok(())
    }

    fn get_blob(&self, _: EdgePayloadBlobId, _: &mut Vec<u8>) -> bool {
        false
    }

    fn drop_blob(&mut self, _: EdgePayloadBlobId) {}
}
