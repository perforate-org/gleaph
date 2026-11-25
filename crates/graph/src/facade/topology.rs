//! The Graph shard's topology: its vertices, edges, their labels and inline values, held by one
//! store. This module is the one owner of that store's state, and the only way to it.
//!
//! ADR 0136 (Decision 11) moves the shard from ic-stable-lara to lara-graph through this module:
//! the store lives here, the rest of the crate reaches it through [`with_graph`] and
//! [`with_graph_mut`], and operations move in here area by area until nothing else imports
//! `ic_stable_lara`. The backend is then replaced behind this module alone.

pub(crate) mod change;
pub(crate) mod counterpart;
pub(crate) mod insert;
pub(crate) mod read;

use core::cell::RefCell;

use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::labeled::DeferredBidirectionalLabeledLaraGraph;

use super::stable::memory::{self, Memory};

/// What a reader receives from the store, named here so that nothing outside this module imports
/// the store for it. They are ic-stable-lara's types until the backend is replaced; this module
/// then defines them with the same fields (the batches' `dense` flag, which describes a slab read,
/// goes with ic-stable-lara).
pub(crate) use ic_stable_lara::DeferredBidirectionalLabeledError as TopologyError;
pub(crate) use ic_stable_lara::labeled::bidirectional::counterpart::CounterpartLookupError;
pub(crate) use ic_stable_lara::labeled::{
    CanonicalEdgeOccurrence, LabeledEdgeInlinePropertyBatch as EdgeInlinePropertyBatch,
    LabeledEdgeInlinePropertyBatchScratch as EdgeInlinePropertyBatchScratch,
    LabeledInlinePropertyValueBatch as InlinePropertyValueBatch,
    LabeledInlinePropertyValueBatchScratch as InlinePropertyValueBatchScratch,
};

/// The store: ic-stable-lara's forward and reverse graphs with their deferred queue.
pub(crate) type StableGraph = DeferredBidirectionalLabeledLaraGraph<Edge, Memory>;

thread_local! {
    static GRAPH: RefCell<StableGraph> = RefCell::new(memory::init_graph());
}

/// Calls `f` with the store, for reading.
pub(crate) fn with_graph<R>(f: impl FnOnce(&StableGraph) -> R) -> R {
    GRAPH.with_borrow(f)
}

/// Calls `f` with the store, for writing.
pub(crate) fn with_graph_mut<R>(f: impl FnOnce(&mut StableGraph) -> R) -> R {
    GRAPH.with_borrow_mut(f)
}
