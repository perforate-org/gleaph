//! Edge-label payload profiles: physical width and semantic interpretation.

use candid::CandidType;
use half::f16;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use thiserror::Error;

use super::weight::{
    EdgeWeightProfile, WeightDecodeError, WeightEncoding, WeightProfilePrepareError,
};

/// Maximum edge-payload byte width supported by labeled storage profiles.
pub const MAX_EDGE_PAYLOAD_BYTES: usize = u16::MAX as usize;

/// Stored edge-payload bytes (not part of the 4-byte labeled CSR row).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, CandidType)]
pub struct EdgePayload(Vec<u8>);

impl EdgePayload {
    pub const EMPTY: Self = Self(Vec::new());

    #[inline]
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Active payload bytes as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl Serialize for EdgePayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EdgePayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() > usize::from(MAX_EDGE_PAYLOAD_BYTES) {
            return Err(serde::de::Error::custom(format!(
                "edge payload length {} exceeds max {}",
                bytes.len(),
                MAX_EDGE_PAYLOAD_BYTES
            )));
        }
        Ok(Self(bytes))
    }
}

/// Semantic interpretation of stored edge-payload bytes.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub enum EdgePayloadEncoding {
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
    WeightRawU16,
    WeightLinearU16 {
        min: f32,
        max: f32,
    },
    WeightLogU16 {
        min: f32,
        max: f32,
    },
    WeightBinary16,
    /// Opaque fixed-width payload; [`EdgePayloadProfile::byte_width`] may be any positive width.
    RawBytes,
}

/// Label-level edge payload configuration.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct EdgePayloadProfile {
    pub byte_width: u16,
    pub encoding: EdgePayloadEncoding,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DecodedEdgePayload {
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
    F32(f32),
    VectorF32(Vec<f32>),
    Weight(f32),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreparedEdgePayloadDecoder {
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
    WeightRawU16,
    WeightLinear { min: f32, scale: f32 },
    WeightLog { min_ln: f32, scale: f32 },
    WeightBinary16,
    RawBytes { byte_width: u16 },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum EdgePayloadProfileError {
    #[error("encoding does not match physical width")]
    WidthEncodingMismatch,
    #[error("{0}")]
    WeightPrepare(#[from] WeightProfilePrepareError),
    #[error("{0}")]
    Decode(#[from] WeightDecodeError),
}

impl From<EdgeWeightProfile> for EdgePayloadProfile {
    fn from(profile: EdgeWeightProfile) -> Self {
        let encoding = match profile.encoding {
            WeightEncoding::RawU16 => EdgePayloadEncoding::WeightRawU16,
            WeightEncoding::Linear { min, max } => {
                EdgePayloadEncoding::WeightLinearU16 { min, max }
            }
            WeightEncoding::Log { min, max } => EdgePayloadEncoding::WeightLogU16 { min, max },
            WeightEncoding::Binary16 => EdgePayloadEncoding::WeightBinary16,
        };
        Self {
            byte_width: 2,
            encoding,
        }
    }
}

impl EdgePayloadProfile {
    pub const fn no_payload() -> Self {
        Self {
            byte_width: 0,
            encoding: EdgePayloadEncoding::RawBytes,
        }
    }

    /// Opaque profile with an arbitrary positive physical width.
    pub const fn opaque_bytes(byte_width: u16) -> Self {
        assert!(
            byte_width > 0,
            "opaque edge payload byte width must be positive"
        );
        Self {
            byte_width,
            encoding: EdgePayloadEncoding::RawBytes,
        }
    }

    pub const fn required_byte_width(&self) -> u16 {
        self.byte_width
    }

    pub fn validate(&self) -> Result<(), EdgePayloadProfileError> {
        let w = self.byte_width;
        let ok = match &self.encoding {
            EdgePayloadEncoding::RawBytes => true,
            _ if w == 0 => false,
            EdgePayloadEncoding::RawU8 | EdgePayloadEncoding::RawI8 => w == 1,
            EdgePayloadEncoding::RawU16
            | EdgePayloadEncoding::RawI16
            | EdgePayloadEncoding::F16
            | EdgePayloadEncoding::WeightRawU16
            | EdgePayloadEncoding::WeightLinearU16 { .. }
            | EdgePayloadEncoding::WeightLogU16 { .. }
            | EdgePayloadEncoding::WeightBinary16 => w == 2,
            EdgePayloadEncoding::RawU32
            | EdgePayloadEncoding::RawI32
            | EdgePayloadEncoding::F32 => w == 4,
            EdgePayloadEncoding::RawU64
            | EdgePayloadEncoding::RawI64
            | EdgePayloadEncoding::F64 => w == 8,
            EdgePayloadEncoding::RawU128 | EdgePayloadEncoding::RawI128 => w == 16,
            EdgePayloadEncoding::RawFixed32 => w == 32,
            EdgePayloadEncoding::RawFixed64 => w == 64,
            EdgePayloadEncoding::VectorF32 { dims } => {
                *dims > 0 && dims.checked_mul(4).is_some_and(|need| need == w)
            }
        };
        if !ok {
            return Err(EdgePayloadProfileError::WidthEncodingMismatch);
        }
        if matches!(
            self.encoding,
            EdgePayloadEncoding::WeightLinearU16 { .. } | EdgePayloadEncoding::WeightLogU16 { .. }
        ) {
            self.validate_weight_ranges()?;
        }
        Ok(())
    }

    fn validate_weight_ranges(&self) -> Result<(), EdgePayloadProfileError> {
        match &self.encoding {
            EdgePayloadEncoding::WeightLinearU16 { min, max } => {
                if !min.is_finite() || !max.is_finite() || min > max {
                    return Err(WeightProfilePrepareError::InvalidLinearRange.into());
                }
            }
            EdgePayloadEncoding::WeightLogU16 { min, max } => {
                if !min.is_finite() || !max.is_finite() || *min <= 0.0 || *max <= 0.0 || min > max {
                    return Err(WeightProfilePrepareError::InvalidLogRange.into());
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn prepare(&self) -> Result<PreparedEdgePayloadDecoder, EdgePayloadProfileError> {
        self.validate()?;
        Ok(match &self.encoding {
            EdgePayloadEncoding::RawU8 => PreparedEdgePayloadDecoder::RawU8,
            EdgePayloadEncoding::RawU16 => PreparedEdgePayloadDecoder::RawU16,
            EdgePayloadEncoding::RawU32 => PreparedEdgePayloadDecoder::RawU32,
            EdgePayloadEncoding::RawU64 => PreparedEdgePayloadDecoder::RawU64,
            EdgePayloadEncoding::RawI8 => PreparedEdgePayloadDecoder::RawI8,
            EdgePayloadEncoding::RawI16 => PreparedEdgePayloadDecoder::RawI16,
            EdgePayloadEncoding::RawI32 => PreparedEdgePayloadDecoder::RawI32,
            EdgePayloadEncoding::RawI64 => PreparedEdgePayloadDecoder::RawI64,
            EdgePayloadEncoding::F16 => PreparedEdgePayloadDecoder::F16,
            EdgePayloadEncoding::F32 => PreparedEdgePayloadDecoder::F32,
            EdgePayloadEncoding::F64 => PreparedEdgePayloadDecoder::F64,
            EdgePayloadEncoding::RawU128 => PreparedEdgePayloadDecoder::RawU128,
            EdgePayloadEncoding::RawI128 => PreparedEdgePayloadDecoder::RawI128,
            EdgePayloadEncoding::RawFixed32 => PreparedEdgePayloadDecoder::RawFixed32,
            EdgePayloadEncoding::RawFixed64 => PreparedEdgePayloadDecoder::RawFixed64,
            EdgePayloadEncoding::VectorF32 { dims } => {
                PreparedEdgePayloadDecoder::VectorF32 { dims: *dims }
            }
            EdgePayloadEncoding::WeightRawU16 => PreparedEdgePayloadDecoder::WeightRawU16,
            EdgePayloadEncoding::WeightLinearU16 { min, max } => {
                let scale = if max > min {
                    (max - min) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedEdgePayloadDecoder::WeightLinear { min: *min, scale }
            }
            EdgePayloadEncoding::WeightLogU16 { min, max } => {
                let min_ln = min.ln();
                let max_ln = max.ln();
                let scale = if max_ln > min_ln {
                    (max_ln - min_ln) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedEdgePayloadDecoder::WeightLog { min_ln, scale }
            }
            EdgePayloadEncoding::WeightBinary16 => PreparedEdgePayloadDecoder::WeightBinary16,
            EdgePayloadEncoding::RawBytes => PreparedEdgePayloadDecoder::RawBytes {
                byte_width: self.byte_width,
            },
        })
    }
}

fn read_fixed<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut buf = [0u8; N];
    let len = bytes.len().min(N);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

pub fn decode_edge_payload(
    decoder: &PreparedEdgePayloadDecoder,
    bytes: &[u8],
) -> Result<DecodedEdgePayload, EdgePayloadProfileError> {
    Ok(match decoder {
        PreparedEdgePayloadDecoder::RawU8 => DecodedEdgePayload::U8(read_fixed::<1>(bytes)[0]),
        PreparedEdgePayloadDecoder::RawU16 => {
            DecodedEdgePayload::U16(u16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawU32 => {
            DecodedEdgePayload::U32(u32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawU64 => {
            DecodedEdgePayload::U64(u64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawI8 => {
            DecodedEdgePayload::I8(i8::from_le_bytes(read_fixed::<1>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawI16 => {
            DecodedEdgePayload::I16(i16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawI32 => {
            DecodedEdgePayload::I32(i32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawI64 => {
            DecodedEdgePayload::I64(i64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgePayloadDecoder::F16 => {
            DecodedEdgePayload::F32(f16::from_le_bytes(read_fixed::<2>(bytes)).to_f32())
        }
        PreparedEdgePayloadDecoder::F32 => {
            DecodedEdgePayload::F32(f32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgePayloadDecoder::F64 => {
            DecodedEdgePayload::F32(f64::from_le_bytes(read_fixed::<8>(bytes)) as f32)
        }
        PreparedEdgePayloadDecoder::RawU128 => {
            DecodedEdgePayload::U128(u128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawI128 => {
            DecodedEdgePayload::I128(i128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgePayloadDecoder::RawFixed32 => {
            DecodedEdgePayload::Fixed32(read_fixed::<32>(bytes))
        }
        PreparedEdgePayloadDecoder::RawFixed64 => {
            DecodedEdgePayload::Fixed64(read_fixed::<64>(bytes))
        }
        PreparedEdgePayloadDecoder::VectorF32 { dims } => {
            let dims = usize::from(*dims);
            let mut values = Vec::with_capacity(dims);
            for chunk in bytes.chunks_exact(4).take(dims) {
                values.push(f32::from_le_bytes(read_fixed::<4>(chunk)));
            }
            DecodedEdgePayload::VectorF32(values)
        }
        PreparedEdgePayloadDecoder::WeightRawU16 => {
            let v = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            validate_weight_f32(v)?;
            DecodedEdgePayload::Weight(v)
        }
        PreparedEdgePayloadDecoder::WeightLinear { min, scale } => {
            let raw = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            let v = min + scale * raw;
            validate_weight_f32(v)?;
            DecodedEdgePayload::Weight(v)
        }
        PreparedEdgePayloadDecoder::WeightLog { min_ln, scale } => {
            let raw = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            let v = (min_ln + scale * raw).exp();
            validate_weight_f32(v)?;
            DecodedEdgePayload::Weight(v)
        }
        PreparedEdgePayloadDecoder::WeightBinary16 => {
            let v = f16::from_le_bytes(read_fixed::<2>(bytes)).to_f32();
            validate_weight_f32(v)?;
            DecodedEdgePayload::Weight(v)
        }
        PreparedEdgePayloadDecoder::RawBytes { byte_width } => {
            let w = usize::from(*byte_width);
            if bytes.len() != w {
                return Err(EdgePayloadProfileError::WidthEncodingMismatch);
            }
            DecodedEdgePayload::Bytes(bytes.to_vec())
        }
    })
}

fn validate_weight_f32(v: f32) -> Result<(), WeightDecodeError> {
    if !v.is_finite() {
        return Err(WeightDecodeError::NonFinite);
    }
    if v < 0.0 {
        return Err(WeightDecodeError::Negative);
    }
    Ok(())
}

/// Decodes traversal weight from edge-payload bytes using a prepared decoder.
pub fn decode_edge_weight(
    decoder: &PreparedEdgePayloadDecoder,
    bytes: &[u8],
) -> Result<f32, EdgePayloadProfileError> {
    match decode_edge_payload(decoder, bytes)? {
        DecodedEdgePayload::Weight(w) => Ok(w),
        _ => Err(EdgePayloadProfileError::WidthEncodingMismatch),
    }
}

impl Storable for EdgePayloadProfile {
    const BOUND: Bound = Bound::Bounded {
        max_size: 512,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            candid::encode_one(self).expect("EdgePayloadProfile candid encode should not fail"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        candid::encode_one(&self).expect("EdgePayloadProfile candid encode should not fail")
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        candid::decode_one(&bytes).expect("EdgePayloadProfile candid decode should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_i32_round_trips() {
        let profile = EdgePayloadProfile {
            byte_width: 4,
            encoding: EdgePayloadEncoding::RawI32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = (-42i32).to_le_bytes();
        assert_eq!(
            decode_edge_payload(&dec, &bytes).expect("decode"),
            DecodedEdgePayload::I32(-42)
        );
    }

    #[test]
    fn weight_u16_profile_decodes() {
        let profile = EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightRawU16,
        };
        let dec = profile.prepare().expect("prepare");
        let w = decode_edge_weight(&dec, &3u16.to_le_bytes()).expect("weight");
        assert_eq!(w, 3.0);
    }

    #[test]
    fn validate_rejects_width_encoding_mismatch() {
        let profile = EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::RawI32,
        };
        assert_eq!(
            profile.validate(),
            Err(EdgePayloadProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn arbitrary_byte_width_validates_for_raw_bytes_profile() {
        let profile = EdgePayloadProfile::opaque_bytes(12);
        profile.validate().expect("opaque width 12 valid");
        let dec = profile.prepare().expect("prepare");
        let payload: Vec<u8> = (0..12).map(|i| i as u8).collect();
        assert_eq!(
            decode_edge_payload(&dec, &payload).expect("decode"),
            DecodedEdgePayload::Bytes(payload)
        );
    }

    #[test]
    fn no_payload_profile_requires_raw_bytes_encoding() {
        assert!(EdgePayloadProfile::no_payload().validate().is_ok());
        let bad = EdgePayloadProfile {
            byte_width: 0,
            encoding: EdgePayloadEncoding::RawU16,
        };
        assert_eq!(
            bad.validate(),
            Err(EdgePayloadProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn weight_log_u16_decodes() {
        let profile = EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightLogU16 {
                min: 1.0,
                max: 10.0,
            },
        };
        let dec = profile.prepare().expect("prepare");
        let w = decode_edge_weight(&dec, &u16::MAX.to_le_bytes()).expect("weight");
        assert!((w - 10.0).abs() < 0.01);
    }

    #[test]
    fn f32_profile_round_trips() {
        let profile = EdgePayloadProfile {
            byte_width: 4,
            encoding: EdgePayloadEncoding::F32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = 3.5f32.to_le_bytes();
        assert_eq!(
            decode_edge_payload(&dec, &bytes).expect("decode"),
            DecodedEdgePayload::F32(3.5)
        );
    }

    #[test]
    fn decode_edge_weight_rejects_non_weight_encoding() {
        let profile = EdgePayloadProfile {
            byte_width: 4,
            encoding: EdgePayloadEncoding::RawI32,
        };
        let dec = profile.prepare().expect("prepare");
        let err = decode_edge_weight(&dec, &42i32.to_le_bytes()).unwrap_err();
        assert_eq!(err, EdgePayloadProfileError::WidthEncodingMismatch);
    }

    #[test]
    fn vector_f32_rejects_width_dimension_mismatch() {
        let profile = EdgePayloadProfile {
            byte_width: 32,
            encoding: EdgePayloadEncoding::VectorF32 { dims: 7 },
        };
        assert_eq!(
            profile.validate(),
            Err(EdgePayloadProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn weight_linear_u16_decodes_scaled() {
        let profile = EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightLinearU16 {
                min: 10.0,
                max: 20.0,
            },
        };
        let dec = profile.prepare().expect("prepare");
        let w = decode_edge_weight(&dec, &u16::MAX.to_le_bytes()).expect("weight");
        assert!((w - 20.0).abs() < 1e-4);
        let w0 = decode_edge_weight(&dec, &0u16.to_le_bytes()).expect("weight min");
        assert!((w0 - 10.0).abs() < 1e-4);
    }

    #[test]
    fn vector_f32_profile_validates_and_decodes() {
        let profile = EdgePayloadProfile {
            byte_width: 32,
            encoding: EdgePayloadEncoding::VectorF32 { dims: 8 },
        };
        let dec = profile.prepare().expect("prepare vector profile");
        let mut bytes = Vec::new();
        for value in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(
            decode_edge_payload(&dec, &bytes).expect("decode"),
            DecodedEdgePayload::VectorF32(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0])
        );
    }

    #[test]
    fn edge_weight_profile_converts_to_payload_profile() {
        use super::super::weight::{EdgeWeightProfile, WeightEncoding};
        let weight = EdgeWeightProfile {
            encoding: WeightEncoding::Linear { min: 0.0, max: 1.0 },
        };
        let profile = EdgePayloadProfile::from(weight);
        assert_eq!(profile.byte_width, 2);
        assert!(matches!(
            profile.encoding,
            EdgePayloadEncoding::WeightLinearU16 { .. }
        ));
        profile.validate().expect("converted profile valid");
    }
}
