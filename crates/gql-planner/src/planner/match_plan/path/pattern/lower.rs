use gleaph_gql::ast::*;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use std::collections::BTreeSet;

use super::super::super::result::flatten_conjunction;
use super::super::filters::{parse_edge_var_property_equality, quantifier_to_var_len};
use crate::anchor::{self, extract_simple_label};
use crate::path_extensions::{PathPatternExtensionContext, PlanBuildOptions, SingleEdgePathInfo};
use crate::plan::*;
use crate::planner::PlannerError;
use crate::stats::GraphStats;

pub(super) fn plan_path_pattern(
    path: &PathPattern,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    where_conjuncts: &mut Vec<Expr>,
    options: PlanBuildOptions<'_>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    // Check for shortest-path prefix.
    let shortest_mode = path.prefix.as_ref().and_then(|p| match p {
        PathPatternPrefix::Search(search) => match search {
            SearchPrefix::AnyShortest { .. } => Some(ShortestMode::AnyShortest),
            SearchPrefix::AllShortest { .. } => Some(ShortestMode::AllShortest),
            SearchPrefix::ShortestK { k, .. } => Some(ShortestMode::ShortestK(*k)),
            _ => None,
        },
        _ => None,
    });
    if path.variable.is_some() && shortest_mode.is_none() {
        return Err(PlannerError::UnsupportedPattern(
            "path variables are only supported for shortest-path patterns".into(),
        ));
    }

    let shortest_path_cost = if path.extensions.is_empty() {
        ShortestPathCost::HopCount
    } else {
        let single_edge = match &path.expr {
            PathPatternExpr::Term(term) => extract_single_edge_path_info(term),
            _ => None,
        };
        let ctx = PathPatternExtensionContext {
            prefix: path.prefix.as_ref(),
            extensions: &path.extensions,
            shortest_mode,
            single_edge,
        };
        options.path_extensions.plan_shortest_path_cost(&ctx)?
    };

    // Walk the path expression to emit scan/expand ops.
    plan_path_expr(
        &path.expr,
        shortest_mode,
        shortest_path_cost,
        path.variable.as_deref(),
        stats,
        conditional_candidates,
        referenced_vars,
        where_conjuncts,
        bound_node_vars,
        optional_node_vars,
        ops,
        annotations,
    )?;
    Ok(())
}

fn extract_single_edge_path_info(term: &PathTerm) -> Option<SingleEdgePathInfo> {
    let term = normalize_path_term(term).ok()?;
    if term.factors.len() != 3 {
        return None;
    }
    let PathPrimary::Node(_) = &term.factors[0].primary else {
        return None;
    };
    let PathPrimary::Edge(edge) = &term.factors[1].primary else {
        return None;
    };
    let PathPrimary::Node(_) = &term.factors[2].primary else {
        return None;
    };
    let (label, label_expr) = plan_edge_expand_labels(edge);
    let var_len = term.factors[1]
        .quantifier
        .as_ref()
        .and_then(quantifier_to_var_len);
    Some(SingleEdgePathInfo {
        edge_var: edge.variable.clone(),
        direction: edge.direction,
        label: label.map(|s| s.to_string()),
        label_expr,
        var_len,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_path_expr(
    expr: &PathPatternExpr,
    shortest_mode: Option<ShortestMode>,
    shortest_path_cost: ShortestPathCost,
    path_var: Option<&str>,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    where_conjuncts: &mut Vec<Expr>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    match expr {
        PathPatternExpr::Term(term) => {
            super::term::plan_path_term(
                term,
                shortest_mode,
                shortest_path_cost.clone(),
                path_var,
                stats,
                conditional_candidates,
                referenced_vars,
                where_conjuncts,
                bound_node_vars,
                optional_node_vars,
                ops,
                annotations,
            )?;
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            if let Some(term) = terms.first() {
                super::term::plan_path_term(
                    term,
                    shortest_mode,
                    shortest_path_cost.clone(),
                    path_var,
                    stats,
                    conditional_candidates,
                    referenced_vars,
                    where_conjuncts,
                    bound_node_vars,
                    optional_node_vars,
                    ops,
                    annotations,
                )?;
            }
        }
    }
    Ok(())
}

/// Pre-extracted path element for lookahead during planning.
pub(super) enum PathElement {
    Node {
        var: String,
        node: NodePattern,
    },
    Edge {
        var: String,
        edge: EdgePattern,
        quantifier: Option<PathQuantifier>,
    },
    Sub(PathPatternExpr),
}

fn node_emits_unlabeled_full_vertex_scan(
    var: &str,
    label: &Option<String>,
    node: &NodePattern,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    annotations: &PlanAnnotations,
) -> bool {
    if label.is_some() {
        return false;
    }
    if let Some(stats) = stats
        && let Some(where_expr) = &node.where_clause
        && anchor::find_index_intersection(var, where_expr, stats).is_some()
    {
        return false;
    }
    if let Some(anchor) = &annotations.optimizer.anchor
        && &*anchor.variable == var
    {
        match &anchor.source {
            AnchorSource::PropertyEquality { .. }
            | AnchorSource::InlinePropertyEquality { .. }
            | AnchorSource::PropertyRange { .. } => return false,
            AnchorSource::LabelCardinality { .. }
            | AnchorSource::SchemaEndpoint
            | AnchorSource::FullScan => {}
        }
    }
    conditional_candidates
        .iter()
        .filter(|c| &*c.variable == var)
        .count()
        == 0
}

fn edge_has_indexed_scannable_equality(
    edge_var: &str,
    edge: &EdgePattern,
    stats: Option<&dyn GraphStats>,
    where_conjuncts: &[Expr],
) -> bool {
    let Some(stats) = stats else {
        return false;
    };
    for p in &edge.properties {
        if stats.is_edge_property_indexed(&p.name)
            && anchor::scan_value_from_expr(&p.value).is_some()
        {
            return true;
        }
    }
    for c in where_conjuncts {
        if let Some((v, prop, _)) = parse_edge_var_property_equality(c)
            && v == edge_var
            && stats.is_edge_property_indexed(&prop)
        {
            return true;
        }
    }
    if let Some(w) = &edge.where_clause {
        for c in flatten_conjunction(w) {
            if let Some((v, prop, _)) = parse_edge_var_property_equality(&c)
                && v == edge_var
                && stats.is_edge_property_indexed(&prop)
            {
                return true;
            }
        }
    }
    false
}

pub(super) fn first_hop_supports_leading_edge_index(
    elements: &[PathElement],
    where_conjuncts: &[Expr],
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    annotations: &PlanAnnotations,
    shortest_mode: Option<ShortestMode>,
) -> bool {
    if shortest_mode.is_some() {
        return false;
    }
    if elements.len() < 3 {
        return false;
    }
    let PathElement::Node { var: nv, node } = &elements[0] else {
        return false;
    };
    let PathElement::Edge {
        edge,
        quantifier,
        var: ev,
    } = &elements[1]
    else {
        return false;
    };
    if quantifier
        .as_ref()
        .and_then(quantifier_to_var_len)
        .is_some()
    {
        return false;
    }
    if !matches!(
        edge.direction,
        EdgeDirection::PointingRight | EdgeDirection::PointingLeft
    ) {
        return false;
    }
    let label = extract_simple_label(&node.label);
    if !node_emits_unlabeled_full_vertex_scan(
        nv,
        &label,
        node,
        stats,
        conditional_candidates,
        annotations,
    ) {
        return false;
    }
    edge_has_indexed_scannable_equality(ev, edge, stats, where_conjuncts)
}

/// Split edge pattern label into a cheap single-name [`PlanOp::Expand::label`] plus an optional
/// [`PlanOp::Expand::label_expr`] for unions, negation, `&`, etc.
pub(super) fn plan_edge_expand_labels(edge: &EdgePattern) -> (Option<Str>, Option<LabelExpr>) {
    match &edge.label {
        None => (None, None),
        Some(LabelExpr::Name(n)) => (Some(Str::from(n.as_str())), None),
        Some(le) => (None, Some(le.clone())),
    }
}

/// Expand §16.10 simplified path factors into ordinary `Edge` / `Node` factors so existing
/// join-order, cycle detection, and `Expand` planning apply.
pub(super) fn normalize_path_term(term: &PathTerm) -> Result<PathTerm, PlannerError> {
    let mut out_factors = Vec::with_capacity(term.factors.len().saturating_mul(2));
    for (i, factor) in term.factors.iter().enumerate() {
        match &factor.primary {
            PathPrimary::Simplified(sp) => {
                let n_el = sp.elements.len();
                if n_el == 0 {
                    return Err(PlannerError::UnsupportedPattern(
                        "empty simplified path segment".into(),
                    ));
                }
                let mut eid = 0usize;
                let mut chunks: Vec<Vec<(EdgePattern, Option<PathQuantifier>)>> =
                    Vec::with_capacity(n_el);
                for elem in &sp.elements {
                    chunks.push(lower_simplified_element_edges(elem, i, &mut eid)?);
                }
                let total_edges: usize = chunks.iter().map(|c| c.len()).sum();
                if factor.quantifier.is_some() && total_edges != 1 {
                    return Err(PlannerError::UnsupportedPattern(
                        "path quantifier on multi-segment simplified edge is not supported".into(),
                    ));
                }
                for (j, chunk) in chunks.into_iter().enumerate() {
                    let n_in_chunk = chunk.len();
                    for (k, (edge_pat, inner_q)) in chunk.into_iter().enumerate() {
                        let quantifier = if total_edges == 1 {
                            match (inner_q, factor.quantifier.clone()) {
                                (Some(q), _) => Some(q),
                                (None, outer) => outer,
                            }
                        } else {
                            inner_q
                        };
                        out_factors.push(PathFactor {
                            span: edge_pat.span,
                            primary: PathPrimary::Edge(edge_pat),
                            quantifier,
                        });
                        if k + 1 < n_in_chunk {
                            let mid_var = format!("__simpl_mid_in_{i}_{j}_{k}");
                            out_factors.push(PathFactor {
                                span: factor.span,
                                primary: PathPrimary::Node(NodePattern {
                                    span: factor.span,
                                    variable: Some(mid_var),
                                    is_or_colon: None,
                                    label: None,
                                    properties: vec![],
                                    where_clause: None,
                                }),
                                quantifier: None,
                            });
                        }
                    }
                    if j + 1 < n_el {
                        let mid_var = format!("__simpl_mid_el_{i}_{j}");
                        out_factors.push(PathFactor {
                            span: factor.span,
                            primary: PathPrimary::Node(NodePattern {
                                span: factor.span,
                                variable: Some(mid_var),
                                is_or_colon: None,
                                label: None,
                                properties: vec![],
                                where_clause: None,
                            }),
                            quantifier: None,
                        });
                    }
                }
            }
            _ => out_factors.push(factor.clone()),
        }
    }
    Ok(PathTerm {
        span: term.span,
        factors: out_factors,
    })
}

fn peel_all_groups(mut c: &SimplifiedContents) -> &SimplifiedContents {
    while let SimplifiedContents::Group(inner) = c {
        c = inner.as_ref();
    }
    c
}

/// True when `c` contains `Concatenation` (juxtaposition of factorLows inside §16.12).
fn has_concatenation(c: &SimplifiedContents) -> bool {
    match c {
        SimplifiedContents::Concatenation(_, _) => true,
        SimplifiedContents::Group(inner)
        | SimplifiedContents::Negation(inner)
        | SimplifiedContents::Quantified(inner, _) => has_concatenation(inner),
        SimplifiedContents::DirectionOverride(_, inner) => has_concatenation(inner),
        SimplifiedContents::Conjunction(a, b)
        | SimplifiedContents::Union(a, b)
        | SimplifiedContents::MultisetAlternation(a, b) => {
            has_concatenation(a) || has_concatenation(b)
        }
        SimplifiedContents::Label(_) => false,
    }
}

fn flatten_alt_branches(c: &SimplifiedContents) -> Vec<&SimplifiedContents> {
    match c {
        SimplifiedContents::Union(a, b) | SimplifiedContents::MultisetAlternation(a, b) => {
            let mut v = flatten_alt_branches(a);
            v.extend(flatten_alt_branches(b));
            v
        }
        _ => vec![c],
    }
}

fn flatten_concat_branches(c: &SimplifiedContents) -> Vec<&SimplifiedContents> {
    match c {
        SimplifiedContents::Concatenation(a, b) => {
            let mut v = flatten_concat_branches(a);
            v.extend(flatten_concat_branches(b));
            v
        }
        _ => vec![c],
    }
}

/// One slash-delimited simplified element (`-/ ... /->` etc.) → 1+ edge tuples.
fn lower_simplified_element_edges(
    elem: &SimplifiedElement,
    factor_idx: usize,
    eid: &mut usize,
) -> Result<Vec<(EdgePattern, Option<PathQuantifier>)>, PlannerError> {
    let c = peel_all_groups(&elem.contents);
    match c {
        SimplifiedContents::Union(_, _) | SimplifiedContents::MultisetAlternation(_, _) => {
            let branches = flatten_alt_branches(c);
            if branches.is_empty() {
                return Err(PlannerError::UnsupportedPattern(
                    "empty simplified path alternative".into(),
                ));
            }
            let mut merged_dir: Option<EdgeDirection> = None;
            let mut label_acc: Option<LabelExpr> = None;
            for b in &branches {
                if has_concatenation(b) {
                    return Err(PlannerError::UnsupportedPattern(
                        "union or |+| combined with concatenated simplified hops is not supported by the planner".into(),
                    ));
                }
                let b = peel_all_groups(b);
                let (branch_q, after_q) = peel_simplified_quantifier(b);
                if branch_q.is_some() {
                    return Err(PlannerError::UnsupportedPattern(
                        "quantified alternatives in a simplified path are not supported by the planner".into(),
                    ));
                }
                let (dir, rest) = peel_simplified_direction_overrides(elem.direction, after_q)?;
                let lbl = simplified_contents_to_label_expr(rest)?;
                match merged_dir {
                    None => merged_dir = Some(dir),
                    Some(d) if d == dir => {}
                    _ => {
                        return Err(PlannerError::UnsupportedPattern(
                            "simplified path alternatives with different directions are not supported by the planner".into(),
                        ));
                    }
                }
                label_acc = Some(match label_acc {
                    None => lbl,
                    Some(prev) => LabelExpr::Or(Box::new(prev), Box::new(lbl)),
                });
            }
            let j = *eid;
            *eid += 1;
            Ok(vec![(
                EdgePattern {
                    span: elem.span,
                    direction: merged_dir.expect("non-empty branches"),
                    variable: Some(format!("__simpl_e{factor_idx}_{j}")),
                    is_or_colon: None,
                    label: Some(label_acc.expect("non-empty branches")),
                    properties: vec![],
                    where_clause: None,
                },
                None,
            )])
        }
        _ => {
            let parts = flatten_concat_branches(c);
            let mut out = Vec::new();
            for p in parts {
                out.append(&mut lower_factor_low_maybe_multi(elem, p, factor_idx, eid)?);
            }
            Ok(out)
        }
    }
}

fn lower_factor_low_maybe_multi(
    elem: &SimplifiedElement,
    factor_low: &SimplifiedContents,
    factor_idx: usize,
    eid: &mut usize,
) -> Result<Vec<(EdgePattern, Option<PathQuantifier>)>, PlannerError> {
    let (quant, after_q) = peel_simplified_quantifier(factor_low);
    let after_q = peel_all_groups(after_q);
    if let SimplifiedContents::Concatenation(_, _) = after_q {
        if quant.is_some() {
            return Err(PlannerError::UnsupportedPattern(
                "quantifier on a concatenated simplified path group is not supported".into(),
            ));
        }
        let mut v = Vec::new();
        for p in flatten_concat_branches(after_q) {
            v.push(lower_one_simplified_edge_piece(
                elem, p, factor_idx, *eid, None,
            )?);
            *eid += 1;
        }
        Ok(v)
    } else {
        let e = lower_one_simplified_edge_piece(elem, after_q, factor_idx, *eid, quant)?;
        *eid += 1;
        Ok(vec![e])
    }
}

fn lower_one_simplified_edge_piece(
    elem: &SimplifiedElement,
    piece: &SimplifiedContents,
    factor_idx: usize,
    j: usize,
    forced_quant: Option<PathQuantifier>,
) -> Result<(EdgePattern, Option<PathQuantifier>), PlannerError> {
    let (inner_q, after_q) = peel_simplified_quantifier(piece);
    if inner_q.is_some() && forced_quant.is_some() {
        return Err(PlannerError::UnsupportedPattern(
            "conflicting quantifiers on simplified path piece".into(),
        ));
    }
    let quant = inner_q.or(forced_quant);
    let (direction, rest) = peel_simplified_direction_overrides(elem.direction, after_q)?;
    if has_concatenation(rest) {
        return Err(PlannerError::UnsupportedPattern(
            "nested simplified path concatenation is not supported by the planner".into(),
        ));
    }
    let label = simplified_contents_to_label_expr(rest)?;
    Ok((
        EdgePattern {
            span: elem.span,
            direction,
            variable: Some(format!("__simpl_e{factor_idx}_{j}")),
            is_or_colon: None,
            label: Some(label),
            properties: vec![],
            where_clause: None,
        },
        quant,
    ))
}

fn peel_simplified_quantifier(
    c: &SimplifiedContents,
) -> (Option<PathQuantifier>, &SimplifiedContents) {
    match c {
        SimplifiedContents::Quantified(inner, q) => (Some(q.clone()), inner.as_ref()),
        SimplifiedContents::Group(inner) => peel_simplified_quantifier(inner),
        _ => (None, c),
    }
}

fn peel_simplified_direction_overrides(
    mut dir: EdgeDirection,
    mut c: &SimplifiedContents,
) -> Result<(EdgeDirection, &SimplifiedContents), PlannerError> {
    loop {
        match c {
            SimplifiedContents::Group(inner) => c = inner,
            SimplifiedContents::DirectionOverride(d, inner) => {
                dir = *d;
                c = inner;
            }
            SimplifiedContents::Quantified(_, _) => {
                return Err(PlannerError::UnsupportedPattern(
                    "mis-ordered quantifier inside simplified path".into(),
                ));
            }
            _ => return Ok((dir, c)),
        }
    }
}

fn simplified_contents_to_label_expr(c: &SimplifiedContents) -> Result<LabelExpr, PlannerError> {
    match c {
        SimplifiedContents::Label(le) => Ok(le.clone()),
        SimplifiedContents::Group(inner) => simplified_contents_to_label_expr(inner),
        SimplifiedContents::Conjunction(a, b) => Ok(LabelExpr::And(
            Box::new(simplified_contents_to_label_expr(a)?),
            Box::new(simplified_contents_to_label_expr(b)?),
        )),
        SimplifiedContents::Union(a, b) => Ok(LabelExpr::Or(
            Box::new(simplified_contents_to_label_expr(a)?),
            Box::new(simplified_contents_to_label_expr(b)?),
        )),
        // Multiset alternation (|+|): planner treats like set union for edge typing; multiplicity is not modeled.
        SimplifiedContents::MultisetAlternation(a, b) => Ok(LabelExpr::Or(
            Box::new(simplified_contents_to_label_expr(a)?),
            Box::new(simplified_contents_to_label_expr(b)?),
        )),
        SimplifiedContents::Negation(inner) => Ok(LabelExpr::Not(Box::new(
            simplified_contents_to_label_expr(inner)?,
        ))),
        SimplifiedContents::Concatenation(_, _) => Err(PlannerError::UnsupportedPattern(
            "concatenated simplified path should be lowered before label conversion".into(),
        )),
        SimplifiedContents::Quantified(_, _) => Err(PlannerError::UnsupportedPattern(
            "unexpected quantifier while lowering simplified path".into(),
        )),
        SimplifiedContents::DirectionOverride(_, _) => Err(PlannerError::UnsupportedPattern(
            "unexpected direction override while lowering simplified path".into(),
        )),
    }
}

/// Per-hop auxiliary binding for [`PlanOp::Expand`] / [`PlanOp::ExpandFilter`] / [`PlanOp::EdgeBindEndpoints`]
/// when the linear query references `{edge_var}__hop_aux`.
pub(super) fn hop_aux_binding_for_edge_if_referenced(
    edge_var: &str,
    referenced: &BTreeSet<String>,
) -> Option<Str> {
    let name = format!("{edge_var}__hop_aux");
    referenced.contains(&name).then_some(name.into())
}
