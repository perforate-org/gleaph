use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;
use std::num::NonZeroU16;

/// Maximum numeric id that may be embedded in [`super::edge::EdgeMeta`] (14 bits, `0x3FFF`).
pub const INLINE_EDGE_LABEL_MAX: u16 = 0x3FFF;

/// First [`LabelId`] reserved for vertex-only / non-inline catalog names (`0x4000`).
pub const VERTEX_LABEL_MIN: u16 = 0x4000;

/// Compact edge label id stored in the low 14 bits of [`super::edge::EdgeMeta`].
///
/// Valid range is `0x0001..=0x3FFF`. The value `0x0000` means “no inline label” in metadata.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlineEdgeLabelId(NonZeroU16);

impl InlineEdgeLabelId {
    /// Returns `Some` when `raw` lies in `0x0001..=0x3FFF`.
    #[inline]
    pub const fn try_from_raw(raw: u16) -> Option<Self> {
        if raw == 0 || raw > INLINE_EDGE_LABEL_MAX {
            return None;
        }
        match NonZeroU16::new(raw) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    #[inline]
    pub fn from_label_id(label: LabelId) -> Option<Self> {
        Self::try_from_raw(label.0)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0.get()
    }

    /// Widens to the global [`LabelId`] namespace (same numeric value).
    #[inline]
    pub const fn to_label_id(self) -> LabelId {
        LabelId(self.0.get())
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LabelId(u16);

impl LabelId {
    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// `true` when this id can be stored in [`super::edge::EdgeMeta`] inline label bits.
    #[inline]
    pub const fn is_edge_inline_capable(self) -> bool {
        let r = self.0;
        r >= 1 && r <= INLINE_EDGE_LABEL_MAX
    }

    /// `true` when this id is in the vertex-only / non-inline allocation band.
    #[inline]
    pub const fn is_vertex_catalog_range(self) -> bool {
        self.0 >= VERTEX_LABEL_MIN
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_le_bytes(bytes))
    }
}

impl Storable for LabelId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 2,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_le_bytes()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_le_bytes())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut out = [0; 2];
        out.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(out)
    }
}

impl fmt::Display for LabelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
