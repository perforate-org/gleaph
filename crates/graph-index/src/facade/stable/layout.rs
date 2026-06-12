//! Graph-index canister stable-memory layout registry — see ADR 0007 and `stable-memory-inventory.md`.

pub use gleaph_graph_kernel::stable_layout::INDEX_STABLE_LAYOUT;

/// Stable region count for this canister (ADR 0007 baseline).
pub const STABLE_REGION_COUNT: usize = INDEX_STABLE_LAYOUT.region_count();

#[cfg(test)]
mod tests {
    use super::{INDEX_STABLE_LAYOUT, STABLE_REGION_COUNT};
    use gleaph_graph_kernel::stable_layout::validate_layout;

    #[test]
    fn graph_index_canister_layout_registry() {
        validate_layout(&INDEX_STABLE_LAYOUT).expect("graph-index layout invariants");
        assert_eq!(STABLE_REGION_COUNT, 5);
    }
}
