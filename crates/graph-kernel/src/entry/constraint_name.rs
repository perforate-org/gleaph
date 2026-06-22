use candid::CandidType;
use ic_stable_structures::{Storable, storable::Bound};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Router-issued constraint name identity within a logical graph (ADR 0030). **`0` is reserved**.
///
/// A constraint is a logical definition distinct from an index; this id is the stable handle
/// referenced by the cross-shard uniqueness reservation key (`(graph_id, constraint_id, value)`).
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
pub struct ConstraintNameId(u16);

pub const CONSTRAINT_NAME_CATALOG_MAX: u16 = u16::MAX - 1;

impl ConstraintNameId {
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
    pub const fn is_reserved(self) -> bool {
        self.0 == 0
    }
}

impl Storable for ConstraintNameId {
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

impl fmt::Display for ConstraintNameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
