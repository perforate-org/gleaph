//! Re-export of stable edge equality postings (see [`crate::facade::edge_equality_index`]).

pub(crate) use crate::facade::edge_equality_index::{
    EdgeEqualityPosting, lookup_equal, record_edge_property_change, remove_all_for_edge,
};
