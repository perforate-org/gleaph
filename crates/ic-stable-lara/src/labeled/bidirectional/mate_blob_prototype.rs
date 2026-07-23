//! Test-only versioned mate blob prototype for ADR 0048.
//!
//! This module intentionally has no stable-memory or runtime entry point. It validates the
//! proposed byte boundary before a production locator/blob store is designed.

const MAGIC: [u8; 4] = *b"MATE";
const VERSION: u8 = 1;
const HEADER_BYTES: usize = 24;
const DIRECTORY_ENTRY_BYTES: usize = 20;
const PHYSICAL_HALVES: u64 = 2;
const SAMPLE_FIELDS: u64 = 2;
const SAMPLE_U32_BYTES: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Sampled { stride: u8 },
    Packed { width_bytes: u8 },
}

impl Mode {
    fn encode(self) -> (u8, u8) {
        match self {
            Self::Sampled { stride } => (1, stride),
            Self::Packed { width_bytes } => (2, width_bytes),
        }
    }

    fn decode(mode: u8, parameter: u8) -> Result<Self, DecodeError> {
        match mode {
            1 if matches!(parameter, 16 | 32 | 64) => Ok(Self::Sampled { stride: parameter }),
            2 if (1..=4).contains(&parameter) => Ok(Self::Packed {
                width_bytes: parameter,
            }),
            1 => Err(DecodeError::UnsupportedSampleStride(parameter)),
            2 => Err(DecodeError::UnsupportedPackedWidth(parameter)),
            other => Err(DecodeError::UnsupportedMode(other)),
        }
    }

    fn mapping_bytes(self, entries: u32) -> Result<usize, DecodeError> {
        let entries = u64::from(entries);
        let bytes = match self {
            Self::Sampled { stride } => {
                let checkpoints = entries
                    .checked_add(u64::from(stride) - 1)
                    .ok_or(DecodeError::ArithmeticOverflow)?
                    / u64::from(stride);
                PHYSICAL_HALVES
                    .checked_mul(checkpoints)
                    .and_then(|value| value.checked_mul(SAMPLE_FIELDS))
                    .and_then(|value| value.checked_mul(SAMPLE_U32_BYTES))
                    .ok_or(DecodeError::ArithmeticOverflow)?
            }
            Self::Packed { width_bytes } => PHYSICAL_HALVES
                .checked_mul(entries)
                .and_then(|value| value.checked_mul(u64::from(width_bytes)))
                .ok_or(DecodeError::ArithmeticOverflow)?,
        };
        usize::try_from(bytes).map_err(|_| DecodeError::ArithmeticOverflow)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bucket {
    pub bucket_id: u32,
    pub entries: u32,
    pub mode: Mode,
    pub mapping: Vec<u8>,
}

impl Bucket {
    fn validate(&self) -> Result<(), EncodeError> {
        let expected = self
            .mode
            .mapping_bytes(self.entries)
            .map_err(EncodeError::from)?;
        if self.entries == 0 || self.mapping.len() != expected {
            return Err(EncodeError::MappingLengthMismatch {
                bucket_id: self.bucket_id,
                expected,
                actual: self.mapping.len(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MateBlob {
    pub buckets: Vec<Bucket>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncodeError {
    EmptyBlob,
    BucketsNotStrictlyIncreasing,
    MappingLengthMismatch {
        bucket_id: u32,
        expected: usize,
        actual: usize,
    },
    ArithmeticOverflow,
    TooLarge,
}

impl From<DecodeError> for EncodeError {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::ArithmeticOverflow => Self::ArithmeticOverflow,
            DecodeError::UnsupportedSampleStride(_)
            | DecodeError::UnsupportedPackedWidth(_)
            | DecodeError::UnsupportedMode(_) => Self::ArithmeticOverflow,
            _ => Self::ArithmeticOverflow,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    InvalidHeaderLength(u16),
    EmptyBlob,
    DirectoryLengthMismatch,
    TotalLengthMismatch,
    UnsupportedMode(u8),
    UnsupportedSampleStride(u8),
    UnsupportedPackedWidth(u8),
    ArithmeticOverflow,
    EmptyBucket,
    BucketOrder,
    MappingOffset,
    MappingLengthMismatch,
    TrailingBytes,
}

fn read<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], DecodeError> {
    let end = offset
        .checked_add(N)
        .ok_or(DecodeError::ArithmeticOverflow)?;
    let value = bytes.get(*offset..end).ok_or(DecodeError::Truncated)?;
    *offset = end;
    value.try_into().map_err(|_| DecodeError::Truncated)
}

fn read_u16(bytes: &[u8], offset: &mut usize) -> Result<u16, DecodeError> {
    Ok(u16::from_be_bytes(read(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, DecodeError> {
    Ok(u32::from_be_bytes(read(bytes, offset)?))
}

impl MateBlob {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        if self.buckets.is_empty() {
            return Err(EncodeError::EmptyBlob);
        }
        let directory_bytes = self
            .buckets
            .len()
            .checked_mul(DIRECTORY_ENTRY_BYTES)
            .ok_or(EncodeError::ArithmeticOverflow)?;
        let mut mapping_bytes = 0usize;
        let mut previous_id = None;
        for bucket in &self.buckets {
            if previous_id.is_some_and(|previous| bucket.bucket_id <= previous) {
                return Err(EncodeError::BucketsNotStrictlyIncreasing);
            }
            bucket.validate()?;
            mapping_bytes = mapping_bytes
                .checked_add(bucket.mapping.len())
                .ok_or(EncodeError::ArithmeticOverflow)?;
            previous_id = Some(bucket.bucket_id);
        }
        let total_bytes = HEADER_BYTES
            .checked_add(directory_bytes)
            .and_then(|value| value.checked_add(mapping_bytes))
            .ok_or(EncodeError::ArithmeticOverflow)?;
        let bucket_count = u32::try_from(self.buckets.len()).map_err(|_| EncodeError::TooLarge)?;
        let directory_bytes = u32::try_from(directory_bytes).map_err(|_| EncodeError::TooLarge)?;
        let mapping_bytes = u32::try_from(mapping_bytes).map_err(|_| EncodeError::TooLarge)?;
        let total_bytes = u32::try_from(total_bytes).map_err(|_| EncodeError::TooLarge)?;

        let mut out = Vec::with_capacity(usize::try_from(total_bytes).expect("u32 fits usize"));
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(0);
        out.extend_from_slice(&(HEADER_BYTES as u16).to_be_bytes());
        out.extend_from_slice(&bucket_count.to_be_bytes());
        out.extend_from_slice(&directory_bytes.to_be_bytes());
        out.extend_from_slice(&mapping_bytes.to_be_bytes());
        out.extend_from_slice(&total_bytes.to_be_bytes());

        let mut mapping_offset = HEADER_BYTES
            .checked_add(usize::try_from(directory_bytes).expect("u32 fits usize"))
            .ok_or(EncodeError::ArithmeticOverflow)?;
        for bucket in &self.buckets {
            let (mode, parameter) = bucket.mode.encode();
            out.extend_from_slice(&bucket.bucket_id.to_be_bytes());
            out.push(mode);
            out.push(parameter);
            out.extend_from_slice(&0u16.to_be_bytes());
            out.extend_from_slice(&bucket.entries.to_be_bytes());
            out.extend_from_slice(
                &u32::try_from(mapping_offset)
                    .map_err(|_| EncodeError::TooLarge)?
                    .to_be_bytes(),
            );
            out.extend_from_slice(
                &u32::try_from(bucket.mapping.len())
                    .map_err(|_| EncodeError::TooLarge)?
                    .to_be_bytes(),
            );
            mapping_offset = mapping_offset
                .checked_add(bucket.mapping.len())
                .ok_or(EncodeError::ArithmeticOverflow)?;
        }
        for bucket in &self.buckets {
            out.extend_from_slice(&bucket.mapping);
        }
        debug_assert_eq!(
            out.len(),
            usize::try_from(total_bytes).expect("u32 fits usize")
        );
        Ok(out)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < HEADER_BYTES {
            return Err(DecodeError::Truncated);
        }
        let mut offset = 0;
        if read::<4>(bytes, &mut offset)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = read::<1>(bytes, &mut offset)?[0];
        if version != VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let _flags = read::<1>(bytes, &mut offset)?[0];
        let header_len = read_u16(bytes, &mut offset)?;
        if header_len != HEADER_BYTES as u16 {
            return Err(DecodeError::InvalidHeaderLength(header_len));
        }
        let bucket_count = read_u32(bytes, &mut offset)?;
        if bucket_count == 0 {
            return Err(DecodeError::EmptyBlob);
        }
        let directory_len = read_u32(bytes, &mut offset)?;
        let mapping_len = read_u32(bytes, &mut offset)?;
        let total_len = read_u32(bytes, &mut offset)?;
        let expected_directory = u64::from(bucket_count)
            .checked_mul(DIRECTORY_ENTRY_BYTES as u64)
            .ok_or(DecodeError::ArithmeticOverflow)?;
        if u64::from(directory_len) != expected_directory {
            return Err(DecodeError::DirectoryLengthMismatch);
        }
        if usize::try_from(total_len).map_err(|_| DecodeError::ArithmeticOverflow)? != bytes.len() {
            return Err(DecodeError::TotalLengthMismatch);
        }
        let mapping_start = HEADER_BYTES
            .checked_add(
                usize::try_from(directory_len).map_err(|_| DecodeError::ArithmeticOverflow)?,
            )
            .ok_or(DecodeError::ArithmeticOverflow)?;
        let mapping_end = mapping_start
            .checked_add(usize::try_from(mapping_len).map_err(|_| DecodeError::ArithmeticOverflow)?)
            .ok_or(DecodeError::ArithmeticOverflow)?;
        if mapping_end != bytes.len() {
            return Err(if mapping_end < bytes.len() {
                DecodeError::TrailingBytes
            } else {
                DecodeError::TotalLengthMismatch
            });
        }

        let mut entries = Vec::with_capacity(
            usize::try_from(bucket_count).map_err(|_| DecodeError::ArithmeticOverflow)?,
        );
        let mut previous_id = None;
        let mut expected_offset = mapping_start;
        for _ in 0..bucket_count {
            let bucket_id = read_u32(bytes, &mut offset)?;
            let mode = read::<1>(bytes, &mut offset)?[0];
            let parameter = read::<1>(bytes, &mut offset)?[0];
            let _reserved = read_u16(bytes, &mut offset)?;
            let entry_count = read_u32(bytes, &mut offset)?;
            let mapping_offset = usize::try_from(read_u32(bytes, &mut offset)?)
                .map_err(|_| DecodeError::ArithmeticOverflow)?;
            let mapping_length = usize::try_from(read_u32(bytes, &mut offset)?)
                .map_err(|_| DecodeError::ArithmeticOverflow)?;
            if previous_id.is_some_and(|previous| bucket_id <= previous) {
                return Err(DecodeError::BucketOrder);
            }
            let mode = Mode::decode(mode, parameter)?;
            if entry_count == 0 {
                return Err(DecodeError::EmptyBucket);
            }
            if mapping_offset != expected_offset || mapping_offset < mapping_start {
                return Err(DecodeError::MappingOffset);
            }
            let expected_length = mode.mapping_bytes(entry_count)?;
            if mapping_length != expected_length {
                return Err(DecodeError::MappingLengthMismatch);
            }
            let end = mapping_offset
                .checked_add(mapping_length)
                .ok_or(DecodeError::ArithmeticOverflow)?;
            if end > mapping_end {
                return Err(DecodeError::MappingLengthMismatch);
            }
            entries.push((bucket_id, entry_count, mode, mapping_offset, mapping_length));
            expected_offset = end;
            previous_id = Some(bucket_id);
        }
        if offset != mapping_start || expected_offset != mapping_end {
            return Err(DecodeError::MappingLengthMismatch);
        }
        let buckets = entries
            .into_iter()
            .map(
                |(bucket_id, entry_count, mode, mapping_offset, mapping_length)| Bucket {
                    bucket_id,
                    entries: entry_count,
                    mode,
                    mapping: bytes[mapping_offset..mapping_offset + mapping_length].to_vec(),
                },
            )
            .collect();
        Ok(Self { buckets })
    }
}

fn bucket(bucket_id: u32, entries: u32, mode: Mode) -> Bucket {
    let length = mode.mapping_bytes(entries).expect("fixture mapping length");
    Bucket {
        bucket_id,
        entries,
        mode,
        mapping: (0..length).map(|index| (index % 251) as u8).collect(),
    }
}

#[test]
fn all_modes_round_trip_and_reopen() {
    for stride in [16, 32, 64] {
        let blob = MateBlob {
            buckets: vec![bucket(2, 128, Mode::Sampled { stride })],
        };
        let bytes = blob.encode().expect("encode sampled");
        assert_eq!(MateBlob::decode(&bytes).expect("decode sampled"), blob);
    }
    for width_bytes in 1..=4 {
        let blob = MateBlob {
            buckets: vec![bucket(2, 128, Mode::Packed { width_bytes })],
        };
        let bytes = blob.encode().expect("encode packed");
        assert_eq!(MateBlob::decode(&bytes).expect("decode packed"), blob);
    }
}

#[test]
fn multi_bucket_directory_amortizes_shared_layout() {
    let blob = MateBlob {
        buckets: vec![
            bucket(2, 8, Mode::Sampled { stride: 16 }),
            bucket(7, 32, Mode::Packed { width_bytes: 2 }),
        ],
    };
    let bytes = blob.encode().expect("encode multi-bucket");
    assert_eq!(
        bytes.len(),
        HEADER_BYTES + 2 * DIRECTORY_ENTRY_BYTES + 16 + 128
    );
    assert_eq!(MateBlob::decode(&bytes).expect("decode multi-bucket"), blob);
}

#[test]
fn corruption_is_rejected_before_a_result_is_returned() {
    let blob = MateBlob {
        buckets: vec![bucket(2, 32, Mode::Packed { width_bytes: 1 })],
    };
    let bytes = blob.encode().expect("encode");

    let mut truncated = bytes.clone();
    truncated.pop();
    assert_eq!(
        MateBlob::decode(&truncated),
        Err(DecodeError::TotalLengthMismatch)
    );

    let mut wrong_version = bytes.clone();
    wrong_version[4] = 9;
    assert_eq!(
        MateBlob::decode(&wrong_version),
        Err(DecodeError::UnsupportedVersion(9))
    );

    let mut wrong_offset = bytes;
    wrong_offset[36] = 0;
    wrong_offset[37] = 0;
    wrong_offset[38] = 0;
    wrong_offset[39] = 1;
    assert_eq!(
        MateBlob::decode(&wrong_offset),
        Err(DecodeError::MappingOffset)
    );
}

#[test]
fn malformed_shapes_and_trailing_bytes_are_rejected() {
    let blob = MateBlob {
        buckets: vec![bucket(2, 1, Mode::Packed { width_bytes: 1 })],
    };
    let mut bytes = blob.encode().expect("encode");
    bytes.push(0);
    assert_eq!(
        MateBlob::decode(&bytes),
        Err(DecodeError::TotalLengthMismatch)
    );

    let mut bytes = blob.encode().expect("encode");
    bytes[12] = 0;
    bytes[13] = 0;
    bytes[14] = 0;
    bytes[15] = 1;
    assert_eq!(
        MateBlob::decode(&bytes),
        Err(DecodeError::DirectoryLengthMismatch)
    );
}

#[test]
fn encoding_rejects_duplicate_or_unsorted_buckets_and_wrong_mapping_length() {
    let duplicate = MateBlob {
        buckets: vec![
            bucket(2, 1, Mode::Packed { width_bytes: 1 }),
            bucket(2, 1, Mode::Packed { width_bytes: 1 }),
        ],
    };
    assert_eq!(
        duplicate.encode(),
        Err(EncodeError::BucketsNotStrictlyIncreasing)
    );

    let mut malformed = bucket(2, 1, Mode::Packed { width_bytes: 1 });
    malformed.mapping.pop();
    assert!(matches!(
        (MateBlob {
            buckets: vec![malformed]
        })
        .encode(),
        Err(EncodeError::MappingLengthMismatch { .. })
    ));
}
