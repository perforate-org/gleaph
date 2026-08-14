//! Experimental bucketized two-choice linear hash map in Internet Computer stable memory.
//!
//! V1 stores fixed-size keys and values in one linear bucket universe. RapidHash V3 exact mode,
//! the two fixed domain constants, candidate order, and linear reduction are part of the version-1
//! routing identity. One persisted hash seed plus domain separation derives two candidates. New
//! entries use the less-loaded candidate; ties use the first. Every bucket has [`BUCKET_SIZE`]
//! slots. An absent insert that
//! would exceed 75% load performs at most one synchronous, bounded linear split.

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]

mod control;
mod header;
mod map;
mod memory;

#[cfg(feature = "canbench")]
mod bench;

pub use header::{ControlRegion, Header, InitError};
pub use map::{
    BUCKET_SIZE, MutationError, ResetError, ScanCursor, ScanError, ScanPage, ScrubCursor,
    ScrubError, ScrubSnapshot, ScrubStep, StableLinearHashMap,
};

use ic_stable_structures::Storable;

/// Supplies the canonical, upgrade-stable bytes used to route a key.
///
/// Stored key bytes remain owned by [`Storable`]; routing bytes are a separate contract so a
/// storage encoding can change only through an explicit layout decision. Equal keys must return
/// identical routing bytes. Implementations must keep those bytes stable across upgrades,
/// platforms, and compiler versions. Changing either the bytes or [`Self::KEY_ROUTING_ID`]
/// requires an explicit rehash/layout decision before reopening existing memory.
///
/// Hash collisions are allowed. The map decodes stored [`Storable`] bytes and uses [`Eq`] to
/// establish key identity.
pub trait StableHashKey: Storable + Eq {
    /// Identifies the canonical fixed-width key payload encoding.
    const KEY_STORAGE_ID: [u8; 16];

    /// Identifies the canonical routing-byte encoding.
    const KEY_ROUTING_ID: [u8; 16];

    /// The borrowing or owned representation of canonical routing bytes.
    type HashBytes<'a>: AsRef<[u8]>
    where
        Self: 'a;

    /// Returns canonical bytes used only for hashing and bucket routing.
    fn stable_hash_bytes(&self) -> Self::HashBytes<'_>;
}

/// Supplies the nominal identity of a fixed-width persisted value encoding.
pub trait StableMapValue: Storable {
    const VALUE_STORAGE_ID: [u8; 16];
}

const fn schema_id(kind: u8, width: u32) -> [u8; 16] {
    let width = width.to_le_bytes();
    [
        b'L', b'H', b'M', 1, kind, width[0], width[1], width[2], width[3], 0, 0, 0, 0, 0, 0, 0,
    ]
}

macro_rules! impl_stable_hash_key_for_unsigned {
    ($(($ty:ty, $bytes:expr, $id:expr)),+ $(,)?) => {
        $(
            impl StableHashKey for $ty {
                const KEY_STORAGE_ID: [u8; 16] = schema_id(1, $bytes);
                const KEY_ROUTING_ID: [u8; 16] = schema_id($id, $bytes);
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
    (u8, 1, 11),
    (u16, 2, 12),
    (u32, 4, 13),
    (u64, 8, 14),
    (u128, 16, 15),
);

macro_rules! impl_stable_map_value_for_unsigned {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl StableMapValue for $ty {
                const VALUE_STORAGE_ID: [u8; 16] = schema_id(2, std::mem::size_of::<$ty>() as u32);
            }
        )+
    };
}

impl_stable_map_value_for_unsigned!(u8, u16, u32, u64, u128);

impl<const N: usize> StableMapValue for [u8; N] {
    const VALUE_STORAGE_ID: [u8; 16] = schema_id(3, N as u32);
}

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
            u8::KEY_ROUTING_ID,
            u16::KEY_ROUTING_ID,
            u32::KEY_ROUTING_ID,
            u64::KEY_ROUTING_ID,
            u128::KEY_ROUTING_ID,
        ];
        for (index, id) in ids.iter().enumerate() {
            assert!(ids[..index].iter().all(|previous| previous != id));
        }
    }
}
