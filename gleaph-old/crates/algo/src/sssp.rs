use crate::{AlgoOutcome, GraphView, budget::InstructionBudget};
use candid::CandidType;
use gleaph_types::{GleaphError, SsspResult, TimestampRange};
use rapidhash::fast::RapidHashMap;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
pub struct SsspConfig {
    pub max_distance: Option<f64>,
    pub max_visited: Option<usize>,
    pub target: Option<u32>,
    pub edge_label: Option<String>,
    pub ts_range: Option<TimestampRange>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HeapItem {
    dist: f64,
    v: u32,
}
impl Eq for HeapItem {}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .dist
            .total_cmp(&self.dist)
            .then_with(|| other.v.cmp(&self.v))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Serializable SSSP checkpoint for resumable execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SsspCheckpoint {
    pub start: u32,
    pub config: SsspConfig,
    pub heap: Vec<(f64, u32)>,
    pub dist: Vec<(u32, f64)>,
    pub prev: Vec<(u32, Option<u32>)>,
    pub visited: usize,
}

/// Backward-compatible Dijkstra: returns `Err(BudgetExhausted)` on budget exhaustion.
pub fn dijkstra<G: GraphView>(
    graph: &G,
    start: u32,
    config: &SsspConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<SsspResult, GleaphError> {
    if !graph.is_vertex_active(start) {
        return Err(GleaphError::VertexNotFound(start));
    }
    let mut dist: RapidHashMap<u32, f64> = RapidHashMap::default();
    dist.insert(start, 0.0f64);
    let mut prev: RapidHashMap<u32, Option<u32>> = RapidHashMap::default();
    prev.insert(start, None);
    let mut heap = BinaryHeap::from([HeapItem {
        dist: 0.0,
        v: start,
    }]);
    let mut visited = 0usize;

    match dijkstra_core(
        graph,
        start,
        config,
        budget,
        &mut heap,
        &mut dist,
        &mut prev,
        &mut visited,
    )? {
        AlgoOutcome::Done(r) => Ok(r),
        AlgoOutcome::Suspended { .. } => Err(GleaphError::BudgetExhausted),
    }
}

/// Starts a resumable Dijkstra. Returns partial result + checkpoint on budget exhaustion.
pub fn dijkstra_resumable<G: GraphView>(
    graph: &G,
    start: u32,
    config: &SsspConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<AlgoOutcome<SsspResult, SsspCheckpoint>, GleaphError> {
    if !graph.is_vertex_active(start) {
        return Err(GleaphError::VertexNotFound(start));
    }
    let mut dist: RapidHashMap<u32, f64> = RapidHashMap::default();
    dist.insert(start, 0.0f64);
    let mut prev: RapidHashMap<u32, Option<u32>> = RapidHashMap::default();
    prev.insert(start, None);
    let mut heap = BinaryHeap::from([HeapItem {
        dist: 0.0,
        v: start,
    }]);
    let mut visited = 0usize;

    dijkstra_core(
        graph,
        start,
        config,
        budget,
        &mut heap,
        &mut dist,
        &mut prev,
        &mut visited,
    )
}

/// Resumes Dijkstra from a checkpoint.
pub fn dijkstra_resume<G: GraphView>(
    graph: &G,
    checkpoint: SsspCheckpoint,
    budget: &mut dyn InstructionBudget,
) -> Result<AlgoOutcome<SsspResult, SsspCheckpoint>, GleaphError> {
    if !graph.is_vertex_active(checkpoint.start) {
        return Err(GleaphError::VertexNotFound(checkpoint.start));
    }
    let mut heap: BinaryHeap<HeapItem> = checkpoint
        .heap
        .into_iter()
        .map(|(dist, v)| HeapItem { dist, v })
        .collect();
    let mut dist: RapidHashMap<u32, f64> = checkpoint.dist.into_iter().collect();
    let mut prev: RapidHashMap<u32, Option<u32>> = checkpoint.prev.into_iter().collect();
    let mut visited = checkpoint.visited;

    dijkstra_core(
        graph,
        checkpoint.start,
        &checkpoint.config,
        budget,
        &mut heap,
        &mut dist,
        &mut prev,
        &mut visited,
    )
}

/// Internal Dijkstra core accepting pre-initialized state.
#[allow(clippy::too_many_arguments)]
fn dijkstra_core<G: GraphView>(
    graph: &G,
    start: u32,
    config: &SsspConfig,
    budget: &mut dyn InstructionBudget,
    heap: &mut BinaryHeap<HeapItem>,
    dist: &mut RapidHashMap<u32, f64>,
    prev: &mut RapidHashMap<u32, Option<u32>>,
    visited: &mut usize,
) -> Result<AlgoOutcome<SsspResult, SsspCheckpoint>, GleaphError> {
    let max_visited = config.max_visited.unwrap_or(10_000);

    while let Some(item) = heap.pop() {
        if budget.consume(1).is_err() {
            heap.push(item);
            return Ok(AlgoOutcome::Suspended {
                partial: build_sssp_result(dist, prev),
                checkpoint: SsspCheckpoint {
                    start,
                    config: config.clone(),
                    heap: heap.iter().map(|h| (h.dist, h.v)).collect(),
                    dist: dist.iter().map(|(&k, &v)| (k, v)).collect(),
                    prev: prev.iter().map(|(&k, v)| (k, *v)).collect(),
                    visited: *visited,
                },
            });
        }
        let HeapItem { dist: d, v } = item;
        if *visited >= max_visited {
            break;
        }
        if d > *dist.get(&v).unwrap_or(&f64::INFINITY) {
            continue;
        }
        *visited += 1;
        if config.target == Some(v) {
            break;
        }
        for (to, w, _ts) in graph.neighbors_filtered(v, config.ts_range.clone()) {
            if !graph.is_vertex_active(to) {
                continue;
            }
            if let Some(label) = &config.edge_label
                && !graph.edge_has_label(v, to, label)
            {
                continue;
            }
            if w < 0.0 {
                return Err(GleaphError::AlgorithmError(
                    "negative edge weights are not supported by dijkstra".into(),
                ));
            }
            let nd = d + f64::from(w);
            if config.max_distance.is_some_and(|m| nd > m) {
                continue;
            }
            if nd < *dist.get(&to).unwrap_or(&f64::INFINITY) {
                dist.insert(to, nd);
                prev.insert(to, Some(v));
                heap.push(HeapItem { dist: nd, v: to });
            }
        }
    }

    Ok(AlgoOutcome::Done(build_sssp_result(dist, prev)))
}

fn build_sssp_result(
    dist: &RapidHashMap<u32, f64>,
    prev: &RapidHashMap<u32, Option<u32>>,
) -> SsspResult {
    let mut distances: Vec<(u32, f64)> = dist.iter().map(|(&k, &v)| (k, v)).collect();
    distances.sort_unstable_by_key(|&(v, _)| v);
    let mut predecessors: Vec<(u32, Option<u32>)> = prev.iter().map(|(&k, v)| (k, *v)).collect();
    predecessors.sort_unstable_by_key(|&(v, _)| v);
    SsspResult {
        distances,
        predecessors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        GraphView, Neighbor,
        budget::{CountingBudget, UnlimitedBudget},
    };
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct G {
        adj: BTreeMap<u32, Vec<Neighbor>>,
        active: BTreeSet<u32>,
        labels: BTreeMap<(u32, u32), String>,
    }
    impl G {
        fn e(&mut self, s: u32, d: u32, w: f32, ts: u64, l: &str) {
            self.active.insert(s);
            self.active.insert(d);
            self.adj.entry(s).or_default().push((d, w, ts));
            self.labels.insert((s, d), l.to_string());
        }
    }
    impl GraphView for G {
        fn vertex_count(&self) -> u64 {
            self.active.len() as u64
        }
        fn edge_count(&self) -> u64 {
            self.adj.values().map(Vec::len).sum::<usize>() as u64
        }
        fn neighbors(&self, v: u32) -> Vec<Neighbor> {
            self.adj.get(&v).cloned().unwrap_or_default()
        }
        fn neighbors_filtered(&self, v: u32, r: Option<TimestampRange>) -> Vec<Neighbor> {
            self.neighbors(v)
                .into_iter()
                .filter(|(_, _, ts)| crate::ts_in_range(*ts, r.as_ref()))
                .collect()
        }
        fn reverse_neighbors(&self, t: u32) -> Vec<Neighbor> {
            let mut out = vec![];
            for (&s, ns) in &self.adj {
                for &(d, w, ts) in ns {
                    if d == t {
                        out.push((s, w, ts));
                    }
                }
            }
            out
        }
        fn is_vertex_active(&self, v: u32) -> bool {
            self.active.contains(&v)
        }
        fn vertex_has_label(&self, _v: u32, _l: &str) -> bool {
            false
        }
        fn edge_has_label(&self, s: u32, d: u32, l: &str) -> bool {
            self.labels.get(&(s, d)).is_some_and(|x| x == l)
        }
        fn edge_label_ref(&self, s: u32, d: u32) -> Option<&str> {
            self.labels.get(&(s, d)).map(|s| s.as_str())
        }
        fn label_name_by_id(&self, _label_id: u32) -> Option<&str> {
            None
        }
        fn all_vertices(&self) -> Vec<u32> {
            self.active.iter().copied().collect()
        }
    }

    #[test]
    fn shortest_path_target_unreachable_and_temporal() {
        let mut g = G::default();
        g.e(1, 2, 5.0, 1, "ROAD");
        g.e(1, 3, 1.0, 1, "ROAD");
        g.e(3, 2, 1.0, 1, "ROAD");
        g.e(2, 4, 1.0, 100, "ROAD");
        g.active.insert(5);

        let mut b = UnlimitedBudget;
        let r = dijkstra(
            &g,
            1,
            &SsspConfig {
                target: Some(2),
                edge_label: Some("ROAD".into()),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        let d2 = r.distances.iter().find(|(v, _)| *v == 2).unwrap().1;
        assert!((d2 - 2.0).abs() < 1e-9);
        assert!(!r.distances.iter().any(|(v, _)| *v == 5));

        let mut b = UnlimitedBudget;
        let t = dijkstra(
            &g,
            1,
            &SsspConfig {
                edge_label: Some("ROAD".into()),
                ts_range: Some(TimestampRange {
                    start: Some(0),
                    end: Some(10),
                }),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert!(!t.distances.iter().any(|(v, _)| *v == 4));
    }

    #[test]
    fn rejects_negative_weights_and_budget_exhaustion() {
        let mut g = G::default();
        g.e(1, 2, -1.0, 1, "NEG");
        let mut b = UnlimitedBudget;
        let err = dijkstra(
            &g,
            1,
            &SsspConfig {
                edge_label: Some("NEG".into()),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap_err();
        assert!(matches!(err, GleaphError::AlgorithmError(_)));

        let mut g = G::default();
        g.e(1, 2, 1.0, 1, "X");
        let mut b = CountingBudget::new(0);
        let err = dijkstra(&g, 1, &SsspConfig::default(), &mut b).unwrap_err();
        assert!(matches!(err, GleaphError::BudgetExhausted));
    }

    #[test]
    fn dijkstra_continuation_resumes_from_checkpoint() {
        // Diamond: 1->2 (w=1), 1->3 (w=2), 2->4 (w=3), 3->4 (w=1)
        // Shortest to 4: 1->3->4 (w=3)
        let mut g = G::default();
        g.e(1, 2, 1.0, 1, "X");
        g.e(1, 3, 2.0, 1, "X");
        g.e(2, 4, 3.0, 1, "X");
        g.e(3, 4, 1.0, 1, "X");
        let config = SsspConfig::default();

        // Tight budget: process only 1 vertex per call
        let mut cp = match dijkstra_resumable(&g, 1, &config, &mut CountingBudget::new(1)).unwrap()
        {
            AlgoOutcome::Suspended { checkpoint, .. } => checkpoint,
            AlgoOutcome::Done(_) => panic!("expected suspension"),
        };

        let final_result = loop {
            match dijkstra_resume(&g, cp, &mut CountingBudget::new(1)).unwrap() {
                AlgoOutcome::Done(r) => break r,
                AlgoOutcome::Suspended {
                    checkpoint: next, ..
                } => cp = next,
            }
        };

        let d4 = final_result
            .distances
            .iter()
            .find(|(v, _)| *v == 4)
            .unwrap()
            .1;
        assert!(
            (d4 - 3.0).abs() < 1e-9,
            "shortest to 4 should be 3.0, got {d4}"
        );
    }

    #[test]
    fn dijkstra_resumable_matches_non_resumable() {
        let mut g = G::default();
        g.e(1, 2, 1.0, 1, "X");
        g.e(1, 3, 2.0, 1, "X");
        g.e(2, 3, 0.5, 1, "X");
        let config = SsspConfig::default();

        let mut b1 = UnlimitedBudget;
        let normal = dijkstra(&g, 1, &config, &mut b1).unwrap();

        let mut b2 = UnlimitedBudget;
        let resumable = match dijkstra_resumable(&g, 1, &config, &mut b2).unwrap() {
            AlgoOutcome::Done(r) => r,
            _ => panic!("should complete"),
        };
        assert_eq!(normal, resumable);
    }
}
