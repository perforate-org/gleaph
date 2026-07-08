//! Inline edge inline value update public API (delegates to edge-profile domain commits).

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    /// Updates the inline edge-inline-value payload at `handle`.
    pub(crate) fn update_edge_inline_value_at_handle(
        &self,
        handle: EdgeHandle,
        inline_value_bytes: &[u8],
    ) -> Result<(), GraphStoreError> {
        self.commit_update_edge_inline_value_at_handle(handle, inline_value_bytes)
    }
}
