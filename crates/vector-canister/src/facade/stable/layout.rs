//! Graph-vector canister stable-memory layout registry — ADR 0007 / ADR 0031, see
//! `stable-memory-inventory.md`.

pub use gleaph_graph_kernel::stable_layout::VECTOR_INDEX_STABLE_LAYOUT;

/// Stable region count for this canister (ADR 0007 / ADR 0064 §7). Counts allocated stores only;
/// the registry keeps 15 numbered slots with MemoryIds 8/11 as explicit `Unallocated` holes.
#[allow(dead_code)]
pub const STABLE_REGION_COUNT: usize = VECTOR_INDEX_STABLE_LAYOUT.allocated_region_count();

#[cfg(test)]
mod tests {
    use super::{STABLE_REGION_COUNT, VECTOR_INDEX_STABLE_LAYOUT};
    use gleaph_graph_kernel::stable_layout::{validate_class_invariants, validate_layout};

    #[test]
    fn vector_canister_layout_registry() {
        validate_layout(&VECTOR_INDEX_STABLE_LAYOUT).expect("vector-index layout invariants");
        validate_class_invariants(&VECTOR_INDEX_STABLE_LAYOUT).expect("class invariants");
        assert_eq!(STABLE_REGION_COUNT, 13, "13 allocated stores");
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.region_count(),
            15,
            "15 numbered slots with 8/11 unallocated"
        );
    }
}
