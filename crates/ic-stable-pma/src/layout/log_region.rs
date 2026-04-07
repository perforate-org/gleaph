//! V1 layout for the **`M_l` journal region** (append-only stream). Named “DGAP-style” in project docs
//! for the *pattern* (log-structured updates), not for binary compatibility with the C++ DGAP reference
//! or with `ic-stable-structures::log::Log` (`GLI`/`GLD`).
//!
//! # V1 header ([`LOG_REGION_VERSION`])
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "DGL"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Reserved                              ↕ 4 bytes
//! --------------------------------------------------
//! append_tail (u64 LE)                  ↕ 8 bytes   (next record starts here; ≥ [`LOG_HEADER_SIZE`])
//! --------------------------------------------------
//! record_count (u64 LE)                 ↕ 8 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 40 bytes
//! -------------------------------------------------- <- Address 64 ([`LOG_HEADER_SIZE`])
//! Record 0: u32 len LE + payload[len]   ↕ 4 + len₀ bytes
//! --------------------------------------------------
//! Record 1: u32 len LE + payload[len]   ↕ 4 + len₁ bytes
//! --------------------------------------------------
//! …
//! --------------------------------------------------
//! Unallocated space
//! ```

use ic_stable_structures::Memory;

use crate::memory_util::{read_u64_le, safe_write};

pub const LOG_REGION_MAGIC: &[u8; 3] = b"DGL";
pub const LOG_REGION_VERSION: u8 = 1;
pub const LOG_HEADER_SIZE: u64 = 64;

const OFFSET_APPEND_TAIL: u64 = 8;
const OFFSET_RECORD_COUNT: u64 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogRegionHeaderV1 {
    /// Byte offset in `M_l` where next record starts (>= LOG_HEADER_SIZE).
    pub append_tail: u64,
    pub record_count: u64,
}

impl LogRegionHeaderV1 {
    pub fn read<M: Memory>(memory: &M) -> Option<Self> {
        let mut magic = [0u8; 3];
        memory.read(0, &mut magic);
        if &magic != LOG_REGION_MAGIC {
            return None;
        }
        let mut ver = [0u8; 1];
        memory.read(3, &mut ver);
        if ver[0] != LOG_REGION_VERSION {
            return None;
        }
        Some(Self {
            append_tail: read_u64_le(memory, OFFSET_APPEND_TAIL),
            record_count: read_u64_le(memory, OFFSET_RECORD_COUNT),
        })
    }

    fn write_to_buffer(&self, buf: &mut [u8; LOG_HEADER_SIZE as usize]) {
        buf.fill(0);
        buf[0..3].copy_from_slice(LOG_REGION_MAGIC);
        buf[3] = LOG_REGION_VERSION;
        buf[8..16].copy_from_slice(&self.append_tail.to_le_bytes());
        buf[16..24].copy_from_slice(&self.record_count.to_le_bytes());
    }

    pub fn write<M: Memory>(&self, memory: &M) {
        let mut buf = [0u8; LOG_HEADER_SIZE as usize];
        self.write_to_buffer(&mut buf);
        memory.write(0, &buf);
    }
}

/// Initialize empty log region: tail = `LOG_HEADER_SIZE`, count = 0.
pub fn init_empty_log_region<M: Memory>(memory: &M) -> Result<(), crate::memory_util::GrowFailed> {
    let h = LogRegionHeaderV1 {
        append_tail: LOG_HEADER_SIZE,
        record_count: 0,
    };
    let mut buf = [0u8; LOG_HEADER_SIZE as usize];
    h.write_to_buffer(&mut buf);
    safe_write(memory, 0, &buf)?;
    Ok(())
}

/// Append one record: `u32` length + payload. Updates header tail and count.
pub fn append_record<M: Memory>(
    memory: &M,
    payload: &[u8],
) -> Result<(), crate::memory_util::GrowFailed> {
    let len_u32 = u32::try_from(payload.len()).expect("log record length fits u32");
    let h = LogRegionHeaderV1::read(memory).unwrap_or(LogRegionHeaderV1 {
        append_tail: LOG_HEADER_SIZE,
        record_count: 0,
    });
    let at = h.append_tail;
    let need = 4 + payload.len() as u64;
    safe_write(memory, at, &len_u32.to_le_bytes())?;
    safe_write(memory, at + 4, payload)?;
    let new_h = LogRegionHeaderV1 {
        append_tail: at + need,
        record_count: h.record_count.saturating_add(1),
    };
    let mut buf = [0u8; LOG_HEADER_SIZE as usize];
    new_h.write_to_buffer(&mut buf);
    safe_write(memory, 0, &buf)?;
    Ok(())
}
