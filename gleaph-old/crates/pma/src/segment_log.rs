use crate::{layout, memory::Memory};
use gleaph_types::LogEntry;

pub const MAX_LOG_ENTRIES: u32 = layout::MAX_LOG_ENTRIES_PER_SEGMENT as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Handle for a single segment's overflow log storage.
pub struct SegmentLog {
    pub base_offset: u64,
    pub idx_offset: u64,
    pub capacity: u32,
    pub seg_id: u32,
}

impl SegmentLog {
    /// Creates a log handle for the specified segment.
    pub fn for_segment(seg_log_base: u64, seg_id: u32, seg_log_idx_base: u64) -> Self {
        Self {
            base_offset: seg_log_base,
            idx_offset: seg_log_idx_base,
            capacity: MAX_LOG_ENTRIES,
            seg_id,
        }
    }

    /// Returns the current number of entries written into the log.
    pub fn fill_count<M: Memory>(&self, mem: &M) -> u32 {
        layout::read_seg_log_fill(mem, self.idx_offset, self.seg_id)
    }

    /// Returns `true` when no more entries can be appended.
    pub fn is_full<M: Memory>(&self, mem: &M) -> bool {
        self.fill_count(mem) >= self.capacity
    }

    /// Appends an entry and returns its slot index, or `None` if full.
    pub fn append<M: Memory>(&self, mem: &mut M, entry: LogEntry) -> Option<u32> {
        let slot = self.fill_count(mem);
        if slot >= self.capacity {
            return None;
        }
        layout::write_log_entry(mem, self.base_offset, self.seg_id, slot, &entry);
        layout::write_seg_log_fill(mem, self.idx_offset, self.seg_id, slot + 1);
        Some(slot)
    }

    /// Reads a log entry if the requested slot is within the current fill count.
    pub fn read_entry<M: Memory>(&self, mem: &M, slot: u32) -> Option<LogEntry> {
        if slot >= self.fill_count(mem) {
            return None;
        }
        Some(layout::read_log_entry(
            mem,
            self.base_offset,
            self.seg_id,
            slot,
        ))
    }

    /// Overwrites an existing entry in place.
    pub fn overwrite<M: Memory>(&self, mem: &mut M, slot: u32, entry: LogEntry) -> bool {
        if slot >= self.fill_count(mem) {
            return false;
        }
        layout::write_log_entry(mem, self.base_offset, self.seg_id, slot, &entry);
        true
    }

    /// Drains all entries, resets the fill count, and returns the drained payload.
    pub fn drain<M: Memory>(&self, mem: &mut M) -> Vec<LogEntry> {
        let fill = self.fill_count(mem);
        let mut out = Vec::with_capacity(fill as usize);
        for i in 0..fill {
            out.push(layout::read_log_entry(
                mem,
                self.base_offset,
                self.seg_id,
                i,
            ));
            layout::write_log_entry(mem, self.base_offset, self.seg_id, i, &LogEntry::default());
        }
        layout::write_seg_log_fill(mem, self.idx_offset, self.seg_id, 0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{layout, memory::VecMemory};

    #[test]
    fn append_and_drain() {
        let mut mem = VecMemory::with_size(layout::total_memory_needed(4, 16, 2) as usize);
        let base = layout::seg_log_base(4, 16, 2);
        let idx = layout::seg_log_idx_base(4, 16, 2);
        let log = SegmentLog::for_segment(base, 1, idx);

        let a = LogEntry {
            src: 3,
            dst: 4,
            weight: 1.0,
            timestamp: 10,
            prev_offset: -1,
            label_and_flags: 0,
            edge_id: 1,
        };
        let b = LogEntry {
            src: 3,
            dst: 5,
            weight: 2.0,
            timestamp: 20,
            prev_offset: 0,
            label_and_flags: 0,
            edge_id: 2,
        };
        assert_eq!(log.append(&mut mem, a), Some(0));
        assert_eq!(log.append(&mut mem, b), Some(1));
        assert_eq!(log.fill_count(&mem), 2);
        assert_eq!(log.read_entry(&mem, 0), Some(a));
        assert_eq!(log.read_entry(&mem, 1), Some(b));

        let drained = log.drain(&mut mem);
        assert_eq!(drained, vec![a, b]);
        assert_eq!(log.fill_count(&mem), 0);
    }
}
