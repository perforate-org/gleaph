//! Common traversal contracts shared by LARA graph implementations.
//!
//! Storage-specific iterators remain owned by their storage module. This module owns the
//! control-flow contract and the typed request boundary used by normal, labeled, bidirectional,
//! and deferred traversal adapters.

pub mod iter;

use std::ops::ControlFlow;

use crate::traits::CsrEdge;

/// Bucket-local logical edge position shared by traversal and Graph edge handles.
///
/// This is a tombstone-inclusive position within one `(owner, label)` row. It is
/// not a physical slab address and carries no orientation; forward/reverse
/// ownership is represented by the surrounding request or occurrence type.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BucketEntryPosition(u32);

impl BucketEntryPosition {
    /// Constructs a position from its raw row-local index.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Constructs a position from its raw row-local index.
    ///
    /// This spelling is retained for edge metadata callers while the canonical
    /// traversal spelling remains [`Self::new`].
    pub const fn from_raw(raw: u32) -> Self {
        Self::new(raw)
    }

    /// Returns the row-local index.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Returns the little-endian representation used by Graph/federation keys.
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Constructs a position from its little-endian representation.
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
}

impl From<u32> for BucketEntryPosition {
    fn from(raw: u32) -> Self {
        Self::new(raw)
    }
}

impl From<BucketEntryPosition> for u32 {
    fn from(position: BucketEntryPosition) -> Self {
        position.raw()
    }
}

impl From<BucketEntryPosition> for u64 {
    fn from(position: BucketEntryPosition) -> Self {
        u64::from(position.raw())
    }
}

/// Logical state returned by a point read.
#[derive(Debug)]
pub enum EdgeSlotState<E> {
    /// No logical slot exists at the requested address.
    Missing,
    /// The logical slot contains a tombstone.
    Tombstone,
    /// The logical slot contains a live edge.
    Live(E),
}

/// Logical order requested for an edge traversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraversalOrder {
    /// Visit the lowest logical slot first.
    Ascending,
    /// Visit the highest logical slot first.
    Descending,
}

/// Execution-time window applied to a logical edge traversal.
///
/// `offset` counts matching live edges in the request's order, while `limit` bounds the number
/// delivered to the visitor. `None` means there is no upper bound. The window is intentionally
/// separate from [`TraversalRequest`]: a request identifies the logical edge set and its order,
/// whereas a window is a reusable execution/paging control and must not affect point-read or
/// replay identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraversalWindow {
    /// Number of matching live edges to skip before invoking the visitor.
    pub offset: u32,
    /// Maximum number of matching live edges to deliver, or `None` for unbounded traversal.
    pub limit: Option<u32>,
}

impl TraversalWindow {
    /// Creates a window with the supplied offset and optional limit.
    pub const fn new(offset: u32, limit: Option<u32>) -> Self {
        Self { offset, limit }
    }

    /// Creates an unbounded window starting at the first matching edge.
    pub const fn unbounded() -> Self {
        Self::new(0, None)
    }

    /// Returns whether this window can deliver any edge.
    pub const fn is_empty(self) -> bool {
        matches!(self.limit, Some(0))
    }
}

#[derive(Debug)]
enum WindowBreak<B> {
    LimitReached,
    Visitor(B),
}

fn window_visitor<S, T, B>(
    window: TraversalWindow,
    mut visit: impl FnMut(S, T) -> ControlFlow<B>,
) -> impl FnMut(S, T) -> ControlFlow<WindowBreak<B>> {
    let mut offset = window.offset;
    let mut remaining = window.limit;
    move |slot, edge| {
        if offset != 0 {
            offset -= 1;
            return ControlFlow::Continue(());
        }
        match visit(slot, edge) {
            ControlFlow::Break(value) => ControlFlow::Break(WindowBreak::Visitor(value)),
            ControlFlow::Continue(()) => match remaining.as_mut() {
                Some(remaining) => {
                    *remaining -= 1;
                    if *remaining == 0 {
                        ControlFlow::Break(WindowBreak::LimitReached)
                    } else {
                        ControlFlow::Continue(())
                    }
                }
                None => ControlFlow::Continue(()),
            },
        }
    }
}

fn finish_window<B, E>(
    result: Result<ControlFlow<WindowBreak<B>>, E>,
) -> Result<ControlFlow<B>, E> {
    match result? {
        ControlFlow::Continue(()) | ControlFlow::Break(WindowBreak::LimitReached) => {
            Ok(ControlFlow::Continue(()))
        }
        ControlFlow::Break(WindowBreak::Visitor(value)) => Ok(ControlFlow::Break(value)),
    }
}

/// Request-side contract shared by traversal implementations.
///
/// Implementations use an associated slot and error type because labeled traversal addresses a
/// bucket-local logical slot while ordinary traversal may use a different slot representation.
/// Domain-specific fields such as owner, label, direction, selected slots, and replay capability
/// belong to the concrete request type.
pub trait TraversalRequest {
    /// Slot type delivered to the visitor.
    type Slot;
    /// Error type returned by the traversal backend.
    type Error;

    /// Returns the canonical order requested by this request.
    fn order(&self) -> TraversalOrder;
}

/// Logical read surface implemented by a concrete LARA traversal backend.
pub trait Traversal {
    /// Request-side parameters for one logical traversal operation.
    type Request: TraversalRequest<Slot = Self::Slot, Error = Self::Error>;
    /// Logical slot address emitted by point reads and visitors.
    type Slot: Copy;
    /// Decoded edge record delivered to callers.
    type Edge: CsrEdge;
    /// Backend-specific state preserving missing, tombstone, and live outcomes.
    type EdgeState;
    /// Backend-specific wrapper containing an edge and its exact inline-property bytes.
    type EdgeWithInlineProperty;
    /// Replay or scratch context accepted by selected-slot traversal.
    type Replay;
    /// Error returned when logical storage validation or decoding fails.
    type Error;

    /// Reads the live edge at `slot`; missing and tombstoned slots return `Ok(None)`.
    fn read_edge(
        &self,
        request: &Self::Request,
        slot: Self::Slot,
    ) -> Result<Option<Self::Edge>, Self::Error>;

    /// Reads `slot` while preserving the distinction between missing, tombstoned, and live state.
    fn read_edge_state(
        &self,
        request: &Self::Request,
        slot: Self::Slot,
    ) -> Result<Self::EdgeState, Self::Error>;

    /// Reads a live edge and attaches the exact inline-property bytes for that logical row.
    fn read_edge_with_inline_property(
        &self,
        request: &Self::Request,
        slot: Self::Slot,
    ) -> Result<Option<Self::EdgeWithInlineProperty>, Self::Error>;

    /// Visits every matching live edge in the request order.
    ///
    /// Returning `ControlFlow::Break` stops the underlying scan immediately and propagates the
    /// break value through the outer `Result`.
    fn visit_edges<B>(
        &self,
        request: &Self::Request,
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error>;

    /// Visits a bounded window of matching live edges in the request order.
    ///
    /// The default implementation delegates to [`Self::visit_edges`]. Implementations with a
    /// storage iterator that can skip efficiently may override this method without changing the
    /// logical contract. Reaching `limit` is normal completion; a visitor break is propagated.
    fn visit_edges_window<B>(
        &self,
        request: &Self::Request,
        window: TraversalWindow,
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        if window.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }
        let mut visitor = window_visitor(window, visit);
        finish_window(self.visit_edges(request, &mut visitor))
    }

    /// Visits every matching live edge with its exact inline-property bytes.
    ///
    /// The visitor receives the same logical slots and ordering as [`Self::visit_edges`].
    fn visit_edges_with_inline_property<B>(
        &self,
        request: &Self::Request,
        visit: impl FnMut(Self::Slot, Self::EdgeWithInlineProperty) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error>;

    /// Visits a bounded window of matching live edges with exact inline-property bytes.
    ///
    /// The window counts live edges in the same order as
    /// [`Self::visit_edges_with_inline_property`].
    fn visit_edges_with_inline_property_window<B>(
        &self,
        request: &Self::Request,
        window: TraversalWindow,
        visit: impl FnMut(Self::Slot, Self::EdgeWithInlineProperty) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        if window.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }
        let mut visitor = window_visitor(window, visit);
        finish_window(self.visit_edges_with_inline_property(request, &mut visitor))
    }

    /// Visits only the requested logical slots, normalized according to the request order.
    ///
    /// Missing and tombstoned selected slots are not emitted as live edges.
    fn visit_edges_at<B>(
        &self,
        request: &Self::Request,
        slots: &[Self::Slot],
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error>;

    /// Visits selected logical slots while optionally reusing the backend replay/scratch context.
    ///
    /// An incompatible or stale replay must be rejected or safely ignored by the implementation;
    /// it must never change the logical result.
    fn visit_edges_at_with_replay<B>(
        &self,
        request: &Self::Request,
        slots: &[Self::Slot],
        replay: Option<&Self::Replay>,
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error>;
}
