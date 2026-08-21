//! Vector canister stable-memory layout registry — ADR 0007 / ADR 0031, see
//! `stable-memory-inventory.md`.

#[cfg(test)]
use gleaph_graph_kernel::stable_layout::VECTOR_INDEX_STABLE_LAYOUT;

#[cfg(test)]
mod tests {
    use super::VECTOR_INDEX_STABLE_LAYOUT;
    use gleaph_graph_kernel::stable_layout::{validate_class_invariants, validate_layout};

    #[test]
    fn vector_canister_layout_registry() {
        validate_layout(&VECTOR_INDEX_STABLE_LAYOUT).expect("vector-canister layout invariants");
        validate_class_invariants(&VECTOR_INDEX_STABLE_LAYOUT).expect("class invariants");
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.allocated_region_count(),
            16,
            "16 allocated stores"
        );
        assert_eq!(
            VECTOR_INDEX_STABLE_LAYOUT.region_count(),
            18,
            "18 numbered slots with 8/11 unallocated"
        );
    }
}
