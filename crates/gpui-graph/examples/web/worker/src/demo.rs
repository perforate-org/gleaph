//! The deterministic demo graph shared by both frame sources.
//!
//! ~100 vertices with a few hubs, mirroring `crates/gpui-graph/examples/
//! force_atlas2.rs` at example scale: hubs repel each other while their
//! neighbors are pulled in, so ForceAtlas2 has visible work to do. The seed is
//! fixed, so every load (and the native round-trip test) builds byte-identical
//! scenes.

use glam::Vec2;
use gpui_graph::{EdgeDirection, GraphBatch};

/// Vertex count of the demo graph.
pub const DEMO_NODE_COUNT: usize = 100;

/// World-space radius of the circular seed placement.
const SEED_RADIUS: f32 = 200.0;

/// Build the demo graph: node key doubles as the display label; edges are
/// undirected and unlabeled.
pub fn demo_batch() -> GraphBatch<String, String, String, String> {
    let mut rng = gpui_graph::Rng::new(42);
    let mut batch = GraphBatch::new();
    for i in 0..DEMO_NODE_COUNT {
        batch = batch.node(format!("n{i}"), format!("n{i}"));
    }
    for i in 0..DEMO_NODE_COUNT {
        // Every tenth vertex is a hub with triple degree.
        let degree = if i % 10 == 0 { 6 } else { 2 };
        for _ in 0..degree {
            let j = (rng.next_f32() * DEMO_NODE_COUNT as f32) as usize % DEMO_NODE_COUNT;
            if i != j {
                batch = batch.edge(
                    format!("e{i}_{j}"),
                    format!("n{i}"),
                    format!("n{j}"),
                    EdgeDirection::Undirected,
                    String::new(),
                );
            }
        }
    }
    batch
}

/// Seed placement for vertex `index` of `count`: one ring around the origin,
/// evenly spaced. The worker replica re-lays these out under ForceAtlas2; the
/// main-thread scene keeps them fixed purely so the view's one-time initial
/// camera fit has content to look at before the first frame arrives.
pub fn initial_position(index: usize, count: usize) -> Vec2 {
    let angle = index as f32 / count.max(1) as f32 * core::f32::consts::TAU;
    Vec2::new(angle.cos(), angle.sin()) * SEED_RADIUS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field-wise equality: `GraphBatch` intentionally has no `PartialEq`.
    fn assert_batches_equal(
        expected: &GraphBatch<String, String, String, String>,
        actual: &GraphBatch<String, String, String, String>,
    ) {
        assert_eq!(expected.nodes, actual.nodes);
        assert_eq!(expected.edges.len(), actual.edges.len());
        for (expected_edge, actual_edge) in expected.edges.iter().zip(&actual.edges) {
            assert_eq!(expected_edge, actual_edge);
        }
    }

    #[test]
    fn demo_batch_is_deterministic_and_fully_connected_enough_for_fa2() {
        let first = demo_batch();
        let second = demo_batch();
        assert_batches_equal(&first, &second);

        assert_eq!(first.nodes.len(), DEMO_NODE_COUNT);
        assert!(
            first.edges.len() >= DEMO_NODE_COUNT,
            "the hub pattern must give the force model edges to work with"
        );

        // Every edge endpoint names an existing node key.
        let keys: std::collections::HashSet<_> = first.nodes.iter().map(|(key, _)| key).collect();
        for (_, source, target, _, _) in &first.edges {
            assert!(keys.contains(source), "edge source {source} must exist");
            assert!(keys.contains(target), "edge target {target} must exist");
        }
    }

    #[test]
    fn initial_positions_form_one_ring_around_the_origin() {
        let count = 12;
        for index in 0..count {
            let position = initial_position(index, count);
            assert!((position.length() - SEED_RADIUS).abs() < 1e-4);
        }
        // Even spacing: adjacent vertices subtend equal angles at the center.
        let a = initial_position(0, 4);
        let b = initial_position(1, 4);
        assert!((a.angle_to(b) - core::f32::consts::TAU / 4.0).abs() < 1e-5);
    }
}
