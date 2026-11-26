//! What the store's operations fail with, in Gleaph's vocabulary: lara-graph's errors are mapped
//! here, so nothing outside `topology` names them.

use core::fmt;

use gleaph_graph_kernel::entry::{TaggedEdgeLabelId, VertexId};
use lara_graph::{Adjacency, Error};

use super::{CanonicalEdgeOccurrence, Store, tagged, vertex_of};

/// A store operation's failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopologyError {
    /// No vertex has this id: `len` vertices exist.
    VertexOutOfRange { vid: VertexId, len: u32 },
    /// The vertex is deleted.
    VertexDeleted(VertexId),
    /// The vertex is being deleted (DETACH DELETE) and takes no new edges.
    VertexDetaching(VertexId),
    /// A detach step was asked of a vertex that is not being deleted.
    VertexNotDetaching(VertexId),
    /// The vertex has the most label buckets one node can hold.
    TooManyLabelBuckets(VertexId),
    /// The label already has inline values of another width.
    WidthConflict { label: u16, declared: u16 },
    /// An edge's inline value does not have its label's width.
    WrongValueLength {
        label: u16,
        expected: u16,
        actual: usize,
    },
    /// The bucket holds values of an older layout of its label and takes no write until migrated.
    StaleValues {
        vertex_id: VertexId,
        label: TaggedEdgeLabelId,
    },
    /// Stable memory could not grow.
    OutOfMemory,
    /// The store's address space is used up.
    AddressSpaceExhausted,
}

impl TopologyError {
    /// The store's `error`, with the vertex count the store holds for an unknown vertex.
    pub(super) fn from_store(graph: &Store, error: Error) -> Self {
        match error {
            Error::UnknownNode(node) => Self::VertexOutOfRange {
                vid: vertex_of(node),
                len: graph.node_count(),
            },
            Error::Deleted(node) => Self::VertexDeleted(vertex_of(node)),
            Error::Deleting(node) => Self::VertexDetaching(vertex_of(node)),
            Error::NotDeleting(node) => Self::VertexNotDetaching(vertex_of(node)),
            Error::TooManyLabelBuckets(node) => Self::TooManyLabelBuckets(vertex_of(node)),
            Error::WidthConflict { label, declared } => Self::WidthConflict {
                label: label.get(),
                declared: declared.get(),
            },
            Error::WrongValueLength {
                label,
                expected,
                actual,
            } => Self::WrongValueLength {
                label: label.get(),
                expected,
                actual,
            },
            Error::StaleValues {
                node,
                label,
                direction,
            } => Self::StaleValues {
                vertex_id: vertex_of(node),
                label: tagged(Adjacency::new(label, direction)).0,
            },
            Error::OutOfMemory(_) => Self::OutOfMemory,
            Error::AddressSpaceExhausted => Self::AddressSpaceExhausted,
            // No proxy is added before sharding (ADR 0136, Decision 10), the store always has its
            // proxies memory, and `NotEmpty` answers operations this module never calls.
            error @ (Error::Proxy(_) | Error::NoProxiesMemory | Error::NotEmpty) => {
                unreachable!("topology does not call what fails with {error:?}")
            }
        }
    }
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} is out of range ({len} vertices)")
            }
            Self::VertexDeleted(vid) => write!(f, "vertex {vid} is deleted"),
            Self::VertexDetaching(vid) => write!(f, "vertex {vid} is being deleted"),
            Self::VertexNotDetaching(vid) => write!(f, "vertex {vid} is not being deleted"),
            Self::TooManyLabelBuckets(vid) => {
                write!(
                    f,
                    "vertex {vid} has the most edge label buckets a vertex can hold"
                )
            }
            Self::WidthConflict { label, declared } => write!(
                f,
                "edge label {label} already has inline values of {declared} bytes"
            ),
            Self::WrongValueLength {
                label,
                expected,
                actual,
            } => write!(
                f,
                "edge label {label} takes inline values of {expected} bytes, not {actual}"
            ),
            Self::StaleValues { vertex_id, label } => write!(
                f,
                "vertex {vertex_id}'s edge label {label} bucket holds values of an older layout"
            ),
            Self::OutOfMemory => f.write_str("stable memory could not grow"),
            Self::AddressSpaceExhausted => f.write_str("the topology store is full"),
        }
    }
}

impl std::error::Error for TopologyError {}

/// Why the other half of an edge, or its canonical half, was not found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CounterpartLookupError {
    /// The occurrence names no live edge: a hole, a position past the bucket, or no bucket.
    SourceNotFound(CanonicalEdgeOccurrence),
    /// The store read failed.
    ReadFailed(TopologyError),
}

impl fmt::Display for CounterpartLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceNotFound(edge) => write!(f, "source edge is not present: {edge:?}"),
            Self::ReadFailed(error) => write!(f, "counterpart lookup failed: {error}"),
        }
    }
}

impl std::error::Error for CounterpartLookupError {}
