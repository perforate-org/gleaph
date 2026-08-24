//! The explorer web entry's deterministic demo graph.
//!
//! [`random_fixture`] mirrors `benches/paint_bench.rs::random`: a ring plus a
//! fixed-stride shortcut over xorshift coordinates, string-keyed for the worker
//! protocol. The main thread injects the batch into the worker replica and
//! holds the same content for its own camera-fit scene; the timing harness
//! times indexed paint-frame builds over it, so browser wiring proof and timed
//! shape are one and the same.

use glam::Vec2;
use gpui_graph::graph::Graph;
use gpui_graph::{EdgeDirection, GraphBatch, NodeId};

/// Deterministic pseudo-random coordinates (xorshift), matching
/// `benches/paint_bench.rs::Lcg`.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn coord(&mut self, span: f32) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0 * span - span * 0.5
    }
}

/// A deterministic random graph as a merge batch plus world-space placement:
/// ring plus a fixed-stride shortcut (`benches/paint_bench.rs::random`), node
/// keys `n{i}`, edge keys `e{index}` in insertion order.
pub struct SceneFixture {
    /// The graph content to merge into a scene.
    pub batch: GraphBatch<String, String, String, String>,
    /// One position per node, in batch insertion order.
    pub positions: Vec<Vec2>,
}

/// Build the deterministic random fixture at `count` nodes.
pub fn random_fixture(count: usize) -> SceneFixture {
    let span = 1500.0;
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let mut graph = Graph::new();
    let mut positions = Vec::with_capacity(count);
    let ids: Vec<NodeId> = (0..count)
        .map(|_| {
            positions.push(Vec2::new(rng.coord(span), rng.coord(span)));
            graph.add_node(())
        })
        .collect();
    for i in 0..count {
        let prev = (i + count - 1) % count;
        graph.add_edge(ids[i], ids[prev], EdgeDirection::Undirected, ());
        let skip = (i + 7) % count;
        if skip != i && skip != prev {
            graph.add_edge(ids[i], ids[skip], EdgeDirection::Undirected, ());
        }
    }

    // Key assignment mirrors benches/paint_bench.rs: nodes n{i} in insertion
    // order, edges e{index} over the logical graph's edge iteration order —
    // identical topology and ordering, string-keyed for the worker protocol.
    let mut batch = GraphBatch::new();
    for (index, _) in graph.nodes().enumerate() {
        batch = batch.node(format!("n{index}"), format!("n{index}"));
    }
    let keys: std::collections::HashMap<NodeId, String> = graph
        .nodes()
        .enumerate()
        .map(|(i, (id, _))| (id, format!("n{i}")))
        .collect();
    for (index, (_, edge)) in graph.edges().enumerate() {
        batch = batch.edge(
            format!("e{index}"),
            keys[&edge.source].clone(),
            keys[&edge.target].clone(),
            edge.direction,
            String::new(),
        );
    }

    SceneFixture { batch, positions }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_batches_carry_display_labels_for_every_node() {
        let fixture = random_fixture(120);
        assert_eq!(fixture.batch.nodes.len(), 120);
        assert_eq!(fixture.positions.len(), 120);
        assert!(
            fixture
                .batch
                .nodes
                .iter()
                .all(|(key, label)| label == key && !label.is_empty())
        );
    }
}
