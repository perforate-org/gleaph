use crate::{AlgoOutcome, GraphView, budget::InstructionBudget};
use candid::CandidType;
use gleaph_types::{
    BfsResult, GleaphError, LabelExpr, TimestampRange, VertexIdSet, matches_edge_label,
};
use rapidhash::fast::RapidHashMap;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
pub struct BfsConfig {
    pub max_depth: Option<u32>,
    pub max_visited: Option<usize>,
    pub target: Option<u32>,
    pub edge_label: Option<String>,
    /// Label expression filter — when set, takes precedence over `edge_label`.
    #[serde(skip)]
    pub edge_label_expr: Option<LabelExpr>,
    pub ts_range: Option<TimestampRange>,
}

/// Serializable BFS checkpoint for resumable execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BfsCheckpoint {
    pub start: u32,
    pub config: BfsConfig,
    pub frontier: Vec<(u32, u32)>,
    pub visited: Vec<u32>,
    pub visit_order: Vec<u32>,
    pub dist: Vec<(u32, u32)>,
    pub pred: Vec<(u32, u32)>,
}

/// Backward-compatible BFS: returns `Err(BudgetExhausted)` on budget exhaustion.
pub fn bfs<G: GraphView>(
    graph: &G,
    start: u32,
    config: &BfsConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<BfsResult, GleaphError> {
    if !graph.is_vertex_active(start) {
        return Err(GleaphError::VertexNotFound(start));
    }
    let mut q = VecDeque::from([(start, 0u32)]);
    let mut visited = VertexIdSet::from_iter([start]);
    let mut visit_order = vec![start];
    let mut dist: RapidHashMap<u32, u32> = RapidHashMap::default();
    dist.insert(start, 0u32);
    let mut pred: RapidHashMap<u32, u32> = RapidHashMap::default();

    match bfs_core(
        graph,
        start,
        config,
        budget,
        &mut q,
        &mut visited,
        &mut visit_order,
        &mut dist,
        &mut pred,
    ) {
        AlgoOutcome::Done(r) => Ok(r),
        AlgoOutcome::Suspended { .. } => Err(GleaphError::BudgetExhausted),
    }
}

/// Starts a resumable BFS. Returns partial result + checkpoint on budget exhaustion.
pub fn bfs_resumable<G: GraphView>(
    graph: &G,
    start: u32,
    config: &BfsConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<AlgoOutcome<BfsResult, BfsCheckpoint>, GleaphError> {
    if !graph.is_vertex_active(start) {
        return Err(GleaphError::VertexNotFound(start));
    }
    let mut q = VecDeque::from([(start, 0u32)]);
    let mut visited = VertexIdSet::from_iter([start]);
    let mut visit_order = vec![start];
    let mut dist: RapidHashMap<u32, u32> = RapidHashMap::default();
    dist.insert(start, 0u32);
    let mut pred: RapidHashMap<u32, u32> = RapidHashMap::default();

    Ok(bfs_core(
        graph,
        start,
        config,
        budget,
        &mut q,
        &mut visited,
        &mut visit_order,
        &mut dist,
        &mut pred,
    ))
}

/// Resumes a BFS from a checkpoint.
pub fn bfs_resume<G: GraphView>(
    graph: &G,
    checkpoint: BfsCheckpoint,
    budget: &mut dyn InstructionBudget,
) -> Result<AlgoOutcome<BfsResult, BfsCheckpoint>, GleaphError> {
    if !graph.is_vertex_active(checkpoint.start) {
        return Err(GleaphError::VertexNotFound(checkpoint.start));
    }
    let mut q: VecDeque<(u32, u32)> = checkpoint.frontier.into_iter().collect();
    let mut visited: VertexIdSet = checkpoint.visited.into_iter().collect();
    let mut visit_order = checkpoint.visit_order;
    let mut dist: RapidHashMap<u32, u32> = checkpoint.dist.into_iter().collect();
    let mut pred: RapidHashMap<u32, u32> = checkpoint.pred.into_iter().collect();

    Ok(bfs_core(
        graph,
        checkpoint.start,
        &checkpoint.config,
        budget,
        &mut q,
        &mut visited,
        &mut visit_order,
        &mut dist,
        &mut pred,
    ))
}

/// Internal BFS core accepting pre-initialized state.
#[allow(clippy::too_many_arguments)]
fn bfs_core<G: GraphView>(
    graph: &G,
    start: u32,
    config: &BfsConfig,
    budget: &mut dyn InstructionBudget,
    q: &mut VecDeque<(u32, u32)>,
    visited: &mut VertexIdSet,
    visit_order: &mut Vec<u32>,
    dist: &mut RapidHashMap<u32, u32>,
    pred: &mut RapidHashMap<u32, u32>,
) -> AlgoOutcome<BfsResult, BfsCheckpoint> {
    let max_visited = config.max_visited.unwrap_or(10_000);

    while let Some((v, depth)) = q.pop_front() {
        if budget.consume(1).is_err() {
            q.push_front((v, depth));
            return AlgoOutcome::Suspended {
                partial: build_bfs_result(start, config, visit_order, dist, pred, visited),
                checkpoint: BfsCheckpoint {
                    start,
                    config: config.clone(),
                    frontier: q.iter().copied().collect(),
                    visited: visited.iter().collect(),
                    visit_order: visit_order.clone(),
                    dist: dist.iter().map(|(&k, &v)| (k, v)).collect(),
                    pred: pred.iter().map(|(&k, &v)| (k, v)).collect(),
                },
            };
        }
        if config.target == Some(v) && v == start {
            break;
        }
        if config.max_depth.is_some_and(|d| depth >= d) {
            continue;
        }

        for (dst, _w, _ts) in graph.neighbors_filtered(v, config.ts_range.clone()) {
            if let Some(expr) = &config.edge_label_expr {
                if !matches_edge_label(expr, graph.edge_label_ref(v, dst)) {
                    continue;
                }
            } else if let Some(label) = &config.edge_label
                && !graph.edge_has_label(v, dst, label)
            {
                continue;
            }
            if !graph.is_vertex_active(dst) || visited.contains(dst) {
                continue;
            }
            if visited.len() as usize >= max_visited {
                break;
            }
            visited.insert(dst);
            visit_order.push(dst);
            dist.insert(dst, depth + 1);
            pred.insert(dst, v);
            if config.target == Some(dst) {
                q.clear();
                break;
            }
            q.push_back((dst, depth + 1));
        }
        if visited.len() as usize >= max_visited {
            break;
        }
    }

    AlgoOutcome::Done(build_bfs_result(
        start,
        config,
        visit_order,
        dist,
        pred,
        visited,
    ))
}

fn build_bfs_result(
    start: u32,
    config: &BfsConfig,
    visit_order: &[u32],
    dist: &RapidHashMap<u32, u32>,
    pred: &RapidHashMap<u32, u32>,
    visited: &VertexIdSet,
) -> BfsResult {
    let path = config.target.and_then(|t| {
        if !visited.contains(t) {
            return None;
        }
        let mut out = vec![t];
        let mut cur = t;
        while cur != start {
            cur = *pred.get(&cur)?;
            out.push(cur);
        }
        out.reverse();
        Some(out)
    });
    let mut distances: Vec<(u32, u32)> = dist.iter().map(|(&k, &v)| (k, v)).collect();
    distances.sort_unstable_by_key(|&(v, _)| v);
    BfsResult {
        visited: visit_order.to_vec(),
        distances,
        path,
    }
}

/// Bidirectional BFS from `start` toward one of `targets`.
///
/// Explores from both ends simultaneously — forward from `start`, backward from
/// `targets` — until the two frontiers meet.  Returns the shortest path found,
/// or `visited = []` when no path exists.
///
/// Filters in `config` (`edge_label`, `ts_range`, `max_depth`) are respected by
/// both frontiers.  `config.target` is ignored (use the `targets` parameter).
pub fn bfs_bidirectional<G: GraphView>(
    graph: &G,
    start: u32,
    targets: &[u32],
    config: &BfsConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<BfsResult, GleaphError> {
    if !graph.is_vertex_active(start) {
        return Err(GleaphError::VertexNotFound(start));
    }
    if targets.is_empty() {
        return Ok(BfsResult {
            visited: vec![],
            distances: vec![],
            path: None,
        });
    }

    // Check if start is itself a target.
    for &t in targets {
        if t == start {
            return Ok(BfsResult {
                visited: vec![start],
                distances: vec![(start, 0)],
                path: Some(vec![start]),
            });
        }
    }

    let max_visited = config.max_visited.unwrap_or(10_000);

    // Forward state: start → targets
    let mut fwd_q: VecDeque<(u32, u32)> = VecDeque::from([(start, 0)]);
    let mut fwd_visited = VertexIdSet::from_iter([start]);
    let mut fwd_dist: RapidHashMap<u32, u32> = RapidHashMap::default();
    fwd_dist.insert(start, 0);
    let mut fwd_pred: RapidHashMap<u32, u32> = RapidHashMap::default();

    // Backward state: targets → start
    let mut bwd_q: VecDeque<(u32, u32)> = VecDeque::new();
    let mut bwd_visited = VertexIdSet::new();
    let mut bwd_dist: RapidHashMap<u32, u32> = RapidHashMap::default();
    let mut bwd_pred: RapidHashMap<u32, u32> = RapidHashMap::default();

    for &t in targets {
        if graph.is_vertex_active(t) && !bwd_visited.contains(t) {
            bwd_q.push_back((t, 0));
            bwd_visited.insert(t);
            bwd_dist.insert(t, 0);
        }
    }

    if bwd_q.is_empty() {
        return Ok(BfsResult {
            visited: vec![],
            distances: vec![],
            path: None,
        });
    }

    // Best meeting point found so far.
    let mut best_meeting: Option<u32> = None;
    let mut best_total_dist: u32 = u32::MAX;

    loop {
        if fwd_q.is_empty() && bwd_q.is_empty() {
            break;
        }
        if (fwd_visited.len() + bwd_visited.len()) as usize >= max_visited {
            break;
        }

        // If we already have a meeting point whose total distance is ≤ the sum of
        // the next frontier depths, we cannot improve — stop early.
        let fwd_front_depth = fwd_q.front().map(|(_, d)| *d).unwrap_or(u32::MAX);
        let bwd_front_depth = bwd_q.front().map(|(_, d)| *d).unwrap_or(u32::MAX);
        if best_meeting.is_some()
            && fwd_front_depth.saturating_add(bwd_front_depth) >= best_total_dist
        {
            break;
        }

        // Expand the smaller frontier (heuristic for balanced exploration).
        let expand_forward = match (fwd_q.is_empty(), bwd_q.is_empty()) {
            (true, _) => false,
            (_, true) => true,
            _ => fwd_q.len() <= bwd_q.len(),
        };

        if expand_forward {
            if let Some((v, depth)) = fwd_q.pop_front() {
                if budget.consume(1).is_err() {
                    return Err(GleaphError::BudgetExhausted);
                }
                if config.max_depth.is_some_and(|d| depth >= d) {
                    continue;
                }

                for (dst, _w, _ts) in graph.neighbors_filtered(v, config.ts_range.clone()) {
                    if let Some(expr) = &config.edge_label_expr {
                        if !matches_edge_label(expr, graph.edge_label_ref(v, dst)) {
                            continue;
                        }
                    } else if let Some(label) = &config.edge_label
                        && !graph.edge_has_label(v, dst, label)
                    {
                        continue;
                    }
                    if !graph.is_vertex_active(dst) || fwd_visited.contains(dst) {
                        continue;
                    }
                    if (fwd_visited.len() + bwd_visited.len()) as usize >= max_visited {
                        break;
                    }
                    fwd_visited.insert(dst);
                    fwd_dist.insert(dst, depth + 1);
                    fwd_pred.insert(dst, v);

                    // Check meeting condition.
                    if bwd_visited.contains(dst) {
                        let total = depth + 1 + bwd_dist.get(&dst).copied().unwrap_or(0);
                        if total < best_total_dist {
                            best_total_dist = total;
                            best_meeting = Some(dst);
                        }
                    }

                    fwd_q.push_back((dst, depth + 1));
                }
            }
        } else if let Some((v, depth)) = bwd_q.pop_front() {
            if budget.consume(1).is_err() {
                return Err(GleaphError::BudgetExhausted);
            }
            if config.max_depth.is_some_and(|d| depth >= d) {
                continue;
            }

            // Backward expansion uses reverse neighbors.
            for (src, _w, _ts) in graph.reverse_neighbors_filtered(v, config.ts_range.clone()) {
                // Edge label check: the actual edge is src→v.
                if let Some(expr) = &config.edge_label_expr {
                    if !matches_edge_label(expr, graph.edge_label_ref(src, v)) {
                        continue;
                    }
                } else if let Some(label) = &config.edge_label
                    && !graph.edge_has_label(src, v, label)
                {
                    continue;
                }
                if !graph.is_vertex_active(src) || bwd_visited.contains(src) {
                    continue;
                }
                if (fwd_visited.len() + bwd_visited.len()) as usize >= max_visited {
                    break;
                }
                bwd_visited.insert(src);
                bwd_dist.insert(src, depth + 1);
                bwd_pred.insert(src, v);

                // Check meeting condition.
                if fwd_visited.contains(src) {
                    let total = fwd_dist.get(&src).copied().unwrap_or(0) + depth + 1;
                    if total < best_total_dist {
                        best_total_dist = total;
                        best_meeting = Some(src);
                    }
                }

                bwd_q.push_back((src, depth + 1));
            }
        }
    }

    // Reconstruct path through meeting point.
    let path = best_meeting.map(|m| {
        // Forward half: start → ... → m
        let mut fwd_half = vec![m];
        let mut cur = m;
        while let Some(&p) = fwd_pred.get(&cur) {
            fwd_half.push(p);
            cur = p;
        }
        fwd_half.reverse();

        // Backward half: m → ... → target
        // bwd_pred[m] is the next vertex toward a target.
        cur = m;
        while let Some(&p) = bwd_pred.get(&cur) {
            fwd_half.push(p);
            cur = p;
        }

        fwd_half
    });

    let all_set = &fwd_visited | &bwd_visited;
    let mut all_visited: Vec<u32> = all_set.iter().collect();
    all_visited.sort_unstable();
    let distances: Vec<(u32, u32)> = all_visited
        .iter()
        .filter_map(|&v| {
            fwd_dist
                .get(&v)
                .copied()
                .or_else(|| bwd_dist.get(&v).copied())
                .map(|d| (v, d))
        })
        .collect();

    Ok(BfsResult {
        visited: all_visited,
        distances,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphView, Neighbor, budget::CountingBudget};
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct MockGraph {
        adj: BTreeMap<u32, Vec<Neighbor>>,
        vlabels: BTreeMap<u32, BTreeSet<String>>,
        elabels: BTreeMap<(u32, u32), String>,
        active: BTreeSet<u32>,
    }

    impl MockGraph {
        fn add_v(&mut self, v: u32) {
            self.active.insert(v);
        }
        fn add_e(&mut self, s: u32, d: u32, w: f32, ts: u64, label: &str) {
            self.add_v(s);
            self.add_v(d);
            self.adj.entry(s).or_default().push((d, w, ts));
            self.elabels.insert((s, d), label.to_string());
        }
    }

    impl GraphView for MockGraph {
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
            let mut out = Vec::new();
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
        fn vertex_has_label(&self, v: u32, l: &str) -> bool {
            self.vlabels.get(&v).is_some_and(|s| s.contains(l))
        }
        fn edge_has_label(&self, s: u32, d: u32, l: &str) -> bool {
            self.elabels.get(&(s, d)).is_some_and(|x| x == l)
        }
        fn edge_label_ref(&self, s: u32, d: u32) -> Option<&str> {
            self.elabels.get(&(s, d)).map(|s| s.as_str())
        }
        fn label_name_by_id(&self, _label_id: u32) -> Option<&str> {
            None
        }
        fn all_vertices(&self) -> Vec<u32> {
            self.active.iter().copied().collect()
        }
    }

    #[test]
    fn bfs_linear_chain_target_path() {
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 10, "X");
        g.add_e(2, 3, 1.0, 20, "X");
        let mut b = crate::budget::UnlimitedBudget;
        let r = bfs(
            &g,
            1,
            &BfsConfig {
                target: Some(3),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert_eq!(r.path, Some(vec![1, 2, 3]));
    }

    #[test]
    fn bfs_label_and_temporal_filters_apply() {
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 10, "A");
        g.add_e(1, 3, 1.0, 100, "B");
        let mut b = crate::budget::UnlimitedBudget;
        let r = bfs(
            &g,
            1,
            &BfsConfig {
                edge_label: Some("A".into()),
                ts_range: Some(TimestampRange {
                    start: Some(5),
                    end: Some(20),
                }),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert!(r.visited.contains(&2));
        assert!(!r.visited.contains(&3));
    }

    #[test]
    fn bfs_budget_exhaustion() {
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 1, "X");
        g.add_e(2, 3, 1.0, 1, "X");
        let mut b = CountingBudget::new(0);
        let err = bfs(&g, 1, &BfsConfig::default(), &mut b).unwrap_err();
        assert!(matches!(err, GleaphError::BudgetExhausted));
    }

    #[test]
    fn bfs_continuation_resumes_from_checkpoint() {
        // 5-node chain: 1->2->3->4->5
        let mut g = MockGraph::default();
        for i in 1..5u32 {
            g.add_e(i, i + 1, 1.0, 1, "X");
        }
        let config = BfsConfig::default();

        // First call with tight budget (2 vertices processed)
        let mut b = CountingBudget::new(2);
        let outcome = bfs_resumable(&g, 1, &config, &mut b).unwrap();
        let cp = match outcome {
            AlgoOutcome::Suspended {
                partial,
                checkpoint,
            } => {
                assert!(!partial.visited.is_empty());
                assert!(partial.visited.len() < 5);
                checkpoint
            }
            AlgoOutcome::Done(_) => panic!("expected suspension with budget=2"),
        };

        // Resume loop until done
        let mut checkpoint = cp;
        let final_result = loop {
            let mut b = CountingBudget::new(2);
            match bfs_resume(&g, checkpoint, &mut b).unwrap() {
                AlgoOutcome::Done(r) => break r,
                AlgoOutcome::Suspended {
                    checkpoint: next, ..
                } => checkpoint = next,
            }
        };
        assert_eq!(final_result.visited.len(), 5);
        assert_eq!(final_result.visited, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn bfs_resumable_matches_non_resumable() {
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 1, "X");
        g.add_e(2, 3, 1.0, 1, "X");
        g.add_e(1, 3, 1.0, 1, "X");
        let config = BfsConfig {
            target: Some(3),
            ..Default::default()
        };

        let mut b1 = crate::budget::UnlimitedBudget;
        let normal = bfs(&g, 1, &config, &mut b1).unwrap();

        let mut b2 = crate::budget::UnlimitedBudget;
        let resumable = match bfs_resumable(&g, 1, &config, &mut b2).unwrap() {
            AlgoOutcome::Done(r) => r,
            _ => panic!("should complete with unlimited budget"),
        };
        assert_eq!(normal, resumable);
    }

    // ---- Bidirectional BFS tests ----

    #[test]
    fn bfs_bidirectional_linear_chain() {
        // 1→2→3→4→5
        let mut g = MockGraph::default();
        for i in 1..5u32 {
            g.add_e(i, i + 1, 1.0, 1, "X");
        }
        let mut b = crate::budget::UnlimitedBudget;
        let r = bfs_bidirectional(&g, 1, &[5], &BfsConfig::default(), &mut b).unwrap();
        assert_eq!(r.path, Some(vec![1, 2, 3, 4, 5]));
    }

    #[test]
    fn bfs_bidirectional_diamond() {
        //   1→2→4
        //   1→3→4
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 1, "X");
        g.add_e(1, 3, 1.0, 1, "X");
        g.add_e(2, 4, 1.0, 1, "X");
        g.add_e(3, 4, 1.0, 1, "X");
        let mut b = crate::budget::UnlimitedBudget;
        let r = bfs_bidirectional(&g, 1, &[4], &BfsConfig::default(), &mut b).unwrap();
        let path = r.path.unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], 1);
        assert_eq!(*path.last().unwrap(), 4);
        // Middle must be 2 or 3 (both valid shortest).
        assert!(path[1] == 2 || path[1] == 3);
    }

    #[test]
    fn bfs_bidirectional_no_path_returns_none() {
        // Disconnected: 1→2, 3→4  (no path 1→4)
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 1, "X");
        g.add_e(3, 4, 1.0, 1, "X");
        let mut b = crate::budget::UnlimitedBudget;
        let r = bfs_bidirectional(&g, 1, &[4], &BfsConfig::default(), &mut b).unwrap();
        assert!(r.path.is_none());
    }

    #[test]
    fn bfs_bidirectional_budget_exhausted() {
        let mut g = MockGraph::default();
        for i in 1..10u32 {
            g.add_e(i, i + 1, 1.0, 1, "X");
        }
        let mut b = CountingBudget::new(0);
        let err = bfs_bidirectional(&g, 1, &[10], &BfsConfig::default(), &mut b).unwrap_err();
        assert!(matches!(err, GleaphError::BudgetExhausted));
    }

    #[test]
    fn bfs_bidirectional_with_edge_label_filter() {
        // 1→2 (label "A"), 2→3 (label "B"), 1→4 (label "A"), 4→3 (label "A")
        // With label filter "A", path should go 1→4→3 (not through edge 2→3 which is "B").
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 1, "A");
        g.add_e(2, 3, 1.0, 1, "B");
        g.add_e(1, 4, 1.0, 1, "A");
        g.add_e(4, 3, 1.0, 1, "A");
        let mut b = crate::budget::UnlimitedBudget;
        let r = bfs_bidirectional(
            &g,
            1,
            &[3],
            &BfsConfig {
                edge_label: Some("A".into()),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert_eq!(r.path, Some(vec![1, 4, 3]));
    }

    #[test]
    fn bfs_bidirectional_with_ts_range_filter() {
        // 1→2 (ts=100), 2→3 (ts=200), 1→4 (ts=150), 4→3 (ts=180)
        // ts range [100, 190]: edge 2→3 (ts=200) excluded → path 1→4→3
        let mut g = MockGraph::default();
        g.add_e(1, 2, 1.0, 100, "X");
        g.add_e(2, 3, 1.0, 200, "X");
        g.add_e(1, 4, 1.0, 150, "X");
        g.add_e(4, 3, 1.0, 180, "X");
        let mut b = crate::budget::UnlimitedBudget;
        let r = bfs_bidirectional(
            &g,
            1,
            &[3],
            &BfsConfig {
                ts_range: Some(TimestampRange {
                    start: Some(100),
                    end: Some(190),
                }),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert_eq!(r.path, Some(vec![1, 4, 3]));
    }
}
