//! Fixed-width [`Storable`] work items for [`super::csr_graph_gc::CsrGraphWithGcQueue`].

use std::borrow::Cow;

use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;

/// Queue entry: physically compact a tombstoned vertex’s remaining adjacency.
pub const GC_TAG_VERTEX: u8 = 0;
/// Queue entry: physically remove a directed tombstone edge `src → dst`.
pub const GC_TAG_EDGE_DIRECTED: u8 = 1;
/// Queue entry: physically remove an undirected logical edge `{u,v}` (four slots).
pub const GC_TAG_EDGE_UNDIRECTED: u8 = 2;

/// One persisted GC job (16 bytes, fixed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcWorkItem(pub [u8; 16]);

impl GcWorkItem {
    /// `vid` must fit the packed `u64` (caller uses `usize` in graph APIs).
    pub fn vertex_delete(vid: u64) -> Self {
        let mut b = [0u8; 16];
        b[0] = GC_TAG_VERTEX;
        b[8..16].copy_from_slice(&vid.to_le_bytes());
        Self(b)
    }

    pub fn edge_directed(src: u32, dst: u32) -> Self {
        let mut b = [0u8; 16];
        b[0] = GC_TAG_EDGE_DIRECTED;
        b[4..8].copy_from_slice(&src.to_le_bytes());
        b[8..12].copy_from_slice(&dst.to_le_bytes());
        Self(b)
    }

    pub fn edge_undirected(u: u32, v: u32) -> Self {
        let mut b = [0u8; 16];
        b[0] = GC_TAG_EDGE_UNDIRECTED;
        b[4..8].copy_from_slice(&u.to_le_bytes());
        b[8..12].copy_from_slice(&v.to_le_bytes());
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

    pub fn edge_endpoints(self) -> Option<(u32, u32)> {
        match self.tag() {
            GC_TAG_EDGE_DIRECTED | GC_TAG_EDGE_UNDIRECTED => Some((
                u32::from_le_bytes(self.0[4..8].try_into().unwrap()),
                u32::from_le_bytes(self.0[8..12].try_into().unwrap()),
            )),
            _ => None,
        }
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
        assert_eq!(bytes.len(), 16, "GcWorkItem expects 16 bytes");
        let mut a = [0u8; 16];
        a.copy_from_slice(bytes.as_ref());
        Self(a)
    }

    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };
}
