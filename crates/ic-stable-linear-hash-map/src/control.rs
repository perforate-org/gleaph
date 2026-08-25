use crate::header::{CONTROL_BYTES, ControlRegion};
use crate::memory::write_u64;
use ic_stable_structures::Memory;

const LEN_OFFSET: u64 = 0;
const PHYSICAL_BUCKETS_OFFSET: u64 = 8;
const MUTATION_EPOCH_OFFSET: u64 = 16;
const INCARNATION_OFFSET: u64 = 24;
const SPLIT_DEBT_OFFSET: u64 = 32;
const OVERFLOW_ENTRIES_OFFSET: u64 = 40;

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
    if bytes[48..].iter().any(|byte| *byte != 0) {
        return Err(());
    }
    Ok(decode(&bytes, hash_seed))
}

/// Single source of truth for `(level, split_cursor)` derived from
/// `physical_buckets`: every reader of persisted control bytes must route
/// through this helper so routing geometry cannot drift between call sites.
///
/// `physical_buckets == 0` is unreachable behind any handle opened through
/// open-time control validation; it degrades to the empty geometry `(0, 0)`
/// instead of trapping or wrapping.
pub(crate) fn derive_geometry(physical_buckets: u64) -> (u8, u64) {
    if physical_buckets == 0 {
        return (0, 0);
    }
    // `physical_buckets >= 1` puts `level` in `0..=63`, so the shift cannot
    // trap, and `1 << level <= physical_buckets`, so the subtraction cannot
    // underflow.
    let level = (u64::BITS - 1 - physical_buckets.leading_zeros()) as u8;
    (level, physical_buckets - (1u64 << level))
}

fn decode(bytes: &[u8; CONTROL_BYTES as usize], hash_seed: u64) -> ControlRegion {
    let physical_buckets = u64_at(bytes, PHYSICAL_BUCKETS_OFFSET);
    let (level, split_cursor) = derive_geometry(physical_buckets);
    ControlRegion {
        len: u64_at(bytes, LEN_OFFSET),
        physical_buckets,
        mutation_epoch: u64_at(bytes, MUTATION_EPOCH_OFFSET),
        incarnation: u64_at(bytes, INCARNATION_OFFSET),
        split_debt: u64_at(bytes, SPLIT_DEBT_OFFSET),
        overflow_entries: u64_at(bytes, OVERFLOW_ENTRIES_OFFSET),
        level,
        split_cursor,
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

pub(crate) fn read_hot_with_epoch<M: Memory>(
    memory: &M,
    offset: u64,
    hash_seed: u64,
) -> (HotControl, u64) {
    let mut bytes = [0; 16];
    memory.read(offset + PHYSICAL_BUCKETS_OFFSET, &mut bytes);
    let physical_buckets = u64::from_le_bytes(bytes[..8].try_into().expect("fixed control field"));
    let epoch = u64::from_le_bytes(bytes[8..].try_into().expect("fixed control field"));
    // Open-time control validation makes `physical_buckets == 0` unreachable
    // here; `derive_geometry` still fails closed on it instead of trapping.
    let (level, split_cursor) = derive_geometry(physical_buckets);
    (
        HotControl {
            level,
            split_cursor,
            hash_seed,
        },
        epoch,
    )
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
    bytes[SPLIT_DEBT_OFFSET as usize..SPLIT_DEBT_OFFSET as usize + 8]
        .copy_from_slice(&control.split_debt.to_le_bytes());
    bytes[OVERFLOW_ENTRIES_OFFSET as usize..OVERFLOW_ENTRIES_OFFSET as usize + 8]
        .copy_from_slice(&control.overflow_entries.to_le_bytes());
    memory.write(offset, &bytes);
}

pub(crate) fn write_mutation_epoch<M: Memory>(memory: &M, offset: u64, epoch: u64) {
    write_u64(memory, offset + MUTATION_EPOCH_OFFSET, epoch);
}

pub(crate) fn publish_split<M: Memory>(
    memory: &M,
    offset: u64,
    physical_buckets: u64,
    len: u64,
    split_debt: u64,
    overflow_entries: u64,
) {
    let mut prefix = [0; 16];
    prefix[..8].copy_from_slice(&len.to_le_bytes());
    prefix[8..].copy_from_slice(&physical_buckets.to_le_bytes());
    memory.write(offset, &prefix);
    let mut suffix = [0; 16];
    suffix[..8].copy_from_slice(&split_debt.to_le_bytes());
    suffix[8..].copy_from_slice(&overflow_entries.to_le_bytes());
    memory.write(offset + SPLIT_DEBT_OFFSET, &suffix);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_geometry_yields_level_and_residual_cursor() {
        assert_eq!(derive_geometry(0), (0, 0));
        for buckets in 1u64..=4096 {
            let (level, split_cursor) = derive_geometry(buckets);
            assert_eq!(
                u64::from(level),
                63 - u64::from(buckets.leading_zeros()),
                "pb {buckets}"
            );
            assert_eq!(split_cursor, buckets - (1u64 << level), "pb {buckets}");
            assert!(split_cursor < (1u64 << level), "pb {buckets}");
        }
        assert_eq!(derive_geometry(u64::MAX), (63, (1u64 << 63) - 1));
    }
}
