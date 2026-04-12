//! PMA **stable root** footer: authoritative [`RegionManager`] snapshot stored at the **tail**
//! of the graph [`Memory`], replacing a separate canister `StableCell` for allocator metadata.
//!
//! ## v1 layout (little-endian, from low to high address)
//!
//! ```text
//! [ ... graph bytes ... ][ candid(RegionManager) payload ][ 20-byte header at tail ]
//! ```
//!
//! The **header** (last 20 bytes of stable memory) is:
//! - `magic: [u8; 8]` — [`PMA_ROOT_MAGIC`]
//! - `format_version: u32` — [`PMA_ROOT_FORMAT_VERSION`]
//! - `payload_len: u64` — byte length of the candid payload immediately preceding the header
//!
//! The footer must start at a byte offset `>= RegionManager::next_extent_addr` so it never
//! overlaps graph data. Future versions may replace candid with a structured directory (see
//! incremental stable persistence design).

use ic_stable_structures::Memory;

use super::hydration::{HydrationError, WritebackError};
use super::manager::RegionManager;

/// Magic bytes for [`PMA_ROOT_FORMAT_VERSION`] = 1.
pub const PMA_ROOT_MAGIC: &[u8; 8] = b"GLEPMA01";

/// Supported on-disk format for the tail footer.
pub const PMA_ROOT_FORMAT_VERSION: u32 = 1;

const HEADER_LEN: u64 = 8 + 4 + 8;
const WASM_PAGE_SIZE: u64 = 65_536;

fn stable_byte_len<M: Memory>(memory: &M) -> Result<u64, HydrationError> {
    memory
        .size()
        .checked_mul(WASM_PAGE_SIZE)
        .ok_or_else(|| HydrationError::PmaStableRoot("stable byte length overflow".into()))
}

fn ensure_stable_covers(
    memory: &impl Memory,
    last_byte_exclusive: u64,
) -> Result<(), WritebackError> {
    let current_pages = memory.size();
    let current_bytes = current_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("address space overflow");
    if current_bytes >= last_byte_exclusive {
        return Ok(());
    }
    let missing_bytes = last_byte_exclusive - current_bytes;
    let delta_pages = missing_bytes.div_ceil(WASM_PAGE_SIZE);
    if memory.grow(delta_pages) == -1 {
        return Err(WritebackError::MemoryGrowFailed {
            current_pages,
            delta_pages,
        });
    }
    Ok(())
}

/// Read a v1 [`RegionManager`] from the graph memory tail, if the magic matches.
///
/// Returns `Ok(None)` when stable memory is too small or the tail is not a PMA root.
pub fn try_read_region_manager<M: Memory>(
    memory: &M,
) -> Result<Option<RegionManager>, HydrationError> {
    let total = stable_byte_len(memory)?;
    if total < HEADER_LEN {
        return Ok(None);
    }
    let mut hdr = [0u8; HEADER_LEN as usize];
    memory.read(total - HEADER_LEN, &mut hdr);
    if hdr[0..8] != PMA_ROOT_MAGIC[..] {
        return Ok(None);
    }
    let format_version = u32::from_le_bytes(hdr[8..12].try_into().expect("u32"));
    if format_version != PMA_ROOT_FORMAT_VERSION {
        return Err(HydrationError::PmaStableRoot(format!(
            "unsupported PMA root format version {format_version} (expected {})",
            PMA_ROOT_FORMAT_VERSION
        )));
    }
    let payload_len = u64::from_le_bytes(hdr[12..20].try_into().expect("u64")) as usize;
    let footer_len = HEADER_LEN.saturating_add(payload_len as u64);
    if total < footer_len {
        return Err(HydrationError::PmaStableRoot(
            "truncated PMA root (payload extends before stable start)".into(),
        ));
    }
    let mut payload = vec![0u8; payload_len];
    memory.read(total - footer_len, &mut payload);
    let rm: RegionManager = candid::decode_one(&payload).map_err(|e| {
        HydrationError::PmaStableRoot(format!("region manager candid decode failed: {e}"))
    })?;
    let root_start = total - footer_len;
    if root_start < rm.next_extent_addr.0 {
        return Err(HydrationError::PmaStableRoot(format!(
            "PMA root overlaps graph data: root starts at {root_start} but next_extent_addr={}",
            rm.next_extent_addr.0
        )));
    }
    Ok(Some(rm))
}

/// Write (or replace) the v1 tail footer for `rm`, growing stable memory as needed.
pub fn write_region_manager_footer<M: Memory>(
    memory: &M,
    rm: &RegionManager,
) -> Result<(), WritebackError> {
    let payload = candid::encode_one(rm).map_err(|e| {
        WritebackError::PmaStableRoot(format!("region manager candid encode failed: {e}"))
    })?;
    let payload_len = payload.len();
    let footer_len = HEADER_LEN
        .checked_add(payload_len as u64)
        .ok_or_else(|| WritebackError::PmaStableRoot("PMA root footer length overflow".into()))?;
    let min_bytes = rm
        .next_extent_addr
        .0
        .checked_add(footer_len)
        .ok_or_else(|| WritebackError::PmaStableRoot("PMA root placement overflow".into()))?;
    ensure_stable_covers(memory, min_bytes)?;
    let total = memory
        .size()
        .checked_mul(WASM_PAGE_SIZE)
        .expect("address space overflow");
    let root_start = total - footer_len;
    if root_start < rm.next_extent_addr.0 {
        return Err(WritebackError::PmaStableRoot(format!(
            "internal error: PMA root at {root_start} overlaps next_extent_addr={}",
            rm.next_extent_addr.0
        )));
    }
    memory.write(root_start, &payload);
    let mut hdr = [0u8; HEADER_LEN as usize];
    hdr[0..8].copy_from_slice(PMA_ROOT_MAGIC);
    hdr[8..12].copy_from_slice(&PMA_ROOT_FORMAT_VERSION.to_le_bytes());
    hdr[12..20].copy_from_slice(&(payload_len as u64).to_le_bytes());
    memory.write(total - HEADER_LEN, &hdr);
    Ok(())
}

/// Decode a [`RegionManager`] for hydration: prefer the tail footer on `memory`, otherwise legacy
/// candid bytes (pre-footer canisters used a separate `StableCell`).
pub fn decode_region_manager_for_hydrate<M: Memory>(
    memory: &M,
    legacy_candid: Option<&[u8]>,
) -> Result<RegionManager, HydrationError> {
    if let Some(m) = try_read_region_manager(memory)? {
        return Ok(m);
    }
    let Some(bytes) = legacy_candid.filter(|b| !b.is_empty()) else {
        return Err(HydrationError::PmaStableRoot(
            "missing PMA stable root footer and no legacy region manager candid".into(),
        ));
    };
    candid::decode_one(bytes).map_err(|e| {
        HydrationError::PmaStableRoot(format!("legacy region manager candid decode failed: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::low_level::{
        BucketSizeInPages, ExtentChain, ExtentId, RegionKind, RegionManager, WasmPages,
    };
    use ic_stable_structures::VectorMemory;

    #[test]
    fn footer_round_trips_region_manager() {
        let mem = VectorMemory::default();
        let mut rm = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        rm.define_extent_region(
            RegionKind::MaintenanceQueue,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                0,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        write_region_manager_footer(&mem, &rm).expect("write footer");
        let got = try_read_region_manager(&mem).expect("read").expect("some");
        assert_eq!(got, rm);
    }

    #[test]
    fn decode_for_hydrate_prefers_footer_over_legacy() {
        let mem = VectorMemory::default();
        let rm = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        write_region_manager_footer(&mem, &rm).expect("write");
        let legacy = candid::encode_one(&RegionManager::default()).expect("legacy bytes");
        let out = decode_region_manager_for_hydrate(&mem, Some(&legacy)).expect("decode");
        assert_eq!(out, rm);
    }
}
