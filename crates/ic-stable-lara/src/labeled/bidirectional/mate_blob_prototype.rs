//! Sole compact mate blob codec for ADR 0048.
//!
//! The storage owner, promotion admission, and reopen validation all use this same compact
//! representation. ScanOnly buckets are omitted by the caller.

const PHYSICAL_HALVES: u64 = 2;
const SAMPLE_FIELDS: u64 = 2;
const SAMPLE_U32_BYTES: u64 = 4;
const HEADER_BYTES: usize = 8;
const DIRECTORY_ENTRY_BYTES: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Sampled { stride: u8 },
    Packed { width_bytes: u8 },
}

impl Mode {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated,
    EmptyBlob,
    TotalLengthMismatch,
    UnsupportedSampleStride(u8),
    UnsupportedPackedWidth(u8),
    ArithmeticOverflow,
    EmptyBucket,
    BucketOrder,
    MappingOffset,
    MappingLengthMismatch,
    TrailingBytes,
    CompactReservedBits,
    CompactFlags(u8),
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

fn encode_compact_flags(mode: Mode) -> Result<u8, EncodeError> {
    let (mode_tag, parameter) = match mode {
        Mode::Sampled { stride } => {
            let code = match stride {
                16 => 0,
                32 => 1,
                64 => 2,
                other => return Err(EncodeError::UnsupportedSampleStride(other)),
            };
            (1u8, code)
        }
        Mode::Packed { width_bytes } => {
            if !(1..=4).contains(&width_bytes) {
                return Err(EncodeError::UnsupportedPackedWidth(width_bytes));
            }
            (2u8, width_bytes - 1)
        }
    };
    Ok((mode_tag << 6) | parameter)
}

fn decode_compact_flags(flags: u8) -> Result<Mode, DecodeError> {
    let mode_tag = flags >> 6;
    let parameter = flags & 0x3f;
    match mode_tag {
        1 => match parameter {
            0 => Ok(Mode::Sampled { stride: 16 }),
            1 => Ok(Mode::Sampled { stride: 32 }),
            2 => Ok(Mode::Sampled { stride: 64 }),
            _ => Err(DecodeError::CompactFlags(flags)),
        },
        2 if parameter < 4 => Ok(Mode::Packed {
            width_bytes: parameter + 1,
        }),
        _ => Err(DecodeError::CompactFlags(flags)),
    }
}

impl MateBlob {
    pub(crate) fn encoded_len(&self) -> Result<usize, EncodeError> {
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
            if previous_id.is_some_and(|previous| {
                (bucket.owner_vertex_id, bucket.bucket_label_key) <= previous
            }) {
                return Err(EncodeError::BucketsNotStrictlyIncreasing);
            }
            bucket.validate()?;
            encode_compact_flags(bucket.mode)?;
            mapping_bytes = mapping_bytes
                .checked_add(bucket.mapping.len())
                .ok_or(EncodeError::ArithmeticOverflow)?;
            previous_id = Some((bucket.owner_vertex_id, bucket.bucket_label_key));
        }
        u16::try_from(self.buckets.len()).map_err(|_| EncodeError::TooLarge)?;
        let total_bytes = HEADER_BYTES
            .checked_add(directory_bytes)
            .and_then(|value| value.checked_add(mapping_bytes))
            .ok_or(EncodeError::ArithmeticOverflow)?;
        u32::try_from(total_bytes).map_err(|_| EncodeError::TooLarge)?;
        Ok(total_bytes)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let total_bytes = self.encoded_len()?;
        let bucket_count = u16::try_from(self.buckets.len()).map_err(|_| EncodeError::TooLarge)?;
        let directory_bytes = self
            .buckets
            .len()
            .checked_mul(DIRECTORY_ENTRY_BYTES)
            .ok_or(EncodeError::ArithmeticOverflow)?;
        let mut out = Vec::with_capacity(total_bytes);
        out.extend_from_slice(&bucket_count.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(
            &u32::try_from(total_bytes)
                .map_err(|_| EncodeError::TooLarge)?
                .to_be_bytes(),
        );
        let mut mapping_offset = HEADER_BYTES
            .checked_add(directory_bytes)
            .ok_or(EncodeError::ArithmeticOverflow)?;
        for bucket in &self.buckets {
            out.extend_from_slice(&bucket.owner_vertex_id.to_be_bytes());
            out.extend_from_slice(&bucket.bucket_label_key.to_be_bytes());
            out.push(encode_compact_flags(bucket.mode)?);
            out.extend_from_slice(&bucket.entries.to_be_bytes());
            out.extend_from_slice(
                &u32::try_from(mapping_offset)
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
        debug_assert_eq!(out.len(), total_bytes);
        Ok(out)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < HEADER_BYTES {
            return Err(DecodeError::Truncated);
        }
        let mut offset = 0;
        let bucket_count = usize::from(read_u16(bytes, &mut offset)?);
        if bucket_count == 0 {
            return Err(DecodeError::EmptyBlob);
        }
        if read_u16(bytes, &mut offset)? != 0 {
            return Err(DecodeError::CompactReservedBits);
        }
        let total_len = usize::try_from(read_u32(bytes, &mut offset)?)
            .map_err(|_| DecodeError::ArithmeticOverflow)?;
        if total_len != bytes.len() {
            return Err(DecodeError::TotalLengthMismatch);
        }
        let directory_len = bucket_count
            .checked_mul(DIRECTORY_ENTRY_BYTES)
            .ok_or(DecodeError::ArithmeticOverflow)?;
        let mapping_start = HEADER_BYTES
            .checked_add(directory_len)
            .ok_or(DecodeError::ArithmeticOverflow)?;
        if mapping_start > bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let mut entries = Vec::with_capacity(bucket_count);
        let mut previous_id = None;
        let mut expected_offset = mapping_start;
        for _ in 0..bucket_count {
            let owner_vertex_id = read_u32(bytes, &mut offset)?;
            let bucket_label_key = read_u16(bytes, &mut offset)?;
            let flags = read::<1>(bytes, &mut offset)?[0];
            let mode = decode_compact_flags(flags)?;
            let entry_count = read_u32(bytes, &mut offset)?;
            let mapping_offset = usize::try_from(read_u32(bytes, &mut offset)?)
                .map_err(|_| DecodeError::ArithmeticOverflow)?;
            if previous_id.is_some_and(|previous| (owner_vertex_id, bucket_label_key) <= previous) {
                return Err(DecodeError::BucketOrder);
            }
            if entry_count == 0 {
                return Err(DecodeError::EmptyBucket);
            }
            if mapping_offset != expected_offset {
                return Err(DecodeError::MappingOffset);
            }
            let mapping_length = mode.mapping_bytes(entry_count)?;
            let end = mapping_offset
                .checked_add(mapping_length)
                .ok_or(DecodeError::ArithmeticOverflow)?;
            if end > bytes.len() {
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
        if offset != mapping_start {
            return Err(DecodeError::MappingOffset);
        }
        if expected_offset != bytes.len() {
            return Err(if expected_offset < bytes.len() {
                DecodeError::TrailingBytes
            } else {
                DecodeError::MappingLengthMismatch
            });
        }
        let buckets = entries
            .into_iter()
            .map(
                |(
                    owner_vertex_id,
                    bucket_label_key,
                    entries,
                    mode,
                    mapping_offset,
                    mapping_length,
                )| Bucket {
                    owner_vertex_id,
                    bucket_label_key,
                    entries,
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
fn compact_blob_round_trip_and_sparse_directory_size() {
    let indexed = MateBlob {
        buckets: vec![
            bucket(2, 7, 8, Mode::Sampled { stride: 16 }),
            bucket(2, 9, 32, Mode::Packed { width_bytes: 2 }),
        ],
    };
    let bytes = indexed.encode().expect("compact encode");
    assert_eq!(
        bytes.len(),
        HEADER_BYTES + 2 * DIRECTORY_ENTRY_BYTES + 8 + 128
    );
    assert_eq!(bytes.len(), indexed.encoded_len().expect("compact length"));
    assert_eq!(MateBlob::decode(&bytes).expect("compact decode"), indexed);

    // ScanOnly buckets are omitted by construction; an indexed bucket is the only directory row.
    let one = MateBlob {
        buckets: vec![bucket(3, 1, 32, Mode::Packed { width_bytes: 1 })],
    };
    let one_bytes = one.encode().expect("single compact encode");
    assert_eq!(one_bytes.len(), HEADER_BYTES + DIRECTORY_ENTRY_BYTES + 64);
}

#[test]
fn compact_blob_rejects_reserved_flags_offsets_and_trailing_bytes() {
    let blob = MateBlob {
        buckets: vec![bucket(2, 7, 32, Mode::Packed { width_bytes: 1 })],
    };
    let encoded = blob.encode().expect("compact encode");

    let mut reserved = encoded.clone();
    reserved[2] = 1;
    assert_eq!(
        MateBlob::decode(&reserved),
        Err(DecodeError::CompactReservedBits)
    );

    let mut flags = encoded.clone();
    flags[14] = 0xff;
    assert_eq!(
        MateBlob::decode(&flags),
        Err(DecodeError::CompactFlags(0xff))
    );

    let mut offset = encoded.clone();
    offset[19..23].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(MateBlob::decode(&offset), Err(DecodeError::MappingOffset));

    let mut trailing = encoded;
    trailing.extend_from_slice(&[0, 0]);
    let total = u32::try_from(trailing.len()).expect("compact fixture fits");
    trailing[4..8].copy_from_slice(&total.to_be_bytes());
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

#[cfg(feature = "canbench")]
fn compact_bench_blob() -> MateBlob {
    MateBlob {
        buckets: vec![
            bucket(2, 7, 32, Mode::Sampled { stride: 32 }),
            bucket(2, 9, 128, Mode::Packed { width_bytes: 2 }),
            bucket(3, 1, 64, Mode::Packed { width_bytes: 1 }),
        ],
    }
}

#[cfg(feature = "canbench")]
#[canbench_rs::bench(raw)]
fn bench_mate_blob_baseline_encode_decode() -> canbench_rs::BenchResult {
    let blob = MateBlob {
        buckets: compact_bench_blob().buckets,
    };
    canbench_rs::bench_fn(|| {
        let bytes = blob.encode().expect("baseline encode");
        let decoded = MateBlob::decode(&bytes).expect("baseline decode");
        std::hint::black_box((bytes.len(), decoded.buckets.len()));
    })
}

#[cfg(feature = "canbench")]
#[canbench_rs::bench(raw)]
fn bench_mate_blob_compact_encode_decode() -> canbench_rs::BenchResult {
    let blob = compact_bench_blob();
    canbench_rs::bench_fn(|| {
        let bytes = blob.encode().expect("compact encode");
        let decoded = MateBlob::decode(&bytes).expect("compact decode");
        std::hint::black_box((bytes.len(), decoded.buckets.len()));
    })
}
