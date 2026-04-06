//! Storable slot encoding shared by [crate::base_vec::BaseVec] and [crate::vec_deque::VecDeque].

use crate::memory::{GrowFailed, read_to_vec, read_u32, safe_write};
use crate::storable::{bounds, bytes_to_store_size_bounded};
use crate::types::Address;
use ic_stable_structures::{Memory, Storable};
use std::borrow::{Borrow, Cow};

pub(crate) fn slot_size<T: Storable>() -> u32 {
    let t_bounds = bounds::<T>();
    t_bounds.max_size + bytes_to_store_size_bounded(&t_bounds)
}

pub(crate) fn write_entry_size<M: Memory, T: Storable>(
    memory: &M,
    offset: u64,
    size: u32,
) -> Result<u64, GrowFailed> {
    let t_bounds = bounds::<T>();
    debug_assert!(size <= t_bounds.max_size);

    if t_bounds.is_fixed_size {
        Ok(offset)
    } else if t_bounds.max_size <= u8::MAX as u32 {
        safe_write(memory, offset, &[size as u8; 1])?;
        Ok(offset + 1)
    } else if t_bounds.max_size <= u16::MAX as u32 {
        safe_write(memory, offset, &(size as u16).to_le_bytes())?;
        Ok(offset + 2)
    } else {
        safe_write(memory, offset, &size.to_le_bytes())?;
        Ok(offset + 4)
    }
}

pub(crate) fn read_entry_size<M: Memory, T: Storable>(memory: &M, offset: u64) -> (u64, usize) {
    let t_bounds = bounds::<T>();
    if t_bounds.is_fixed_size {
        (offset, t_bounds.max_size as usize)
    } else if t_bounds.max_size <= u8::MAX as u32 {
        let mut size = [0u8; 1];
        memory.read(offset, &mut size);
        (offset + 1, size[0] as usize)
    } else if t_bounds.max_size <= u16::MAX as u32 {
        let mut size = [0u8; 2];
        memory.read(offset, &mut size);
        (offset + 2, u16::from_le_bytes(size) as usize)
    } else {
        let size = read_u32(memory, Address::from(offset));
        (offset + 4, size as usize)
    }
}

pub(crate) fn read_entry_to<M: Memory, T: Storable>(
    memory: &M,
    slot_start: u64,
    buf: &mut std::vec::Vec<u8>,
) {
    let (data_offset, data_size) = read_entry_size::<M, T>(memory, slot_start);
    read_to_vec(memory, data_offset.into(), buf, data_size);
}

pub(crate) fn read_slot<M: Memory, T: Storable>(memory: &M, slot_start: u64) -> T {
    let mut data = std::vec::Vec::new();
    read_entry_to::<M, T>(memory, slot_start, &mut data);
    T::from_bytes(Cow::Owned(data))
}

pub(crate) fn write_slot<M: Memory, T: Storable>(
    memory: &M,
    slot_start: u64,
    item: &T,
) -> Result<(), GrowFailed> {
    let bytes = item.to_bytes_checked();
    let data_offset = write_entry_size::<M, T>(memory, slot_start, bytes.len() as u32)?;
    safe_write(memory, data_offset, bytes.borrow())?;
    Ok(())
}
