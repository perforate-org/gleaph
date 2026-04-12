//! PIDX v3 on-disk header layout (shared by region I/O and the btree subregion adapter).

use super::errors::PropertyIndexError;

pub const PIDX_V3_MAGIC: [u8; 4] = *b"PID3";
pub const PIDX_V3_LAYOUT_VERSION: u8 = 1;
pub const PIDX_V3_HEADER_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyIndexRegionHeaderV3 {
    pub btree_payload_len: u64,
}

impl PropertyIndexRegionHeaderV3 {
    pub fn encode(self) -> [u8; PIDX_V3_HEADER_LEN] {
        let mut out = [0u8; PIDX_V3_HEADER_LEN];
        out[0..4].copy_from_slice(&PIDX_V3_MAGIC);
        out[4] = PIDX_V3_LAYOUT_VERSION;
        out[5..8].copy_from_slice(&[0u8; 3]);
        out[8..16].copy_from_slice(&self.btree_payload_len.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() != PIDX_V3_HEADER_LEN {
            return Err(PropertyIndexError::InvalidRegionHeaderLength(bytes.len()));
        }
        if bytes[0..4] != PIDX_V3_MAGIC {
            return Err(PropertyIndexError::InvalidMagic(bytes[0..4].to_vec()));
        }
        if bytes[4] != PIDX_V3_LAYOUT_VERSION {
            return Err(PropertyIndexError::UnsupportedVersion(bytes[4]));
        }
        let mut len = [0u8; 8];
        len.copy_from_slice(&bytes[8..16]);
        Ok(Self {
            btree_payload_len: u64::from_le_bytes(len),
        })
    }
}
