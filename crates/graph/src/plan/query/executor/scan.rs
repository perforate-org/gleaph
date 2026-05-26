//! Vertex/index scan operators and limit+offset streaming execution.

use crate::plan::query::row::PlanRow;

pub(crate) const LIMITED_STREAMING_REMOTE_EXPAND_SOURCE: &str =
    "LimitedStreamingPrefix.remote_expand_source";

#[cfg(test)]
mod test_counters {
    use std::cell::Cell;

    thread_local! {
        pub(crate) static NODE_SCAN_VISITS: Cell<usize> = const { Cell::new(0) };
        pub(crate) static EDGE_STREAM_VISITS: Cell<usize> = const { Cell::new(0) };
    }
}

#[cfg(test)]
pub(crate) use test_counters::{EDGE_STREAM_VISITS, NODE_SCAN_VISITS};

pub(crate) struct LimitedStreamingPrefixResult {
    pub(crate) rows: Vec<PlanRow>,
    pub(crate) clears_active_aggregate: bool,
}

mod index;
mod streaming;

pub(crate) use index::{
    execute_conditional_index_scan, execute_index_intersection, execute_index_scan,
    execute_node_scan, federation_routing, resolve_scan_value_bytes,
};
pub(crate) use streaming::{execute_limited_streaming_prefix, limited_streaming_prefix_limit_idx};

#[cfg(test)]
mod tests;
