use crate::{AlgoOutcome, GraphView, budget::InstructionBudget};
use candid::CandidType;
use gleaph_types::{GleaphError, PageRankResult, TimestampRange};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub struct PageRankConfig {
    pub damping: f64,
    pub max_iterations: u32,
    pub convergence_threshold: f64,
    pub ts_range: Option<TimestampRange>,
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iterations: 20,
            convergence_threshold: 1e-6,
            ts_range: None,
        }
    }
}

/// Serializable PageRank checkpoint for resumable execution.
///
/// Adjacency lists are NOT stored — they are recomputed from the graph on resume.
/// This trades O(V+E) recomputation per resume for significantly smaller checkpoint size.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageRankCheckpoint {
    pub config: PageRankConfig,
    pub rank: Vec<(u32, f64)>,
    pub iteration: u32,
}

/// Backward-compatible PageRank: returns partial results on budget exhaustion.
pub fn pagerank<G: GraphView>(
    graph: &G,
    config: &PageRankConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<PageRankResult, GleaphError> {
    let (vertices, out_neighbors, incoming_neighbors) = precompute_adjacency(graph, config);
    if vertices.is_empty() {
        return Ok(PageRankResult::default());
    }
    let n = vertices.len() as f64;
    let mut rank = vec![1.0 / n; vertices.len()];

    match pagerank_core(
        config,
        budget,
        &vertices,
        &out_neighbors,
        &incoming_neighbors,
        &mut rank,
        0,
    ) {
        AlgoOutcome::Done(r) | AlgoOutcome::Suspended { partial: r, .. } => Ok(r),
    }
}

/// Starts a resumable PageRank.
pub fn pagerank_resumable<G: GraphView>(
    graph: &G,
    config: &PageRankConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<AlgoOutcome<PageRankResult, PageRankCheckpoint>, GleaphError> {
    let (vertices, out_neighbors, incoming_neighbors) = precompute_adjacency(graph, config);
    if vertices.is_empty() {
        return Ok(AlgoOutcome::Done(PageRankResult::default()));
    }
    let n = vertices.len() as f64;
    let mut rank = vec![1.0 / n; vertices.len()];

    Ok(pagerank_core(
        config,
        budget,
        &vertices,
        &out_neighbors,
        &incoming_neighbors,
        &mut rank,
        0,
    ))
}

/// Resumes PageRank from a checkpoint. Recomputes adjacency from the graph.
pub fn pagerank_resume<G: GraphView>(
    graph: &G,
    checkpoint: PageRankCheckpoint,
    budget: &mut dyn InstructionBudget,
) -> Result<AlgoOutcome<PageRankResult, PageRankCheckpoint>, GleaphError> {
    let (vertices, out_neighbors, incoming_neighbors) =
        precompute_adjacency(graph, &checkpoint.config);
    if vertices.is_empty() {
        return Ok(AlgoOutcome::Done(PageRankResult::default()));
    }
    let rank_by_vertex: HashMap<u32, f64> = checkpoint.rank.into_iter().collect();
    let mut rank = vertices
        .iter()
        .map(|v| rank_by_vertex.get(v).copied().unwrap_or(0.0))
        .collect();

    Ok(pagerank_core(
        &checkpoint.config,
        budget,
        &vertices,
        &out_neighbors,
        &incoming_neighbors,
        &mut rank,
        checkpoint.iteration,
    ))
}

/// Precompute adjacency lists for PageRank.
#[allow(clippy::type_complexity)]
fn precompute_adjacency<G: GraphView>(
    graph: &G,
    config: &PageRankConfig,
) -> (Vec<u32>, Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let vertices: Vec<u32> = graph
        .all_vertices()
        .into_iter()
        .filter(|v| graph.is_vertex_active(*v))
        .collect();

    let vertex_to_idx: HashMap<u32, usize> = vertices
        .iter()
        .enumerate()
        .map(|(idx, &vertex)| (vertex, idx))
        .collect();
    let mut out_neighbors = vec![Vec::new(); vertices.len()];
    let mut incoming_neighbors = vec![Vec::new(); vertices.len()];
    for (src_idx, &src) in vertices.iter().enumerate() {
        let out_live: Vec<usize> = graph
            .neighbors_filtered(src, config.ts_range.clone())
            .into_iter()
            .filter_map(|(dst, _w, _ts)| {
                graph
                    .is_vertex_active(dst)
                    .then(|| vertex_to_idx.get(&dst).copied())
                    .flatten()
            })
            .collect();
        for &dst_idx in &out_live {
            incoming_neighbors[dst_idx].push(src_idx);
        }
        out_neighbors[src_idx] = out_live;
    }

    (vertices, out_neighbors, incoming_neighbors)
}

/// Internal PageRank core. Runs from `start_iteration` using pre-initialized rank.
fn pagerank_core(
    config: &PageRankConfig,
    budget: &mut dyn InstructionBudget,
    vertices: &[u32],
    out_neighbors: &[Vec<usize>],
    incoming_neighbors: &[Vec<usize>],
    rank: &mut Vec<f64>,
    start_iteration: u32,
) -> AlgoOutcome<PageRankResult, PageRankCheckpoint> {
    let n = vertices.len() as f64;
    let base = (1.0 - config.damping) / n;

    let mut converged = false;
    let mut ran_iters = start_iteration;
    for iter in start_iteration..config.max_iterations {
        let dangling_mass: f64 = out_neighbors
            .iter()
            .enumerate()
            .map(|(idx, out)| if out.is_empty() { rank[idx] } else { 0.0 })
            .sum();
        let dangling_per_node = dangling_mass / n;

        let mut next = vec![0.0; vertices.len()];
        let mut max_delta = 0.0f64;
        for (v_idx, _) in vertices.iter().enumerate() {
            if budget.consume(1).is_err() {
                // Budget exhausted mid-iteration: return current rank (not partial `next`)
                return AlgoOutcome::Suspended {
                    partial: build_pagerank_result(vertices, rank, iter, false),
                    checkpoint: PageRankCheckpoint {
                        config: config.clone(),
                        rank: vertices.iter().copied().zip(rank.iter().copied()).collect(),
                        iteration: iter,
                    },
                };
            }
            let mut incoming_sum = 0.0;
            for &u_idx in &incoming_neighbors[v_idx] {
                let out_degree = out_neighbors[u_idx].len();
                if out_degree == 0 {
                    continue;
                }
                incoming_sum += rank[u_idx] / out_degree as f64;
            }
            let new_r = base + config.damping * (incoming_sum + dangling_per_node);
            let old_r = rank[v_idx];
            max_delta = max_delta.max((new_r - old_r).abs());
            next[v_idx] = new_r;
        }
        *rank = next;
        ran_iters = iter + 1;
        if max_delta < config.convergence_threshold {
            converged = true;
            break;
        }
    }
    AlgoOutcome::Done(build_pagerank_result(vertices, rank, ran_iters, converged))
}

fn build_pagerank_result(
    vertices: &[u32],
    rank: &[f64],
    iterations: u32,
    converged: bool,
) -> PageRankResult {
    let mut scores: Vec<_> = vertices.iter().copied().zip(rank.iter().copied()).collect();
    scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    PageRankResult {
        scores,
        iterations,
        converged,
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
        fn v(&mut self, id: u32) {
            self.active.insert(id);
        }
        fn e(&mut self, s: u32, d: u32, ts: u64) {
            self.v(s);
            self.v(d);
            self.adj.entry(s).or_default().push((d, 1.0, ts));
            self.labels.insert((s, d), "X".into());
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
    fn triangle_converges_to_equal_scores() {
        let mut g = G::default();
        g.e(1, 2, 1);
        g.e(2, 3, 1);
        g.e(3, 1, 1);
        let mut b = UnlimitedBudget;
        let r = pagerank(
            &g,
            &PageRankConfig {
                max_iterations: 100,
                convergence_threshold: 1e-12,
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert!(r.converged);
        assert_eq!(r.scores.len(), 3);
        let s: Vec<f64> = r.scores.iter().map(|(_, x)| *x).collect();
        assert!(s.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6));
    }

    #[test]
    fn star_and_temporal_filter_change_ranking() {
        let mut g = G::default();
        g.e(2, 1, 1);
        g.e(3, 1, 1);
        g.e(4, 1, 1);
        g.e(1, 2, 1);
        g.e(1, 3, 100);
        g.e(1, 4, 100);

        let mut b = UnlimitedBudget;
        let all = pagerank(
            &g,
            &PageRankConfig {
                max_iterations: 100,
                convergence_threshold: 1e-9,
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        let r2_all = all.scores.iter().find(|(v, _)| *v == 2).unwrap().1;
        let r3_all = all.scores.iter().find(|(v, _)| *v == 3).unwrap().1;

        let mut b = UnlimitedBudget;
        let filtered = pagerank(
            &g,
            &PageRankConfig {
                ts_range: Some(TimestampRange {
                    start: Some(0),
                    end: Some(10),
                }),
                max_iterations: 100,
                convergence_threshold: 1e-9,
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        let r2_filtered = filtered.scores.iter().find(|(v, _)| *v == 2).unwrap().1;
        let r3_filtered = filtered.scores.iter().find(|(v, _)| *v == 3).unwrap().1;

        assert!(
            r2_filtered > r2_all,
            "node 2 rank should be higher when 1→3/4 edges are excluded"
        );
        assert!(
            r3_filtered < r3_all,
            "node 3 rank should be lower when 1→3 edge is excluded"
        );
    }

    #[test]
    fn budget_exhaustion_returns_partial_results() {
        let mut g = G::default();
        g.e(1, 2, 1);
        g.e(2, 1, 1);
        let mut b = CountingBudget::new(0);
        let r = pagerank(&g, &PageRankConfig::default(), &mut b).unwrap();
        assert!(!r.converged);
        assert_eq!(r.iterations, 0);
        assert!(!r.scores.is_empty());
    }

    #[test]
    fn pagerank_continuation_converges_across_calls() {
        // Triangle: 1->2->3->1, converge with tight budget per call
        let mut g = G::default();
        g.e(1, 2, 1);
        g.e(2, 3, 1);
        g.e(3, 1, 1);
        let config = PageRankConfig {
            max_iterations: 100,
            convergence_threshold: 1e-12,
            ..Default::default()
        };

        // Budget = 2 vertices per call (3 vertices per iteration, so suspends mid-iteration)
        let mut b = CountingBudget::new(2);
        let outcome = pagerank_resumable(&g, &config, &mut b).unwrap();
        let mut cp = match outcome {
            AlgoOutcome::Suspended { checkpoint, .. } => checkpoint,
            AlgoOutcome::Done(_) => panic!("expected suspension with budget=2"),
        };

        // Resume loop until done
        let final_result = loop {
            let mut b = CountingBudget::new(4); // enough for ~1 iteration
            match pagerank_resume(&g, cp, &mut b).unwrap() {
                AlgoOutcome::Done(r) => break r,
                AlgoOutcome::Suspended {
                    checkpoint: next, ..
                } => cp = next,
            }
        };
        assert!(final_result.converged);
        assert_eq!(final_result.scores.len(), 3);
        let s: Vec<f64> = final_result.scores.iter().map(|(_, x)| *x).collect();
        assert!(s.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6));
    }

    #[test]
    fn pagerank_resumable_matches_non_resumable() {
        let mut g = G::default();
        g.e(1, 2, 1);
        g.e(2, 3, 1);
        g.e(3, 1, 1);
        let config = PageRankConfig {
            max_iterations: 20,
            convergence_threshold: 1e-6,
            ..Default::default()
        };

        let mut b1 = UnlimitedBudget;
        let normal = pagerank(&g, &config, &mut b1).unwrap();

        let mut b2 = UnlimitedBudget;
        let resumable = match pagerank_resumable(&g, &config, &mut b2).unwrap() {
            AlgoOutcome::Done(r) => r,
            _ => panic!("should complete"),
        };
        assert_eq!(normal, resumable);
    }
}
