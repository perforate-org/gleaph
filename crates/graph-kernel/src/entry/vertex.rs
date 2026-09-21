//! Labeled CSR vertex row, shared with `ic-stable-lara` labeled storage.
//!
//! The wire layout matches [`ic_stable_lara::labeled::record::LabeledVertex`]. This
//! newtype exists so the graph kernel can add Gleaph-facing helpers without orphan
//! rules on foreign types.

use super::label::VertexLabelId;
use ic_stable_lara::labeled::record::LabeledVertex;
use ic_stable_lara::traits::{CsrVertex, CsrVertexTombstone};
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;
use std::ops::{Deref, DerefMut};

/// Per-vertex locator for labeled CSR (and graph canister metadata compatibility).
///
/// See [`LabeledVertex`] for the exact field semantics and packed `metadata` layout.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vertex(pub LabeledVertex);

impl Vertex {
    /// Fixed byte width of one encoded vertex row.
    pub const BYTES: usize = LabeledVertex::BYTES;

    #[inline]
    pub const fn from_labeled(inner: LabeledVertex) -> Self {
        Self(inner)
    }

    #[inline]
    pub const fn into_labeled(self) -> LabeledVertex {
        self.0
    }

    /// Vertex labels are stored in the graph canister's `VertexLabelStore`; the CSR
    /// row does not retain an inline primary label. Use the label store for reads.
    #[inline]
    pub fn primary_label_id(self) -> Option<VertexLabelId> {
        let _ = self;
        None
    }

    #[inline]
    pub fn with_primary_label_id(self, _label_id: Option<VertexLabelId>) -> Self {
        self
    }

    /// Whether additional labels live in the sidecar map (not encoded on this row).
    #[inline]
    pub fn has_label_sidecar(self) -> bool {
        let _ = self;
        false
    }

    #[inline]
    pub fn with_label_sidecar(self, _has_sidecar: bool) -> Self {
        self
    }
}

impl Deref for Vertex {
    type Target = LabeledVertex;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Vertex {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<LabeledVertex> for Vertex {
    fn from(value: LabeledVertex) -> Self {
        Self(value)
    }
}

impl From<Vertex> for LabeledVertex {
    fn from(value: Vertex) -> Self {
        value.0
    }
}

impl CsrVertex for Vertex {
    const BYTES: usize = LabeledVertex::BYTES;

    fn base_slot_start(&self) -> u64 {
        self.0.base_slot_start()
    }

    fn degree(&self) -> u32 {
        self.0.degree()
    }

    fn stored_degree(&self) -> u32 {
        self.0.stored_degree()
    }

    fn with_base_slot_start(self, start: u64) -> Self {
        Self(self.0.with_base_slot_start(start))
    }

    fn with_degree(self, degree: u32) -> Self {
        Self(self.0.with_degree(degree))
    }

    fn spill_run(self) -> u32 {
        self.0.spill_run()
    }

    fn try_with_spill_run(self, run: u32) -> Result<Self, ()> {
        self.0.try_with_spill_run(run).map(Self)
    }

    fn after_slab_tombstone_delete(self) -> Self {
        Self(self.0.after_slab_tombstone_delete())
    }

    fn try_grow_packed_slab_by_one(self) -> Result<Self, ()> {
        self.0.try_grow_packed_slab_by_one().map(Self)
    }

    fn grow_packed_slab_by_one(self) -> Self {
        Self(self.0.grow_packed_slab_by_one())
    }

    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        Self(self.0.after_slab_insert_reuse_tail_tombstone())
    }
}

impl CsrVertexTombstone for Vertex {
    fn is_tombstone(&self) -> bool {
        self.0.is_tombstone()
    }

    fn with_tombstone(self, tomb: bool) -> Self {
        Self(self.0.with_tombstone(tomb))
    }
}

impl Storable for Vertex {
    const BOUND: Bound = LabeledVertex::BOUND;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.0.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(LabeledVertex::from_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_growth_preserves_inner_overflow_errors() {
        use ic_stable_lara::labeled::BucketLabelKey;
        let bypass =
            LabeledVertex::default().with_homogeneous_bypass_label(BucketLabelKey::from_raw(1));
        for inner in [
            bypass.with_degree(u32::MAX).with_stored_slots(u32::MAX),
            bypass.with_degree(1).with_stored_slots(u32::MAX),
            LabeledVertex::default()
                .with_degree(ic_stable_lara::labeled::record::MAX_VERTEX_LABEL_BUCKETS),
        ] {
            assert!(inner.try_grow_packed_slab_by_one().is_err());
            assert_eq!(Vertex(inner).try_grow_packed_slab_by_one(), Err(()));
        }
        let inner = bypass.with_degree(7).with_stored_slots(9);
        assert_eq!(
            Vertex(inner).try_grow_packed_slab_by_one(),
            inner.try_grow_packed_slab_by_one().map(Vertex)
        );
    }

    #[test]
    fn vertex_width_matches_labeled_vertex_layout() {
        assert_eq!(Vertex::BYTES, 25);
        assert!(
            core::mem::size_of::<Vertex>() >= Vertex::BYTES,
            "Rust layout may include tail padding; wire width is {}",
            Vertex::BYTES
        );
    }
}
