//! One physical slot: occupied (`T` + generation) or vacant (generation + freelist link).

use crate::memory::{GrowFailed, safe_write};
use crate::slot::{read_slot, read_t_payload_slice, slot_size, write_slot, write_t_payload_slice};
use crate::storable::slot_payload_bytes;
use ic_stable_structures::Memory;
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;

pub(crate) const TAG_OCCUPIED: u8 = 0;
pub(crate) const TAG_VACANT: u8 = 1;

/// Bytes before the `T` payload sub-slot inside one cell.
pub(crate) const SLOT_PREFIX: u64 = 5;

pub(crate) fn slot_cell_size<T: Storable>() -> u32 {
    SLOT_PREFIX as u32 + slot_payload_bytes::<T>()
}

/// First byte of a persisted cell: [`TAG_OCCUPIED`] or [`TAG_VACANT`].
pub(crate) fn read_cell_tag<M: Memory>(memory: &M, cell_offset: u64) -> u8 {
    let mut tag = [0u8; 1];
    memory.read(cell_offset, &mut tag);
    tag[0]
}

pub(crate) enum SlotCell<T: Storable> {
    Occupied { generation: u32, value: T },
    Vacant { generation: u32, next_free: u32 },
}

impl<T: Storable> SlotCell<T> {
    pub(crate) fn read_from_memory<M: Memory>(memory: &M, cell_offset: u64) -> Self {
        let mut tag = [0u8; 1];
        memory.read(cell_offset, &mut tag);
        let mut gen_bytes = [0u8; 4];
        memory.read(cell_offset + 1, &mut gen_bytes);
        let generation = u32::from_le_bytes(gen_bytes);
        match tag[0] {
            TAG_OCCUPIED => {
                let value = read_slot::<M, T>(memory, cell_offset + SLOT_PREFIX);
                Self::Occupied { generation, value }
            }
            TAG_VACANT => {
                let mut n = [0u8; 4];
                memory.read(cell_offset + SLOT_PREFIX, &mut n);
                let next_free = u32::from_le_bytes(n);
                Self::Vacant {
                    generation,
                    next_free,
                }
            }
            _ => Self::Vacant {
                generation: 1,
                next_free: u32::MAX,
            },
        }
    }

    pub(crate) fn write_to_memory<M: Memory>(
        &self,
        memory: &M,
        cell_offset: u64,
    ) -> Result<(), GrowFailed> {
        match self {
            SlotCell::Occupied { generation, value } => {
                safe_write(memory, cell_offset, &[TAG_OCCUPIED])?;
                safe_write(memory, cell_offset + 1, &generation.to_le_bytes())?;
                write_slot::<M, T>(memory, cell_offset + SLOT_PREFIX, value)?;
            }
            SlotCell::Vacant {
                generation,
                next_free,
            } => {
                safe_write(memory, cell_offset, &[TAG_VACANT])?;
                safe_write(memory, cell_offset + 1, &generation.to_le_bytes())?;
                let psz = slot_payload_bytes::<T>() as usize;
                let mut payload = vec![0u8; psz];
                payload[0..4].copy_from_slice(&next_free.to_le_bytes());
                safe_write(memory, cell_offset + SLOT_PREFIX, &payload)?;
            }
        }
        Ok(())
    }
}

impl<T: Storable> Storable for SlotCell<T> {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let sz = slot_cell_size::<T>() as usize;
        let mut v = vec![0u8; sz];
        match self {
            SlotCell::Occupied { generation, value } => {
                v[0] = TAG_OCCUPIED;
                v[1..5].copy_from_slice(&generation.to_le_bytes());
                let ss = slot_size::<T>() as usize;
                write_t_payload_slice::<T>(&mut v[5..5 + ss], value);
            }
            SlotCell::Vacant {
                generation,
                next_free,
            } => {
                v[0] = TAG_VACANT;
                v[1..5].copy_from_slice(&generation.to_le_bytes());
                v[5..9].copy_from_slice(&next_free.to_le_bytes());
            }
        }
        Cow::Owned(v)
    }

    fn into_bytes(self) -> std::vec::Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        let expect = slot_cell_size::<T>() as usize;
        assert!(
            b.len() >= expect,
            "SlotCell: expected at least {} bytes",
            expect
        );
        match b[0] {
            TAG_OCCUPIED => {
                let generation = u32::from_le_bytes(b[1..5].try_into().unwrap());
                let ss = slot_size::<T>() as usize;
                let value = read_t_payload_slice::<T>(&b[5..5 + ss]);
                Self::Occupied { generation, value }
            }
            _ => {
                let generation = u32::from_le_bytes(b[1..5].try_into().unwrap());
                let next_free = u32::from_le_bytes(b[5..9].try_into().unwrap());
                Self::Vacant {
                    generation,
                    next_free,
                }
            }
        }
    }

    const BOUND: Bound = Bound::Bounded {
        max_size: (SLOT_PREFIX as u32).saturating_add(slot_payload_bytes::<T>()),
        is_fixed_size: true,
    };
}
