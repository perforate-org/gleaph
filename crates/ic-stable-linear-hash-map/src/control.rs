use crate::header::{CONTROL_BYTES, ControlRegion};
use crate::memory::write_u64;
use ic_stable_structures::Memory;

const LEN_OFFSET: u64 = 0;
const PHYSICAL_BUCKETS_OFFSET: u64 = 8;
const MUTATION_EPOCH_OFFSET: u64 = 16;
const INCARNATION_OFFSET: u64 = 24;
const BACKWARD_RELOCATION_GENERATION_OFFSET: u64 = 32;

pub(crate) const INITIAL_MUTATION_EPOCH: u64 = 0;
pub(crate) const INITIAL_INCARNATION: u64 = 1;

#[derive(Clone, Copy)]
pub(crate) struct HotControl {
    pub(crate) level: u8,
    pub(crate) split_cursor: u64,
    pub(crate) hash_seed: u64,
}

pub(crate) fn read<M: Memory>(memory: &M, offset: u64, hash_seed: u64) -> ControlRegion {
    let mut bytes = [0; CONTROL_BYTES as usize];
    memory.read(offset, &mut bytes);
    decode(&bytes, hash_seed)
}

pub(crate) fn read_for_open<M: Memory>(
    memory: &M,
    offset: u64,
    hash_seed: u64,
) -> Result<ControlRegion, ()> {
    let mut bytes = [0; CONTROL_BYTES as usize];
    memory.read(offset, &mut bytes);
    if bytes[40..].iter().any(|byte| *byte != 0) {
        return Err(());
    }
    Ok(decode(&bytes, hash_seed))
}

fn decode(bytes: &[u8; CONTROL_BYTES as usize], hash_seed: u64) -> ControlRegion {
    let physical_buckets = u64_at(bytes, PHYSICAL_BUCKETS_OFFSET);
    let level = if physical_buckets == 0 {
        0
    } else {
        (u64::BITS - 1 - physical_buckets.leading_zeros()) as u8
    };
    ControlRegion {
        len: u64_at(bytes, LEN_OFFSET),
        physical_buckets,
        mutation_epoch: u64_at(bytes, MUTATION_EPOCH_OFFSET),
        incarnation: u64_at(bytes, INCARNATION_OFFSET),
        backward_relocation_generation: u64_at(bytes, BACKWARD_RELOCATION_GENERATION_OFFSET),
        level,
        split_cursor: physical_buckets
            .saturating_sub(1u64.checked_shl(u32::from(level)).unwrap_or(0)),
        hash_seed,
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

pub(crate) fn read_mutation_epoch<M: Memory>(memory: &M, offset: u64) -> u64 {
    let mut bytes = [0; 8];
    memory.read(offset + MUTATION_EPOCH_OFFSET, &mut bytes);
    u64::from_le_bytes(bytes)
}

pub(crate) fn read_hot<M: Memory>(memory: &M, offset: u64, hash_seed: u64) -> HotControl {
    let mut bytes = [0; 8];
    memory.read(offset + PHYSICAL_BUCKETS_OFFSET, &mut bytes);
    let physical_buckets = u64::from_le_bytes(bytes);
    let level = (u64::BITS - 1 - physical_buckets.leading_zeros()) as u8;
    HotControl {
        level,
        split_cursor: physical_buckets - (1u64 << level),
        hash_seed,
    }
}

pub(crate) fn write<M: Memory>(memory: &M, offset: u64, control: ControlRegion) {
    let mut bytes = [0; CONTROL_BYTES as usize];
    bytes[LEN_OFFSET as usize..LEN_OFFSET as usize + 8].copy_from_slice(&control.len.to_le_bytes());
    bytes[PHYSICAL_BUCKETS_OFFSET as usize..PHYSICAL_BUCKETS_OFFSET as usize + 8]
        .copy_from_slice(&control.physical_buckets.to_le_bytes());
    bytes[MUTATION_EPOCH_OFFSET as usize..MUTATION_EPOCH_OFFSET as usize + 8]
        .copy_from_slice(&control.mutation_epoch.to_le_bytes());
    bytes[INCARNATION_OFFSET as usize..INCARNATION_OFFSET as usize + 8]
        .copy_from_slice(&control.incarnation.to_le_bytes());
    bytes[BACKWARD_RELOCATION_GENERATION_OFFSET as usize
        ..BACKWARD_RELOCATION_GENERATION_OFFSET as usize + 8]
        .copy_from_slice(&control.backward_relocation_generation.to_le_bytes());
    memory.write(offset, &bytes);
}

pub(crate) fn write_len<M: Memory>(memory: &M, offset: u64, len: u64) {
    write_u64(memory, offset + LEN_OFFSET, len);
}

pub(crate) fn write_mutation_epoch<M: Memory>(memory: &M, offset: u64, epoch: u64) {
    write_u64(memory, offset + MUTATION_EPOCH_OFFSET, epoch);
}

pub(crate) fn write_backward_relocation_generation<M: Memory>(
    memory: &M,
    offset: u64,
    generation: u64,
) {
    write_u64(
        memory,
        offset + BACKWARD_RELOCATION_GENERATION_OFFSET,
        generation,
    );
}

pub(crate) fn publish_split<M: Memory>(
    memory: &M,
    offset: u64,
    _level: u8,
    _split_cursor: u64,
    physical_buckets: u64,
    len: u64,
) {
    write_u64(memory, offset + PHYSICAL_BUCKETS_OFFSET, physical_buckets);
    write_u64(memory, offset + LEN_OFFSET, len);
}
