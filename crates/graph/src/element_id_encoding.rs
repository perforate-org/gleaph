use gleaph_graph_kernel::federation::ElementIdEncodingKey;

/// Resolve the element-id encoding key for a synchronous encoding span.
///
/// ADR 0019: production element ids are encoded with the **router-issued, per-graph** key carried in
/// [`crate::gql_execution_context::GqlExecutionContext`]. The key is *owned data* threaded through
/// the query evaluator, the materialization context, and the canonical mutation segment — it is
/// **never** parked in thread-local storage across an `await`.
///
/// Why owned-data threading instead of ambient TLS: a graph canister can host shards of different
/// logical graphs, so two interleaved messages can legitimately carry *different* keys. On the IC
/// another message runs during any inter-canister `await`, so ambient TLS held across an `await`
/// can be overwritten (wrong key) or restored to `None` (missing key) by a sibling message before
/// the suspended message resumes. Carrying the key as a plain value in each message's own future
/// makes element-id encoding immune to that interleaving.
///
/// Host-only unit tests and canbench that do not run through router graph registration fall back to
/// [`ElementIdEncodingKey::host_test_fixture`].
pub(crate) fn resolve_or_host_fixture(key: Option<ElementIdEncodingKey>) -> ElementIdEncodingKey {
    key.unwrap_or_else(|| {
        #[cfg(any(test, feature = "canbench"))]
        {
            ElementIdEncodingKey::host_test_fixture()
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            panic!("element id encoding key must be set before ELEMENT_ID or path encoding");
        }
    })
}
