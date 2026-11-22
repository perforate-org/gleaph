//! Router API error type (Candid-exported).

pub use gleaph_graph_kernel::federation::{RouterError, VectorActivationBlockReason};

#[cfg(test)]
mod wire_mirror_tests {
    use candid::CandidType;

    /// `gleaph-router-wire` keeps a hand-written copy of [`super::RouterError`] so client SDKs do not
    /// depend on `gleaph-graph-kernel`. The two must describe the same Candid type, or a client
    /// cannot decode a variant the Router sends (as happened when `AckConflict` was removed from
    /// one side only).
    #[test]
    fn router_wire_router_error_has_the_router_candid_type() {
        assert_eq!(
            gleaph_router_wire::RouterError::ty(),
            super::RouterError::ty(),
            "gleaph_router_wire::RouterError must mirror gleaph_graph_kernel::federation::RouterError"
        );
    }
}
