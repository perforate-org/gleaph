use ic_stable_structures::Memory;

pub(crate) const WASM_PAGE_SIZE: u64 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrowError {
    OutOfMemory,
    CapacityOverflow,
}

pub(crate) fn grow_to_bytes<M: Memory>(memory: &M, bytes: u64) -> Result<(), GrowError> {
    let current = memory
        .size()
        .checked_mul(WASM_PAGE_SIZE)
        .ok_or(GrowError::CapacityOverflow)?;
    if current >= bytes {
        return Ok(());
    }
    let delta = bytes
        .checked_sub(current)
        .and_then(|value| value.checked_add(WASM_PAGE_SIZE - 1))
        .ok_or(GrowError::CapacityOverflow)?
        / WASM_PAGE_SIZE;
    if memory.grow(delta) == -1 {
        Err(GrowError::OutOfMemory)
    } else {
        Ok(())
    }
}

pub(crate) fn write_u64<M: Memory>(memory: &M, offset: u64, value: u64) {
    memory.write(offset, &value.to_le_bytes());
}
