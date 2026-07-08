//! Edge-label inline value profiles: physical width and semantic interpretation.

use candid::CandidType;
use half::f16;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use thiserror::Error;

use super::weight::{
    EdgeWeightProfile, WeightDecodeError, WeightEncoding, WeightProfilePrepareError,
};

/// Maximum edge-inline-value byte width supported by labeled storage profiles.
pub const MAX_EDGE_INLINE_VALUE_BYTES: usize = u16::MAX as usize;

/// Stored edge-inline-value bytes (not part of the 4-byte labeled CSR row).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, CandidType)]
pub struct EdgeInlineValue(Vec<u8>);

impl EdgeInlineValue {
    pub const EMPTY: Self = Self(Vec::new());

    #[inline]
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Active inline value bytes as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl Serialize for EdgeInlineValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EdgeInlineValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() > MAX_EDGE_INLINE_VALUE_BYTES {
            return Err(serde::de::Error::custom(format!(
                "edge inline value length {} exceeds max {}",
                bytes.len(),
                MAX_EDGE_INLINE_VALUE_BYTES
            )));
        }
        Ok(Self(bytes))
    }
}

/// Semantic interpretation of stored edge-inline-value bytes.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub enum EdgeInlineValueEncoding {
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
    /// Opaque fixed-width inline value; [`EdgeInlineValueProfile::byte_width`] may be any positive width.
    RawBytes,
}

/// Label-level edge inline value configuration.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct EdgeInlineValueProfile {
    pub byte_width: u16,
    pub encoding: EdgeInlineValueEncoding,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DecodedEdgeInlineValue {
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
    Weight(f32),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreparedEdgeInlineValueDecoder {
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
pub enum EdgeInlineValueProfileError {
    #[error("encoding does not match physical width")]
    WidthEncodingMismatch,
    #[error("{0}")]
    WeightPrepare(#[from] WeightProfilePrepareError),
    #[error("{0}")]
    Decode(#[from] WeightDecodeError),
}

impl From<EdgeWeightProfile> for EdgeInlineValueProfile {
    fn from(profile: EdgeWeightProfile) -> Self {
        let encoding = match profile.encoding {
            WeightEncoding::RawU16 => EdgeInlineValueEncoding::WeightRawU16,
            WeightEncoding::Linear { min, max } => {
                EdgeInlineValueEncoding::WeightLinearU16 { min, max }
            }
            WeightEncoding::Log { min, max } => EdgeInlineValueEncoding::WeightLogU16 { min, max },
            WeightEncoding::Binary16 => EdgeInlineValueEncoding::WeightBinary16,
        };
        Self {
            byte_width: 2,
            encoding,
        }
    }
}

impl EdgeInlineValueProfile {
    /// Returns a legacy [`EdgeWeightProfile`] when this inline value profile uses a weight encoding.
    pub fn to_weight_profile(&self) -> Option<EdgeWeightProfile> {
        if self.byte_width != 2 {
            return None;
        }
        let encoding = match self.encoding {
            EdgeInlineValueEncoding::WeightRawU16 => WeightEncoding::RawU16,
            EdgeInlineValueEncoding::WeightLinearU16 { min, max } => {
                WeightEncoding::Linear { min, max }
            }
            EdgeInlineValueEncoding::WeightLogU16 { min, max } => WeightEncoding::Log { min, max },
            EdgeInlineValueEncoding::WeightBinary16 => WeightEncoding::Binary16,
            _ => return None,
        };
        let profile = EdgeWeightProfile { encoding };
        profile.validate().ok()?;
        Some(profile)
    }

    pub const fn no_inline_value() -> Self {
        Self {
            byte_width: 0,
            encoding: EdgeInlineValueEncoding::RawBytes,
        }
    }

    /// Opaque profile with an arbitrary positive physical width.
    pub const fn opaque_bytes(byte_width: u16) -> Self {
        assert!(
            byte_width > 0,
            "opaque edge inline value byte width must be positive"
        );
        Self {
            byte_width,
            encoding: EdgeInlineValueEncoding::RawBytes,
        }
    }

    pub const fn required_byte_width(&self) -> u16 {
        self.byte_width
    }

    pub fn validate(&self) -> Result<(), EdgeInlineValueProfileError> {
        let w = self.byte_width;
        let ok = match &self.encoding {
            EdgeInlineValueEncoding::RawBytes => true,
            _ if w == 0 => false,
            EdgeInlineValueEncoding::RawU8 | EdgeInlineValueEncoding::RawI8 => w == 1,
            EdgeInlineValueEncoding::RawU16
            | EdgeInlineValueEncoding::RawI16
            | EdgeInlineValueEncoding::F16
            | EdgeInlineValueEncoding::WeightRawU16
            | EdgeInlineValueEncoding::WeightLinearU16 { .. }
            | EdgeInlineValueEncoding::WeightLogU16 { .. }
            | EdgeInlineValueEncoding::WeightBinary16 => w == 2,
            EdgeInlineValueEncoding::RawU32
            | EdgeInlineValueEncoding::RawI32
            | EdgeInlineValueEncoding::F32 => w == 4,
            EdgeInlineValueEncoding::RawU64
            | EdgeInlineValueEncoding::RawI64
            | EdgeInlineValueEncoding::F64 => w == 8,
            EdgeInlineValueEncoding::RawU128 | EdgeInlineValueEncoding::RawI128 => w == 16,
            EdgeInlineValueEncoding::RawFixed32 => w == 32,
            EdgeInlineValueEncoding::RawFixed64 => w == 64,
            EdgeInlineValueEncoding::VectorF32 { dims } => {
                *dims > 0 && dims.checked_mul(4).is_some_and(|need| need == w)
            }
        };
        if !ok {
            return Err(EdgeInlineValueProfileError::WidthEncodingMismatch);
        }
        if matches!(
            self.encoding,
            EdgeInlineValueEncoding::WeightLinearU16 { .. }
                | EdgeInlineValueEncoding::WeightLogU16 { .. }
        ) {
            self.validate_weight_ranges()?;
        }
        Ok(())
    }

    fn validate_weight_ranges(&self) -> Result<(), EdgeInlineValueProfileError> {
        match &self.encoding {
            EdgeInlineValueEncoding::WeightLinearU16 { min, max } => {
                if !min.is_finite() || !max.is_finite() || min > max {
                    return Err(WeightProfilePrepareError::InvalidLinearRange.into());
                }
            }
            EdgeInlineValueEncoding::WeightLogU16 { min, max }
                if (!min.is_finite()
                    || !max.is_finite()
                    || *min <= 0.0
                    || *max <= 0.0
                    || min > max) =>
            {
                return Err(WeightProfilePrepareError::InvalidLogRange.into());
            }
            _ => {}
        }
        Ok(())
    }

    pub fn prepare(&self) -> Result<PreparedEdgeInlineValueDecoder, EdgeInlineValueProfileError> {
        self.validate()?;
        Ok(match &self.encoding {
            EdgeInlineValueEncoding::RawU8 => PreparedEdgeInlineValueDecoder::RawU8,
            EdgeInlineValueEncoding::RawU16 => PreparedEdgeInlineValueDecoder::RawU16,
            EdgeInlineValueEncoding::RawU32 => PreparedEdgeInlineValueDecoder::RawU32,
            EdgeInlineValueEncoding::RawU64 => PreparedEdgeInlineValueDecoder::RawU64,
            EdgeInlineValueEncoding::RawI8 => PreparedEdgeInlineValueDecoder::RawI8,
            EdgeInlineValueEncoding::RawI16 => PreparedEdgeInlineValueDecoder::RawI16,
            EdgeInlineValueEncoding::RawI32 => PreparedEdgeInlineValueDecoder::RawI32,
            EdgeInlineValueEncoding::RawI64 => PreparedEdgeInlineValueDecoder::RawI64,
            EdgeInlineValueEncoding::F16 => PreparedEdgeInlineValueDecoder::F16,
            EdgeInlineValueEncoding::F32 => PreparedEdgeInlineValueDecoder::F32,
            EdgeInlineValueEncoding::F64 => PreparedEdgeInlineValueDecoder::F64,
            EdgeInlineValueEncoding::RawU128 => PreparedEdgeInlineValueDecoder::RawU128,
            EdgeInlineValueEncoding::RawI128 => PreparedEdgeInlineValueDecoder::RawI128,
            EdgeInlineValueEncoding::RawFixed32 => PreparedEdgeInlineValueDecoder::RawFixed32,
            EdgeInlineValueEncoding::RawFixed64 => PreparedEdgeInlineValueDecoder::RawFixed64,
            EdgeInlineValueEncoding::VectorF32 { dims } => {
                PreparedEdgeInlineValueDecoder::VectorF32 { dims: *dims }
            }
            EdgeInlineValueEncoding::WeightRawU16 => PreparedEdgeInlineValueDecoder::WeightRawU16,
            EdgeInlineValueEncoding::WeightLinearU16 { min, max } => {
                let scale = if max > min {
                    (max - min) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedEdgeInlineValueDecoder::WeightLinear { min: *min, scale }
            }
            EdgeInlineValueEncoding::WeightLogU16 { min, max } => {
                let min_ln = min.ln();
                let max_ln = max.ln();
                let scale = if max_ln > min_ln {
                    (max_ln - min_ln) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedEdgeInlineValueDecoder::WeightLog { min_ln, scale }
            }
            EdgeInlineValueEncoding::WeightBinary16 => {
                PreparedEdgeInlineValueDecoder::WeightBinary16
            }
            EdgeInlineValueEncoding::RawBytes => PreparedEdgeInlineValueDecoder::RawBytes {
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

pub fn decode_edge_inline_value(
    decoder: &PreparedEdgeInlineValueDecoder,
    bytes: &[u8],
) -> Result<DecodedEdgeInlineValue, EdgeInlineValueProfileError> {
    Ok(match decoder {
        PreparedEdgeInlineValueDecoder::RawU8 => {
            DecodedEdgeInlineValue::U8(read_fixed::<1>(bytes)[0])
        }
        PreparedEdgeInlineValueDecoder::RawU16 => {
            DecodedEdgeInlineValue::U16(u16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawU32 => {
            DecodedEdgeInlineValue::U32(u32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawU64 => {
            DecodedEdgeInlineValue::U64(u64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawI8 => {
            DecodedEdgeInlineValue::I8(i8::from_le_bytes(read_fixed::<1>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawI16 => {
            DecodedEdgeInlineValue::I16(i16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawI32 => {
            DecodedEdgeInlineValue::I32(i32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawI64 => {
            DecodedEdgeInlineValue::I64(i64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::F16 => {
            DecodedEdgeInlineValue::F16(f16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::F32 => {
            DecodedEdgeInlineValue::F32(f32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::F64 => {
            DecodedEdgeInlineValue::F64(f64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawU128 => {
            DecodedEdgeInlineValue::U128(u128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawI128 => {
            DecodedEdgeInlineValue::I128(i128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgeInlineValueDecoder::RawFixed32 => {
            DecodedEdgeInlineValue::Fixed32(read_fixed::<32>(bytes))
        }
        PreparedEdgeInlineValueDecoder::RawFixed64 => {
            DecodedEdgeInlineValue::Fixed64(read_fixed::<64>(bytes))
        }
        PreparedEdgeInlineValueDecoder::VectorF32 { dims } => {
            let dims = usize::from(*dims);
            let mut values = Vec::with_capacity(dims);
            for chunk in bytes.chunks_exact(4).take(dims) {
                values.push(f32::from_le_bytes(read_fixed::<4>(chunk)));
            }
            DecodedEdgeInlineValue::VectorF32(values)
        }
        PreparedEdgeInlineValueDecoder::WeightRawU16 => {
            let v = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            validate_weight_f32(v)?;
            DecodedEdgeInlineValue::Weight(v)
        }
        PreparedEdgeInlineValueDecoder::WeightLinear { min, scale } => {
            let raw = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            let v = min + scale * raw;
            validate_weight_f32(v)?;
            DecodedEdgeInlineValue::Weight(v)
        }
        PreparedEdgeInlineValueDecoder::WeightLog { min_ln, scale } => {
            let raw = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            let v = (min_ln + scale * raw).exp();
            validate_weight_f32(v)?;
            DecodedEdgeInlineValue::Weight(v)
        }
        PreparedEdgeInlineValueDecoder::WeightBinary16 => {
            let v = f16::from_le_bytes(read_fixed::<2>(bytes)).to_f32();
            validate_weight_f32(v)?;
            DecodedEdgeInlineValue::Weight(v)
        }
        PreparedEdgeInlineValueDecoder::RawBytes { byte_width } => {
            let w = usize::from(*byte_width);
            if bytes.len() != w {
                return Err(EdgeInlineValueProfileError::WidthEncodingMismatch);
            }
            DecodedEdgeInlineValue::Bytes(bytes.to_vec())
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

/// Decodes traversal weight from edge-inline-value bytes using a prepared decoder.
pub fn decode_edge_weight(
    decoder: &PreparedEdgeInlineValueDecoder,
    bytes: &[u8],
) -> Result<f32, EdgeInlineValueProfileError> {
    match decode_edge_inline_value(decoder, bytes)? {
        DecodedEdgeInlineValue::Weight(w) => Ok(w),
        _ => Err(EdgeInlineValueProfileError::WidthEncodingMismatch),
    }
}

impl Storable for EdgeInlineValueProfile {
    const BOUND: Bound = Bound::Bounded {
        max_size: 512,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            candid::encode_one(self).expect("EdgeInlineValueProfile candid encode should not fail"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        candid::encode_one(&self).expect("EdgeInlineValueProfile candid encode should not fail")
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        candid::decode_one(&bytes).expect("EdgeInlineValueProfile candid decode should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_i32_round_trips() {
        let profile = EdgeInlineValueProfile {
            byte_width: 4,
            encoding: EdgeInlineValueEncoding::RawI32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = (-42i32).to_le_bytes();
        assert_eq!(
            decode_edge_inline_value(&dec, &bytes).expect("decode"),
            DecodedEdgeInlineValue::I32(-42)
        );
    }

    #[test]
    fn weight_u16_profile_decodes() {
        let profile = EdgeInlineValueProfile {
            byte_width: 2,
            encoding: EdgeInlineValueEncoding::WeightRawU16,
        };
        let dec = profile.prepare().expect("prepare");
        let w = decode_edge_weight(&dec, &3u16.to_le_bytes()).expect("weight");
        assert_eq!(w, 3.0);
    }

    #[test]
    fn validate_rejects_width_encoding_mismatch() {
        let profile = EdgeInlineValueProfile {
            byte_width: 2,
            encoding: EdgeInlineValueEncoding::RawI32,
        };
        assert_eq!(
            profile.validate(),
            Err(EdgeInlineValueProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn arbitrary_byte_width_validates_for_raw_bytes_profile() {
        let profile = EdgeInlineValueProfile::opaque_bytes(12);
        profile.validate().expect("opaque width 12 valid");
        let dec = profile.prepare().expect("prepare");
        let payload: Vec<u8> = (0..12).map(|i| i as u8).collect();
        assert_eq!(
            decode_edge_inline_value(&dec, &payload).expect("decode"),
            DecodedEdgeInlineValue::Bytes(payload)
        );
    }

    #[test]
    fn no_inline_value_profile_requires_raw_bytes_encoding() {
        assert!(EdgeInlineValueProfile::no_inline_value().validate().is_ok());
        let bad = EdgeInlineValueProfile {
            byte_width: 0,
            encoding: EdgeInlineValueEncoding::RawU16,
        };
        assert_eq!(
            bad.validate(),
            Err(EdgeInlineValueProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn weight_log_u16_decodes() {
        let profile = EdgeInlineValueProfile {
            byte_width: 2,
            encoding: EdgeInlineValueEncoding::WeightLogU16 {
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
        let profile = EdgeInlineValueProfile {
            byte_width: 4,
            encoding: EdgeInlineValueEncoding::F32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = 3.5f32.to_le_bytes();
        assert_eq!(
            decode_edge_inline_value(&dec, &bytes).expect("decode"),
            DecodedEdgeInlineValue::F32(3.5)
        );
    }

    #[test]
    fn decode_edge_weight_rejects_non_weight_encoding() {
        let profile = EdgeInlineValueProfile {
            byte_width: 4,
            encoding: EdgeInlineValueEncoding::RawI32,
        };
        let dec = profile.prepare().expect("prepare");
        let err = decode_edge_weight(&dec, &42i32.to_le_bytes()).unwrap_err();
        assert_eq!(err, EdgeInlineValueProfileError::WidthEncodingMismatch);
    }

    #[test]
    fn vector_f32_rejects_width_dimension_mismatch() {
        let profile = EdgeInlineValueProfile {
            byte_width: 32,
            encoding: EdgeInlineValueEncoding::VectorF32 { dims: 7 },
        };
        assert_eq!(
            profile.validate(),
            Err(EdgeInlineValueProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn weight_linear_u16_decodes_scaled() {
        let profile = EdgeInlineValueProfile {
            byte_width: 2,
            encoding: EdgeInlineValueEncoding::WeightLinearU16 {
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
        let profile = EdgeInlineValueProfile {
            byte_width: 32,
            encoding: EdgeInlineValueEncoding::VectorF32 { dims: 8 },
        };
        let dec = profile.prepare().expect("prepare vector profile");
        let mut bytes = Vec::new();
        for value in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(
            decode_edge_inline_value(&dec, &bytes).expect("decode"),
            DecodedEdgeInlineValue::VectorF32(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0])
        );
    }

    #[test]
    fn edge_weight_profile_converts_to_inline_value_profile() {
        use super::super::weight::{EdgeWeightProfile, WeightEncoding};
        let weight = EdgeWeightProfile {
            encoding: WeightEncoding::Linear { min: 0.0, max: 1.0 },
        };
        let profile = EdgeInlineValueProfile::from(weight.clone());
        assert_eq!(profile.byte_width, 2);
        assert!(matches!(
            profile.encoding,
            EdgeInlineValueEncoding::WeightLinearU16 { .. }
        ));
        profile.validate().expect("converted profile valid");
        assert_eq!(profile.to_weight_profile(), Some(weight));
    }

    #[test]
    fn non_weight_inline_value_profile_has_no_weight_view() {
        let profile = EdgeInlineValueProfile {
            byte_width: 4,
            encoding: EdgeInlineValueEncoding::RawI32,
        };
        assert_eq!(profile.to_weight_profile(), None);
    }
    #[test]
    fn f16_profile_decodes_to_f16_value() {
        let profile = EdgeInlineValueProfile {
            byte_width: 2,
            encoding: EdgeInlineValueEncoding::F16,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = half::f16::from_f32(1.5).to_le_bytes();
        assert_eq!(
            decode_edge_inline_value(&dec, &bytes).expect("decode"),
            DecodedEdgeInlineValue::F16(half::f16::from_f32(1.5))
        );
    }

    #[test]
    fn f64_profile_decodes_to_f64_value() {
        let profile = EdgeInlineValueProfile {
            byte_width: 8,
            encoding: EdgeInlineValueEncoding::F64,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = 1.23456789f64.to_le_bytes();
        assert_eq!(
            decode_edge_inline_value(&dec, &bytes).expect("decode"),
            DecodedEdgeInlineValue::F64(1.23456789)
        );
    }
}
