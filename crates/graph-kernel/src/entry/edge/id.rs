use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;

/// Slot index wrapper for an edge inside one `(vertex, label)` adjacency row.
///
/// Compact edge rows do not persist a self-identifying edge id. During scans this value is attached
/// from the physical slot being read.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeSlotIndex(u32);

impl EdgeSlotIndex {
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
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
        Self(u32::from_le_bytes(bytes))
    }
}

impl Storable for EdgeSlotIndex {
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
        let mut out = [0; 4];
        out.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(out)
    }
}

impl fmt::Display for EdgeSlotIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
