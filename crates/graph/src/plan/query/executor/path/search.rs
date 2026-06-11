use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use gleaph_gql::Value;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};
use gleaph_graph_kernel::entry::EdgeLabelId;
use ic_stable_lara::VertexId;
use nohash_hasher::IntSet;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

use super::{
    PathSearchNode, ShortestExpandOptions, ShortestFixedLabelExpand, ShortestPathSearchResult,
};
use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::EdgeSequenceOrder;
use crate::plan::query::executor::expand::{
    ExpandDst, edge_binding_matches_label_expr, expand_candidates_into,
};

struct HopQueueEntry {
    depth: u64,
    tie: u64,
    state_idx: usize,
}

impl Eq for HopQueueEntry {}

impl PartialEq for HopQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.depth == other.depth && self.tie == other.tie
    }
}

impl Ord for HopQueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.depth.cmp(&other.depth).reverse() {
            Ordering::Equal => other.tie.cmp(&self.tie),
            non_eq => non_eq,
        }
    }
}

impl PartialOrd for HopQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn shortest_paths_between(
    store: &GraphStore,
    src: VertexId,
    dst: VertexId,
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    label_expr: Option<&LabelExpr>,
    execution: &GqlExecutionContext,
    var_len: &Option<VarLenSpec>,
    mode: ShortestMode,
    store_hop_edges: bool,
    load_edge_payloads: bool,
) -> Result<ShortestPathSearchResult, PlanQueryError> {
    if let Some(k) = mode.shortest_k_limit() {
        return shortest_k_hop_paths_between(
            store,
            src,
            dst,
            direction,
            label_id,
            label_expr,
            execution,
            var_len,
            k,
            store_hop_edges,
            load_edge_payloads,
        );
    }

    let bounds = var_len.unwrap_or(VarLenSpec {
        min: 1,
        max: Some(1),
    });
    let vertex_count = u64::from(u32::from(store.vertex_count()));
    let max_hops = bounds.max.unwrap_or_else(|| vertex_count.saturating_sub(1));

    let mut found_depth = None;
    let mut found = Vec::new();
    let mut any_visited = if matches!(mode, ShortestMode::AnyShortest) && bounds.min <= 1 {
        let mut visited = IntSet::default();
        visited.insert(u32::from(src));
        Some(visited)
    } else {
        None
    };
    let mut states = vec![PathSearchNode {
        current: src,
        previous: None,
        edge: None,
        depth: 0,
    }];
    let mut queue = vec![0usize];
    let mut queue_head = 0usize;
    let mut candidates = Vec::new();
    let fixed_label_expand = match label_id {
        Some(lid) => Some(ShortestFixedLabelExpand::new(direction, lid)?),
        None => None,
    };

    while queue_head < queue.len() {
        let state_idx = queue[queue_head];
        queue_head += 1;
        let current = states[state_idx].current;
        let depth = states[state_idx].depth;
        if found_depth.is_some_and(|d| depth > d) {
            break;
        }
        if depth >= bounds.min && current == dst {
            found_depth = Some(depth);
            found.push(state_idx);
            if matches!(mode, ShortestMode::AnyShortest) {
                break;
            }
            continue;
        }
        if found_depth.is_some_and(|d| depth >= d) {
            continue;
        }
        if depth >= max_hops {
            continue;
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _expand_scope = bench_scope("shortest_bfs_expand");
        candidates.clear();
        match fixed_label_expand {
            Some(prep) => prep.expand_into(
                store,
                current,
                &mut candidates,
                ShortestExpandOptions {
                    load_payloads: load_edge_payloads,
                    payload_scratch: None,
                },
            )?,
            None => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _generic_scope = bench_scope("shortest_bfs_expand_generic");
                expand_candidates_into(
                    store,
                    &crate::gql_execution_context::GqlExecutionContext::default(),
                    current,
                    direction,
                    label_id,
                    EdgeSequenceOrder::Descending,
                    None,
                    None,
                    None,
                    &BTreeMap::new(),
                    &mut candidates,
                )?;
            }
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _relax_scope = bench_scope("shortest_bfs_relax_neighbors");
        for (edge_dst, edge_binding) in candidates.iter().cloned() {
            if let Some(expr) = label_expr
                && !edge_binding_matches_label_expr(execution, expr, &edge_binding)
            {
                continue;
            }
            let ExpandDst::Local(next) = edge_dst else {
                continue;
            };
            let next_depth = depth + 1;
            if let Some(visited) = any_visited.as_mut() {
                if !visited.insert(u32::from(next)) {
                    continue;
                }
            } else if path_search_contains_vertex(&states, state_idx, next) {
                continue;
            }
            let next_state_idx = states.len();
            states.push(PathSearchNode {
                current: next,
                previous: Some(state_idx),
                edge: store_hop_edges.then_some(edge_binding),
                depth: next_depth,
            });
            if next == dst && next_depth >= bounds.min {
                if matches!(mode, ShortestMode::AnyShortest) {
                    return Ok(ShortestPathSearchResult {
                        states,
                        found: vec![next_state_idx],
                    });
                }
                found_depth = Some(next_depth);
                found.push(next_state_idx);
                continue;
            }
            queue.push(next_state_idx);
        }
    }

    Ok(ShortestPathSearchResult { states, found })
}

fn shortest_k_hop_paths_between(
    store: &GraphStore,
    src: VertexId,
    dst: VertexId,
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    label_expr: Option<&LabelExpr>,
    execution: &GqlExecutionContext,
    var_len: &Option<VarLenSpec>,
    k: u64,
    store_hop_edges: bool,
    load_edge_payloads: bool,
) -> Result<ShortestPathSearchResult, PlanQueryError> {
    let k = usize::try_from(k).map_err(|_| PlanQueryError::InvalidLimit {
        value: Value::Uint64(k),
    })?;
    let bounds = var_len.unwrap_or(VarLenSpec {
        min: 1,
        max: Some(1),
    });
    let vertex_count = u64::from(u32::from(store.vertex_count()));
    let max_hops = bounds.max.unwrap_or_else(|| vertex_count.saturating_sub(1));

    let mut states = vec![PathSearchNode {
        current: src,
        previous: None,
        edge: None,
        depth: 0,
    }];
    if k == 0 {
        return Ok(ShortestPathSearchResult {
            states,
            found: Vec::new(),
        });
    }

    let mut heap = BinaryHeap::new();
    let mut tie = 0u64;
    heap.push(HopQueueEntry {
        depth: 0,
        tie,
        state_idx: 0,
    });

    let mut found = Vec::with_capacity(k.min(8));
    let mut candidates = Vec::new();
    let fixed_label_expand = match label_id {
        Some(lid) => Some(ShortestFixedLabelExpand::new(direction, lid)?),
        None => None,
    };

    while let Some(entry) = heap.pop() {
        if found.len() >= k {
            break;
        }
        let state_idx = entry.state_idx;
        let current = states[state_idx].current;
        let depth = states[state_idx].depth;
        if depth >= bounds.min && current == dst {
            found.push(state_idx);
            continue;
        }
        if depth >= max_hops {
            continue;
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _expand_scope = bench_scope("shortest_k_expand");
        candidates.clear();
        match fixed_label_expand {
            Some(prep) => prep.expand_into(
                store,
                current,
                &mut candidates,
                ShortestExpandOptions {
                    load_payloads: load_edge_payloads,
                    payload_scratch: None,
                },
            )?,
            None => {
                expand_candidates_into(
                    store,
                    &crate::gql_execution_context::GqlExecutionContext::default(),
                    current,
                    direction,
                    label_id,
                    EdgeSequenceOrder::Descending,
                    None,
                    None,
                    None,
                    &BTreeMap::new(),
                    &mut candidates,
                )?;
            }
        }

        for (edge_dst, edge_binding) in candidates.iter().cloned() {
            if let Some(expr) = label_expr
                && !edge_binding_matches_label_expr(execution, expr, &edge_binding)
            {
                continue;
            }
            let ExpandDst::Local(next) = edge_dst else {
                continue;
            };
            if path_search_contains_vertex(&states, state_idx, next) {
                continue;
            }
            let next_depth = depth + 1;
            tie += 1;
            let next_state_idx = states.len();
            states.push(PathSearchNode {
                current: next,
                previous: Some(state_idx),
                edge: store_hop_edges.then_some(edge_binding),
                depth: next_depth,
            });
            heap.push(HopQueueEntry {
                depth: next_depth,
                tie,
                state_idx: next_state_idx,
            });
        }
    }

    Ok(ShortestPathSearchResult { states, found })
}

pub(crate) fn path_search_contains_vertex(
    states: &[PathSearchNode],
    mut state_idx: usize,
    vertex: VertexId,
) -> bool {
    loop {
        let state = &states[state_idx];
        if state.current == vertex {
            return true;
        }
        let Some(previous) = state.previous else {
            return false;
        };
        state_idx = previous;
    }
}
