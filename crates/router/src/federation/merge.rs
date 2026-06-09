//! Merge partial results from per-shard graph execution.
//!
//! Federation v1 returns row counts from each shard. Full row-batch merge for
//! cross-shard joins and aggregates remains future work (see
//! `design/sharding/federation-target.md`).

/// Sum shard-local row counts for independent query fragments.
pub fn merge_row_counts(shard_row_counts: impl IntoIterator<Item = u64>) -> u64 {
    shard_row_counts
        .into_iter()
        .fold(0u64, |total, rows| total.saturating_add(rows))
}

/// Accumulate one shard's row count into a running total.
#[inline]
pub fn merge_add_row_count(total: u64, shard_rows: u64) -> u64 {
    total.saturating_add(shard_rows)
}

#[cfg(test)]
mod tests {
    use super::{merge_add_row_count, merge_row_counts};

    #[test]
    fn merge_row_counts_saturates_on_overflow() {
        assert_eq!(merge_row_counts([1, 2, 3]), 6);
        assert_eq!(merge_row_counts(std::iter::empty::<u64>()), 0);
        assert_eq!(merge_row_counts([u64::MAX, 1]), u64::MAX,);
    }

    #[test]
    fn merge_add_row_count_matches_fold() {
        let total = merge_add_row_count(merge_add_row_count(0, 4), 9);
        assert_eq!(total, 13);
    }
}
