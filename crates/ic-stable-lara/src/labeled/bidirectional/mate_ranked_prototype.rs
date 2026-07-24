//! Fixture-only rank-indexed mate blob prototype.
//!
//! Each mapping entry is addressed by canonical equal-neighbor occurrence rank, so the
//! persisted mapping stores only the counterpart slot.  This is intentionally separate from
//! the production codec until publication-time validation and size evidence justify adoption.

#![expect(dead_code, reason = "prototype is consumed by measurement fixtures")]

const HEADER_BYTES: usize = 24;
const DIRECTORY_ENTRY_BYTES: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RankedBucket {
    pub(crate) owner_vertex_id: u32,
    pub(crate) bucket_label_key: u16,
    pub(crate) entries: u32,
    pub(crate) width_bytes: u8,
    pub(crate) mate_slots: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RankedBlob {
    pub(crate) buckets: Vec<RankedBucket>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RankedError {
    EmptyBlob,
    BucketOrder,
    EmptyBucket,
    UnsupportedWidth(u8),
    LengthMismatch { expected: usize, actual: usize },
    ArithmeticOverflow,
    Truncated,
    InvalidHeader,
    InvalidOffset,
    RankOutOfRange,
}

fn mapping_len(entries: u32, width_bytes: u8) -> Result<usize, RankedError> {
    if !(1..=4).contains(&width_bytes) {
        return Err(RankedError::UnsupportedWidth(width_bytes));
    }
    usize::try_from(u64::from(entries) * u64::from(width_bytes))
        .map_err(|_| RankedError::ArithmeticOverflow)
}

impl RankedBucket {
    pub(crate) fn validate(&self) -> Result<(), RankedError> {
        if self.entries == 0 {
            return Err(RankedError::EmptyBucket);
        }
        let expected = mapping_len(self.entries, self.width_bytes)?;
        if self.mate_slots.len() != expected {
            return Err(RankedError::LengthMismatch {
                expected,
                actual: self.mate_slots.len(),
            });
        }
        Ok(())
    }

    pub(crate) fn mate_slot_for_rank(&self, rank: u32) -> Result<u32, RankedError> {
        if rank >= self.entries {
            return Err(RankedError::RankOutOfRange);
        }
        let width = usize::from(self.width_bytes);
        let start = usize::try_from(rank)
            .map_err(|_| RankedError::ArithmeticOverflow)?
            .checked_mul(width)
            .ok_or(RankedError::ArithmeticOverflow)?;
        let end = start
            .checked_add(width)
            .ok_or(RankedError::ArithmeticOverflow)?;
        let bytes = self
            .mate_slots
            .get(start..end)
            .ok_or(RankedError::Truncated)?;
        let mut padded = [0u8; 4];
        padded[4 - width..].copy_from_slice(bytes);
        Ok(u32::from_be_bytes(padded))
    }
}

impl RankedBlob {
    pub(crate) fn encoded_len(&self) -> Result<usize, RankedError> {
        if self.buckets.is_empty() {
            return Err(RankedError::EmptyBlob);
        }
        let mut previous = None;
        let mut mapping_bytes = 0usize;
        for bucket in &self.buckets {
            if previous.is_some_and(|key| (bucket.owner_vertex_id, bucket.bucket_label_key) <= key)
            {
                return Err(RankedError::BucketOrder);
            }
            bucket.validate()?;
            mapping_bytes = mapping_bytes
                .checked_add(bucket.mate_slots.len())
                .ok_or(RankedError::ArithmeticOverflow)?;
            previous = Some((bucket.owner_vertex_id, bucket.bucket_label_key));
        }
        HEADER_BYTES
            .checked_add(
                self.buckets
                    .len()
                    .checked_mul(DIRECTORY_ENTRY_BYTES)
                    .ok_or(RankedError::ArithmeticOverflow)?,
            )
            .and_then(|value| value.checked_add(mapping_bytes))
            .ok_or(RankedError::ArithmeticOverflow)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, RankedError> {
        let total = self.encoded_len()?;
        let directory_bytes = self
            .buckets
            .len()
            .checked_mul(DIRECTORY_ENTRY_BYTES)
            .ok_or(RankedError::ArithmeticOverflow)?;
        let mapping_offset = HEADER_BYTES
            .checked_add(directory_bytes)
            .ok_or(RankedError::ArithmeticOverflow)?;
        let mut output = Vec::with_capacity(total);
        output.extend_from_slice(b"MATR");
        output.push(1);
        output.push(0);
        output.extend_from_slice(&(HEADER_BYTES as u16).to_be_bytes());
        output.extend_from_slice(
            &u32::try_from(self.buckets.len())
                .map_err(|_| RankedError::ArithmeticOverflow)?
                .to_be_bytes(),
        );
        output.extend_from_slice(
            &u32::try_from(directory_bytes)
                .map_err(|_| RankedError::ArithmeticOverflow)?
                .to_be_bytes(),
        );
        let mapping_bytes = total
            .checked_sub(mapping_offset)
            .ok_or(RankedError::ArithmeticOverflow)?;
        output.extend_from_slice(
            &u32::try_from(mapping_bytes)
                .map_err(|_| RankedError::ArithmeticOverflow)?
                .to_be_bytes(),
        );
        output.extend_from_slice(
            &u32::try_from(total)
                .map_err(|_| RankedError::ArithmeticOverflow)?
                .to_be_bytes(),
        );
        let mut offset = mapping_offset;
        for bucket in &self.buckets {
            output.extend_from_slice(&bucket.owner_vertex_id.to_be_bytes());
            output.extend_from_slice(&bucket.bucket_label_key.to_be_bytes());
            output.push(bucket.width_bytes);
            output.push(0);
            output.extend_from_slice(&bucket.entries.to_be_bytes());
            output.extend_from_slice(
                &u32::try_from(offset)
                    .map_err(|_| RankedError::ArithmeticOverflow)?
                    .to_be_bytes(),
            );
            output.extend_from_slice(
                &u32::try_from(bucket.mate_slots.len())
                    .map_err(|_| RankedError::ArithmeticOverflow)?
                    .to_be_bytes(),
            );
            offset = offset
                .checked_add(bucket.mate_slots.len())
                .ok_or(RankedError::ArithmeticOverflow)?;
        }
        for bucket in &self.buckets {
            output.extend_from_slice(&bucket.mate_slots);
        }
        debug_assert_eq!(output.len(), total);
        Ok(output)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, RankedError> {
        if bytes.len() < HEADER_BYTES || bytes.get(..4) != Some(b"MATR") || bytes[4] != 1 {
            return Err(RankedError::InvalidHeader);
        }
        let header_len = usize::from(u16::from_be_bytes(
            bytes[6..8].try_into().map_err(|_| RankedError::Truncated)?,
        ));
        if header_len != HEADER_BYTES {
            return Err(RankedError::InvalidHeader);
        }
        let bucket_count = u32::from_be_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| RankedError::Truncated)?,
        );
        let directory_bytes = usize::try_from(u32::from_be_bytes(
            bytes[12..16]
                .try_into()
                .map_err(|_| RankedError::Truncated)?,
        ))
        .map_err(|_| RankedError::ArithmeticOverflow)?;
        let mapping_bytes = usize::try_from(u32::from_be_bytes(
            bytes[16..20]
                .try_into()
                .map_err(|_| RankedError::Truncated)?,
        ))
        .map_err(|_| RankedError::ArithmeticOverflow)?;
        let total_bytes = usize::try_from(u32::from_be_bytes(
            bytes[20..24]
                .try_into()
                .map_err(|_| RankedError::Truncated)?,
        ))
        .map_err(|_| RankedError::ArithmeticOverflow)?;
        let expected_directory = usize::try_from(bucket_count)
            .map_err(|_| RankedError::ArithmeticOverflow)?
            .checked_mul(DIRECTORY_ENTRY_BYTES)
            .ok_or(RankedError::ArithmeticOverflow)?;
        let mapping_offset = HEADER_BYTES
            .checked_add(directory_bytes)
            .ok_or(RankedError::ArithmeticOverflow)?;
        if directory_bytes != expected_directory
            || mapping_offset
                .checked_add(mapping_bytes)
                .ok_or(RankedError::ArithmeticOverflow)?
                != total_bytes
            || total_bytes != bytes.len()
        {
            return Err(RankedError::InvalidHeader);
        }
        let mut buckets = Vec::with_capacity(usize::try_from(bucket_count).unwrap_or(0));
        let mut previous = None;
        for index in 0..bucket_count {
            let start = HEADER_BYTES
                .checked_add(
                    usize::try_from(index)
                        .map_err(|_| RankedError::ArithmeticOverflow)?
                        .checked_mul(DIRECTORY_ENTRY_BYTES)
                        .ok_or(RankedError::ArithmeticOverflow)?,
                )
                .ok_or(RankedError::ArithmeticOverflow)?;
            let owner = u32::from_be_bytes(
                bytes[start..start + 4]
                    .try_into()
                    .map_err(|_| RankedError::Truncated)?,
            );
            let label = u16::from_be_bytes(
                bytes[start + 4..start + 6]
                    .try_into()
                    .map_err(|_| RankedError::Truncated)?,
            );
            if previous.is_some_and(|key| (owner, label) <= key) {
                return Err(RankedError::BucketOrder);
            }
            let width = bytes[start + 6];
            let entries = u32::from_be_bytes(
                bytes[start + 8..start + 12]
                    .try_into()
                    .map_err(|_| RankedError::Truncated)?,
            );
            let offset = usize::try_from(u32::from_be_bytes(
                bytes[start + 12..start + 16]
                    .try_into()
                    .map_err(|_| RankedError::Truncated)?,
            ))
            .map_err(|_| RankedError::ArithmeticOverflow)?;
            let length = usize::try_from(u32::from_be_bytes(
                bytes[start + 16..start + 20]
                    .try_into()
                    .map_err(|_| RankedError::Truncated)?,
            ))
            .map_err(|_| RankedError::ArithmeticOverflow)?;
            let end = offset
                .checked_add(length)
                .ok_or(RankedError::ArithmeticOverflow)?;
            if offset < mapping_offset || end > total_bytes {
                return Err(RankedError::InvalidOffset);
            }
            let bucket = RankedBucket {
                owner_vertex_id: owner,
                bucket_label_key: label,
                entries,
                width_bytes: width,
                mate_slots: bytes[offset..end].to_vec(),
            };
            bucket.validate()?;
            previous = Some((owner, label));
            buckets.push(bucket);
        }
        let blob = Self { buckets };
        if blob.encoded_len()? != bytes.len() {
            return Err(RankedError::InvalidOffset);
        }
        Ok(blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labeled::bidirectional::mate_blob_prototype::{Bucket, MateBlob, Mode};

    fn bucket(entries: u32, width_bytes: u8) -> RankedBucket {
        RankedBucket {
            owner_vertex_id: 1,
            bucket_label_key: 2,
            entries,
            width_bytes,
            mate_slots: vec![0; usize::try_from(entries * u32::from(width_bytes)).unwrap()],
        }
    }

    #[test]
    fn mate_only_mapping_has_one_slot_per_rank() {
        let blob = RankedBlob {
            buckets: vec![bucket(32, 1)],
        };
        assert_eq!(blob.encoded_len().unwrap(), 24 + 20 + 32);
        assert_eq!(blob.encode().unwrap().len(), 76);
    }

    #[test]
    fn rejects_wrong_mapping_length() {
        let mut value = bucket(4, 2);
        value.mate_slots.pop();
        assert_eq!(
            value.validate(),
            Err(RankedError::LengthMismatch {
                expected: 8,
                actual: 7,
            })
        );
    }

    #[test]
    fn rejects_duplicate_bucket_order() {
        let blob = RankedBlob {
            buckets: vec![bucket(1, 1), bucket(1, 1)],
        };
        assert_eq!(blob.encoded_len(), Err(RankedError::BucketOrder));
    }

    #[test]
    fn rejects_unsupported_width() {
        assert_eq!(
            bucket(1, 5).validate(),
            Err(RankedError::UnsupportedWidth(5))
        );
    }

    #[test]
    fn ranked_round_trip_and_rank_lookup_are_exact() {
        let blob = RankedBlob {
            buckets: vec![bucket(4, 1)],
        };
        let bytes = blob.encode().unwrap();
        let decoded = RankedBlob::decode(&bytes).unwrap();
        assert_eq!(decoded, blob);
        assert_eq!(decoded.buckets[0].mate_slot_for_rank(2), Ok(0));
        assert_eq!(
            decoded.buckets[0].mate_slot_for_rank(4),
            Err(RankedError::RankOutOfRange)
        );
    }

    #[test]
    fn size_series_is_half_mapping_cost_for_packed_width_one() {
        for entries in [32, 64, 128, 256] {
            let ranked = RankedBlob {
                buckets: vec![bucket(entries, 1)],
            }
            .encoded_len()
            .unwrap();
            let current = MateBlob {
                buckets: vec![Bucket {
                    owner_vertex_id: 1,
                    bucket_label_key: 2,
                    entries,
                    mode: Mode::Packed { width_bytes: 1 },
                    mapping: vec![0; usize::try_from(entries * 2).unwrap()],
                }],
            }
            .encoded_len()
            .unwrap();
            assert_eq!(current - ranked, usize::try_from(entries).unwrap());
        }
    }
}
