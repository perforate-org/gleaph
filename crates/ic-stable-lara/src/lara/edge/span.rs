//! EdgeStore `span` implementation.

use crate::{GrowFailed, traits::CsrEdge};
use ic_stable_structures::Memory;

use super::EdgeStore;
use super::free_span::FreeSpan;

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub(super) fn spans_overlap(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
        let a_end = a_start.saturating_add(a_len);
        let b_end = b_start.saturating_add(b_len);
        a_start < b_end && b_start < a_end
    }
    pub(crate) fn allocate_span(&self, len: u64) -> Result<u64, GrowFailed> {
        self.allocate_span_avoiding(len, None)
    }
    pub(crate) fn allocate_span_avoiding(
        &self,
        len: u64,
        avoid: Option<(u64, u64)>,
    ) -> Result<u64, GrowFailed> {
        let cap = self.header().elem_capacity;
        if len == 0 {
            return Ok(cap);
        }
        let map_err = |_| GrowFailed {
            current_size: 0,
            delta: 0,
        };
        if let Some(span) = self.free_spans.take_best_fit(len).map_err(map_err)? {
            crate::slab_index::checked_add_slot_exclusive_end(span.start_slot, len).ok_or(
                GrowFailed {
                    current_size: 0,
                    delta: 0,
                },
            )?;
            if let Some((avoid_start, avoid_len)) = avoid {
                if Self::spans_overlap(span.start_slot, len, avoid_start, avoid_len) {
                    self.free_spans
                        .release(FreeSpan {
                            start_slot: span.start_slot,
                            len,
                        })
                        .map_err(map_err)?;
                } else {
                    return Ok(span.start_slot);
                }
            } else {
                return Ok(span.start_slot);
            }
        }

        let start = cap;
        let new_cap =
            crate::slab_index::checked_add_slot_exclusive_end(start, len).ok_or(GrowFailed {
                current_size: 0,
                delta: 0,
            })?;
        self.set_elem_capacity(new_cap)?;
        Ok(start)
    }
    pub(crate) fn release_span(&self, start_slot: u64, len: u64) -> Result<(), GrowFailed> {
        if len > 0 {
            self.free_spans
                .release(FreeSpan { start_slot, len })
                .map_err(|_| GrowFailed {
                    current_size: 0,
                    delta: 0,
                })?;
        }
        Ok(())
    }
}
