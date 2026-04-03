//! Property-store region v1: small fixed header + one [`ic_stable_structures::StableBTreeMap`] blob.
//!
//! Layout matches [`crate::property_index::pidx_v3_layout`] (same byte widths), with a distinct magic.

use super::PropertyStoreError;

pub const PROP_STORE_V1_MAGIC: [u8; 4] = *b"PSB1";
pub const PROP_STORE_V1_LAYOUT_VERSION: u8 = 1;
pub const PROP_STORE_V1_HEADER_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyStoreRegionHeaderV1 {
    pub btree_payload_len: u64,
}

impl PropertyStoreRegionHeaderV1 {
    pub fn encode(self) -> [u8; PROP_STORE_V1_HEADER_LEN] {
        let mut out = [0u8; PROP_STORE_V1_HEADER_LEN];
        out[0..4].copy_from_slice(&PROP_STORE_V1_MAGIC);
        out[4] = PROP_STORE_V1_LAYOUT_VERSION;
        out[5..8].copy_from_slice(&[0u8; 3]);
        out[8..16].copy_from_slice(&self.btree_payload_len.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyStoreError> {
        if bytes.len() != PROP_STORE_V1_HEADER_LEN {
            return Err(PropertyStoreError::InvalidHeaderLength(bytes.len()));
        }
        if bytes[0..4] != PROP_STORE_V1_MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&bytes[0..4]);
            return Err(PropertyStoreError::PStoreInvalidMagic(m));
        }
        if bytes[4] != PROP_STORE_V1_LAYOUT_VERSION {
            return Err(PropertyStoreError::PStoreUnsupportedVersion(bytes[4]));
        }
        let mut len = [0u8; 8];
        len.copy_from_slice(&bytes[8..16]);
        Ok(Self {
            btree_payload_len: u64::from_le_bytes(len),
        })
    }
}
