//! One vertex's bucket lookup, borrowed for one operation.
//!
//! ADR 0100's bucket lookup family is small in code and large in invariant surface: `find_bucket` is one ~300-line
//! implementation with a last-bucket cache, a per-label cache array, degree fast paths and a bulk binary search.
//! What was spread out is the **consequence** of a bucket row changing - the caches that described it are void - which
//! nine sites answered by hand, each with its own `if` about whether the row actually moved.
//!
//! This view is that answer's owner: `resolve` asks the question, `rewrite_row` performs the rewrite and voids the
//! caches in the same operation, taking the "did the row move" bit as an argument rather than leaving each caller to
//! remember it.

use crate::VertexId;
use crate::labeled::bucket_label_key::BucketLabelKey;
use crate::labeled::graph::BucketSearch;
use crate::labeled::graph::error::LabeledOperationError;
use crate::traits::CsrEdge;
use ic_stable_structures::Memory;

use super::LabeledLaraGraph;

/// See the module documentation.
pub(crate) struct BucketLookupPlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
}

impl<'a, E, M> BucketLookupPlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) const fn new(graph: &'a LabeledLaraGraph<E, M>) -> Self {
        Self { graph }
    }

    /// Where `(src, label)`'s bucket is, or where it would go.
    ///
    /// The answer is the same `BucketSearch` the graph has always returned; the plane exists so that the caches
    /// behind it have one owner when they are invalidated.
    pub(crate) fn resolve(
        &self,
        src: VertexId,
        label: BucketLabelKey,
    ) -> Result<BucketSearch, LabeledOperationError> {
        let vertex = self.graph.vertices().get(src);
        self.graph.find_bucket(src, &vertex, label)
    }

    /// Empties `vid`'s bucket row and voids the lookup caches that described it: the rewrite case with no survivor.
    pub(crate) fn clear_row(&self, vid: VertexId) -> Result<(), LabeledOperationError> {
        self.rewrite_row(vid, |graph| {
            graph
                .buckets
                .clear_vertex_label_buckets(&graph.vertices, vid)?;
            Ok(((), true))
        })
    }

    /// Performs a rewrite of `vid`'s bucket row and voids the lookup caches that described it.
    ///
    /// The closure answers "did this change the row the caches search": a rewrite that only touched a descriptor keeps
    /// the caches, which is what the hot insert path relies on. The bit travels with the rewrite instead of coming
    /// back as a caller's `if`, so a writer cannot perform the rewrite and forget the consequence. When the closure
    /// fails nothing is invalidated, because nothing was published either.
    pub(crate) fn rewrite_row<T>(
        &self,
        vid: VertexId,
        rewrite: impl FnOnce(&'a LabeledLaraGraph<E, M>) -> Result<(T, bool), LabeledOperationError>,
    ) -> Result<T, LabeledOperationError> {
        let (published, moved) = rewrite(self.graph)?;
        if moved {
            self.graph
                .invalidate_bucket_lookup_caches_for_bucket_segment(vid)?;
        }
        Ok(published)
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The lookup plane of one graph (ADR 0100: the bucket-lookup family's only door).
    pub(crate) fn bucket_lookup(&self) -> BucketLookupPlane<'_, E, M> {
        BucketLookupPlane::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::EdgePlacementPolicy;
    use crate::labeled::graph::test_support::{TestEdge, test_graph};
    use crate::labeled::record::LabeledVertex;

    type Graph = LabeledLaraGraph<TestEdge, VectorMemory>;

    fn graph() -> Graph {
        test_graph()
    }

    fn label() -> BucketLabelKey {
        BucketLabelKey::directed_from_index(2)
    }

    /// `resolve` is the graph's answer, not a second opinion about it.
    #[test]
    fn resolve_agrees_with_the_lookup_it_owns() {
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        for target in 100..104u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.vertices().get(vid);
        for label in [label(), BucketLabelKey::directed_from_index(9)] {
            let via_plane = graph.bucket_lookup().resolve(vid, label).expect("plane");
            let direct = graph.find_bucket(vid, &vertex, label).expect("direct");
            match (via_plane, direct) {
                (
                    BucketSearch::Found {
                        slot: a,
                        bucket: ba,
                        ..
                    },
                    BucketSearch::Found {
                        slot: b,
                        bucket: bb,
                        ..
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(ba.bucket_label_key(), bb.bucket_label_key());
                }
                (
                    BucketSearch::Missing { insert_index: a },
                    BucketSearch::Missing { insert_index: b },
                ) => assert_eq!(a, b),
                _ => panic!("the two answers disagree"),
            }
        }
    }

    /// A failing rewrite publishes nothing, so it invalidates nothing: the plane hands the error back untouched.
    #[test]
    fn a_failed_rewrite_publishes_and_invalidates_nothing() {
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let outcome: Result<(), LabeledOperationError> =
            graph.bucket_lookup().rewrite_row(vid, |_| {
                Err(LabeledOperationError::from(
                    crate::lara::operation_error::LaraOperationError::overflow(),
                ))
            });
        assert!(
            outcome.is_err(),
            "the closure's failure is the caller's answer"
        );
        // The graph still resolves: a failed rewrite left it alone.
        assert!(
            graph.bucket_lookup().resolve(vid, label()).is_ok(),
            "a failed rewrite must leave the lookup usable"
        );
    }

    /// A successful rewrite hands back the closure's value, which is how a caller gets the refreshed descriptor.
    #[test]
    fn a_successful_rewrite_hands_back_what_it_published() {
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let published = graph
            .bucket_lookup()
            .rewrite_row(vid, |g| Ok((g.vertices().get(vid), true)))
            .expect("rewrite");
        assert_eq!(
            published.degree,
            graph.vertices().get(vid).degree,
            "the plane returns the closure's value"
        );
    }
}
