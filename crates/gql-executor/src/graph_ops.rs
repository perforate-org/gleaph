use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::PathElement;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::plan::{ScanValue, ShortestMode, VarLenSpec, WcojEdge};
use gleaph_graph_kernel::{EdgeId, EdgeRecord, GraphRead, NodeId};
use rapidhash::{HashMapExt, RapidHashMap};

use super::{
    BindingRow, BindingValue, ExecutionContext, ExecutionError, ExecutionResultExt,
    ExpandIndexSpec, ExpandSpec, ShortestBfsSpec, ShortestPathBfsState, ShortestPathPredecessors,
    ShortestPathSpec, WcojDfsSpec, WcojDfsState, edge_label_filter_for_expand,
    edge_satisfies_expand_labels, eval_expr, exec_property_filter, resolve_scan_value,
    wcoj_edge_satisfies_labels,
};

/// Upper bound on hop count for unbounded shortest-path patterns (safety).
const SHORTEST_PATH_MAX_HOPS: u32 = 4096;
/// Cap on rows when enumerating all equal-length shortest paths.
const SHORTEST_PATH_ENUM_CAP: usize = 10_000;
/// Safety cap for WCOJ row enumeration per input row.
const WCOJ_MAX_ROWS_PER_INPUT: usize = 50_000;
const WCOJ_VARLEN_MAX_DEPTH: u32 = 64;
const WCOJ_VARLEN_MAX_STATES: usize = 200_000;
const EXPAND_VAR_LEN_MAX_FRONTIER_STATES: usize = 200_000;

pub(crate) fn insert_hop_aux_binding(
    row: &mut BindingRow,
    binding_key: Rc<str>,
    aux_bytes: Option<Vec<u8>>,
) {
    let v = match aux_bytes {
        Some(b) => Value::Bytes(b),
        None => Value::Null,
    };
    row.insert(binding_key, BindingValue::Scalar(v));
}

fn dst_filter_holds<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    dst_filter: &[Expr],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<bool> {
    for pred in dst_filter {
        match eval_expr(graph, row, pred, ctx)? {
            Value::Bool(true) => {}
            Value::Bool(false) | Value::Null => return Ok(false),
            _ => {
                return Err(ExecutionError::TypeMismatch(
                    "ExpandFilter.dst_filter predicate must be boolean",
                ));
            }
        }
    }
    Ok(true)
}

/// Split for variable-length expand so the hot loop does not branch on `hop_aux` presence.
trait VarLenHopAuxBinding {
    fn bind_min_h_zero(&self, row: &mut BindingRow);
    fn bind_after_expand(&self, row: &mut BindingRow, shard_principal: Option<Vec<u8>>);
}

struct VarLenNoHopAux;

impl VarLenHopAuxBinding for VarLenNoHopAux {
    fn bind_min_h_zero(&self, _row: &mut BindingRow) {}
    fn bind_after_expand(&self, _row: &mut BindingRow, _shard_principal: Option<Vec<u8>>) {}
}

struct VarLenYesHopAux(Rc<str>);

impl VarLenHopAuxBinding for VarLenYesHopAux {
    fn bind_min_h_zero(&self, row: &mut BindingRow) {
        insert_hop_aux_binding(row, self.0.clone(), None);
    }

    fn bind_after_expand(&self, row: &mut BindingRow, shard_principal: Option<Vec<u8>>) {
        insert_hop_aux_binding(row, self.0.clone(), shard_principal);
    }
}

fn exec_expand_var_len_impl<const HAS_DST_FILTER: bool, G: GraphRead, B: VarLenHopAuxBinding>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandSpec<'_>,
    var_len: &VarLenSpec,
    indexed_edge_equality: Option<(&str, &ScanValue)>,
    ctx: &ExecutionContext,
    hop_binding: B,
    dst_filter: &[Expr],
) -> ExecutionResultExt<Vec<BindingRow>> {
    let (min_h, max_h_cap) = shortest_hop_bounds(Some(var_len));
    if max_h_cap < min_h {
        return Ok(Vec::new());
    }

    let mut name_scratch = Vec::new();
    let (filter, post_filter) =
        edge_label_filter_for_expand(spec.label, spec.label_expr, &mut name_scratch);

    let indexed_property = indexed_edge_equality.map(|(prop, scan_val)| {
        let resolved = resolve_scan_value(scan_val, ctx)?;
        Ok::<(&str, Value), ExecutionError>((prop, resolved))
    });
    let indexed_property = match indexed_property {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Err(e),
        None => None,
    };

    let edge_names = spec.edge_property_names.as_deref();
    let dst_names = spec.dst_property_names.as_deref();

    let mut out = Vec::new();
    const MAX_OUTPUT_PER_INPUT_ROW: usize = SHORTEST_PATH_ENUM_CAP;

    for row in input {
        let src_node = match row.get(spec.src) {
            Some(BindingValue::Node(node)) => node,
            Some(_) => return Err(ExecutionError::TypeMismatch("expand source must be a node")),
            None => return Err(ExecutionError::MissingBinding(spec.src.to_owned())),
        };

        let mut emitted_this_row = 0usize;
        let mut frontier = vec![src_node.id];

        if min_h == 0 {
            let mut next = row.clone();
            next.insert(
                Rc::<str>::from(spec.edge),
                BindingValue::Scalar(Value::Null),
            );
            next.insert(
                Rc::<str>::from(spec.dst),
                BindingValue::Node(src_node.clone()),
            );
            hop_binding.bind_min_h_zero(&mut next);
            let emit = if HAS_DST_FILTER {
                dst_filter.is_empty() || dst_filter_holds(graph, &next, dst_filter, ctx)?
            } else {
                true
            };
            if emit {
                out.push(next);
                emitted_this_row += 1;
            }
        }

        if max_h_cap == 0 {
            continue;
        }

        'depths: for depth in 1..=max_h_cap {
            if frontier.is_empty() || emitted_this_row >= MAX_OUTPUT_PER_INPUT_ROW {
                break 'depths;
            }

            let mut next_frontier = Vec::new();
            for from in frontier {
                if next_frontier.len() >= EXPAND_VAR_LEN_MAX_FRONTIER_STATES {
                    break;
                }
                let hops = graph.expand_hops_with_shard_meta(
                    from,
                    spec.direction,
                    filter,
                    edge_names,
                    dst_names,
                )?;
                for hop in hops {
                    let exp = hop.expansion;
                    if post_filter
                        && !edge_satisfies_expand_labels(
                            exp.edge.label.as_deref(),
                            spec.label,
                            spec.label_expr,
                        )
                    {
                        continue;
                    }
                    if let Some((prop, resolved_value)) = &indexed_property
                        && !expand_indexed_edge_property_matches(&exp.edge, prop, resolved_value)
                    {
                        continue;
                    }

                    if depth >= min_h {
                        let mut next = row.clone();
                        next.insert(
                            Rc::<str>::from(spec.edge),
                            BindingValue::Edge(exp.edge.clone()),
                        );
                        next.insert(
                            Rc::<str>::from(spec.dst),
                            BindingValue::Node(exp.node.clone()),
                        );
                        hop_binding.bind_after_expand(&mut next, hop.shard_canister_principal);
                        let emit = if HAS_DST_FILTER {
                            dst_filter.is_empty() || dst_filter_holds(graph, &next, dst_filter, ctx)?
                        } else {
                            true
                        };
                        if emit {
                            out.push(next);
                            emitted_this_row += 1;
                            if emitted_this_row >= MAX_OUTPUT_PER_INPUT_ROW {
                                break 'depths;
                            }
                        }
                    }

                    if next_frontier.len() < EXPAND_VAR_LEN_MAX_FRONTIER_STATES {
                        next_frontier.push(exp.node.id);
                    } else {
                        break;
                    }
                }
            }
            frontier = next_frontier;
        }
    }

    Ok(out)
}

fn exec_expand_no_hop_aux<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandSpec<'_>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut name_scratch = Vec::new();
    let (filter, post_filter) =
        edge_label_filter_for_expand(spec.label, spec.label_expr, &mut name_scratch);
    let edge_names = spec.edge_property_names.as_deref();
    let dst_names = spec.dst_property_names.as_deref();
    let n_in = input.len();
    let mut out = Vec::new();
    let mut reserved = false;
    for row in input {
        let src_node = match row.get(spec.src) {
            Some(BindingValue::Node(node)) => node,
            Some(_) => return Err(ExecutionError::TypeMismatch("expand source must be a node")),
            None => return Err(ExecutionError::MissingBinding(spec.src.to_owned())),
        };

        let hops = graph.expand_hops_with_shard_meta(
            src_node.id,
            spec.direction,
            filter,
            edge_names,
            dst_names,
        )?;

        if !reserved {
            out.reserve(hops.len().saturating_mul(n_in));
            reserved = true;
        }

        for hop in hops {
            let expansion = hop.expansion;
            if post_filter
                && !edge_satisfies_expand_labels(
                    expansion.edge.label.as_deref(),
                    spec.label,
                    spec.label_expr,
                )
            {
                continue;
            }
            let mut next = row.clone();
            next.insert(
                Rc::<str>::from(spec.edge),
                BindingValue::Edge(expansion.edge),
            );
            next.insert(
                Rc::<str>::from(spec.dst),
                BindingValue::Node(expansion.node),
            );
            out.push(next);
        }
    }
    Ok(out)
}

fn exec_expand_with_hop_aux<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandSpec<'_>,
    hop_aux_key: Rc<str>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut name_scratch = Vec::new();
    let (filter, post_filter) =
        edge_label_filter_for_expand(spec.label, spec.label_expr, &mut name_scratch);
    let edge_names = spec.edge_property_names.as_deref();
    let dst_names = spec.dst_property_names.as_deref();
    let n_in = input.len();
    let mut out = Vec::new();
    let mut reserved = false;
    for row in input {
        let src_node = match row.get(spec.src) {
            Some(BindingValue::Node(node)) => node,
            Some(_) => return Err(ExecutionError::TypeMismatch("expand source must be a node")),
            None => return Err(ExecutionError::MissingBinding(spec.src.to_owned())),
        };

        let hops = graph.expand_hops_with_shard_meta(
            src_node.id,
            spec.direction,
            filter,
            edge_names,
            dst_names,
        )?;

        if !reserved {
            out.reserve(hops.len().saturating_mul(n_in));
            reserved = true;
        }

        for hop in hops {
            let expansion = hop.expansion;
            if post_filter
                && !edge_satisfies_expand_labels(
                    expansion.edge.label.as_deref(),
                    spec.label,
                    spec.label_expr,
                )
            {
                continue;
            }
            let mut next = row.clone();
            next.insert(
                Rc::<str>::from(spec.edge),
                BindingValue::Edge(expansion.edge),
            );
            next.insert(
                Rc::<str>::from(spec.dst),
                BindingValue::Node(expansion.node),
            );
            insert_hop_aux_binding(
                &mut next,
                hop_aux_key.clone(),
                hop.shard_canister_principal,
            );
            out.push(next);
        }
    }
    Ok(out)
}

pub(crate) fn exec_expand<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandSpec<'_>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    match spec.hop_aux_var {
        None => exec_expand_no_hop_aux(graph, input, spec),
        Some(name) => exec_expand_with_hop_aux(graph, input, spec, Rc::from(name)),
    }
}

fn shortest_hop_bounds(var_len: Option<&VarLenSpec>) -> (u32, u32) {
    match var_len {
        None => (1u32, 1u32),
        Some(v) => {
            let min_h = u32::try_from(v.min).unwrap_or(SHORTEST_PATH_MAX_HOPS);
            let max_h = v
                .max
                .map(|m| u32::try_from(m).unwrap_or(SHORTEST_PATH_MAX_HOPS))
                .unwrap_or(SHORTEST_PATH_MAX_HOPS);
            let min_h = min_h.min(SHORTEST_PATH_MAX_HOPS);
            let max_h = max_h.min(SHORTEST_PATH_MAX_HOPS).max(min_h);
            (min_h, max_h)
        }
    }
}

fn shortest_bfs_multi_pred<G: GraphRead>(
    graph: &G,
    spec: ShortestBfsSpec<'_, '_>,
) -> ExecutionResultExt<ShortestPathBfsState> {
    let mut dist: HashMap<NodeId, u32> = HashMap::new();
    let mut preds: ShortestPathPredecessors = HashMap::new();
    let mut q: VecDeque<NodeId> = VecDeque::new();
    dist.insert(spec.start, 0);
    preds.insert(spec.start, Vec::new());
    q.push_back(spec.start);

    while let Some(u) = q.pop_front() {
        let d_u = *dist.get(&u).unwrap();
        if d_u >= spec.max_depth {
            continue;
        }
        for hop in graph.expand_hops_with_shard_meta(u, spec.direction, spec.filter, None, None)? {
            let exp = hop.expansion;
            if spec.post_filter
                && !edge_satisfies_expand_labels(
                    exp.edge.label.as_deref(),
                    spec.label,
                    spec.label_expr,
                )
            {
                continue;
            }
            let v = exp.node.id;
            let cand = d_u.saturating_add(1);
            if cand > spec.max_depth {
                continue;
            }
            match dist.entry(v) {
                Entry::Occupied(mut e) => {
                    let dv = *e.get();
                    if cand < dv {
                        e.insert(cand);
                        preds.insert(v, vec![(u, exp.edge.clone())]);
                        q.push_back(v);
                    } else if cand == dv {
                        preds.entry(v).or_default().push((u, exp.edge.clone()));
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(cand);
                    preds.insert(v, vec![(u, exp.edge.clone())]);
                    q.push_back(v);
                }
            }
        }
    }

    Ok((dist, preds))
}

fn enumerate_shortest_paths(
    preds: &ShortestPathPredecessors,
    src: NodeId,
    dst: NodeId,
    max_paths: usize,
) -> Vec<Vec<EdgeRecord>> {
    if max_paths == 0 {
        return Vec::new();
    }
    if dst == src {
        return vec![Vec::new()];
    }
    let mut out: Vec<Vec<EdgeRecord>> = Vec::new();
    let mut stack: Vec<EdgeRecord> = Vec::new();

    fn walk(
        preds: &ShortestPathPredecessors,
        cur: NodeId,
        src: NodeId,
        stack: &mut Vec<EdgeRecord>,
        out: &mut Vec<Vec<EdgeRecord>>,
        max_paths: usize,
    ) {
        if out.len() >= max_paths {
            return;
        }
        if cur == src {
            out.push(stack.iter().rev().cloned().collect());
            return;
        }
        let Some(ps) = preds.get(&cur) else {
            return;
        };
        for (pr, e) in ps {
            stack.push(e.clone());
            walk(preds, *pr, src, stack, out, max_paths);
            stack.pop();
            if out.len() >= max_paths {
                return;
            }
        }
    }

    walk(preds, dst, src, &mut stack, &mut out, max_paths);
    out
}

pub(crate) fn exec_shortest_path<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ShortestPathSpec<'_>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let (min_h, max_h_cap) = shortest_hop_bounds(spec.var_len);
    if max_h_cap < min_h {
        return Ok(Vec::new());
    }

    let k_row_budget = match spec.mode {
        ShortestMode::ShortestK(k) => usize::try_from(k).ok(),
        _ => None,
    };
    if matches!(spec.mode, ShortestMode::ShortestK(0)) {
        return Ok(Vec::new());
    }

    let mut name_scratch = Vec::new();
    let (filter, post_filter) =
        edge_label_filter_for_expand(spec.label, spec.label_expr, &mut name_scratch);

    let mut out = Vec::new();

    for row in input {
        let src_id = match row.get(spec.src) {
            Some(BindingValue::Node(node)) => node.id,
            Some(_) => {
                return Err(ExecutionError::TypeMismatch(
                    "ShortestPath source must be a node",
                ));
            }
            None => return Err(ExecutionError::MissingBinding(spec.src.to_owned())),
        };

        let (dist, preds) = shortest_bfs_multi_pred(
            graph,
            ShortestBfsSpec {
                start: src_id,
                direction: spec.direction,
                filter,
                post_filter,
                label: spec.label,
                label_expr: spec.label_expr,
                max_depth: max_h_cap,
            },
        )?;

        let mut emitted_this_row = 0usize;
        let mut emit_path = |v: NodeId, path: Vec<EdgeRecord>| -> ExecutionResultExt<bool> {
            if let Some(limit) = k_row_budget
                && emitted_this_row >= limit
            {
                return Ok(true);
            }
            let Some(dst_rec) = graph.get_node(v)? else {
                return Ok(false);
            };
            let mut next = row.clone();
            next.insert(Rc::<str>::from(spec.dst), BindingValue::Node(dst_rec));
            let edge_bind = match path.last() {
                Some(e) => BindingValue::Edge(e.clone()),
                None => BindingValue::Scalar(Value::Null),
            };
            next.insert(Rc::<str>::from(spec.edge_var), edge_bind);

            if let Some(pv) = spec.path_var {
                let path_elems = if path.is_empty() {
                    vec![PathElement::Vertex(src_id.into())]
                } else {
                    let mut elems = Vec::with_capacity(path.len() * 2 + 1);
                    elems.push(PathElement::Vertex(src_id.into()));
                    for e in &path {
                        elems.push(PathElement::Edge {
                            src: e.src.into(),
                            dst: e.dst.into(),
                            label: e.label.clone(),
                        });
                        elems.push(PathElement::Vertex(e.dst.into()));
                    }
                    elems
                };
                next.insert(
                    Rc::<str>::from(pv),
                    BindingValue::Scalar(Value::Path(path_elems)),
                );
            }
            out.push(next);
            emitted_this_row += 1;
            Ok(false)
        };

        let mut dests: Vec<(NodeId, u32)> = dist.iter().map(|(&id, &d)| (id, d)).collect();
        dests.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

        'dest: for (v, d) in dests {
            if d < min_h || d > max_h_cap {
                continue;
            }
            let max_p = match spec.mode {
                ShortestMode::AnyShortest => 1,
                ShortestMode::AllShortest => SHORTEST_PATH_ENUM_CAP,
                ShortestMode::ShortestK(_) => SHORTEST_PATH_ENUM_CAP,
            };
            let paths = enumerate_shortest_paths(&preds, src_id, v, max_p);
            for path in paths {
                if emit_path(v, path)? {
                    break 'dest;
                }
            }
        }
    }

    Ok(out)
}

fn normalize_wcoj_ring_vars(variables: &[Rc<str>]) -> Vec<Rc<str>> {
    if variables.len() >= 2 && variables.first() == variables.last() {
        variables[..variables.len() - 1].to_vec()
    } else {
        variables.to_vec()
    }
}

fn wcoj_var_len_bounds(spec: &VarLenSpec) -> (u32, u32) {
    let min_d = u32::try_from(spec.min).unwrap_or(0);
    let max_d = spec
        .max
        .map(|m| u32::try_from(m).unwrap_or(WCOJ_VARLEN_MAX_DEPTH))
        .unwrap_or(WCOJ_VARLEN_MAX_DEPTH);
    let max_d = max_d.min(WCOJ_VARLEN_MAX_DEPTH).max(min_d);
    (min_d, max_d)
}

fn wcoj_unindexed_step_expansions<G: GraphRead>(
    graph: &G,
    from: NodeId,
    spec: &WcojEdge,
) -> ExecutionResultExt<Vec<(NodeId, EdgeRecord)>> {
    let mut v = Vec::new();
    let mut buf = Vec::new();
    let (filter, post) =
        edge_label_filter_for_expand(spec.label.as_deref(), spec.label_expr.as_ref(), &mut buf);
    for hop in graph.expand_hops_with_shard_meta(from, spec.direction, filter, None, None)? {
        let exp = hop.expansion;
        if post && !wcoj_edge_satisfies_labels(spec, exp.edge.label.as_deref()) {
            continue;
        }
        v.push((exp.node.id, exp.edge));
    }
    Ok(v)
}

fn wcoj_indexed_endpoints<G: GraphRead>(
    graph: &G,
    from: NodeId,
    spec: &WcojEdge,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<(NodeId, EdgeRecord)>> {
    let Some((prop, val)) = spec.indexed_edge_equality.as_ref() else {
        return wcoj_unindexed_step_expansions(graph, from, spec);
    };
    let resolved = resolve_scan_value(val, ctx)?;
    let mut out = Vec::new();
    for e in graph.scan_edges_by_property(prop.as_ref(), &resolved)? {
        if !wcoj_edge_satisfies_labels(spec, e.label.as_deref()) {
            continue;
        }
        if !edge_matches_expand_direction(&e, from, spec.direction) {
            continue;
        }
        out.push((far_endpoint_for_expand(&e, from), e.clone()));
    }
    Ok(out)
}

fn wcoj_segment_transitions<G: GraphRead>(
    graph: &G,
    from: NodeId,
    spec: &WcojEdge,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<(NodeId, Option<EdgeRecord>)>> {
    if spec.var_len.is_none() {
        return Ok(wcoj_indexed_endpoints(graph, from, spec, ctx)?
            .into_iter()
            .map(|(a, b)| (a, Some(b)))
            .collect());
    }

    let (min_d, max_d) = wcoj_var_len_bounds(spec.var_len.as_ref().unwrap());
    let mut results = Vec::new();
    let mut q = VecDeque::new();
    let mut seen = 0usize;
    q.push_back((from, 0u32, None::<EdgeRecord>));

    while let Some((u, d, last_e)) = q.pop_front() {
        if seen >= WCOJ_VARLEN_MAX_STATES {
            break;
        }
        seen += 1;

        if d >= min_d && d <= max_d {
            if d == 0 {
                results.push((u, None));
            } else if let Some(ref le) = last_e {
                results.push((u, Some(le.clone())));
            }
        }
        if d >= max_d {
            continue;
        }
        for (v, e) in wcoj_unindexed_step_expansions(graph, u, spec)? {
            q.push_back((v, d + 1, Some(e)));
        }
    }

    Ok(results)
}

fn wcoj_connect_last_edges<G: GraphRead>(
    graph: &G,
    from: NodeId,
    to: NodeId,
    spec: &WcojEdge,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<Option<EdgeRecord>>> {
    if spec.var_len.is_none() {
        let outs = if spec.indexed_edge_equality.is_some() {
            wcoj_indexed_endpoints(graph, from, spec, ctx)?
                .into_iter()
                .filter(|(n, _)| *n == to)
                .map(|(_, e)| Some(e))
                .collect()
        } else {
            let mut v = Vec::new();
            let mut buf = Vec::new();
            let (filter, post) = edge_label_filter_for_expand(
                spec.label.as_deref(),
                spec.label_expr.as_ref(),
                &mut buf,
            );
            for hop in graph.expand_hops_with_shard_meta(from, spec.direction, filter, None, None)? {
                let exp = hop.expansion;
                if post && !wcoj_edge_satisfies_labels(spec, exp.edge.label.as_deref()) {
                    continue;
                }
                if exp.node.id == to {
                    v.push(Some(exp.edge));
                }
            }
            v
        };
        return Ok(outs);
    }

    let (min_d, max_d) = wcoj_var_len_bounds(spec.var_len.as_ref().unwrap());
    let mut results = Vec::new();
    let mut q = VecDeque::new();
    let mut seen = 0usize;
    q.push_back((from, 0u32, None::<EdgeRecord>));

    while let Some((u, d, last_e)) = q.pop_front() {
        if seen >= WCOJ_VARLEN_MAX_STATES {
            break;
        }
        seen += 1;

        if u == to && d >= min_d && d <= max_d {
            if d == 0 {
                results.push(None);
            } else if let Some(ref le) = last_e {
                results.push(Some(le.clone()));
            }
        }
        if d >= max_d {
            continue;
        }
        for (v, e) in wcoj_unindexed_step_expansions(graph, u, spec)? {
            q.push_back((v, d + 1, Some(e)));
        }
    }

    Ok(results)
}

fn wcoj_dst_filter_holds<G: GraphRead>(
    graph: &G,
    base_row: &BindingRow,
    nodes: &HashMap<Rc<str>, NodeId>,
    dst_var: &Rc<str>,
    dst_id: NodeId,
    preds: &[Expr],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<bool> {
    if preds.is_empty() {
        return Ok(true);
    }
    let Some(dst_nr) = graph.get_node(dst_id)? else {
        return Ok(false);
    };
    let mut row = base_row.clone();
    for (vk, vid) in nodes {
        if vk == dst_var {
            continue;
        }
        let Some(nr) = graph.get_node(*vid)? else {
            return Ok(false);
        };
        row.insert(vk.clone(), BindingValue::Node(nr));
    }
    row.insert(dst_var.clone(), BindingValue::Node(dst_nr));
    for p in preds {
        match eval_expr(graph, &row, p, ctx)? {
            Value::Bool(true) => {}
            Value::Bool(false) | Value::Null => return Ok(false),
            _ => {
                return Err(ExecutionError::TypeMismatch(
                    "WCOJ dst filter expects boolean",
                ));
            }
        }
    }
    Ok(true)
}

fn wcoj_emit_row<G: GraphRead>(
    graph: &G,
    base_row: &BindingRow,
    vars: &[Rc<str>],
    edges: &[WcojEdge],
    nodes: &HashMap<Rc<str>, NodeId>,
    edgs: &HashMap<Rc<str>, Option<EdgeRecord>>,
    hop_aux_cache: &mut RapidHashMap<EdgeId, Option<Vec<u8>>>,
    out: &mut Vec<BindingRow>,
) -> ExecutionResultExt<()> {
    let mut row = base_row.clone();
    for v in vars {
        let id = *nodes
            .get(v)
            .ok_or_else(|| ExecutionError::InvalidPlan("WCOJ: missing node var".into()))?;
        let Some(nr) = graph.get_node(id)? else {
            return Ok(());
        };
        row.insert(v.clone(), BindingValue::Node(nr));
    }
    for e in edges {
        let binding = match edgs.get(&e.variable) {
            Some(Some(er)) => BindingValue::Edge(er.clone()),
            Some(None) => BindingValue::Scalar(Value::Null),
            None => {
                return Err(ExecutionError::InvalidPlan(
                    "WCOJ: missing edge binding".into(),
                ));
            }
        };
        row.insert(e.variable.clone(), binding);
    }
    for e in edges {
        let Some(name) = e.hop_aux_binding.as_ref() else {
            continue;
        };
        let Some(Some(er)) = edgs.get(&e.variable) else {
            continue;
        };
        let hop_aux = if let Some(existing) = hop_aux_cache.get(&er.id) {
            existing.clone()
        } else {
            let b = graph.hop_aux_bytes_for_edge(er.id)?;
            hop_aux_cache.insert(er.id, b.clone());
            b
        };
        insert_hop_aux_binding(&mut row, Rc::from(name.as_ref()), hop_aux);
    }
    out.push(row);
    Ok(())
}

fn wcoj_dfs<G: GraphRead>(
    graph: &G,
    spec: &WcojDfsSpec<'_>,
    hop_aux_cache: &mut RapidHashMap<EdgeId, Option<Vec<u8>>>,
    k: usize,
    state: &mut WcojDfsState<'_>,
) -> ExecutionResultExt<()> {
    if *state.budget == 0 {
        return Ok(());
    }
    if k == spec.n {
        let a = *state.nodes.get(&spec.vars[spec.n - 1]).unwrap();
        let b = *state.nodes.get(&spec.vars[0]).unwrap();
        let edge_spec = &spec.edges[spec.n - 1];
        for opt_er in wcoj_connect_last_edges(graph, a, b, edge_spec, spec.ctx)? {
            state.edgs.insert(edge_spec.variable.clone(), opt_er);
            wcoj_emit_row(
                graph,
                spec.base_row,
                spec.vars,
                spec.edges,
                state.nodes,
                state.edgs,
                hop_aux_cache,
                state.out,
            )?;
            state.edgs.remove(&edge_spec.variable);
            *state.budget = (*state.budget).saturating_sub(1);
            if *state.budget == 0 {
                return Ok(());
            }
        }
        return Ok(());
    }

    let vk = spec.vars[k].clone();

    if let Some(BindingValue::Node(nr)) = spec.base_row.get(vk.as_ref()) {
        let id = nr.id;
        if k > 0 {
            let hop = &spec.edges[k - 1];
            if wcoj_connect_last_edges(
                graph,
                *state.nodes.get(&spec.vars[k - 1]).unwrap(),
                id,
                hop,
                spec.ctx,
            )?
            .is_empty()
            {
                return Ok(());
            }
            for opt_er in wcoj_connect_last_edges(
                graph,
                *state.nodes.get(&spec.vars[k - 1]).unwrap(),
                id,
                hop,
                spec.ctx,
            )? {
                if !wcoj_dst_filter_holds(
                    graph,
                    spec.base_row,
                    state.nodes,
                    &vk,
                    id,
                    &hop.dst_filter,
                    spec.ctx,
                )? {
                    continue;
                }
                state.nodes.insert(vk.clone(), id);
                state.edgs.insert(hop.variable.clone(), opt_er);
                wcoj_dfs(graph, spec, hop_aux_cache, k + 1, state)?;
                state.edgs.remove(&hop.variable);
                state.nodes.remove(&vk);
                if *state.budget == 0 {
                    return Ok(());
                }
            }
            return Ok(());
        }
        state.nodes.insert(vk.clone(), id);
        wcoj_dfs(graph, spec, hop_aux_cache, k + 1, state)?;
        state.nodes.remove(&vk);
        return Ok(());
    }

    if k == 0 {
        for nr in graph.scan_nodes(None)? {
            state.nodes.insert(vk.clone(), nr.id);
            wcoj_dfs(graph, spec, hop_aux_cache, k + 1, state)?;
            state.nodes.remove(&vk);
            if *state.budget == 0 {
                return Ok(());
            }
        }
        return Ok(());
    }

    let prev = spec.vars[k - 1].clone();
    let pid = *state.nodes.get(&prev).unwrap();
    let hop = &spec.edges[k - 1];
    for (nid, opt_erec) in wcoj_segment_transitions(graph, pid, hop, spec.ctx)? {
        if !wcoj_dst_filter_holds(
            graph,
            spec.base_row,
            state.nodes,
            &vk,
            nid,
            &hop.dst_filter,
            spec.ctx,
        )? {
            continue;
        }
        state.nodes.insert(vk.clone(), nid);
        state.edgs.insert(hop.variable.clone(), opt_erec);
        wcoj_dfs(graph, spec, hop_aux_cache, k + 1, state)?;
        state.nodes.remove(&vk);
        state.edgs.remove(&hop.variable);
        if *state.budget == 0 {
            return Ok(());
        }
    }

    Ok(())
}

pub(crate) fn exec_worst_case_optimal_join<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    variables: &[Rc<str>],
    edges: &[WcojEdge],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let vars = normalize_wcoj_ring_vars(variables);
    let n = vars.len();
    if n < 3 || edges.len() != n {
        return Err(ExecutionError::InvalidPlan(
            "WorstCaseOptimalJoin: need |vars|>=3 and |edges|==|vars|".into(),
        ));
    }
    for i in 0..n {
        if edges[i].src.as_ref() != vars[i].as_ref()
            || edges[i].dst.as_ref() != vars[(i + 1) % n].as_ref()
        {
            return Err(ExecutionError::InvalidPlan(
                "WorstCaseOptimalJoin: edge endpoints must follow variable ring".into(),
            ));
        }
        if edges[i].var_len.is_some() && edges[i].indexed_edge_equality.is_some() {
            return Err(ExecutionError::InvalidPlan(
                "WorstCaseOptimalJoin: indexed edge equality with var_len is not allowed".into(),
            ));
        }
    }

    let mut out = Vec::new();
    for base_row in input {
        let mut nodes = HashMap::<Rc<str>, NodeId>::new();
        let mut edgs = HashMap::<Rc<str>, Option<EdgeRecord>>::new();
        let mut budget = WCOJ_MAX_ROWS_PER_INPUT;
        let mut hop_aux_cache: RapidHashMap<EdgeId, Option<Vec<u8>>> = RapidHashMap::with_capacity(8);
        let spec = WcojDfsSpec {
            base_row: &base_row,
            vars: &vars,
            edges,
            n,
            ctx,
        };
        let mut state = WcojDfsState {
            nodes: &mut nodes,
            edgs: &mut edgs,
            out: &mut out,
            budget: &mut budget,
        };
        wcoj_dfs(graph, &spec, &mut hop_aux_cache, 0, &mut state)?;
    }
    Ok(out)
}

fn edge_matches_expand_direction(
    edge: &EdgeRecord,
    from: NodeId,
    direction: gleaph_gql::types::EdgeDirection,
) -> bool {
    use gleaph_gql::types::EdgeDirection;
    match direction {
        EdgeDirection::PointingRight => edge.src == from,
        EdgeDirection::PointingLeft => edge.dst == from,
        EdgeDirection::LeftOrRight
        | EdgeDirection::Undirected
        | EdgeDirection::LeftOrUndirected
        | EdgeDirection::UndirectedOrRight
        | EdgeDirection::AnyDirection => edge.src == from || edge.dst == from,
    }
}

fn far_endpoint_for_expand(edge: &EdgeRecord, from: NodeId) -> NodeId {
    if edge.src == from { edge.dst } else { edge.src }
}

pub(crate) fn exec_expand_edge_index<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandIndexSpec<'_>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let resolved = resolve_scan_value(spec.value, ctx)?;
    let edge_proj = spec.expand.edge_property_names.as_deref();
    let candidates = match edge_proj {
        None => graph.scan_edges_by_property(spec.property, &resolved)?,
        Some(names) => graph.scan_edges_by_property_projected(spec.property, &resolved, names)?,
    };
    let dst_proj = spec.expand.dst_property_names.as_deref();
    let hop_aux_key = spec.expand.hop_aux_var.map(|s| Rc::<str>::from(s));
    let mut hop_aux_by_edge: Option<RapidHashMap<EdgeId, Option<Vec<u8>>>> = None;
    if hop_aux_key.is_some() {
        let mut map: RapidHashMap<EdgeId, Option<Vec<u8>>> =
            RapidHashMap::with_capacity(candidates.len());
        for e in &candidates {
            if map.contains_key(&e.id) {
                continue;
            }
            map.insert(e.id, graph.hop_aux_bytes_for_edge(e.id)?);
        }
        hop_aux_by_edge = Some(map);
    }
    let mut out = Vec::new();
    for row in input {
        let src_node = match row.get(spec.expand.src) {
            Some(BindingValue::Node(node)) => node,
            Some(_) => return Err(ExecutionError::TypeMismatch("expand source must be a node")),
            None => return Err(ExecutionError::MissingBinding(spec.expand.src.to_owned())),
        };
        for e in &candidates {
            if !edge_satisfies_expand_labels(
                e.label.as_deref(),
                spec.expand.label,
                spec.expand.label_expr,
            ) {
                continue;
            }
            if !edge_matches_expand_direction(e, src_node.id, spec.expand.direction) {
                continue;
            }
            let far = far_endpoint_for_expand(e, src_node.id);
            let Some(dst_node) = (match dst_proj {
                None => graph.get_node(far)?,
                Some(names) => graph.get_node_projected(far, names)?,
            }) else {
                continue;
            };
            let mut next = row.clone();
            next.insert(
                Rc::<str>::from(spec.expand.edge),
                BindingValue::Edge(e.clone()),
            );
            next.insert(
                Rc::<str>::from(spec.expand.dst),
                BindingValue::Node(dst_node),
            );
            if let (Some(key), Some(cache)) = (hop_aux_key.as_ref(), hop_aux_by_edge.as_ref()) {
                let hop_aux = cache
                    .get(&e.id)
                    .expect("hop_aux cache covers all candidate edges")
                    .clone();
                insert_hop_aux_binding(&mut next, key.clone(), hop_aux);
            }
            out.push(next);
        }
    }
    Ok(out)
}

pub(crate) fn exec_expand_filter<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandSpec<'_>,
    dst_filter: &[Expr],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let expanded = exec_expand(graph, input, spec)?;
    exec_property_filter(graph, expanded, dst_filter, ctx)
}

fn expand_indexed_edge_property_matches(
    edge: &EdgeRecord,
    property: &str,
    resolved_value: &Value,
) -> bool {
    let Some(prop_value) = edge.properties.get(property) else {
        return false;
    };
    compare_values(prop_value, resolved_value).is_some_and(|ord| ord == Ordering::Equal)
}

pub(crate) fn exec_expand_var_len<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandSpec<'_>,
    var_len: &VarLenSpec,
    indexed_edge_equality: Option<(&str, &ScanValue)>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    match spec.hop_aux_var {
        None => exec_expand_var_len_impl::<false, _, _>(
            graph,
            input,
            spec,
            var_len,
            indexed_edge_equality,
            ctx,
            VarLenNoHopAux,
            &[],
        ),
        Some(name) => exec_expand_var_len_impl::<false, _, _>(
            graph,
            input,
            spec,
            var_len,
            indexed_edge_equality,
            ctx,
            VarLenYesHopAux(Rc::from(name)),
            &[],
        ),
    }
}

pub(crate) fn exec_expand_filter_var_len<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    spec: ExpandSpec<'_>,
    var_len: &VarLenSpec,
    dst_filter: &[Expr],
    indexed_edge_equality: Option<(&str, &ScanValue)>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    match spec.hop_aux_var {
        None => exec_expand_var_len_impl::<true, _, _>(
            graph,
            input,
            spec,
            var_len,
            indexed_edge_equality,
            ctx,
            VarLenNoHopAux,
            dst_filter,
        ),
        Some(name) => exec_expand_var_len_impl::<true, _, _>(
            graph,
            input,
            spec,
            var_len,
            indexed_edge_equality,
            ctx,
            VarLenYesHopAux(Rc::from(name)),
            dst_filter,
        ),
    }
}
