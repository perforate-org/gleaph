use crate::{GraphView, budget::InstructionBudget};
use candid::CandidType;
use gleaph_types::{GleaphError, Recommendation, TimestampRange, VertexIdSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub struct RecommendConfig {
    pub edge_label: String,
    pub max_hops: u8,
    pub limit: u32,
    pub ts_range: Option<TimestampRange>,
    pub exclude_known: bool,
}

impl Default for RecommendConfig {
    fn default() -> Self {
        Self {
            edge_label: "FOLLOW".into(),
            max_hops: 2,
            limit: 10,
            ts_range: None,
            exclude_known: true,
        }
    }
}

pub fn recommend<G: GraphView>(
    graph: &G,
    user: u32,
    config: &RecommendConfig,
    budget: &mut dyn InstructionBudget,
) -> Result<Vec<Recommendation>, GleaphError> {
    if !graph.is_vertex_active(user) {
        return Err(GleaphError::VertexNotFound(user));
    }
    if config.max_hops < 2 {
        return Err(GleaphError::AlgorithmError(
            "recommend requires max_hops >= 2".into(),
        ));
    }
    let label_matches = |src: u32, dst: u32| {
        config.edge_label.is_empty() || graph.edge_has_label(src, dst, &config.edge_label)
    };

    let owned: VertexIdSet = graph
        .neighbors_filtered(user, config.ts_range.clone())
        .into_iter()
        .filter(|(dst, _, _)| label_matches(user, *dst))
        .map(|(dst, _, _)| dst)
        .collect();

    if config.max_hops == 2 {
        return recommend_two_hop(graph, user, config, budget, &owned, &label_matches);
    }

    // frontier maps each node to ALL paths reaching it, so that multiple paths
    // through the same intermediate node are each explored independently.
    let mut frontier: BTreeMap<u32, Vec<Vec<u32>>> = BTreeMap::new();
    for item in owned.iter() {
        frontier.entry(item).or_default().push(vec![user, item]);
    }

    let mut scores: BTreeMap<u32, (f64, Vec<u32>)> = BTreeMap::new();
    for depth in 1..=config.max_hops {
        let reverse_step = depth % 2 == 1;
        let mut next: BTreeMap<u32, Vec<Vec<u32>>> = BTreeMap::new();
        for (node, paths) in frontier {
            budget.consume(1)?;
            let neighbors = if reverse_step {
                graph
                    .reverse_neighbors(node)
                    .into_iter()
                    .filter(|(_, _, ts)| crate::ts_in_range(*ts, config.ts_range.as_ref()))
                    .collect::<Vec<_>>()
            } else {
                graph.neighbors_filtered(node, config.ts_range.clone())
            };
            for (next_node, w, _ts) in neighbors {
                let (src, dst) = if reverse_step {
                    (next_node, node)
                } else {
                    (node, next_node)
                };
                if !label_matches(src, dst) || !graph.is_vertex_active(next_node) {
                    continue;
                }
                for path in &paths {
                    if path.contains(&next_node) {
                        continue;
                    }
                    let mut next_path = path.clone();
                    next_path.push(next_node);
                    if !(reverse_step || config.exclude_known && owned.contains(next_node)) {
                        let entry = scores.entry(next_node).or_insert((0.0, next_path.clone()));
                        entry.0 += f64::from(w.max(1.0));
                        if next_path.len() < entry.1.len() {
                            entry.1 = next_path.clone();
                        }
                    }
                    next.entry(next_node).or_default().push(next_path);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() && depth < config.max_hops {
            break;
        }
    }

    let mut out: Vec<_> = scores
        .into_iter()
        .map(|(vertex_id, (score, path))| Recommendation {
            vertex_id,
            score,
            path,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.vertex_id.cmp(&b.vertex_id))
    });
    out.truncate(config.limit as usize);
    Ok(out)
}

fn recommend_two_hop<G: GraphView, F: Fn(u32, u32) -> bool>(
    graph: &G,
    user: u32,
    config: &RecommendConfig,
    budget: &mut dyn InstructionBudget,
    owned: &VertexIdSet,
    label_matches: &F,
) -> Result<Vec<Recommendation>, GleaphError> {
    let mut scores: BTreeMap<u32, (f64, Vec<u32>)> = BTreeMap::new();

    for owned_item in owned.iter() {
        budget.consume(1)?;
        for (other_user, _w, ts) in graph.reverse_neighbors(owned_item) {
            if !crate::ts_in_range(ts, config.ts_range.as_ref())
                || !label_matches(other_user, owned_item)
                || !graph.is_vertex_active(other_user)
            {
                continue;
            }
            for (candidate, w, _ts) in graph.neighbors_filtered(other_user, config.ts_range.clone())
            {
                if !label_matches(other_user, candidate) || !graph.is_vertex_active(candidate) {
                    continue;
                }
                if candidate == user
                    || candidate == owned_item
                    || candidate == other_user
                    || (config.exclude_known && owned.contains(candidate))
                {
                    continue;
                }
                let path = vec![user, owned_item, other_user, candidate];
                let entry = scores.entry(candidate).or_insert((0.0, path.clone()));
                entry.0 += f64::from(w.max(1.0));
                if path.len() < entry.1.len() {
                    entry.1 = path;
                }
            }
        }
    }

    let mut out: Vec<_> = scores
        .into_iter()
        .map(|(vertex_id, (score, path))| Recommendation {
            vertex_id,
            score,
            path,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.vertex_id.cmp(&b.vertex_id))
    });
    out.truncate(config.limit as usize);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphView, budget::CountingBudget};
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct G {
        adj: BTreeMap<u32, Vec<(u32, f32, u64)>>,
        labels: BTreeMap<(u32, u32), String>,
        active: BTreeSet<u32>,
    }
    impl G {
        fn e(&mut self, s: u32, d: u32, l: &str) {
            self.e_ts(s, d, l, 1);
        }
        fn e_ts(&mut self, s: u32, d: u32, l: &str, ts: u64) {
            self.active.insert(s);
            self.active.insert(d);
            self.adj.entry(s).or_default().push((d, 1.0, ts));
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
        fn neighbors(&self, v: u32) -> Vec<(u32, f32, u64)> {
            self.adj.get(&v).cloned().unwrap_or_default()
        }
        fn neighbors_filtered(&self, v: u32, r: Option<TimestampRange>) -> Vec<(u32, f32, u64)> {
            self.neighbors(v)
                .into_iter()
                .filter(|(_, _, ts)| crate::ts_in_range(*ts, r.as_ref()))
                .collect()
        }
        fn reverse_neighbors(&self, t: u32) -> Vec<(u32, f32, u64)> {
            let mut o = vec![];
            for (&s, ns) in &self.adj {
                for &(d, w, ts) in ns {
                    if d == t {
                        o.push((s, w, ts));
                    }
                }
            }
            o
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
    fn two_hop_also_bought() {
        let mut g = G::default();
        g.e(1, 10, "FOLLOW");
        g.e(2, 10, "FOLLOW");
        g.e(2, 20, "FOLLOW");
        let mut b = crate::budget::UnlimitedBudget;
        let r = recommend(&g, 1, &RecommendConfig::default(), &mut b).unwrap();
        assert_eq!(r.first().map(|x| x.vertex_id), Some(20));
    }

    #[test]
    fn supports_multi_hop_temporal_and_exclude_known() {
        let mut g = G::default();
        g.e_ts(1, 10, "FOLLOW", 1);
        g.e_ts(2, 10, "FOLLOW", 1);
        g.e_ts(2, 20, "FOLLOW", 1);
        g.e_ts(3, 20, "FOLLOW", 1);
        g.e_ts(3, 30, "FOLLOW", 99);
        g.e_ts(2, 40, "FOLLOW", 1);

        let mut b = crate::budget::UnlimitedBudget;
        let r = recommend(
            &g,
            1,
            &RecommendConfig {
                max_hops: 2,
                ts_range: Some(TimestampRange {
                    start: Some(0),
                    end: Some(10),
                }),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert!(r.iter().any(|x| x.vertex_id == 40));
        assert!(r.iter().all(|x| x.vertex_id != 30));
        assert!(r.iter().all(|x| x.vertex_id != 10));

        let mut b = crate::budget::UnlimitedBudget;
        let incl = recommend(
            &g,
            1,
            &RecommendConfig {
                exclude_known: false,
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert!(incl.iter().any(|x| x.vertex_id == 10 || x.vertex_id == 20));
    }

    #[test]
    fn empty_graph_and_budget_exhaustion() {
        let g = G::default();
        let mut b = crate::budget::UnlimitedBudget;
        let err = recommend(&g, 1, &RecommendConfig::default(), &mut b).unwrap_err();
        assert!(matches!(err, GleaphError::VertexNotFound(1)));

        let mut g = G::default();
        g.e(1, 10, "FOLLOW");
        g.e(2, 10, "FOLLOW");
        g.e(2, 20, "FOLLOW");
        let mut b = CountingBudget::new(0);
        let err = recommend(&g, 1, &RecommendConfig::default(), &mut b).unwrap_err();
        assert!(matches!(err, GleaphError::BudgetExhausted));
    }

    #[test]
    fn empty_edge_label_treats_edges_as_wildcard() {
        let mut g = G::default();
        g.e(1, 10, "A");
        g.e(2, 10, "B");
        g.e(2, 20, "C");
        let mut b = crate::budget::UnlimitedBudget;
        let r = recommend(
            &g,
            1,
            &RecommendConfig {
                edge_label: "".into(),
                ..Default::default()
            },
            &mut b,
        )
        .unwrap();
        assert!(r.iter().any(|x| x.vertex_id == 20));
    }
}
