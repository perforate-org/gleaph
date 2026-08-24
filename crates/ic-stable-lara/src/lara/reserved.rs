//! Shared LARA header reserved-region primitives.
//!
//! Every LARA V1 store header declares a fixed reserved byte region that must
//! stay zero. Writers zero-fill it explicitly ([`write_zeroes`]) so the wire
//! bytes do not depend on freshly allocated stable pages reading as zeros, and
//! open paths reject any nonzero reserved byte ([`region_is_zero`]) so foreign
//! or corrupt layouts fail closed at init instead of opening silently.

use crate::{GrowFailed, safe_write};
use ic_stable_structures::Memory;

/// Widest reserved region declared by any current LARA store header
/// (the vertex header tail, 52 bytes).
const MAX_REGION_BYTES: usize = 64;

const ZERO_REGION: [u8; MAX_REGION_BYTES] = [0u8; MAX_REGION_BYTES];

/// Zero-fills a declared header reserved region, growing stable memory if needed.
pub(crate) fn write_zeroes<M: Memory>(
    memory: &M,
    offset: u64,
    len: usize,
) -> Result<(), GrowFailed> {
    assert!(
        len <= MAX_REGION_BYTES,
        "reserved region exceeds helper buffer"
    );
    safe_write(memory, offset, &ZERO_REGION[..len])
}

/// Returns `true` when every byte of a declared header reserved region reads as zero.
pub(crate) fn region_is_zero<M: Memory>(memory: &M, offset: u64, len: usize) -> bool {
    assert!(
        len <= MAX_REGION_BYTES,
        "reserved region exceeds helper buffer"
    );
    let mut buf = [0u8; MAX_REGION_BYTES];
    memory.read(offset, &mut buf[..len]);
    buf[..len].iter().all(|&byte| byte == 0)
}
