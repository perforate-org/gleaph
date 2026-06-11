//! Shard-local handles for cross-shard CSR edge endpoints.

use ic_stable_lara::VertexId;
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;

/// Dense shard-local handle stored in remote [`super::vertex_ref::VertexRef`] slots.
///
/// Many edges may share one `RemoteVertexId` for the same global target vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteVertexId(u32);

const REMOTE_VERTEX_ID_MASK: u32 = (1 << 30) - 1;

impl RemoteVertexId {
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw & REMOTE_VERTEX_ID_MASK)
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self::from_raw(u32::from_le_bytes(bytes))
    }
}

impl Storable for RemoteVertexId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 4,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_le_bytes()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_le_bytes())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0; 4];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(raw)
    }
}

/// Resolved edge endpoint on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeTarget {
    Local(VertexId),
    Remote(RemoteVertexId),
}

impl EdgeTarget {
    #[inline]
    pub const fn is_remote(self) -> bool {
        matches!(self, Self::Remote(_))
    }
}
