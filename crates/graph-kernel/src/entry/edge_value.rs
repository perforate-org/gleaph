//! Edge-label value profiles: physical width and semantic interpretation.

use half::f16;
use ic_stable_lara::labeled::slot_index::ValueWidthCode;
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;
use thiserror::Error;

use super::weight::{
    EdgeWeightProfile, WeightDecodeError, WeightEncoding, WeightProfilePrepareError,
};

/// Physical storage width for edge values (maps to LARA `value_width_code`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Serialize, serde::Deserialize,
)]
pub enum EdgeValueWidth {
    Zero,
    W1,
    W2,
    W4,
    W8,
    W16,
    W32,
    W64,
}

impl EdgeValueWidth {
    #[inline]
    pub const fn byte_width(self) -> u8 {
        match self {
            Self::Zero => 0,
            Self::W1 => 1,
            Self::W2 => 2,
            Self::W4 => 4,
            Self::W8 => 8,
            Self::W16 => 16,
            Self::W32 => 32,
            Self::W64 => 64,
        }
    }

    #[inline]
    pub const fn to_width_code(self) -> ValueWidthCode {
        match self {
            Self::Zero => ValueWidthCode::Zero,
            Self::W1 => ValueWidthCode::W1,
            Self::W2 => ValueWidthCode::W2,
            Self::W4 => ValueWidthCode::W4,
            Self::W8 => ValueWidthCode::W8,
            Self::W16 => ValueWidthCode::W16,
            Self::W32 => ValueWidthCode::W32,
            Self::W64 => ValueWidthCode::W64,
        }
    }

    #[inline]
    pub const fn from_byte_width(width: u8) -> Option<Self> {
        match width {
            0 => Some(Self::Zero),
            1 => Some(Self::W1),
            2 => Some(Self::W2),
            4 => Some(Self::W4),
            8 => Some(Self::W8),
            16 => Some(Self::W16),
            32 => Some(Self::W32),
            64 => Some(Self::W64),
            _ => None,
        }
    }
}

/// Semantic interpretation of stored edge-value bytes.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub enum EdgeValueEncoding {
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
    WeightRawU16,
    WeightLinearU16 { min: f32, max: f32 },
    WeightLogU16 { min: f32, max: f32 },
    WeightBinary16,
}

/// Label-level edge value configuration.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct EdgeValueProfile {
    pub width: EdgeValueWidth,
    pub encoding: EdgeValueEncoding,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DecodedEdgeValue {
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
    Weight(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreparedEdgeValueDecoder {
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
    WeightRawU16,
    WeightLinear { min: f32, scale: f32 },
    WeightLog { min_ln: f32, scale: f32 },
    WeightBinary16,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum EdgeValueProfileError {
    #[error("encoding does not match physical width")]
    WidthEncodingMismatch,
    #[error("{0}")]
    WeightPrepare(#[from] WeightProfilePrepareError),
    #[error("{0}")]
    Decode(#[from] WeightDecodeError),
}

impl From<EdgeWeightProfile> for EdgeValueProfile {
    fn from(profile: EdgeWeightProfile) -> Self {
        let encoding = match profile.encoding {
            WeightEncoding::RawU16 => EdgeValueEncoding::WeightRawU16,
            WeightEncoding::Linear { min, max } => EdgeValueEncoding::WeightLinearU16 { min, max },
            WeightEncoding::Log { min, max } => EdgeValueEncoding::WeightLogU16 { min, max },
            WeightEncoding::Binary16 => EdgeValueEncoding::WeightBinary16,
        };
        Self {
            width: EdgeValueWidth::W2,
            encoding,
        }
    }
}

impl EdgeValueProfile {
    pub const fn no_value() -> Self {
        Self {
            width: EdgeValueWidth::Zero,
            encoding: EdgeValueEncoding::RawU8,
        }
    }

    pub fn required_byte_width(&self) -> u8 {
        self.width.byte_width()
    }

    pub fn validate(&self) -> Result<(), EdgeValueProfileError> {
        let w = self.width.byte_width();
        let ok = match (&self.width, &self.encoding) {
            (EdgeValueWidth::Zero, _) => w == 0,
            (EdgeValueWidth::W1, EdgeValueEncoding::RawU8 | EdgeValueEncoding::RawI8) => w == 1,
            (EdgeValueWidth::W2, _) => {
                matches!(
                    self.encoding,
                    EdgeValueEncoding::RawU16
                        | EdgeValueEncoding::RawI16
                        | EdgeValueEncoding::F16
                        | EdgeValueEncoding::WeightRawU16
                        | EdgeValueEncoding::WeightLinearU16 { .. }
                        | EdgeValueEncoding::WeightLogU16 { .. }
                        | EdgeValueEncoding::WeightBinary16
                )
            }
            (
                EdgeValueWidth::W4,
                EdgeValueEncoding::RawU32 | EdgeValueEncoding::RawI32 | EdgeValueEncoding::F32,
            ) => w == 4,
            (
                EdgeValueWidth::W8,
                EdgeValueEncoding::RawU64 | EdgeValueEncoding::RawI64 | EdgeValueEncoding::F64,
            ) => w == 8,
            (EdgeValueWidth::W16, EdgeValueEncoding::RawU128 | EdgeValueEncoding::RawI128) => {
                w == 16
            }
            (EdgeValueWidth::W32, EdgeValueEncoding::RawFixed32) => w == 32,
            (EdgeValueWidth::W64, EdgeValueEncoding::RawFixed64) => w == 64,
            _ => false,
        };
        if !ok {
            return Err(EdgeValueProfileError::WidthEncodingMismatch);
        }
        if matches!(
            self.encoding,
            EdgeValueEncoding::WeightLinearU16 { .. } | EdgeValueEncoding::WeightLogU16 { .. }
        ) {
            self.validate_weight_ranges()?;
        }
        Ok(())
    }

    fn validate_weight_ranges(&self) -> Result<(), EdgeValueProfileError> {
        match &self.encoding {
            EdgeValueEncoding::WeightLinearU16 { min, max } => {
                if !min.is_finite() || !max.is_finite() || min > max {
                    return Err(WeightProfilePrepareError::InvalidLinearRange.into());
                }
            }
            EdgeValueEncoding::WeightLogU16 { min, max } => {
                if !min.is_finite() || !max.is_finite() || *min <= 0.0 || *max <= 0.0 || min > max {
                    return Err(WeightProfilePrepareError::InvalidLogRange.into());
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn prepare(&self) -> Result<PreparedEdgeValueDecoder, EdgeValueProfileError> {
        self.validate()?;
        Ok(match &self.encoding {
            EdgeValueEncoding::RawU8 => PreparedEdgeValueDecoder::RawU8,
            EdgeValueEncoding::RawU16 => PreparedEdgeValueDecoder::RawU16,
            EdgeValueEncoding::RawU32 => PreparedEdgeValueDecoder::RawU32,
            EdgeValueEncoding::RawU64 => PreparedEdgeValueDecoder::RawU64,
            EdgeValueEncoding::RawI8 => PreparedEdgeValueDecoder::RawI8,
            EdgeValueEncoding::RawI16 => PreparedEdgeValueDecoder::RawI16,
            EdgeValueEncoding::RawI32 => PreparedEdgeValueDecoder::RawI32,
            EdgeValueEncoding::RawI64 => PreparedEdgeValueDecoder::RawI64,
            EdgeValueEncoding::F16 => PreparedEdgeValueDecoder::F16,
            EdgeValueEncoding::F32 => PreparedEdgeValueDecoder::F32,
            EdgeValueEncoding::F64 => PreparedEdgeValueDecoder::F64,
            EdgeValueEncoding::RawU128 => PreparedEdgeValueDecoder::RawU128,
            EdgeValueEncoding::RawI128 => PreparedEdgeValueDecoder::RawI128,
            EdgeValueEncoding::RawFixed32 => PreparedEdgeValueDecoder::RawFixed32,
            EdgeValueEncoding::RawFixed64 => PreparedEdgeValueDecoder::RawFixed64,
            EdgeValueEncoding::WeightRawU16 => PreparedEdgeValueDecoder::WeightRawU16,
            EdgeValueEncoding::WeightLinearU16 { min, max } => {
                let scale = if max > min {
                    (max - min) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedEdgeValueDecoder::WeightLinear { min: *min, scale }
            }
            EdgeValueEncoding::WeightLogU16 { min, max } => {
                let min_ln = min.ln();
                let max_ln = max.ln();
                let scale = if max_ln > min_ln {
                    (max_ln - min_ln) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedEdgeValueDecoder::WeightLog { min_ln, scale }
            }
            EdgeValueEncoding::WeightBinary16 => PreparedEdgeValueDecoder::WeightBinary16,
        })
    }
}

fn read_fixed<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut buf = [0u8; N];
    let len = bytes.len().min(N);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

pub fn decode_edge_value(
    decoder: &PreparedEdgeValueDecoder,
    bytes: &[u8],
) -> Result<DecodedEdgeValue, EdgeValueProfileError> {
    Ok(match decoder {
        PreparedEdgeValueDecoder::RawU8 => DecodedEdgeValue::U8(read_fixed::<1>(bytes)[0]),
        PreparedEdgeValueDecoder::RawU16 => {
            DecodedEdgeValue::U16(u16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeValueDecoder::RawU32 => {
            DecodedEdgeValue::U32(u32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeValueDecoder::RawU64 => {
            DecodedEdgeValue::U64(u64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeValueDecoder::RawI8 => {
            DecodedEdgeValue::I8(i8::from_le_bytes(read_fixed::<1>(bytes)))
        }
        PreparedEdgeValueDecoder::RawI16 => {
            DecodedEdgeValue::I16(i16::from_le_bytes(read_fixed::<2>(bytes)))
        }
        PreparedEdgeValueDecoder::RawI32 => {
            DecodedEdgeValue::I32(i32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeValueDecoder::RawI64 => {
            DecodedEdgeValue::I64(i64::from_le_bytes(read_fixed::<8>(bytes)))
        }
        PreparedEdgeValueDecoder::F16 => {
            DecodedEdgeValue::F32(f16::from_le_bytes(read_fixed::<2>(bytes)).to_f32())
        }
        PreparedEdgeValueDecoder::F32 => {
            DecodedEdgeValue::F32(f32::from_le_bytes(read_fixed::<4>(bytes)))
        }
        PreparedEdgeValueDecoder::F64 => {
            DecodedEdgeValue::F32(f64::from_le_bytes(read_fixed::<8>(bytes)) as f32)
        }
        PreparedEdgeValueDecoder::RawU128 => {
            DecodedEdgeValue::U128(u128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgeValueDecoder::RawI128 => {
            DecodedEdgeValue::I128(i128::from_le_bytes(read_fixed::<16>(bytes)))
        }
        PreparedEdgeValueDecoder::RawFixed32 => DecodedEdgeValue::Fixed32(read_fixed::<32>(bytes)),
        PreparedEdgeValueDecoder::RawFixed64 => DecodedEdgeValue::Fixed64(read_fixed::<64>(bytes)),
        PreparedEdgeValueDecoder::WeightRawU16 => {
            let v = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            validate_weight_f32(v)?;
            DecodedEdgeValue::Weight(v)
        }
        PreparedEdgeValueDecoder::WeightLinear { min, scale } => {
            let raw = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            let v = min + scale * raw;
            validate_weight_f32(v)?;
            DecodedEdgeValue::Weight(v)
        }
        PreparedEdgeValueDecoder::WeightLog { min_ln, scale } => {
            let raw = u16::from_le_bytes(read_fixed::<2>(bytes)) as f32;
            let v = (min_ln + scale * raw).exp();
            validate_weight_f32(v)?;
            DecodedEdgeValue::Weight(v)
        }
        PreparedEdgeValueDecoder::WeightBinary16 => {
            let v = f16::from_le_bytes(read_fixed::<2>(bytes)).to_f32();
            validate_weight_f32(v)?;
            DecodedEdgeValue::Weight(v)
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

/// Decodes traversal weight from edge-value bytes using a prepared decoder.
pub fn decode_edge_weight(
    decoder: &PreparedEdgeValueDecoder,
    bytes: &[u8],
) -> Result<f32, EdgeValueProfileError> {
    match decode_edge_value(decoder, bytes)? {
        DecodedEdgeValue::Weight(w) => Ok(w),
        _ => Err(EdgeValueProfileError::WidthEncodingMismatch),
    }
}

impl Storable for EdgeValueProfile {
    const BOUND: Bound = Bound::Bounded {
        max_size: 512,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            candid::encode_one(self).expect("EdgeValueProfile candid encode should not fail"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        candid::encode_one(&self).expect("EdgeValueProfile candid encode should not fail")
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        candid::decode_one(&bytes).expect("EdgeValueProfile candid decode should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_i32_round_trips() {
        let profile = EdgeValueProfile {
            width: EdgeValueWidth::W4,
            encoding: EdgeValueEncoding::RawI32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = (-42i32).to_le_bytes();
        assert_eq!(
            decode_edge_value(&dec, &bytes).expect("decode"),
            DecodedEdgeValue::I32(-42)
        );
    }

    #[test]
    fn weight_u16_profile_decodes() {
        let profile = EdgeValueProfile {
            width: EdgeValueWidth::W2,
            encoding: EdgeValueEncoding::WeightRawU16,
        };
        let dec = profile.prepare().expect("prepare");
        let w = decode_edge_weight(&dec, &3u16.to_le_bytes()).expect("weight");
        assert_eq!(w, 3.0);
    }

    #[test]
    fn validate_rejects_width_encoding_mismatch() {
        let profile = EdgeValueProfile {
            width: EdgeValueWidth::W2,
            encoding: EdgeValueEncoding::RawI32,
        };
        assert_eq!(
            profile.validate(),
            Err(EdgeValueProfileError::WidthEncodingMismatch)
        );
    }

    #[test]
    fn weight_linear_u16_decodes_scaled() {
        let profile = EdgeValueProfile {
            width: EdgeValueWidth::W2,
            encoding: EdgeValueEncoding::WeightLinearU16 {
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
    fn weight_log_u16_decodes() {
        let profile = EdgeValueProfile {
            width: EdgeValueWidth::W2,
            encoding: EdgeValueEncoding::WeightLogU16 {
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
        let profile = EdgeValueProfile {
            width: EdgeValueWidth::W4,
            encoding: EdgeValueEncoding::F32,
        };
        let dec = profile.prepare().expect("prepare");
        let bytes = 3.5f32.to_le_bytes();
        assert_eq!(
            decode_edge_value(&dec, &bytes).expect("decode"),
            DecodedEdgeValue::F32(3.5)
        );
    }

    #[test]
    fn decode_edge_weight_rejects_non_weight_encoding() {
        let profile = EdgeValueProfile {
            width: EdgeValueWidth::W4,
            encoding: EdgeValueEncoding::RawI32,
        };
        let dec = profile.prepare().expect("prepare");
        let err = decode_edge_weight(&dec, &42i32.to_le_bytes()).unwrap_err();
        assert_eq!(err, EdgeValueProfileError::WidthEncodingMismatch);
    }

    #[test]
    fn wide_fixed_profiles_round_trip() {
        for (width, encoding, expected) in [
            (
                EdgeValueWidth::W16,
                EdgeValueEncoding::RawU128,
                DecodedEdgeValue::U128(0x0123_4567_89AB_CDEF_u128),
            ),
            (
                EdgeValueWidth::W32,
                EdgeValueEncoding::RawFixed32,
                DecodedEdgeValue::Fixed32([7u8; 32]),
            ),
            (
                EdgeValueWidth::W64,
                EdgeValueEncoding::RawFixed64,
                DecodedEdgeValue::Fixed64([9u8; 64]),
            ),
        ] {
            let profile = EdgeValueProfile { width, encoding };
            let dec = profile.prepare().expect("prepare");
            let bytes = match &expected {
                DecodedEdgeValue::U128(v) => v.to_le_bytes().to_vec(),
                DecodedEdgeValue::Fixed32(b) => b.to_vec(),
                DecodedEdgeValue::Fixed64(b) => b.to_vec(),
                _ => panic!("unexpected test case"),
            };
            assert_eq!(
                decode_edge_value(&dec, &bytes).expect("decode"),
                expected,
                "width {:?}",
                width
            );
        }
    }

    #[test]
    fn edge_weight_profile_converts_to_value_profile() {
        use super::super::weight::{EdgeWeightProfile, WeightEncoding};
        let weight = EdgeWeightProfile {
            encoding: WeightEncoding::Linear { min: 0.0, max: 1.0 },
        };
        let profile = EdgeValueProfile::from(weight);
        assert_eq!(profile.width, EdgeValueWidth::W2);
        assert!(matches!(
            profile.encoding,
            EdgeValueEncoding::WeightLinearU16 { .. }
        ));
        profile.validate().expect("converted profile valid");
    }
}
