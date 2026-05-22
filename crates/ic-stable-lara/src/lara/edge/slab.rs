//! EdgeStore `slab` implementation.

use crate::{GrowFailed, traits::CsrEdge};
use ic_stable_structures::Memory;

use super::{EdgeStore, INLINE_EDGE_BYTES};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub fn read_slot(&self, slot: u64) -> E {
        if E::BYTES <= 8 {
            let mut buf = [0u8; 8];
            self.edges.read_slot(slot, &mut buf[..E::BYTES]);
            E::read_from(&buf[..E::BYTES])
        } else if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            self.edges.read_slot(slot, &mut buf[..E::BYTES]);
            E::read_from(&buf[..E::BYTES])
        } else {
            let mut buf = vec![0u8; E::BYTES];
            self.edges.read_slot(slot, &mut buf);
            E::read_from(&buf)
        }
    }
    pub(crate) fn read_slots_contiguous(&self, start_slot: u64, out: &mut [u8]) {
        self.edges.read_slots_contiguous(start_slot, out);
    }
    pub(crate) fn write_slots_contiguous(
        &self,
        start_slot: u64,
        bytes: &[u8],
    ) -> Result<(), GrowFailed> {
        self.edges.write_slots_contiguous(start_slot, bytes)
    }
    pub fn write_slot(&self, slot: u64, edge: E) -> Result<(), GrowFailed> {
        if E::BYTES <= 8 {
            let mut buf = [0u8; 8];
            edge.write_to(&mut buf[..E::BYTES]);
            self.edges.write_slot(slot, &buf[..E::BYTES])
        } else if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut buf[..E::BYTES]);
            self.edges.write_slot(slot, &buf[..E::BYTES])
        } else {
            let mut buf = vec![0u8; E::BYTES];
            edge.write_to(&mut buf);
            self.edges.write_slot(slot, &buf)
        }
    }
}
