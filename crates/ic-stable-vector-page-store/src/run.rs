//! Run table entries: shard sharing across contiguous rows.
//!
//! Rows within a run share a shard, so each row stores only its 30-bit vertex id; the shard is
//! recorded once per run in the page's run table. Runs are created at the page tail as rows are
//! appended, and the table is bounded by `run_capacity = min(owned_shards, MAX_RUNS)`.

/// A run table entry: the shared shard id and the number of contiguous rows in the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunEntry {
    /// Shard id shared by every row in the run.
    pub shard_id: u32,
    /// Number of contiguous rows in the run.
    pub run_len: u32,
}

impl RunEntry {
    /// Constructs a run entry.
    pub const fn new(shard_id: u32, run_len: u32) -> Self {
        Self { shard_id, run_len }
    }

    /// On-disk width of a run entry (`u32 + u32`).
    pub const SIZE: usize = 8;

    /// Encodes the entry into its on-disk representation.
    pub fn to_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&self.shard_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.run_len.to_le_bytes());
        out
    }

    /// Decodes an entry from its on-disk representation.
    pub fn from_bytes(bytes: &[u8; 8]) -> Self {
        let mut shard = [0u8; 4];
        shard.copy_from_slice(&bytes[..4]);
        let mut len = [0u8; 4];
        len.copy_from_slice(&bytes[4..8]);
        Self {
            shard_id: u32::from_le_bytes(shard),
            run_len: u32::from_le_bytes(len),
        }
    }
}

/// Reads the run entry at `index` within the run-table byte span, or `None` when out of bounds.
pub fn read_run(bytes: &[u8], index: usize) -> Option<RunEntry> {
    let start = index.checked_mul(RunEntry::SIZE)?;
    let end = start.checked_add(RunEntry::SIZE)?;
    let entry = bytes.get(start..end)?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(entry);
    Some(RunEntry::from_bytes(&buf))
}

/// Writes the run entry at `index` within the run-table byte span; `None` when out of bounds.
pub fn write_run(bytes: &mut [u8], index: usize, entry: RunEntry) -> Option<()> {
    let start = index.checked_mul(RunEntry::SIZE)?;
    let end = start.checked_add(RunEntry::SIZE)?;
    let slot = bytes.get_mut(start..end)?;
    slot.copy_from_slice(&entry.to_bytes());
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_entry_roundtrips() {
        let entry = RunEntry::new(0xDEAD_BEEF, 1024);
        assert_eq!(RunEntry::from_bytes(&entry.to_bytes()), entry);
        assert_eq!(RunEntry::new(0, 0), RunEntry::from_bytes(&[0; 8]));
    }

    #[test]
    fn run_table_read_write_and_bounds() {
        let mut table = vec![0u8; 3 * RunEntry::SIZE];
        assert!(write_run(&mut table, 1, RunEntry::new(5, 64)).is_some());
        assert_eq!(read_run(&table, 1), Some(RunEntry::new(5, 64)));
        assert_eq!(read_run(&table, 0), Some(RunEntry::new(0, 0)));
        assert_eq!(read_run(&table, 3), None);
        assert!(write_run(&mut table, 3, RunEntry::new(1, 1)).is_none());
    }
}
