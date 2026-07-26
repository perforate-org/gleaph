//! External blob storage for inline property bytes overflow log entries wider than 8 bytes.

use super::blob_id::EdgeInlinePropertyBytesBlobId;
use std::fmt;

/// Errors returned by edge-inline-value blob storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobStoreError {
    /// The requested blob inline property bytes exceeds the blob store's representable size.
    ValueTooLarge,
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueTooLarge => write!(f, "edge inline property bytes blob is too large"),
        }
    }
}

impl std::error::Error for BlobStoreError {}

/// Store for edge inline property bytes keyed by overflow log site.
pub trait EdgeInlinePropertyBytesBlobStore {
    /// Stores `bytes` under `id`.
    fn put_blob(
        &mut self,
        id: EdgeInlinePropertyBytesBlobId,
        bytes: &[u8],
    ) -> Result<(), BlobStoreError>;
    /// Reads the blob under `id` into `out`, returning whether it existed.
    fn get_blob(&self, id: EdgeInlinePropertyBytesBlobId, out: &mut Vec<u8>) -> bool;
    /// Deletes the blob under `id` if present.
    fn drop_blob(&mut self, id: EdgeInlinePropertyBytesBlobId);

    /// Deletes the blob associated with one inline property bytes overflow-log site.
    #[inline]
    fn drop_log_site(&mut self, leaf: u32, entry_idx: u32) {
        self.drop_blob(EdgeInlinePropertyBytesBlobId::from_log_site(
            leaf, entry_idx,
        ));
    }

    /// Deletes blobs for all allocated log entries in a leaf segment.
    fn drain_leaf_segment(&mut self, leaf: u32, high_water_entry_idx: u32) {
        for entry_idx in 0..high_water_entry_idx {
            self.drop_log_site(leaf, entry_idx);
        }
    }
}

/// No-op blob store for graphs/tests without external value blobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopEdgeInlinePropertyBytesBlobStore;

impl EdgeInlinePropertyBytesBlobStore for NoopEdgeInlinePropertyBytesBlobStore {
    fn put_blob(
        &mut self,
        _: EdgeInlinePropertyBytesBlobId,
        _: &[u8],
    ) -> Result<(), BlobStoreError> {
        Ok(())
    }

    fn get_blob(&self, _: EdgeInlinePropertyBytesBlobId, _: &mut Vec<u8>) -> bool {
        false
    }

    fn drop_blob(&mut self, _: EdgeInlinePropertyBytesBlobId) {}
}
