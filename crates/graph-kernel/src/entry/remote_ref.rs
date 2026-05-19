//! Shard-local interned references to logical vertices on other graph shards.

use ic_stable_lara::VertexId;

/// Dense shard-local handle stored in remote [`super::vertex_ref::VertexRef`] slots.
///
/// Many edges may share one `RemoteRefId` for the same [`LogicalVertexId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RemoteRefId(u32);

const REMOTE_REF_MASK: u32 = (1 << 30) - 1;

impl RemoteRefId {
    pub const INVALID: Self = Self(0);

    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw & REMOTE_REF_MASK)
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Resolved edge endpoint on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeTarget {
    Local(VertexId),
    Remote(RemoteRefId),
}

impl EdgeTarget {
    #[inline]
    pub const fn is_remote(self) -> bool {
        matches!(self, Self::Remote(_))
    }
}
