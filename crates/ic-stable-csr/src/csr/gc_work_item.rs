//! Fixed-width [`Storable`] work items for [`super::csr_graph_gc::CsrGraphWithGcQueue`].

use std::borrow::Cow;

use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;

/// Queue entry: physically maintain one DGAP leaf segment on the **forward** CSR column.
pub const GC_TAG_SEGMENT_FWD: u8 = 1;
/// Queue entry: physically maintain one DGAP leaf segment on the **reverse** CSR column.
pub const GC_TAG_SEGMENT_REV: u8 = 2;
/// Queue entry: drain a tombstoned vertex’s adjacency via repeated segment maintenance.
pub const GC_TAG_VERTEX: u8 = 0;

/// One persisted GC job (24 bytes, fixed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcWorkItem(pub [u8; 24]);

impl GcWorkItem {
    /// `vid` must fit the packed `u64` (caller uses `usize` in graph APIs).
    pub fn vertex_delete(vid: u64) -> Self {
        let mut b = [0u8; 24];
        b[0] = GC_TAG_VERTEX;
        b[8..16].copy_from_slice(&vid.to_le_bytes());
        Self(b)
    }

    /// `leaf_segment_id` is [`crate::layout::dgap::dgap_leaf_segment_id`] for any vertex in that leaf.
    pub fn segment_maintain_forward(leaf_segment_id: u32) -> Self {
        let mut b = [0u8; 24];
        b[0] = GC_TAG_SEGMENT_FWD;
        b[4..8].copy_from_slice(&leaf_segment_id.to_le_bytes());
        Self(b)
    }

    pub fn segment_maintain_reverse(leaf_segment_id: u32) -> Self {
        let mut b = [0u8; 24];
        b[0] = GC_TAG_SEGMENT_REV;
        b[4..8].copy_from_slice(&leaf_segment_id.to_le_bytes());
        Self(b)
    }

    pub fn tag(self) -> u8 {
        self.0[0]
    }

    pub fn vertex_id(self) -> Option<u64> {
        if self.tag() != GC_TAG_VERTEX {
            return None;
        }
        Some(u64::from_le_bytes(self.0[8..16].try_into().unwrap()))
    }

    pub fn leaf_segment_id(self) -> Option<u32> {
        match self.tag() {
            GC_TAG_SEGMENT_FWD | GC_TAG_SEGMENT_REV => {
                Some(u32::from_le_bytes(self.0[4..8].try_into().unwrap()))
            }
            _ => None,
        }
    }

    pub fn is_segment_forward(self) -> bool {
        self.tag() == GC_TAG_SEGMENT_FWD
    }
}

impl Storable for GcWorkItem {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.0.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        assert_eq!(bytes.len(), 24, "GcWorkItem expects 24 bytes");
        let mut a = [0u8; 24];
        a.copy_from_slice(bytes.as_ref());
        Self(a)
    }

    const BOUND: Bound = Bound::Bounded {
        max_size: 24,
        is_fixed_size: true,
    };
}
