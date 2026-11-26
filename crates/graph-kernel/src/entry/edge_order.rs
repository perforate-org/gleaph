//! Which side of an edge a read is on, and the order a read walks a vertex's edges in.

/// The side of a directed edge: its source's (forward, out-adjacency) or its target's (reverse,
/// in-adjacency). An undirected edge has a half at each end, read from either side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeOrientation {
    Forward,
    Reverse,
}

/// The order a read walks a vertex's edges in: labels and positions ascending, or both reversed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EdgeOrder {
    #[default]
    Ascending,
    Descending,
}
