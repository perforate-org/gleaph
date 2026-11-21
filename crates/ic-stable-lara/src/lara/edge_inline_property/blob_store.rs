//! Errors of the external blob storage for values spill rows wider than 8 bytes.

use std::fmt;

/// Errors returned by edge-inline-property-bytes blob storage.
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
