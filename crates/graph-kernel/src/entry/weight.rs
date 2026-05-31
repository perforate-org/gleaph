//! Edge-label weight profiles and prepared decoders for traversal-time inline u16 payloads.
//!
//! [`EdgeWeightProfile`] is catalog metadata attached to an edge-capable label. At query preparation
//! time it is compiled into a [`PreparedWeightDecoder`] so the traversal hot path only reads
//! stored edge payload bytes (typically 2-byte u16) and applies the decoder.

use half::f16;
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;
use thiserror::Error;

/// Label-level configuration for interpreting stored edge-payload bytes as a traversal weight.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct EdgeWeightProfile {
    pub encoding: WeightEncoding,
}

#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub enum WeightEncoding {
    /// Raw `u16` promoted to `f32` (always finite, non-negative).
    RawU16,
    /// Map `u16` linearly across `[min, max]` inclusive endpoints.
    Linear { min: f32, max: f32 },
    /// Map `u16` linearly in log-space from `ln(min)` to `ln(max)`.
    Log { min: f32, max: f32 },
    /// Interpret stored u16 bits as IEEE 754 binary16, then widen to `f32`.
    Binary16,
}

/// Prepared decoder kept on the query execution hot path (no catalog lookup per edge).
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedWeightDecoder {
    RawU16,
    Linear { min: f32, scale: f32 },
    Log { min_ln: f32, scale: f32 },
    Binary16,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WeightProfilePrepareError {
    #[error("linear weight encoding requires finite min/max with min <= max")]
    InvalidLinearRange,
    #[error("log weight encoding requires finite strictly-positive min/max with min <= max")]
    InvalidLogRange,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WeightDecodeError {
    #[error("decoded weight is not finite")]
    NonFinite,
    #[error("decoded weight is negative")]
    Negative,
}

impl EdgeWeightProfile {
    /// Validates profile invariants needed before compilation.
    pub fn validate(&self) -> Result<(), WeightProfilePrepareError> {
        match &self.encoding {
            WeightEncoding::RawU16 | WeightEncoding::Binary16 => Ok(()),
            WeightEncoding::Linear { min, max } => {
                if !min.is_finite() || !max.is_finite() || min > max {
                    return Err(WeightProfilePrepareError::InvalidLinearRange);
                }
                Ok(())
            }
            WeightEncoding::Log { min, max } => {
                if !min.is_finite() || !max.is_finite() || *min <= 0.0 || *max <= 0.0 || min > max {
                    return Err(WeightProfilePrepareError::InvalidLogRange);
                }
                Ok(())
            }
        }
    }

    /// Compiles this profile into a [`PreparedWeightDecoder`].
    pub fn prepare(&self) -> Result<PreparedWeightDecoder, WeightProfilePrepareError> {
        self.validate()?;
        Ok(match &self.encoding {
            WeightEncoding::RawU16 => PreparedWeightDecoder::RawU16,
            WeightEncoding::Linear { min, max } => {
                let scale = if max > min {
                    (max - min) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedWeightDecoder::Linear { min: *min, scale }
            }
            WeightEncoding::Log { min, max } => {
                let min_ln = min.ln();
                let max_ln = max.ln();
                let scale = if max_ln > min_ln {
                    (max_ln - min_ln) / u16::MAX as f32
                } else {
                    0.0
                };
                PreparedWeightDecoder::Log { min_ln, scale }
            }
            WeightEncoding::Binary16 => PreparedWeightDecoder::Binary16,
        })
    }
}

impl Storable for EdgeWeightProfile {
    const BOUND: Bound = Bound::Bounded {
        max_size: 256,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            candid::encode_one(self).expect("EdgeWeightProfile candid encode should not fail"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        candid::encode_one(&self).expect("EdgeWeightProfile candid encode should not fail")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        candid::decode_one(&bytes).expect("EdgeWeightProfile candid decode should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::edge_payload::{EdgePayloadProfile, decode_edge_weight};

    fn decode_weight_bytes(profile: &EdgeWeightProfile, bytes: &[u8]) -> f32 {
        let payload_profile = EdgePayloadProfile::from(profile.clone());
        let decoder = payload_profile.prepare().expect("prepare");
        decode_edge_weight(&decoder, bytes).expect("decode")
    }

    #[test]
    fn raw_u16_round_trip() {
        let p = EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        };
        assert_eq!(decode_weight_bytes(&p, &0u16.to_le_bytes()), 0.0);
        assert_eq!(decode_weight_bytes(&p, &65535u16.to_le_bytes()), 65535.0);
    }

    #[test]
    fn linear_endpoints() {
        let p = EdgeWeightProfile {
            encoding: WeightEncoding::Linear {
                min: 10.0,
                max: 20.0,
            },
        };
        assert!((decode_weight_bytes(&p, &0u16.to_le_bytes()) - 10.0).abs() < 1e-4);
        assert!((decode_weight_bytes(&p, &u16::MAX.to_le_bytes()) - 20.0).abs() < 1e-3);
    }

    #[test]
    fn binary16_positive() {
        let p = EdgeWeightProfile {
            encoding: WeightEncoding::Binary16,
        };
        let bits = f16::from_f32(1.5).to_bits();
        assert!((decode_weight_bytes(&p, &bits.to_le_bytes()) - 1.5).abs() < 1e-3);
    }

    #[test]
    fn storable_roundtrip() {
        let profile = EdgeWeightProfile {
            encoding: WeightEncoding::Linear {
                min: 1.0,
                max: 10.0,
            },
        };
        let decoded = EdgeWeightProfile::from_bytes(profile.to_bytes());
        assert_eq!(decoded, profile);
    }
}
