//! In-memory edge value bytes shared across storage, federation wire, and query bindings.

use candid::CandidType;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum edge-value byte width supported by labeled storage profiles.
pub const MAX_EDGE_VALUE_BYTE_WIDTH: u16 = u16::MAX;

/// Stored edge-value bytes (not part of the 4-byte labeled CSR row).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct EdgeValuePayload {
    bytes: Vec<u8>,
}

impl CandidType for EdgeValuePayload {
    fn _ty() -> candid::types::Type {
        <Vec<u8> as CandidType>::_ty()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        self.bytes.as_slice().idl_serialize(serializer)
    }
}

impl EdgeValuePayload {
    pub const EMPTY: Self = Self { bytes: Vec::new() };

    #[inline]
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
        }
    }

    #[inline]
    pub fn from_inline_u16(inline: u16) -> Self {
        Self::from_slice(&inline.to_le_bytes())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Active value bytes as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    #[inline]
    pub fn bytes_slice(&self) -> &[u8] {
        self.as_slice()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Little-endian `u16` view when length is exactly `2`.
    #[inline]
    pub fn inline_u16(&self) -> u16 {
        if self.bytes.len() == 2 {
            u16::from_le_bytes(self.bytes[0..2].try_into().unwrap())
        } else {
            0
        }
    }
}

impl Serialize for EdgeValuePayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.bytes.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EdgeValuePayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() > usize::from(MAX_EDGE_VALUE_BYTE_WIDTH) {
            return Err(serde::de::Error::custom(format!(
                "edge value length {} exceeds max {}",
                bytes.len(),
                MAX_EDGE_VALUE_BYTE_WIDTH
            )));
        }
        Ok(Self { bytes })
    }
}
