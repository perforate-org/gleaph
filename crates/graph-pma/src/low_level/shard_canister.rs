//! Compact shard-canister slots and principal resolution for cross-canister edges.
//!
//! When [`super::edge::EdgeMeta::is_shard_canister`] is set, the 16-bit payload names a row
//! in [`ShardCanisterDirectory`], not a graph [`LabelId`](gleaph_graph_kernel::LabelId).
//!
//! # Stable persistence (`SCD1`)
//!
//! The directory is serialized with [`Self::encode_bytes`] / [`Self::decode_bytes`] (wire magic
//! [`SHARD_CANISTER_DIRECTORY_MAGIC`]). The [`crate::facade::GraphPma`] stores this payload in
//! stable memory under [`super::region::RegionKind::ShardCanisterDirectory`] (extent-backed), and
//! writes it on [`crate::facade::GraphPma::try_write_all_to_stable_memory`] /
//! [`crate::facade::GraphPma::try_refresh_and_write_dirty_to_stable_memory`].
//!
//! # Hydration consistency (strict)
//!
//! After loading the directory, [`crate::low_level::GraphRuntime::validate_shard_canister_slots`]
//! runs during [`crate::facade::GraphPma::hydrate_from_stable_memory`]: every **live**
//! (non-tombstone) cross-shard edge on forward and reverse surfaces must reference a slot strictly
//! less than `directory.len()`. Otherwise hydration returns
//! [`crate::low_level::HydrationError::ShardCanisterSlotOutOfRange`].
//!
//! If the region is missing or its recorded logical length is zero, the directory is treated as
//! empty and the same validation applies (so a live cross-shard edge with slot `0` still fails).
//!
//! Malformed `SCD1` bytes fail with [`crate::low_level::HydrationError::InvalidShardCanisterDirectory`].
//!
//! At runtime, [`ShardCanisterDirectory::principal`] returns [`None`] for out-of-range slots as a
//! defensive read path after successful hydration.

use candid::Principal;

/// Slot id stored in the 16-bit [`super::edge::EdgeMeta`] payload when the cross-shard flag is set.
pub type ShardCanisterSlot = u16;

/// Maps compact shard slots to remote canister principals; persisted via [`RegionKind::ShardCanisterDirectory`](super::region::RegionKind::ShardCanisterDirectory) on the graph facade.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShardCanisterDirectory {
    principals: Vec<Principal>,
}

/// Magic + `u32` little-endian count, then each principal as `u16` length + raw bytes.
pub const SHARD_CANISTER_DIRECTORY_MAGIC: [u8; 4] = *b"SCD1";

impl ShardCanisterDirectory {
    /// Resolves a slot to a principal when registered.
    pub fn principal(&self, slot: ShardCanisterSlot) -> Option<Principal> {
        self.principals.get(slot as usize).copied()
    }

    /// Returns the number of registered shard canisters (exclusive end of dense slot ids).
    pub fn len(&self) -> usize {
        self.principals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.principals.is_empty()
    }

    /// Appends a principal as the next slot; returns its slot id.
    ///
    /// Returns `None` if the directory is full (16-bit slot space) or on duplicate `principal`
    /// when `reject_duplicates` is true.
    pub fn push_principal(
        &mut self,
        principal: Principal,
        reject_duplicates: bool,
    ) -> Option<ShardCanisterSlot> {
        if reject_duplicates && self.principals.contains(&principal) {
            return None;
        }
        let slot = u16::try_from(self.principals.len()).ok()?;
        self.principals.push(principal);
        Some(slot)
    }

    /// Serializes this directory for stable storage (wire format for future region/hydration).
    pub fn encode_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SHARD_CANISTER_DIRECTORY_MAGIC);
        let n = u32::try_from(self.principals.len()).expect("shard directory length fits u32");
        out.extend_from_slice(&n.to_le_bytes());
        for p in &self.principals {
            let slice = p.as_slice();
            let len = u16::try_from(slice.len()).expect("principal length fits u16");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(slice);
        }
        out
    }

    /// Decodes [`Self::encode_bytes`] output.
    pub fn decode_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < 8 {
            return Err("shard directory too short");
        }
        if bytes[..4] != SHARD_CANISTER_DIRECTORY_MAGIC {
            return Err("shard directory bad magic");
        }
        let n = u32::from_le_bytes(bytes[4..8].try_into().expect("u32")) as usize;
        let mut pos = 8usize;
        let mut principals = Vec::with_capacity(n);
        for _ in 0..n {
            if pos + 2 > bytes.len() {
                return Err("shard directory truncated count");
            }
            let len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().expect("u16")) as usize;
            pos += 2;
            let end = pos
                .checked_add(len)
                .ok_or("shard directory length overflow")?;
            if end > bytes.len() {
                return Err("shard directory truncated principal");
            }
            principals.push(Principal::from_slice(&bytes[pos..end]));
            pos = end;
        }
        Ok(Self { principals })
    }
}

#[cfg(test)]
mod tests {
    use super::{SHARD_CANISTER_DIRECTORY_MAGIC, ShardCanisterDirectory};
    use candid::Principal;

    #[test]
    fn shard_canister_directory_encode_round_trips() {
        let mut dir = ShardCanisterDirectory::default();
        let p0 = Principal::anonymous();
        let p1 = Principal::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(dir.push_principal(p0, false), Some(0));
        assert_eq!(dir.push_principal(p1, false), Some(1));
        let bytes = dir.encode_bytes();
        assert_eq!(&bytes[..4], &SHARD_CANISTER_DIRECTORY_MAGIC);
        let got = ShardCanisterDirectory::decode_bytes(&bytes).expect("decode");
        assert_eq!(got, dir);
    }
}
