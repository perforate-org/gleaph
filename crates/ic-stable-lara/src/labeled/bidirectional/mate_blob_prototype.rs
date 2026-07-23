//! Versioned mate blob codec for ADR 0048's dormant storage foundation.
//!
//! The codec is internal-only: it has no graph lookup or promotion entry point. The storage
//! foundation uses it to validate blobs before publication and on reopen; runtime promotion is
//! deferred to a later slice.

#![expect(dead_code, reason = "codec is dormant until the promotion slice")]

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
                if !matches!(stride, 16 | 32 | 64) {
                    return Err(DecodeError::UnsupportedSampleStride(stride));
                }
                let checkpoints = entries
                    .checked_add(u64::from(stride) - 1)
                    .ok_or(DecodeError::ArithmeticOverflow)?
                    / u64::from(stride);
                // One directory bucket stores one source/mate pair per checkpoint. A
                // non-self logical edge has two such buckets (forward and reverse), hence
                // the two-half accounting is 16 bytes per checkpoint in the ADR.
                checkpoints
                    .checked_mul(SAMPLE_FIELDS)
                    .and_then(|value| value.checked_mul(SAMPLE_U32_BYTES))
                    .ok_or(DecodeError::ArithmeticOverflow)?
            }
            Self::Packed { width_bytes } => {
                if !(1..=4).contains(&width_bytes) {
                    return Err(DecodeError::UnsupportedPackedWidth(width_bytes));
                }
                PHYSICAL_HALVES
                    .checked_mul(entries)
                    .and_then(|value| value.checked_mul(u64::from(width_bytes)))
                    .ok_or(DecodeError::ArithmeticOverflow)?
            }
        };
        usize::try_from(bytes).map_err(|_| DecodeError::ArithmeticOverflow)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bucket {
    pub owner_vertex_id: u32,
    pub bucket_label_key: u16,
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
                owner_vertex_id: self.owner_vertex_id,
                bucket_label_key: self.bucket_label_key,
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
        owner_vertex_id: u32,
        bucket_label_key: u16,
        expected: usize,
        actual: usize,
    },
    UnsupportedSampleStride(u8),
    UnsupportedPackedWidth(u8),
    ArithmeticOverflow,
    TooLarge,
}

impl From<DecodeError> for EncodeError {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::ArithmeticOverflow => Self::ArithmeticOverflow,
            DecodeError::UnsupportedSampleStride(value) => Self::UnsupportedSampleStride(value),
            DecodeError::UnsupportedPackedWidth(value) => Self::UnsupportedPackedWidth(value),
            _ => Self::ArithmeticOverflow,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    UnsupportedFlags(u8),
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
    /// Returns the exact encoded size after applying the same validation used by [`Self::encode`].
    pub(crate) fn encoded_len(&self) -> Result<usize, EncodeError> {
        let directory_bytes = self
            .buckets
            .len()
            .checked_mul(DIRECTORY_ENTRY_BYTES)
            .ok_or(EncodeError::ArithmeticOverflow)?;
        if self.buckets.is_empty() {
            return Err(EncodeError::EmptyBlob);
        }
        let mut mapping_bytes = 0usize;
        let mut previous_id = None;
        for bucket in &self.buckets {
            if previous_id.is_some_and(|previous| {
                (bucket.owner_vertex_id, bucket.bucket_label_key) <= previous
            }) {
                return Err(EncodeError::BucketsNotStrictlyIncreasing);
            }
            bucket.validate()?;
            mapping_bytes = mapping_bytes
                .checked_add(bucket.mapping.len())
                .ok_or(EncodeError::ArithmeticOverflow)?;
            previous_id = Some((bucket.owner_vertex_id, bucket.bucket_label_key));
        }
        let total_bytes = HEADER_BYTES
            .checked_add(directory_bytes)
            .and_then(|value| value.checked_add(mapping_bytes))
            .ok_or(EncodeError::ArithmeticOverflow)?;
        u32::try_from(self.buckets.len()).map_err(|_| EncodeError::TooLarge)?;
        u32::try_from(directory_bytes).map_err(|_| EncodeError::TooLarge)?;
        u32::try_from(mapping_bytes).map_err(|_| EncodeError::TooLarge)?;
        u32::try_from(total_bytes).map_err(|_| EncodeError::TooLarge)?;
        Ok(total_bytes)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let total_bytes = self.encoded_len()?;
        let directory_bytes = self
            .buckets
            .len()
            .checked_mul(DIRECTORY_ENTRY_BYTES)
            .ok_or(EncodeError::ArithmeticOverflow)?;
        let mapping_bytes = total_bytes
            .checked_sub(HEADER_BYTES)
            .and_then(|value| value.checked_sub(directory_bytes))
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
            out.extend_from_slice(&bucket.owner_vertex_id.to_be_bytes());
            out.extend_from_slice(&bucket.bucket_label_key.to_be_bytes());
            out.push(mode);
            out.push(parameter);
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
        let flags = read::<1>(bytes, &mut offset)?[0];
        if flags != 0 {
            return Err(DecodeError::UnsupportedFlags(flags));
        }
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
            let owner_vertex_id = read_u32(bytes, &mut offset)?;
            let bucket_label_key = read_u16(bytes, &mut offset)?;
            let mode = read::<1>(bytes, &mut offset)?[0];
            let parameter = read::<1>(bytes, &mut offset)?[0];
            let entry_count = read_u32(bytes, &mut offset)?;
            let mapping_offset = usize::try_from(read_u32(bytes, &mut offset)?)
                .map_err(|_| DecodeError::ArithmeticOverflow)?;
            let mapping_length = usize::try_from(read_u32(bytes, &mut offset)?)
                .map_err(|_| DecodeError::ArithmeticOverflow)?;
            if previous_id.is_some_and(|previous| (owner_vertex_id, bucket_label_key) <= previous) {
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
            entries.push((
                owner_vertex_id,
                bucket_label_key,
                entry_count,
                mode,
                mapping_offset,
                mapping_length,
            ));
            expected_offset = end;
            previous_id = Some((owner_vertex_id, bucket_label_key));
        }
        if offset != mapping_start || expected_offset != mapping_end {
            return Err(DecodeError::MappingLengthMismatch);
        }
        let buckets = entries
            .into_iter()
            .map(
                |(
                    owner_vertex_id,
                    bucket_label_key,
                    entry_count,
                    mode,
                    mapping_offset,
                    mapping_length,
                )| Bucket {
                    owner_vertex_id,
                    bucket_label_key,
                    entries: entry_count,
                    mode,
                    mapping: bytes[mapping_offset..mapping_offset + mapping_length].to_vec(),
                },
            )
            .collect();
        Ok(Self { buckets })
    }
}

fn bucket(owner_vertex_id: u32, bucket_label_key: u16, entries: u32, mode: Mode) -> Bucket {
    let length = mode.mapping_bytes(entries).expect("fixture mapping length");
    Bucket {
        owner_vertex_id,
        bucket_label_key,
        entries,
        mode,
        mapping: (0..length).map(|index| (index % 251) as u8).collect(),
    }
}

#[test]
fn all_modes_round_trip_and_reopen() {
    for stride in [16, 32, 64] {
        let blob = MateBlob {
            buckets: vec![bucket(2, 7, 128, Mode::Sampled { stride })],
        };
        let bytes = blob.encode().expect("encode sampled");
        assert_eq!(MateBlob::decode(&bytes).expect("decode sampled"), blob);
    }
    for width_bytes in 1..=4 {
        let blob = MateBlob {
            buckets: vec![bucket(2, 7, 128, Mode::Packed { width_bytes })],
        };
        let bytes = blob.encode().expect("encode packed");
        assert_eq!(MateBlob::decode(&bytes).expect("decode packed"), blob);
    }
}

#[test]
fn multi_bucket_directory_amortizes_shared_layout() {
    let blob = MateBlob {
        buckets: vec![
            bucket(2, 7, 8, Mode::Sampled { stride: 16 }),
            bucket(2, 9, 32, Mode::Packed { width_bytes: 2 }),
        ],
    };
    let bytes = blob.encode().expect("encode multi-bucket");
    assert_eq!(
        bytes.len(),
        HEADER_BYTES + 2 * DIRECTORY_ENTRY_BYTES + 8 + 128
    );
    assert_eq!(MateBlob::decode(&bytes).expect("decode multi-bucket"), blob);
}

#[test]
fn corruption_is_rejected_before_a_result_is_returned() {
    let blob = MateBlob {
        buckets: vec![bucket(2, 7, 32, Mode::Packed { width_bytes: 1 })],
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
        buckets: vec![bucket(2, 7, 1, Mode::Packed { width_bytes: 1 })],
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

    let mut bytes = blob.encode().expect("encode");
    bytes[5] = 1;
    assert_eq!(
        MateBlob::decode(&bytes),
        Err(DecodeError::UnsupportedFlags(1))
    );

    let mut bytes = blob.encode().expect("encode");
    bytes[6] = 0;
    bytes[7] = 23;
    assert_eq!(
        MateBlob::decode(&bytes),
        Err(DecodeError::InvalidHeaderLength(23))
    );

    for (mode, parameter, expected) in [
        (3, 1, DecodeError::UnsupportedMode(3)),
        (1, 8, DecodeError::UnsupportedSampleStride(8)),
        (2, 5, DecodeError::UnsupportedPackedWidth(5)),
    ] {
        let mut bytes = blob.encode().expect("encode");
        bytes[30] = mode;
        bytes[31] = parameter;
        assert_eq!(MateBlob::decode(&bytes), Err(expected));
    }

    let mut bytes = blob.encode().expect("encode");
    bytes[32..36].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(MateBlob::decode(&bytes), Err(DecodeError::EmptyBucket));

    assert_eq!(
        (MateBlob { buckets: vec![] }).encode(),
        Err(EncodeError::EmptyBlob)
    );

    let mut trailing = blob.encode().expect("encode");
    trailing.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    let trailing_len = u32::try_from(trailing.len()).expect("fixture fits u32");
    trailing[20..24].copy_from_slice(&trailing_len.to_be_bytes());
    assert_eq!(MateBlob::decode(&trailing), Err(DecodeError::TrailingBytes));
}

#[test]
fn encoding_rejects_duplicate_or_unsorted_buckets_and_wrong_mapping_length() {
    let duplicate = MateBlob {
        buckets: vec![
            bucket(2, 7, 1, Mode::Packed { width_bytes: 1 }),
            bucket(2, 7, 1, Mode::Packed { width_bytes: 1 }),
        ],
    };
    assert_eq!(
        duplicate.encode(),
        Err(EncodeError::BucketsNotStrictlyIncreasing)
    );

    let unsorted = MateBlob {
        buckets: vec![
            bucket(2, 9, 1, Mode::Packed { width_bytes: 1 }),
            bucket(2, 7, 1, Mode::Packed { width_bytes: 1 }),
        ],
    };
    assert_eq!(
        unsorted.encode(),
        Err(EncodeError::BucketsNotStrictlyIncreasing)
    );

    for mode in [Mode::Sampled { stride: 8 }, Mode::Packed { width_bytes: 5 }] {
        let invalid = MateBlob {
            buckets: vec![Bucket {
                owner_vertex_id: 2,
                bucket_label_key: 7,
                entries: 1,
                mode,
                mapping: Vec::new(),
            }],
        };
        let error = invalid.encode().expect_err("unsupported mode must reject");
        assert!(matches!(
            (mode, error),
            (
                Mode::Sampled { .. },
                EncodeError::UnsupportedSampleStride(8)
            ) | (Mode::Packed { .. }, EncodeError::UnsupportedPackedWidth(5))
        ));
    }

    let mut malformed = bucket(2, 7, 1, Mode::Packed { width_bytes: 1 });
    malformed.mapping.pop();
    assert!(matches!(
        (MateBlob {
            buckets: vec![malformed]
        })
        .encode(),
        Err(EncodeError::MappingLengthMismatch { .. })
    ));
}

#[test]
fn decoding_rejects_unsorted_canonical_identities() {
    let blob = MateBlob {
        buckets: vec![
            bucket(2, 7, 1, Mode::Packed { width_bytes: 1 }),
            bucket(2, 9, 1, Mode::Packed { width_bytes: 1 }),
        ],
    };
    let mut bytes = blob.encode().expect("encode");
    bytes[44..48].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(MateBlob::decode(&bytes), Err(DecodeError::BucketOrder));
}
