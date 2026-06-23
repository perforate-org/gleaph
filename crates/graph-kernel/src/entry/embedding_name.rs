use candid::CandidType;
use ic_stable_structures::{Storable, storable::Bound};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Embedding name identity within a logical graph. **`0` is reserved** to mean
/// "unset / no embedding name".
///
/// In this slice the id is internal and not yet issued by a public Router/admin catalog;
/// name allocation is owned by a later phase.
#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    CandidType,
    Serialize,
    Deserialize,
)]
pub struct EmbeddingNameId(u16);

pub const EMBEDDING_NAME_CATALOG_MAX: u16 = u16::MAX - 1;

impl EmbeddingNameId {
    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
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

    #[inline]
    pub const fn to_be_bytes(self) -> [u8; 2] {
        self.0.to_be_bytes()
    }

    #[inline]
    pub const fn from_be_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_be_bytes(bytes))
    }

    #[inline]
    pub const fn is_reserved(self) -> bool {
        self.0 == 0
    }
}

impl Storable for EmbeddingNameId {
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

impl fmt::Display for EmbeddingNameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
