//! Router canister stable-memory layout registry — see ADR 0007 and `stable-memory-inventory.md`.

pub use gleaph_graph_kernel::stable_layout::ROUTER_STABLE_LAYOUT;

/// Stable region count for this canister (ADR 0007 baseline).
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "layout baseline constant; asserted in layout registry tests"
    )
)]
pub const STABLE_REGION_COUNT: usize = ROUTER_STABLE_LAYOUT.region_count();

#[cfg(test)]
mod tests {
    use super::{ROUTER_STABLE_LAYOUT, STABLE_REGION_COUNT};
    use gleaph_graph_kernel::stable_layout::validate_layout;

    #[test]
    fn router_canister_layout_registry() {
        validate_layout(&ROUTER_STABLE_LAYOUT).expect("router layout invariants");
        assert_eq!(STABLE_REGION_COUNT, 30);
    }
}
