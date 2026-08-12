//! Experimental bucketized two-choice linear hash map in Internet Computer stable memory.
//!
//! V1 stores fixed-size keys and values in one linear bucket universe. One persisted hash seed plus
//! domain separation derives two candidates for each key. New entries use the less-loaded
//! candidate; ties use the first. Every bucket has [`BUCKET_SIZE`] slots. An absent insert that
//! would exceed 75% load performs at most one synchronous, bounded linear split.

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]

mod control;
mod header;
mod map;
mod memory;

#[cfg(feature = "canbench")]
mod bench;

pub use header::{ControlRegion, Header, InitError};
pub use map::{BUCKET_SIZE, MutationError, StableLinearHashMap};

use ic_stable_structures::Storable;

/// Supplies the canonical, upgrade-stable bytes used to route a key.
///
/// Stored key bytes remain owned by [`Storable`]; routing bytes are a separate contract so a
/// storage encoding can change only through an explicit layout decision. Equal keys must return
/// identical routing bytes. Implementations must keep those bytes stable across upgrades,
/// platforms, and compiler versions. Changing either the bytes or [`Self::HASH_ENCODING_ID`]
/// requires an explicit rehash/layout decision before reopening existing memory.
///
/// Hash collisions are allowed. The map decodes stored [`Storable`] bytes and uses [`Eq`] to
/// establish key identity.
pub trait StableHashKey: Storable + Eq {
    /// Identifies the canonical routing-byte encoding persisted with the map control region.
    ///
    /// The value must be frozen and distinct from every incompatible key routing encoding,
    /// including same-width encodings introduced later.
    const HASH_ENCODING_ID: u64;

    /// The borrowing or owned representation of canonical routing bytes.
    type HashBytes<'a>: AsRef<[u8]>
    where
        Self: 'a;

    /// Returns canonical bytes used only for hashing and bucket routing.
    fn stable_hash_bytes(&self) -> Self::HashBytes<'_>;
}

macro_rules! impl_stable_hash_key_for_unsigned {
    ($(($ty:ty, $bytes:expr, $id:expr)),+ $(,)?) => {
        $(
            impl StableHashKey for $ty {
                const HASH_ENCODING_ID: u64 = $id;
                type HashBytes<'a> = [u8; $bytes] where Self: 'a;

                #[inline]
                fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
                    self.to_be_bytes()
                }
            }
        )+
    };
}

impl_stable_hash_key_for_unsigned!(
    (u8, 1, 0x4c48_4d00_0000_0001),
    (u16, 2, 0x4c48_4d00_0000_0002),
    (u32, 4, 0x4c48_4d00_0000_0003),
    (u64, 8, 0x4c48_4d00_0000_0004),
    (u128, 16, 0x4c48_4d00_0000_0005),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_keys_have_big_endian_bytes_and_distinct_encoding_ids() {
        assert_eq!(u8::stable_hash_bytes(&0xa5).as_ref(), &[0xa5]);
        assert_eq!(u16::stable_hash_bytes(&0x0123).as_ref(), &[0x01, 0x23]);
        assert_eq!(
            u32::stable_hash_bytes(&0x0123_4567).as_ref(),
            &[0x01, 0x23, 0x45, 0x67]
        );
        assert_eq!(
            u64::stable_hash_bytes(&0x0123_4567_89ab_cdef).as_ref(),
            &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
        );
        assert_eq!(
            u128::stable_hash_bytes(&0x0123_4567_89ab_cdef_0123_4567_89ab_cdef).as_ref(),
            &[
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );

        let ids = [
            u8::HASH_ENCODING_ID,
            u16::HASH_ENCODING_ID,
            u32::HASH_ENCODING_ID,
            u64::HASH_ENCODING_ID,
            u128::HASH_ENCODING_ID,
        ];
        for (index, id) in ids.iter().enumerate() {
            assert!(ids[..index].iter().all(|previous| previous != id));
        }
    }
}
