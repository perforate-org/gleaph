//! Graph-vector-index canister stable-memory layout registry — ADR 0007 / ADR 0031, see
//! `stable-memory-inventory.md`.

pub use gleaph_graph_kernel::stable_layout::VECTOR_INDEX_STABLE_LAYOUT;

/// Stable region count for this canister (ADR 0031 Slice 2 baseline).
#[allow(dead_code)]
pub const STABLE_REGION_COUNT: usize = VECTOR_INDEX_STABLE_LAYOUT.region_count();

#[cfg(test)]
mod tests {
    use super::{STABLE_REGION_COUNT, VECTOR_INDEX_STABLE_LAYOUT};
    use gleaph_graph_kernel::stable_layout::{validate_class_invariants, validate_layout};

    #[test]
    fn vector_index_canister_layout_registry() {
        validate_layout(&VECTOR_INDEX_STABLE_LAYOUT).expect("vector-index layout invariants");
        validate_class_invariants(&VECTOR_INDEX_STABLE_LAYOUT).expect("class invariants");
        assert_eq!(STABLE_REGION_COUNT, 14);
    }
}
