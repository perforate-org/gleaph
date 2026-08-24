//! Uncompressed block-max score table (Block-Max WAND support).
//!
//! One caller-supplied `u32` per logical block of [`LOGICAL_BLOCK_SIZE`] postings — the
//! same 128-doc granularity as FOR's physical blocks, and the agreed logical blocking for
//! EF/PEF lists so per-block comparisons stay fair across encodings. This crate never
//! computes scores: values arrive from the scoring layer above and are stored verbatim.
//!
//! Heap-backed this slice; a stable-memory store will wrap the same table later.

/// Postings per logical block-max entry; matches `enc::FOR_BLOCK_SIZE`.
pub const LOGICAL_BLOCK_SIZE: u32 = 128;

/// Number of blocks a posting list of `list_len` postings occupies.
pub fn logical_block_count(list_len: u32) -> u32 {
    list_len.div_ceil(LOGICAL_BLOCK_SIZE)
}

/// Verbatim per-block maxima for one posting list.
#[derive(Debug)]
pub struct BlockMaxTable {
    list_len: u32,
    values: Vec<u32>,
}

impl BlockMaxTable {
    /// Adopts caller-supplied maxima, enforcing the alignment invariant.
    ///
    /// # Panics
    /// Panics unless exactly [`logical_block_count(list_len)`](logical_block_count)
    /// values are supplied.
    pub fn new(list_len: u32, values: Vec<u32>) -> Self {
        let expected = logical_block_count(list_len);
        assert!(
            values.len() == expected as usize,
            "block-max table needs {expected} entries for {list_len} postings, got {}",
            values.len()
        );
        Self { list_len, values }
    }

    /// Posting-list length the table was built for.
    pub fn list_len(&self) -> u32 {
        self.list_len
    }

    /// Number of blocks (= number of entries).
    pub fn block_count(&self) -> u32 {
        self.values.len() as u32
    }

    /// Stored maximum of `block`.
    ///
    /// # Panics
    /// Panics when `block >= block_count()`.
    pub fn block_max(&self, block: u32) -> u32 {
        *self
            .values
            .get(block as usize)
            .unwrap_or_else(|| panic!("block-max index {} out of range", block))
    }

    /// The raw stored maxima.
    pub fn values(&self) -> &[u32] {
        &self.values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_block_count_covers_boundaries() {
        assert_eq!(logical_block_count(0), 0);
        assert_eq!(logical_block_count(1), 1);
        assert_eq!(logical_block_count(127), 1);
        assert_eq!(logical_block_count(128), 1);
        assert_eq!(logical_block_count(129), 2);
        assert_eq!(logical_block_count(300), 3);
    }

    #[test]
    fn table_stores_caller_values_verbatim() {
        let table = BlockMaxTable::new(300, vec![7, 9, 42]);
        assert_eq!(table.list_len(), 300);
        assert_eq!(table.block_count(), 3);
        assert_eq!(table.values(), &[7, 9, 42]);
        assert_eq!(table.block_max(2), 42);
    }

    #[test]
    #[should_panic(expected = "block-max table needs")]
    fn wrong_entry_count_is_rejected() {
        BlockMaxTable::new(129, vec![1]);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn out_of_range_block_panics() {
        let table = BlockMaxTable::new(10, vec![3]);
        table.block_max(1);
    }
}
