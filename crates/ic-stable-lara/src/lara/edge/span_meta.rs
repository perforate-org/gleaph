//! Stable LARA segment span metadata.
//!
//! This store is placement metadata for update/maintenance work. Clean query
//! scans must not read it.

use crate::{GrowFailed, read_u64, safe_write, types::Address, write_u64};
use ic_stable_structures::Memory;
use std::{fmt, marker::PhantomData};

pub const MAGIC: [u8; 3] = *b"LSP";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 32;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 12;
const ENTRY_SIZE: u64 = 8;

#[derive(Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u64,
    stride: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentSpanMeta {
    pub physical_start: u64,
}

#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    StrideMismatch { expected: u32, actual: u32 },
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(fmt, "bad segment span magic {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(version) => {
                write!(fmt, "unsupported segment span layout version {version}")
            }
            Self::StrideMismatch { expected, actual } => write!(
                fmt,
                "segment span stride mismatch: expected {expected}, got {actual}"
            ),
            Self::OutOfMemory => write!(fmt, "failed to allocate segment span metadata"),
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Clone, Debug)]
pub struct SegmentSpanMetaStore<M: Memory> {
    memory: M,
    _marker: PhantomData<SegmentSpanMeta>,
}

impl<M: Memory> SegmentSpanMetaStore<M> {
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
            stride: ENTRY_SIZE as u32,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let header = Self::read_header(&memory);
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if header.stride != ENTRY_SIZE as u32 {
            return Err(InitError::StrideMismatch {
                expected: ENTRY_SIZE as u32,
                actual: header.stride,
            });
        }
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn len(&self) -> u64 {
        read_u64(&self.memory, Address::from(LEN_OFFSET))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: u64) -> SegmentSpanMeta {
        assert!(index < self.len());
        SegmentSpanMeta {
            physical_start: read_u64(&self.memory, Address::from(Self::entry_offset(index))),
        }
    }

    pub fn set(&self, index: u64, item: &SegmentSpanMeta) {
        assert!(index < self.len());
        write_u64(
            &self.memory,
            Address::from(Self::entry_offset(index)),
            item.physical_start,
        );
    }

    pub fn push(&self, item: SegmentSpanMeta) -> Result<(), GrowFailed> {
        let len = self.len();
        let new_len = len
            .checked_add(1)
            .expect("segment span vector length overflow");
        safe_write(
            &self.memory,
            Self::entry_offset(len),
            &item.physical_start.to_le_bytes(),
        )?;
        self.set_len(new_len);
        Ok(())
    }

    fn set_len(&self, new_len: u64) {
        write_u64(&self.memory, Address::from(LEN_OFFSET), new_len);
    }

    #[inline]
    fn entry_offset(index: u64) -> u64 {
        DATA_OFFSET + ENTRY_SIZE * index
    }

    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version; 1]);
        write_u64(memory, Address::from(LEN_OFFSET), header.len);
        crate::write_u32(memory, Address::from(STRIDE_OFFSET), header.stride);
        Ok(())
    }

    fn read_header(memory: &M) -> HeaderV1 {
        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        HeaderV1 {
            magic,
            version: version[0],
            len: read_u64(memory, Address::from(LEN_OFFSET)),
            stride: crate::read_u32(memory, Address::from(STRIDE_OFFSET)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    #[test]
    fn segment_span_meta_store_reopens_physical_starts() {
        let memory = vector_memory();
        let store = SegmentSpanMetaStore::new(memory.clone()).unwrap();
        store.push(SegmentSpanMeta { physical_start: 12 }).unwrap();
        store.push(SegmentSpanMeta { physical_start: 48 }).unwrap();

        let reopened = SegmentSpanMetaStore::init(memory).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get(0), SegmentSpanMeta { physical_start: 12 });
        assert_eq!(reopened.get(1), SegmentSpanMeta { physical_start: 48 });
    }
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{SegmentSpanMeta, SegmentSpanMetaStore};
    use crate::{bench as helper, test_support::vector_memory};

    /// Measures segment span metadata append, read, update, and reopen. This
    /// protects the tiny placement-metadata store used by relocation while
    /// keeping query scans independent of it.
    #[bench(raw)]
    fn bench_lara_span_meta_push_get_set_reopen_1024() -> canbench_rs::BenchResult {
        let memory = vector_memory();
        let store = SegmentSpanMetaStore::new(memory.clone()).expect("span meta");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_span_meta_push_get_set_reopen");
            for i in 0..helper::MEDIUM_N {
                store
                    .push(SegmentSpanMeta {
                        physical_start: black_box(i * 16),
                    })
                    .expect("push span meta");
            }
            let mut sum = 0u64;
            for i in 0..helper::MEDIUM_N {
                let meta = store.get(i);
                sum ^= meta.physical_start;
                store.set(
                    i,
                    &SegmentSpanMeta {
                        physical_start: meta.physical_start + 1,
                    },
                );
            }
            let reopened = SegmentSpanMetaStore::init(memory.clone()).expect("reopen span meta");
            black_box(sum ^ reopened.get(helper::MEDIUM_N - 1).physical_start);
        })
    }
}
