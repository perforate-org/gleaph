//! Internal one-orientation batch write orchestration for ADR 0045.
//!
//! This module consumes the read-only placement summary produced by
//! [`super::batch_placement`] and turns it into a one-orientation write plan
//! for the LARA labeled-graph layer.  It does not add a public or Candid API.
//!
//! The slice supported here matches Plan 0122:
//! - one orientation only (forward or reverse);
//! - one bounded batch of already-live vertices and already-valid catalog labels;
//! - bucket runs that fit the current clean slab window without overflow-log,
//!   rebalance, relocation, or dynamic leaf expansion;
//! - scalar fallback for everything else.

use std::collections::BTreeMap;

use ic_stable_lara::labeled::batch_write::{
    OneOrientationBatchEdge, OneOrientationBatchPlan, OneOrientationBucketRun,
};
use ic_stable_lara::{CsrEdge, labeled::LabeledOrientation};

use super::GraphStore;
use super::batch_placement::{BatchEdgeIntent, BatchPlacementError, BatchPlacementKey};

/// One-orientation batch write request derived from a placement summary.
///
/// This is the internal wire shape between GraphStore and the LARA labeled-graph
/// layer.  It carries only the physical half-edges for a single orientation;
/// GraphStore keeps the logical-to-physical expansion and orchestrates the
/// forward/reverse/undirected alias pairs at a higher layer.
pub struct OneOrientationBatchWriteRequest<E: CsrEdge> {
    pub orientation: LabeledOrientation,
    pub plan: OneOrientationBatchPlan<E>,
}

impl GraphStore {
    /// Convert physical intents into per-orientation batch write plans.
    ///
    /// This is the bridge between Plan 0121 (read-only placement) and Plan 0122
    /// (one-orientation batch commit).  It groups physical intents by orientation
    /// and bucket run, preserving logical ordinals for edge/payload alignment.
    ///
    /// The returned plans are still heap-only metadata; no canonical state is written.
    pub fn build_one_orientation_batch_plans<E: CsrEdge>(
        &self,
        intents: &[BatchEdgeIntent],
        encode_edge: impl Fn(&BatchEdgeIntent) -> Result<E, BatchPlacementError>,
    ) -> Result<Vec<OneOrientationBatchWriteRequest<E>>, BatchPlacementError> {
        let mut forward_runs: BTreeMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>> =
            BTreeMap::new();
        let mut reverse_runs: BTreeMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>> =
            BTreeMap::new();

        for intent in intents {
            let key = BatchPlacementKey {
                orientation: intent.orientation,
                leaf_segment: super::batch_placement::leaf_index_for_vertex(
                    intent.owner_vertex_id,
                    super::batch_placement::segment_size(),
                ),
                owner_vertex_id: intent.owner_vertex_id,
                storage_label: intent.storage_label,
                inline_value_width: intent.inline_value_width,
            };
            let edge = encode_edge(intent)?;
            let entry = OneOrientationBatchEdge {
                logical_ordinal: intent.logical_ordinal,
                owner_vertex_id: intent.owner_vertex_id,
                neighbor_vertex_id: intent.neighbor_vertex_id,
                label_id: intent.storage_label,
                edge,
            };
            match intent.orientation {
                LabeledOrientation::Forward => forward_runs.entry(key).or_default().push(entry),
                LabeledOrientation::Reverse => reverse_runs.entry(key).or_default().push(entry),
            }
        }

        // Ensure each run is sorted by logical ordinal so edge/payload alignment is
        // deterministic.  The LARA reserve step also checks this, but doing it here
        // keeps the GraphStore contract closer to the source of physical intents.
        for runs in [&mut forward_runs, &mut reverse_runs] {
            for edges in runs.values_mut() {
                edges.sort_by_key(|e| e.logical_ordinal);
            }
        }

        let mut requests = Vec::with_capacity(2);
        if !forward_runs.is_empty() {
            requests.push(OneOrientationBatchWriteRequest {
                orientation: LabeledOrientation::Forward,
                plan: OneOrientationBatchPlan {
                    runs: runs_from_map(forward_runs),
                },
            });
        }
        if !reverse_runs.is_empty() {
            requests.push(OneOrientationBatchWriteRequest {
                orientation: LabeledOrientation::Reverse,
                plan: OneOrientationBatchPlan {
                    runs: runs_from_map(reverse_runs),
                },
            });
        }

        Ok(requests)
    }
}

fn runs_from_map<E: CsrEdge>(
    map: BTreeMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>>,
) -> Vec<OneOrientationBucketRun<E>> {
    map.into_iter()
        .map(|(key, edges)| OneOrientationBucketRun {
            owner_vertex_id: key.owner_vertex_id,
            label_id: key.storage_label,
            inline_value_width: key.inline_value_width,
            edges,
        })
        .collect()
}
