//! Graph shard partition identifier (`0..n-1` under strategy A).

use candid::CandidType;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Numeric graph shard identity. Shard `0` is valid (sole shard in standalone mode).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct ShardId(pub u32);

impl ShardId {
    #[inline]
    pub const fn new(raw: u32) -> Self {
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

    #[inline]
    pub fn checked_add(self, delta: u32) -> Option<Self> {
        self.0.checked_add(delta).map(Self)
    }
}

impl From<u32> for ShardId {
    #[inline]
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<ShardId> for u32 {
    #[inline]
    fn from(value: ShardId) -> Self {
        value.0
    }
}

impl From<ShardId> for u64 {
    #[inline]
    fn from(value: ShardId) -> Self {
        u64::from(value.0)
    }
}

impl fmt::Display for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Storable for ShardId {
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
        let mut raw = [0u8; 4];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::Storable;

    #[test]
    fn shard_id_zero_is_valid() {
        let id = ShardId::new(0);
        assert_eq!(id.raw(), 0);
        assert_eq!(ShardId::from_le_bytes(id.to_le_bytes()), id);
    }

    #[test]
    fn shard_id_storable_roundtrip() {
        let id = ShardId::new(3);
        assert_eq!(id, ShardId::from_bytes(id.to_bytes()));
    }

    #[test]
    fn shard_id_u32_conversions() {
        assert_eq!(ShardId::from(7u32), ShardId::new(7));
        assert_eq!(u32::from(ShardId::new(7)), 7);
    }
}
