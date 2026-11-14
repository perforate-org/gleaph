//! Ordinal identifiers for graph shards and index resources.
//!
//! All four are the same contract: a transparent `u32` whose `0` is valid, with the same
//! conversions, little-endian encoding and fixed-size `Storable` form. One macro owns that
//! contract so a method added to one id cannot be forgotten on another.

use candid::CandidType;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

macro_rules! ordinal_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize,
            Deserialize,
        )]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub u32);

        impl $name {
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

        impl From<u32> for $name {
            #[inline]
            fn from(value: u32) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u32 {
            #[inline]
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl From<$name> for u64 {
            #[inline]
            fn from(value: $name) -> Self {
                u64::from(value.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl Storable for $name {
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
    };
}

ordinal_id! {
    /// Numeric graph shard identity. Shard `0` is valid (sole shard in standalone mode).
    ShardId
}

ordinal_id! {
    /// Graph shard group (index cluster) ordinal. Property Index canisters are assigned per group
    /// (`index_cluster[group_index]`), so this identifies the group an index serves.
    IndexClusterId
}

ordinal_id! {
    /// Vector index group ordinal. Today a graph has a single vector target (group `None` = whole
    /// graph); the future shard-group split assigns a `VectorIndexId` per contiguous graph shard
    /// group, mirroring `IndexClusterId`. `0` is valid (the sole/whole-graph group).
    VectorIndexId
}

ordinal_id! {
    /// Text index resource ordinal. One issued text index canister per id (plan 0297); `0` is valid.
    /// Mirrors [`VectorIndexId`] so `LogicalResource::TextIndex` keeps the fixed-size tag+u32 stable
    /// encoding shared by Router and Provision.
    TextIndexId
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::Storable;

    #[test]
    fn all_ordinal_ids_share_the_same_contract() {
        assert_eq!(ShardId::new(0).raw(), 0, "zero is a valid ordinal");
        assert_eq!(
            IndexClusterId::checked_add(IndexClusterId::new(1), 2),
            Some(IndexClusterId::new(3))
        );
        assert_eq!(
            VectorIndexId::checked_add(VectorIndexId::new(u32::MAX), 1),
            None
        );
        assert_eq!(u64::from(TextIndexId::new(7)), 7);
        assert_eq!(u32::from(ShardId::new(7)), 7);
        assert_eq!(ShardId::from(7u32), ShardId::new(7));
        assert_eq!(TextIndexId::from(11u32), TextIndexId::new(11));
    }

    #[test]
    fn all_ordinal_ids_round_trip_their_bytes_and_storable() {
        macro_rules! check {
            ($($ty:ty),+ $(,)?) => {
                $(
                    let id = <$ty>::new(0xA1B2_C3D4);
                    assert_eq!(<$ty>::from_le_bytes(id.to_le_bytes()), id);
                    assert_eq!(<$ty>::from_bytes(id.to_bytes()), id);
                    assert_eq!(id.to_string(), "2712847316");
                )+
            };
        }
        check!(ShardId, IndexClusterId, VectorIndexId, TextIndexId);
    }
}
