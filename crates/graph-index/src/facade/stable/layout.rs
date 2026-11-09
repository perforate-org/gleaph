//! Graph-index canister stable-memory layout registry — see ADR 0007 and `stable-memory-inventory.md`.

#[cfg(test)]
mod tests {
    use gleaph_graph_kernel::stable_layout::{INDEX_STABLE_LAYOUT, validate_layout};

    #[test]
    fn graph_index_canister_layout_registry() {
        validate_layout(&INDEX_STABLE_LAYOUT).expect("graph-index layout invariants");
        // ADR 0007 baseline: the count is asserted from the layout itself rather than through a
        // constant that only restates it.
        assert_eq!(INDEX_STABLE_LAYOUT.region_count(), 9);
    }
}
