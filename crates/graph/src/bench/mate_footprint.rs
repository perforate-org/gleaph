//! Pure logical-byte accounting for the planned ADR 0048 mate blob.
//!
//! This module deliberately does not access LARA, stable memory, or a
//! `StableBTreeMap`. It accounts for the two physical halves of one non-self
//! logical edge and keeps substrate overhead outside the result.

#![expect(
    dead_code,
    reason = "pure accounting model is exercised by focused tests"
)]

const PHYSICAL_HALVES: u64 = 2;
const LOCATOR_BYTES_PER_ROW: u64 = 5;
const SAMPLE_CHECKPOINT_FIELDS: u64 = 2;
const SAMPLE_U32_BYTES: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MateMode {
    Sampled { stride: u64 },
    Packed { width_bytes: u64 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MateSharedOverhead {
    pub blob_header_bytes: u64,
    pub indexed_bucket_directory_bytes: u64,
    pub free_span_bytes: u64,
    pub rebuild_reserve_bytes: u64,
}

impl MateSharedOverhead {
    pub const fn zero() -> Self {
        Self {
            blob_header_bytes: 0,
            indexed_bucket_directory_bytes: 0,
            free_span_bytes: 0,
            rebuild_reserve_bytes: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MateFootprintInput {
    pub entries: u64,
    pub mode: MateMode,
    pub shared: MateSharedOverhead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MateFootprint {
    pub locator_bytes: u64,
    pub blob_header_bytes: u64,
    pub indexed_bucket_directory_bytes: u64,
    pub mapping_bytes: u64,
    pub free_span_bytes: u64,
    pub rebuild_reserve_bytes: u64,
    pub known_logical_bytes: u64,
    pub alias_headroom_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MateFootprintError {
    EmptyBucket,
    UnsupportedSampleStride(u64),
    UnsupportedPackedWidth(u64),
    ArithmeticOverflow,
}

impl MateFootprintInput {
    pub fn calculate(self) -> Result<MateFootprint, MateFootprintError> {
        if self.entries == 0 {
            return Err(MateFootprintError::EmptyBucket);
        }

        let mapping_bytes = match self.mode {
            MateMode::Sampled { stride } => {
                if !matches!(stride, 16 | 32 | 64) {
                    return Err(MateFootprintError::UnsupportedSampleStride(stride));
                }
                let checkpoints = self
                    .entries
                    .checked_add(stride - 1)
                    .ok_or(MateFootprintError::ArithmeticOverflow)?
                    / stride;
                PHYSICAL_HALVES
                    .checked_mul(checkpoints)
                    .and_then(|bytes| bytes.checked_mul(SAMPLE_CHECKPOINT_FIELDS))
                    .and_then(|bytes| bytes.checked_mul(SAMPLE_U32_BYTES))
                    .ok_or(MateFootprintError::ArithmeticOverflow)?
            }
            MateMode::Packed { width_bytes } => {
                if !matches!(width_bytes, 1..=4) {
                    return Err(MateFootprintError::UnsupportedPackedWidth(width_bytes));
                }
                PHYSICAL_HALVES
                    .checked_mul(self.entries)
                    .and_then(|bytes| bytes.checked_mul(width_bytes))
                    .ok_or(MateFootprintError::ArithmeticOverflow)?
            }
        };

        let locator_bytes = PHYSICAL_HALVES
            .checked_mul(LOCATOR_BYTES_PER_ROW)
            .ok_or(MateFootprintError::ArithmeticOverflow)?;
        let known_logical_bytes = locator_bytes
            .checked_add(self.shared.blob_header_bytes)
            .and_then(|bytes| bytes.checked_add(self.shared.indexed_bucket_directory_bytes))
            .and_then(|bytes| bytes.checked_add(mapping_bytes))
            .and_then(|bytes| bytes.checked_add(self.shared.free_span_bytes))
            .and_then(|bytes| bytes.checked_add(self.shared.rebuild_reserve_bytes))
            .ok_or(MateFootprintError::ArithmeticOverflow)?;
        let alias_bytes = self
            .entries
            .checked_mul(18)
            .ok_or(MateFootprintError::ArithmeticOverflow)?;

        Ok(MateFootprint {
            locator_bytes,
            blob_header_bytes: self.shared.blob_header_bytes,
            indexed_bucket_directory_bytes: self.shared.indexed_bucket_directory_bytes,
            mapping_bytes,
            free_span_bytes: self.shared.free_span_bytes,
            rebuild_reserve_bytes: self.shared.rebuild_reserve_bytes,
            known_logical_bytes,
            alias_headroom_bytes: alias_bytes
                .checked_sub(known_logical_bytes)
                .filter(|budget| *budget > 0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEGREES: [u64; 7] = [1, 8, 32, 128, 1_024, 65_536, u64::MAX];

    #[test]
    fn sampled_uses_exact_checkpoint_count_and_two_locators() {
        let one = MateFootprintInput {
            entries: 1,
            mode: MateMode::Sampled { stride: 16 },
            shared: MateSharedOverhead::zero(),
        }
        .calculate()
        .expect("sampled footprint");
        assert_eq!(one.mapping_bytes, 16);
        assert_eq!(one.known_logical_bytes, 26);

        let thirty_two = MateFootprintInput {
            entries: 32,
            mode: MateMode::Sampled { stride: 32 },
            shared: MateSharedOverhead::zero(),
        }
        .calculate()
        .expect("sampled footprint");
        assert_eq!(thirty_two.mapping_bytes, 16);
        assert_eq!(thirty_two.known_logical_bytes, 26);
    }

    #[test]
    fn packed_widths_cover_requested_table() {
        for width in 1..=4 {
            let footprint = MateFootprintInput {
                entries: 128,
                mode: MateMode::Packed { width_bytes: width },
                shared: MateSharedOverhead::zero(),
            }
            .calculate()
            .expect("packed footprint");
            assert_eq!(footprint.mapping_bytes, 256 * width);
            assert_eq!(footprint.known_logical_bytes, 256 * width + 10);
        }
    }

    #[test]
    fn all_requested_degrees_and_modes_are_checked() {
        for entries in DEGREES[..6].iter().copied() {
            for stride in [16, 32, 64] {
                MateFootprintInput {
                    entries,
                    mode: MateMode::Sampled { stride },
                    shared: MateSharedOverhead::zero(),
                }
                .calculate()
                .expect("sampled degree");
            }
            for width in 1..=4 {
                MateFootprintInput {
                    entries,
                    mode: MateMode::Packed { width_bytes: width },
                    shared: MateSharedOverhead::zero(),
                }
                .calculate()
                .expect("packed degree");
            }
        }
    }

    #[test]
    fn shared_components_and_alias_gate_are_separate() {
        let footprint = MateFootprintInput {
            entries: 128,
            mode: MateMode::Packed { width_bytes: 1 },
            shared: MateSharedOverhead {
                blob_header_bytes: 8,
                indexed_bucket_directory_bytes: 12,
                free_span_bytes: 6,
                rebuild_reserve_bytes: 20,
            },
        }
        .calculate()
        .expect("footprint");
        assert_eq!(footprint.locator_bytes, 10);
        assert_eq!(footprint.mapping_bytes, 256);
        assert_eq!(footprint.known_logical_bytes, 312);
        assert_eq!(footprint.alias_headroom_bytes, Some(1_992));
    }

    #[test]
    fn invalid_inputs_fail_closed() {
        assert_eq!(
            MateFootprintInput {
                entries: 0,
                mode: MateMode::Packed { width_bytes: 1 },
                shared: MateSharedOverhead::zero(),
            }
            .calculate(),
            Err(MateFootprintError::EmptyBucket)
        );
        assert_eq!(
            MateFootprintInput {
                entries: 1,
                mode: MateMode::Sampled { stride: 8 },
                shared: MateSharedOverhead::zero(),
            }
            .calculate(),
            Err(MateFootprintError::UnsupportedSampleStride(8))
        );
        assert_eq!(
            MateFootprintInput {
                entries: 1,
                mode: MateMode::Packed { width_bytes: 5 },
                shared: MateSharedOverhead::zero(),
            }
            .calculate(),
            Err(MateFootprintError::UnsupportedPackedWidth(5))
        );
    }

    #[test]
    fn arithmetic_overflow_is_rejected() {
        let error = MateFootprintInput {
            entries: u64::MAX,
            mode: MateMode::Packed { width_bytes: 4 },
            shared: MateSharedOverhead::zero(),
        }
        .calculate()
        .expect_err("overflow must fail closed");
        assert_eq!(error, MateFootprintError::ArithmeticOverflow);
    }

    #[test]
    fn non_positive_alias_headroom_does_not_pass_the_gate() {
        let equal = MateFootprintInput {
            entries: 1,
            mode: MateMode::Packed { width_bytes: 4 },
            shared: MateSharedOverhead::zero(),
        }
        .calculate()
        .expect("footprint");
        assert_eq!(equal.alias_headroom_bytes, None);

        let over = MateFootprintInput {
            entries: 1,
            mode: MateMode::Packed { width_bytes: 1 },
            shared: MateSharedOverhead {
                blob_header_bytes: 7,
                ..MateSharedOverhead::zero()
            },
        }
        .calculate()
        .expect("footprint");
        assert_eq!(over.alias_headroom_bytes, None);
    }
}
