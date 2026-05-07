use ic_stable_lara::{
    VertexId,
    traits::{CsrEdge, CsrEdgeUndirected},
};

mod meta;

pub use meta::{EdgeFlags, EdgeMeta, SideCarKind};

/// Fixed-size adjacency entry.
///
/// This is the LARA-style base entry stored in a surface edge region (CSR slab slot).
/// It intentionally contains only the neighbor vertex ref and edge-local hot
/// metadata.
///
/// Invariant:
/// - one [`Edge`] is always exactly 8 bytes (4-byte LE [`VertexId`] + 4-byte LE [`EdgeMeta`])
/// - semantic edge identity is stored elsewhere, not here
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Edge {
    pub target: VertexId,
    pub meta: EdgeMeta,
}

impl CsrEdge for Edge {
    const BYTES: usize = 8;

    fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("CsrEdge::read_from expects exactly 8 bytes");
        Edge {
            target: VertexId::from_le_bytes(chunk[0..4].try_into().unwrap()),
            meta: EdgeMeta::from_le_bytes(chunk[4..8].try_into().unwrap()),
        }
    }

    fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(
            bytes.len(),
            Self::BYTES,
            "CsrEdge::write_to expects exactly 8 bytes"
        );
        let out = &mut bytes[..Self::BYTES];
        out[..4].copy_from_slice(&self.target.to_le_bytes());
        out[4..].copy_from_slice(&self.meta.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        self.target
    }

    fn with_neighbor_vid(self, vid: VertexId) -> Self {
        Self {
            target: vid,
            meta: self.meta,
        }
    }
}

impl CsrEdgeUndirected for Edge {
    fn is_undirected(&self) -> bool {
        self.meta.is_undirected()
    }

    fn with_undirected(self, undirected: bool) -> Self {
        Self {
            target: self.target,
            meta: self.meta.with_undirected(undirected),
        }
    }
}
