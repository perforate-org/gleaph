//! Fixture helpers that force a specific bucket geometry.
//!
//! Compiled for the crate's own tests and, under the `test-support` feature, for a dependent
//! crate's tests. It exists because a bucket that carries inline property bytes cannot be given a
//! span from outside: those inserts go through the deferred path, which skips the leaf cascade and
//! spills from the first row, and the maintenance fold resolves `SlackMayBeDropped`, which refuses
//! to grow the leaf for rows that already live in the run. The rows therefore never reach the span
//! a fixture wants to test.

use super::*;
use crate::traits::CsrEdgeTombstone;

/// Folds one bucket's spill run into its slab span, growing the leaf when it must.
///
/// This is the eager `SpanResolutionPolicy::Required` compaction the write path uses, exposed so a
/// fixture can reach a span-resident bucket that holds inline property values.
pub fn force_span_fold<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    bucket_index: u32,
) -> Result<(), LabeledOperationError> {
    graph.compact_vertex_edge_span(vid, bucket_index)
}
