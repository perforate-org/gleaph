//! Edge-label inline property bytes profiles: physical width and semantic interpretation.

use candid::CandidType;
use half::f16;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use thiserror::Error;

/// Maximum edge-inline-property-bytes byte width supported by labeled storage profiles.
pub const MAX_EDGE_INLINE_PROPERTY_BYTES: usize = u16::MAX as usize;

/// Stored edge-inline-property-bytes bytes (not part of the 4-byte labeled CSR row).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, CandidType)]
pub struct EdgeInlinePropertyBytes(Vec<u8>);

impl EdgeInlinePropertyBytes {
    pub const EMPTY: Self = Self(Vec::new());

    #[inline]
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Active inline property bytes bytes as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl Serialize for EdgeInlinePropertyBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EdgeInlinePropertyBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() > MAX_EDGE_INLINE_PROPERTY_BYTES {
            return Err(serde::de::Error::custom(format!(
                "edge inline property bytes length {} exceeds max {}",
                bytes.len(),
                MAX_EDGE_INLINE_PROPERTY_BYTES
            )));
        }
        Ok(Self(bytes))
    }
}

/// Semantic interpretation of stored edge-inline-property-bytes bytes.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub enum EdgeInlinePropertyEncoding {
    RawU8,
    RawU16,
    RawU32,
    RawU64,
    RawI8,
    RawI16,
    RawI32,
    RawI64,
    F16,
    F32,
    F64,
    RawU128,
    RawI128,
    RawFixed32,
    RawFixed64,
    VectorF32 {
        dims: u16,
    },
    /// Opaque fixed-width inline property bytes; [`EdgeInlinePropertyProfile::byte_width`] may be any positive width.
    RawBytes,
}

/// Label-level edge inline property bytes configuration.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct EdgeInlinePropertyProfile {
    pub byte_width: u16,
    pub encoding: EdgeInlinePropertyEncoding,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DecodedEdgeInlinePropertyBytes {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    U128(u128),
    I128(i128),
    Fixed32([u8; 32]),
    Fixed64([u8; 64]),
    F16(f16),
    F32(f32),
    F64(f64),
    VectorF32(Vec<f32>),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreparedEdgeInlinePropertyBytesDecoder {
    RawU8,
    RawU16,
    RawU32,
    RawU64,
    RawI8,
    RawI16,
    RawI32,
    RawI64,
    F16,
    F32,
    F64,
    RawU128,
    RawI128,
    RawFixed32,
    RawFixed64,
    VectorF32 { dims: u16 },
    RawBytes { byte_width: u16 },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum EdgeInlinePropertyProfileError {
    #[error("encoding does not match physical width")]
    WidthEncodingMismatch,
}

impl EdgeInlinePropertyProfile {
    pub const fn no_inline_property() -> Self {
        Self {
            byte_width: 0,
            encoding: EdgeInlinePropertyEncoding::RawBytes,
        }
    }

    /// Opaque profile with an arbitrary positive physical width.
    pub const fn opaque_bytes(byte_width: u16) -> Self {
        assert!(
            byte_width > 0,
            "opaque edge inline property bytes byte width must be positive"
        );
        Self {
            byte_width,
            encoding: EdgeInlinePropertyEncoding::RawBytes,
        }
    }

    pub const fn required_byte_width(&self) -> u16 {
        self.byte_width
    }

    pub fn validate(&self) -> Result<(), EdgeInlinePropertyProfileError> {
        let w = self.byte_width;
        let ok = match &self.encoding {
            EdgeInlinePropertyEncoding::RawBytes => true,
            _ if w == 0 => false,
            EdgeInlinePropertyEncoding::RawU8 | EdgeInlinePropertyEncoding::RawI8 => w == 1,
            EdgeInlinePropertyEncoding::RawU16
            | EdgeInlinePropertyEncoding::RawI16
            | EdgeInlinePropertyEncoding::F16 => w == 2,
            EdgeInlinePropertyEncoding::RawU32
            | EdgeInlinePropertyEncoding::RawI32
            | EdgeInlinePropertyEncoding::F32 => w == 4,
            EdgeInlinePropertyEncoding::RawU64
            | EdgeInlinePropertyEncoding::RawI64
            | EdgeInlinePropertyEncoding::F64 => w == 8,
            EdgeInlinePropertyEncoding::RawU128 | EdgeInlinePropertyEncoding::RawI128 => w == 16,
            EdgeInlinePropertyEncoding::RawFixed32 => w == 32,
            EdgeInlinePropertyEncoding::RawFixed64 => w == 64,
            EdgeInlinePropertyEncoding::VectorF32 { dims } => {
                *dims > 0 && dims.checked_mul(4).is_some_and(|need| need == w)
            }
        };
        if !ok {
            return Err(EdgeInlinePropertyProfileError::WidthEncodingMismatch);
        }
        Ok(())
    }

    pub fn prepare(
        &self,
    ) -> Result<PreparedEdgeInlinePropertyBytesDecoder, EdgeInlinePropertyProfileError> {
        self.validate()?;
        Ok(match &self.encoding {
            EdgeInlinePropertyEncoding::RawU8 => PreparedEdgeInlinePropertyBytesDecoder::RawU8,
            EdgeInlinePropertyEncoding::RawU16 => PreparedEdgeInlinePropertyBytesDecoder::RawU16,
            EdgeInlinePropertyEncoding::RawU32 => PreparedEdgeInlinePropertyBytesDecoder::RawU32,
            EdgeInlinePropertyEncoding::RawU64 => PreparedEdgeInlinePropertyBytesDecoder::RawU64,
            EdgeInlinePropertyEncoding::RawI8 => PreparedEdgeInlinePropertyBytesDecoder::RawI8,
            EdgeInlinePropertyEncoding::RawI16 => PreparedEdgeInlinePropertyBytesDecoder::RawI16,
            EdgeInlinePropertyEncoding::RawI32 => PreparedEdgeInlinePropertyBytesDecoder::RawI32,
            EdgeInlinePropertyEncoding::RawI64 => PreparedEdgeInlinePropertyBytesDecoder::RawI64,
            EdgeInlinePropertyEncoding::F16 => PreparedEdgeInlinePropertyBytesDecoder::F16,
            EdgeInlinePropertyEncoding::F32 => PreparedEdgeInlinePropertyBytesDecoder::F32,
            EdgeInlinePropertyEncoding::F64 => PreparedEdgeInlinePropertyBytesDecoder::F64,
            EdgeInlinePropertyEncoding::RawU128 => PreparedEdgeInlinePropertyBytesDecoder::RawU128,
            EdgeInlinePropertyEncoding::RawI128 => PreparedEdgeInlinePropertyBytesDecoder::RawI128,
            EdgeInlinePropertyEncoding::RawFixed32 => {
                PreparedEdgeInlinePropertyBytesDecoder::RawFixed32
            }
            EdgeInlinePropertyEncoding::RawFixed64 => {
                PreparedEdgeInlinePropertyBytesDecoder::RawFixed64
            }
            EdgeInlinePropertyEncoding::VectorF32 { dims } => {
                PreparedEdgeInlinePropertyBytesDecoder::VectorF32 { dims: *dims }
            }
            EdgeInlinePropertyEncoding::RawBytes => {
                PreparedEdgeInlinePropertyBytesDecoder::RawBytes {
                    byte_width: self.byte_width,
                }
            }
        })
    }
}

fn read_fixed<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut buf = [0u8; N];
    let len = bytes.len().min(N);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

pub fn decode_edge_inline_property(
    decoder: &PreparedEdgeInlinePropertyBytesDecoder,
    bytes: &[u8],
) -> Result<DecodedEdgeInlinePropertyBytes, EdgeInlinePropertyProfileError> {
    Ok(match decoder {
        PreparedEdgeInlinePropertyBytesDecoder::RawU8 => {
            DecodedEdgeInlinePropertyBytes::U8(read_fixed::<1>(bytes)[0])
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawU16 => {
            DecodedEdgeInlinePropertyBytes::U16(u16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawU32 => {
            DecodedEdgeInlinePropertyBytes::U32(u32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawU64 => {
            DecodedEdgeInlinePropertyBytes::U64(u64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawI8 => {
            DecodedEdgeInlinePropertyBytes::I8(i8::from_le_bytes(read_fixed::<1>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawI16 => {
            DecodedEdgeInlinePropertyBytes::I16(i16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawI32 => {
            DecodedEdgeInlinePropertyBytes::I32(i32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawI64 => {
            DecodedEdgeInlinePropertyBytes::I64(i64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::F16 => {
            DecodedEdgeInlinePropertyBytes::F16(f16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::F32 => {
            DecodedEdgeInlinePropertyBytes::F32(f32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::F64 => {
            DecodedEdgeInlinePropertyBytes::F64(f64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawU128 => {
            DecodedEdgeInlinePropertyBytes::U128(u128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawI128 => {
            DecodedEdgeInlinePropertyBytes::I128(i128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawFixed32 => {
            DecodedEdgeInlinePropertyBytes::Fixed32(read_fixed::<32>(bytes))
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawFixed64 => {
            DecodedEdgeInlinePropertyBytes::Fixed64(read_fixed::<64>(bytes))
        }
        PreparedEdgeInlinePropertyBytesDecoder::VectorF32 { dims } => {
            let dims = usize::from(*dims);
            let mut values = Vec::with_capacity(dims);
            for chunk in bytes.as_chunks::<4>().0.iter().take(dims) {
                values.push(f32::from_le_bytes(read_fixed::<4>(chunk)));
            }
            DecodedEdgeInlinePropertyBytes::VectorF32(values)
        }
        PreparedEdgeInlinePropertyBytesDecoder::RawBytes { byte_width } => {
            let w = usize::from(*byte_width);
            if bytes.len() != w {
                return Err(EdgeInlinePropertyProfileError::WidthEncodingMismatch);
            }
            DecodedEdgeInlinePropertyBytes::Bytes(bytes.to_vec())
        }
    })
}

impl Storable for EdgeInlinePropertyProfile {
    const BOUND: Bound = Bound::Bounded {
        max_size: 512,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            candid::encode_one(self)
                .expect("EdgeInlinePropertyProfile candid encode should not fail"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        candid::encode_one(&self).expect("EdgeInlinePropertyProfile candid encode should not fail")
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        candid::decode_one(&bytes).expect("EdgeInlinePropertyProfile candid decode should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_i32_round_trips() {
        let profile = EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::RawI32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = (-42i32).to_le_bytes();
        assert_eq!(
            decode_edge_inline_property(&dec, &bytes).expect("decode"),
            DecodedEdgeInlinePropertyBytes::I32(-42)
        );
    }

    #[test]
    fn validate_rejects_width_encoding_mismatch() {
        let profile = EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: EdgeInlinePropertyEncoding::RawI32,
        };
        assert_eq!(
            profile.validate(),
            Err(EdgeInlinePropertyProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn arbitrary_byte_width_validates_for_raw_bytes_profile() {
        let profile = EdgeInlinePropertyProfile::opaque_bytes(12);
        profile.validate().expect("opaque width 12 valid");
        let dec = profile.prepare().expect("prepare");
        let inline_property_bytes: Vec<u8> = (0..12).map(|i| i as u8).collect();
        assert_eq!(
            decode_edge_inline_property(&dec, &inline_property_bytes).expect("decode"),
            DecodedEdgeInlinePropertyBytes::Bytes(inline_property_bytes)
        );
    }

    #[test]
    fn no_inline_property_profile_requires_raw_bytes_encoding() {
        assert!(
            EdgeInlinePropertyProfile::no_inline_property()
                .validate()
                .is_ok()
        );
        let bad = EdgeInlinePropertyProfile {
            byte_width: 0,
            encoding: EdgeInlinePropertyEncoding::RawU16,
        };
        assert_eq!(
            bad.validate(),
            Err(EdgeInlinePropertyProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn f32_profile_round_trips() {
        let profile = EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::F32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = 3.5f32.to_le_bytes();
        assert_eq!(
            decode_edge_inline_property(&dec, &bytes).expect("decode"),
            DecodedEdgeInlinePropertyBytes::F32(3.5)
        );
    }

    #[test]
    fn vector_f32_rejects_width_dimension_mismatch() {
        let profile = EdgeInlinePropertyProfile {
            byte_width: 32,
            encoding: EdgeInlinePropertyEncoding::VectorF32 { dims: 7 },
        };
        assert_eq!(
            profile.validate(),
            Err(EdgeInlinePropertyProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn vector_f32_profile_validates_and_decodes() {
        let profile = EdgeInlinePropertyProfile {
            byte_width: 32,
            encoding: EdgeInlinePropertyEncoding::VectorF32 { dims: 8 },
        };
        let dec = profile.prepare().expect("prepare vector profile");
        let mut bytes = Vec::new();
        for value in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(
            decode_edge_inline_property(&dec, &bytes).expect("decode"),
            DecodedEdgeInlinePropertyBytes::VectorF32(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0])
        );
    }

    #[test]
    fn f16_profile_decodes_to_f16_value() {
        let profile = EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: EdgeInlinePropertyEncoding::F16,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = half::f16::from_f32(1.5).to_le_bytes();
        assert_eq!(
            decode_edge_inline_property(&dec, &bytes).expect("decode"),
            DecodedEdgeInlinePropertyBytes::F16(half::f16::from_f32(1.5))
        );
    }

    #[test]
    fn f64_profile_decodes_to_f64_value() {
        let profile = EdgeInlinePropertyProfile {
            byte_width: 8,
            encoding: EdgeInlinePropertyEncoding::F64,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = 1.23456789f64.to_le_bytes();
        assert_eq!(
            decode_edge_inline_property(&dec, &bytes).expect("decode"),
            DecodedEdgeInlinePropertyBytes::F64(1.23456789)
        );
    }
}
