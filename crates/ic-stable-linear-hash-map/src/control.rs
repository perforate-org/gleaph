use crate::header::{CONTROL_BYTES, ControlRegion};
use crate::memory::write_u64;
use ic_stable_structures::Memory;

const LEN_OFFSET: u64 = 0;
const LEVEL_OFFSET: u64 = 8;
const SPLIT_CURSOR_OFFSET: u64 = 16;
const PHYSICAL_BUCKETS_OFFSET: u64 = 24;
const HASH_SEED_OFFSET: u64 = 32;
const SPLIT_WORK_CURSOR_OFFSET: u64 = 40;
const MUTATION_EPOCH_OFFSET: u64 = 48;
const HASH_ENCODING_ID_OFFSET: u64 = 56;

pub(crate) const INITIAL_MUTATION_EPOCH: u64 = 0;

#[derive(Clone, Copy)]
pub(crate) struct HotControl {
    pub(crate) len: u64,
    pub(crate) level: u8,
    pub(crate) split_cursor: u64,
    pub(crate) hash_seed: u64,
}

pub(crate) fn read<M: Memory>(memory: &M, offset: u64) -> ControlRegion {
    let mut bytes = [0; CONTROL_BYTES as usize];
    memory.read(offset, &mut bytes);
    ControlRegion {
        len: u64_at(&bytes, LEN_OFFSET),
        level: bytes[LEVEL_OFFSET as usize],
        split_state: bytes[LEVEL_OFFSET as usize + 1],
        journal_state: bytes[LEVEL_OFFSET as usize + 2],
        split_cursor: u64_at(&bytes, SPLIT_CURSOR_OFFSET),
        physical_buckets: u64_at(&bytes, PHYSICAL_BUCKETS_OFFSET),
        hash_seed: u64_at(&bytes, HASH_SEED_OFFSET),
        split_work_cursor: u64_at(&bytes, SPLIT_WORK_CURSOR_OFFSET),
        mutation_epoch: u64_at(&bytes, MUTATION_EPOCH_OFFSET),
        hash_encoding_id: u64_at(&bytes, HASH_ENCODING_ID_OFFSET),
    }
}

fn u64_at(bytes: &[u8; CONTROL_BYTES as usize], offset: u64) -> u64 {
    let start = offset as usize;
    u64::from_le_bytes(
        bytes[start..start + 8]
            .try_into()
            .expect("fixed control field"),
    )
}

pub(crate) fn read_len<M: Memory>(memory: &M, offset: u64) -> u64 {
    let mut bytes = [0; 8];
    memory.read(offset + LEN_OFFSET, &mut bytes);
    u64::from_le_bytes(bytes)
}

pub(crate) fn read_hash_seed<M: Memory>(memory: &M, offset: u64) -> u64 {
    let mut bytes = [0; 8];
    memory.read(offset + HASH_SEED_OFFSET, &mut bytes);
    u64::from_le_bytes(bytes)
}

pub(crate) fn read_mutation_epoch<M: Memory>(memory: &M, offset: u64) -> u64 {
    let mut bytes = [0; 8];
    memory.read(offset + MUTATION_EPOCH_OFFSET, &mut bytes);
    u64::from_le_bytes(bytes)
}

pub(crate) fn read_hot<M: Memory>(memory: &M, offset: u64) -> HotControl {
    let mut bytes = [0; (HASH_SEED_OFFSET + 8) as usize];
    memory.read(offset, &mut bytes);
    HotControl {
        len: u64::from_le_bytes(bytes[0..8].try_into().expect("fixed length field")),
        level: bytes[LEVEL_OFFSET as usize],
        split_cursor: u64::from_le_bytes(
            bytes[SPLIT_CURSOR_OFFSET as usize..SPLIT_CURSOR_OFFSET as usize + 8]
                .try_into()
                .expect("fixed split cursor field"),
        ),
        hash_seed: u64::from_le_bytes(
            bytes[HASH_SEED_OFFSET as usize..HASH_SEED_OFFSET as usize + 8]
                .try_into()
                .expect("fixed hash seed field"),
        ),
    }
}

pub(crate) fn write<M: Memory>(memory: &M, offset: u64, control: ControlRegion) {
    memory.write(offset, &[0; CONTROL_BYTES as usize]);
    write_u64(memory, offset + LEN_OFFSET, control.len);
    memory.write(
        offset + LEVEL_OFFSET,
        &[control.level, control.split_state, control.journal_state, 0],
    );
    write_u64(memory, offset + SPLIT_CURSOR_OFFSET, control.split_cursor);
    write_u64(
        memory,
        offset + PHYSICAL_BUCKETS_OFFSET,
        control.physical_buckets,
    );
    write_u64(memory, offset + HASH_SEED_OFFSET, control.hash_seed);
    write_u64(
        memory,
        offset + SPLIT_WORK_CURSOR_OFFSET,
        control.split_work_cursor,
    );
    write_u64(
        memory,
        offset + MUTATION_EPOCH_OFFSET,
        control.mutation_epoch,
    );
    write_u64(
        memory,
        offset + HASH_ENCODING_ID_OFFSET,
        control.hash_encoding_id,
    );
}

pub(crate) fn write_len<M: Memory>(memory: &M, offset: u64, len: u64) {
    write_u64(memory, offset + LEN_OFFSET, len);
}

pub(crate) fn write_hash_seed<M: Memory>(memory: &M, offset: u64, seed: u64) {
    write_u64(memory, offset + HASH_SEED_OFFSET, seed);
}

pub(crate) fn write_mutation_epoch<M: Memory>(memory: &M, offset: u64, epoch: u64) {
    write_u64(memory, offset + MUTATION_EPOCH_OFFSET, epoch);
}

/// Publishes settled split metadata while the caller owns an odd mutation epoch.
///
/// This deliberately does not reuse [`write`]: that initialization helper clears the complete
/// control page before rewriting it and could expose an even epoch between writes.  Each field
/// below is independent of the epoch field, which remains odd until `MutationGuard::finish`.
pub(crate) fn publish_split<M: Memory>(
    memory: &M,
    offset: u64,
    level: u8,
    split_cursor: u64,
    physical_buckets: u64,
    len: u64,
) {
    memory.write(offset + LEVEL_OFFSET, &[level]);
    write_u64(memory, offset + SPLIT_CURSOR_OFFSET, split_cursor);
    write_u64(memory, offset + PHYSICAL_BUCKETS_OFFSET, physical_buckets);
    write_u64(memory, offset + LEN_OFFSET, len);
}
