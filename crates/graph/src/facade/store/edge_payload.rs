//! Inline edge payload update public API (delegates to edge-profile domain commits).

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    /// Updates the inline edge-payload payload at `handle`.
    pub(crate) fn update_edge_payload_at_handle(
        &self,
        handle: EdgeHandle,
        payload_bytes: &[u8],
    ) -> Result<(), GraphStoreError> {
        self.commit_update_edge_payload_at_handle(handle, payload_bytes)
    }
}
