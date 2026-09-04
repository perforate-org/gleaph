//! ADR 0092 chain-survivor receipts: per-candidate-seed evaluation of the lowered
//! authorization chain at the SEARCH binding stage.
//!
//! The `PlanOp::Search` operator is the search-binding stage: dispatched vector
//! candidates become join-relevant here. Before the prefix join runs, every dispatched
//! candidate seed is evaluated against the lowered authorization chain components that
//! are pipeline-visible for the SEARCH binding — label-membership facts, chain-stage
//! `PropertyFilter` predicates referencing only the binding, and `SemiApply` probes
//! bound at the binding (grant coverage + ReBAC exists traversal + policy predicates
//! arrive pre-lowered through the single-source authorization pipeline). Candidates
//! that fail the chain are dead seeds: they never join, and their survival outcome is
//! reported per shard in the execution receipt.

use std::collections::BTreeMap;

use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql::types::LabelExpr;
use gleaph_gql_planner::collect_expr_variables;
use gleaph_gql_planner::plan::{PROPERTY_FILTER_STAGE_POLICY_CHAIN, PlanOp};
use gleaph_graph_kernel::entry::VertexLabelId;
use gleaph_graph_kernel::plan_exec::SearchChainReceiptRecord;
use ic_stable_lara::VertexId;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::context::ExecuteCtx;
use super::ops::execute_ops_from;
use super::row_matches_all;
use crate::gql_execution_context::GqlExecutionContext;

/// The lowered authorization chain scoped to one SEARCH binding, collected from the
/// pipeline operators that precede `PlanOp::Search`.
pub(crate) struct SearchBindingChain<'a> {
    /// Label-membership requirements on the binding (pattern label facts).
    required_labels: Vec<VertexLabelId>,
    /// Chain-stage predicate sets that reference only the binding.
    chain_predicates: Vec<Vec<Expr>>,
    /// Chain probes (`SemiApply` sub-plans) bound at the binding.
    chain_probes: Vec<&'a [PlanOp]>,
}

impl<'a> SearchBindingChain<'a> {
    /// Collect the chain components for `binding` from the pipeline prefix that binds
    /// the SEARCH binding (everything before the `PlanOp::Search` op).
    pub(crate) fn collect(
        ops: &'a [PlanOp],
        binding: &str,
        execution: &GqlExecutionContext,
    ) -> Result<Self, PlanQueryError> {
        let mut required_labels: Vec<VertexLabelId> = Vec::new();
        let mut chain_predicates: Vec<Vec<Expr>> = Vec::new();
        let mut chain_probes: Vec<&'a [PlanOp]> = Vec::new();
        for op in ops {
            match op {
                // The binding site's own label fact (leading shape: the searched label).
                PlanOp::NodeScan {
                    variable,
                    label: Some(label),
                    ..
                } if variable.as_ref() == binding => {
                    let Some(label_id) = execution.resolved_vertex_label_id(label.as_ref()) else {
                        return Err(PlanQueryError::MissingResolvedLabel {
                            namespace: "node",
                            name: label.to_string(),
                        });
                    };
                    required_labels.push(label_id);
                }
                // Expanded binding: the planner's dst label fact carries the searched
                // label. Negated label facts stay post-join row filters (ADR 0092 counts
                // positive label membership and chain loss, never user-authored
                // negations).
                PlanOp::ExpandFilter {
                    dst, dst_filter, ..
                } if dst.as_ref() == binding => {
                    for expr in dst_filter {
                        if let Some(label) = positive_label_fact(expr, binding) {
                            let Some(label_id) = execution.resolved_vertex_label_id(&label) else {
                                return Err(PlanQueryError::MissingResolvedLabel {
                                    namespace: "node",
                                    name: label.to_string(),
                                });
                            };
                            required_labels.push(label_id);
                        }
                    }
                }
                // ADR 0092 chain contract: policy predicates the authorization lowering
                // inserted (chain stage marker) participate in the per-seed chain
                // evaluation when they reference only the binding. User-authored filters
                // (stage 0) never participate; they keep applying as row filters.
                PlanOp::PropertyFilter {
                    predicates,
                    stage: PROPERTY_FILTER_STAGE_POLICY_CHAIN,
                } => {
                    if predicates
                        .iter()
                        .all(|predicate| expr_references_only(predicate, binding))
                    {
                        chain_predicates.push(predicates.clone());
                    }
                }
                // Chain probes are lowering-only operators; a probe bound at the SEARCH
                // binding is the ReBAC exists-traversal component of the chain.
                PlanOp::SemiApply {
                    source, sub_plan, ..
                } if source.as_ref() == binding => {
                    chain_probes.push(sub_plan.as_slice());
                }
                _ => {}
            }
        }
        Ok(Self {
            required_labels,
            chain_predicates,
            chain_probes,
        })
    }

    /// Evaluates the chain for one candidate vertex.
    ///
    /// Returns `Ok(true)` (survivor), `Ok(false)` (chain-rejected — evaluation errors
    /// fail closed toward rejection so the deepening loss signal never under-observes),
    /// and propagates only structural store errors.
    pub(crate) async fn candidate_survives(
        &self,
        ctx: &ExecuteCtx<'_>,
        binding: &str,
        vertex_id: VertexId,
    ) -> Result<bool, PlanQueryError> {
        let store = ctx.store;
        let Some(vertex) = store.vertex(vertex_id) else {
            return Ok(false);
        };
        if vertex.is_tombstone() {
            return Ok(false);
        }
        let labels = store.vertex_labels(vertex_id, vertex);
        if !self
            .required_labels
            .iter()
            .all(|required| labels.contains(required))
        {
            return Ok(false);
        }
        let mut seed_row = PlanRow::new();
        seed_row.insert(binding.to_string(), super::PlanBinding::Vertex(vertex_id));
        let evaluator = ctx.expr_evaluator(None);
        for predicates in &self.chain_predicates {
            match row_matches_all(&evaluator, &seed_row, predicates) {
                Ok(true) => {}
                Ok(false) => return Ok(false),
                Err(_) => return Ok(false),
            }
        }
        for probe in &self.chain_probes {
            let matches = execute_ops_from(ctx, probe, vec![seed_row.clone()]).await?;
            if matches.is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Extracts a positive `IsLabeled(binding, label)` fact from a filter expression.
fn positive_label_fact(expr: &Expr, binding: &str) -> Option<String> {
    let ExprKind::IsLabeled {
        expr: target,
        label,
        negated,
    } = &expr.kind
    else {
        return None;
    };
    let ExprKind::Variable(variable) = &target.kind else {
        return None;
    };
    if variable != binding || *negated {
        return None;
    }
    // Compound label expressions stay post-join row filters (ADR 0092 counts
    // single-name label-membership facts at the seed stage).
    match label {
        LabelExpr::Name(name) => Some(name.clone()),
        _ => None,
    }
}

/// `true` when the expression's variable references are a subset of `{binding}`.
fn expr_references_only(expr: &Expr, binding: &str) -> bool {
    collect_expr_variables(expr)
        .iter()
        .all(|variable| variable == binding)
}

/// ADR 0092: evaluate the lowered chain per dispatched candidate seed, drop dead seeds
/// from the join lookup (they never join), and produce the shard's receipt record.
pub(crate) async fn evaluate_search_chain_survivors(
    ctx: &ExecuteCtx<'_>,
    ops: &[PlanOp],
    search_idx: usize,
    binding: &str,
    wire: &gleaph_graph_kernel::plan_exec::ResolvedSearchWire,
    lookup: &mut BTreeMap<u32, f64>,
) -> Result<SearchChainReceiptRecord, PlanQueryError> {
    let store = ctx.store;
    let chain = SearchBindingChain::collect(&ops[..search_idx], binding, &ctx.execution)?;
    let shard_id = store
        .federation_routing()
        .map(|routing| routing.shard_id.raw())
        .unwrap_or(0);
    let mut dispatched: u64 = 0;
    let mut chain_survivors: u64 = 0;
    for hit in &wire.vertex_hits {
        dispatched += 1;
        let vertex_id = VertexId::from(hit.local_vertex_id);
        match chain.candidate_survives(ctx, binding, vertex_id).await {
            Ok(true) => chain_survivors += 1,
            Ok(false) => {
                lookup.remove(&hit.local_vertex_id);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(SearchChainReceiptRecord {
        shard_id,
        dispatched,
        chain_survivors,
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::execute_plan_query_bindings_with_outcome;
    use super::super::test_support::*;
    use super::*;
    use gleaph_gql_planner::plan::{
        EdgeLabelRef, NodeLabelRef, PROPERTY_FILTER_STAGE_POLICY_CHAIN, PhysicalPlan,
    };
    use std::collections::BTreeMap;

    use crate::plan::empty_row_for_plan;

    fn chain_stage_filter(predicates: Vec<Expr>) -> PlanOp {
        PlanOp::PropertyFilter {
            predicates,
            stage: PROPERTY_FILTER_STAGE_POLICY_CHAIN,
        }
    }

    fn equality_pred(variable: &str, property: &str, value: Value) -> Expr {
        Expr::new(ExprKind::Compare {
            left: Box::new(prop(variable, property)),
            op: CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(value))),
        })
    }

    fn search_op(binding: &str, alias: &str) -> PlanOp {
        PlanOp::Search {
            binding: Str::from(binding),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec![Str::from("receipt_vec")],
                query: Expr::var("query"),
                limit: Expr::int(10),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: Str::from(alias),
            },
        }
    }

    fn resolved_search(binding: &str, alias: &str, hits: &[u32]) -> GqlExecutionContext {
        GqlExecutionContext {
            resolved_search: Some(ResolvedSearchWire {
                binding: binding.to_string(),
                output_alias: alias.to_string(),
                vertex_hits: hits
                    .iter()
                    .map(|local_vertex_id| ResolvedSearchVertexHitWire {
                        local_vertex_id: *local_vertex_id,
                        value: 0.5,
                    })
                    .collect(),
            }),
            ..GqlExecutionContext::default()
        }
    }

    fn run(
        store: &GraphStore,
        plan: &PhysicalPlan,
        execution: GqlExecutionContext,
    ) -> (
        Vec<BTreeMap<String, Value>>,
        Option<SearchChainReceiptRecord>,
    ) {
        pollster::block_on(execute_plan_query_bindings_with_outcome(
            store,
            plan,
            &params(),
            None,
            execution,
            vec![empty_row_for_plan(plan)],
            false,
        ))
        .map(|outcome| {
            (
                crate::plan::materialize_plan_rows(
                    store,
                    &crate::element_id_encoding::resolve_or_host_fixture(None),
                    &outcome.rows,
                )
                .expect("materialize"),
                outcome.search_chain_receipt,
            )
        })
        .expect("search receipt execution")
    }

    /// Chain で拒否された seed は join に回らず、dispatched は計上される。
    #[test]
    fn chain_rejected_seed_never_joins_and_counts_as_dead() {
        let store = GraphStore::new();
        let public = store
            .insert_vertex_named(
                ["ReceiptDoc"],
                [("visibility", Value::Text("public".into()))],
            )
            .expect("insert public");
        let hidden = store
            .insert_vertex_named(
                ["ReceiptDoc"],
                [("visibility", Value::Text("secret".into()))],
            )
            .expect("insert hidden");

        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: Str::from("d"),
                label: Some(NodeLabelRef::from("ReceiptDoc")),
                property_projection: None,
            },
            chain_stage_filter(vec![equality_pred(
                "d",
                "visibility",
                Value::Text("public".into()),
            )]),
            search_op("d", "distance"),
            PlanOp::Project {
                columns: vec![project(var("d"), "d")],
                distinct: false,
            },
        ]);

        let (rows, receipt) = run(
            &store,
            &plan,
            resolved_search("d", "distance", &[u32::from(public), u32::from(hidden)]),
        );

        // Both candidates were dispatched; only the public one survived the chain.
        let receipt = receipt.expect("SEARCH-bearing execution must carry the receipt");
        assert_eq!(receipt.dispatched, 2);
        assert_eq!(receipt.chain_survivors, 1);
        assert_eq!(receipt.shard_id, 0, "standalone execution reports shard 0");

        // The chain-rejected seed never joins: the hidden doc has no row.
        assert_eq!(rows.len(), 1, "dead seed must not join");
        let Value::Record(record) = rows[0].get("d").expect("d") else {
            panic!("expected vertex record for d");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value.clone())),
            Some(Value::Uint64(u64::from(public))),
            "the surviving candidate is the chain-passing one"
        );
    }

    /// chain を通過したが join が空の seed も survivor として計上される(生存は prefix
    /// join から独立 — ADR 0034 Slice 5 sparsity)。
    #[test]
    fn chain_survivor_with_empty_prefix_join_is_counted() {
        let store = GraphStore::new();
        let survivor = store
            .insert_vertex_named(["ReceiptDoc"], Vec::<(&str, Value)>::new())
            .expect("insert survivor");
        // No Author vertices and no WROTE edges: the prefix join is empty.

        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: Str::from("a"),
                label: Some(NodeLabelRef::from("ReceiptAuthor")),
                property_projection: None,
            },
            PlanOp::ExpandFilter {
                src: Str::from("a"),
                edge: Str::from("__receipt_e1"),
                dst: Str::from("d"),
                direction: EdgeDirection::PointingRight,
                label: Some(EdgeLabelRef::from("RECEIPT_WROTE")),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_inline_property_predicate: None,
                edge_inline_vector_predicate: None,
                dst_filter: vec![Expr::new(ExprKind::IsLabeled {
                    expr: Box::new(var("d")),
                    label: LabelExpr::Name("ReceiptDoc".to_string()),
                    negated: false,
                })],
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: false,
                near_group_var: None,
                far_group_var: None,
                path_var: None,
                emit_path_binding: false,
            },
            search_op("d", "distance"),
            PlanOp::Project {
                columns: vec![project(var("d"), "d")],
                distinct: false,
            },
        ]);

        let (rows, receipt) = run(
            &store,
            &plan,
            resolved_search("d", "distance", &[u32::from(survivor)]),
        );

        // The candidate survived the chain (label fact + no chain rejection) even though
        // the prefix join is empty: sparsity, not authorization loss.
        let receipt = receipt.expect("SEARCH-bearing execution must carry the receipt");
        assert_eq!(receipt.dispatched, 1);
        assert_eq!(receipt.chain_survivors, 1);
        assert!(rows.is_empty(), "empty join with a surviving seed");
    }

    /// 生存 seed は通常どおり join 行を返す。
    #[test]
    fn surviving_seed_joins_normally() {
        let store = GraphStore::new();
        let doc = store
            .insert_vertex_named(
                ["ReceiptDoc"],
                [("visibility", Value::Text("public".into()))],
            )
            .expect("insert doc");

        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: Str::from("d"),
                label: Some(NodeLabelRef::from("ReceiptDoc")),
                property_projection: None,
            },
            chain_stage_filter(vec![equality_pred(
                "d",
                "visibility",
                Value::Text("public".into()),
            )]),
            search_op("d", "distance"),
            PlanOp::Project {
                columns: vec![project(var("d"), "d"), project(var("distance"), "distance")],
                distinct: false,
            },
        ]);

        let (rows, receipt) = run(
            &store,
            &plan,
            resolved_search("d", "distance", &[u32::from(doc)]),
        );

        let receipt = receipt.expect("receipt present");
        assert_eq!(receipt.dispatched, 1);
        assert_eq!(receipt.chain_survivors, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("distance"),
            Some(&Value::Float64(0.5)),
            "the search alias binds on the surviving row"
        );
    }

    /// chain 構成要素が無い(テナント等)場合は全 candidate が survivor。
    #[test]
    fn no_chain_components_counts_every_candidate_as_survivor() {
        let store = GraphStore::new();
        let doc = store
            .insert_vertex_named(["ReceiptDoc"], Vec::<(&str, Value)>::new())
            .expect("insert doc");
        let other = store
            .insert_vertex_named(["ReceiptOther"], Vec::<(&str, Value)>::new())
            .expect("insert other");

        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: Str::from("d"),
                label: Some(NodeLabelRef::from("ReceiptDoc")),
                property_projection: None,
            },
            search_op("d", "distance"),
            PlanOp::Project {
                columns: vec![project(var("d"), "d")],
                distinct: false,
            },
        ]);

        let (rows, receipt) = run(
            &store,
            &plan,
            resolved_search("d", "distance", &[u32::from(doc), u32::from(other)]),
        );

        // Both dispatched; the other-labeled hit is a stale-index candidate: the
        // binding's label fact rejects it as a non-survivor.
        let receipt = receipt.expect("receipt present");
        assert_eq!(receipt.dispatched, 2);
        assert_eq!(
            receipt.chain_survivors, 1,
            "label membership is a chain fact"
        );
        assert_eq!(rows.len(), 1);
        let Value::Record(record) = rows[0].get("d").expect("d") else {
            panic!("expected vertex record for d");
        };
        assert_eq!(
            record
                .iter()
                .find_map(|(key, value)| (key == "id").then_some(value.clone())),
            Some(Value::Uint64(u64::from(doc))),
        );
    }

    /// SEARCH を含まない実行は receipt を運ばない。
    #[test]
    fn non_search_execution_carries_no_receipt() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["ReceiptPlain"], Vec::<(&str, Value)>::new())
            .expect("insert");
        let plan = plan_gql("MATCH (n:ReceiptPlain) RETURN n");
        let outcome = pollster::block_on(execute_plan_query_bindings_with_outcome(
            &store,
            &plan,
            &params(),
            None,
            GqlExecutionContext::default(),
            vec![empty_row_for_plan(&plan)],
            false,
        ))
        .expect("plain execution");
        assert!(outcome.search_chain_receipt.is_none());
    }

    /// 複数 SEARCH は実行器でも fail-closed(ADR 0092 §5)。
    #[test]
    fn second_search_op_fails_closed() {
        let store = GraphStore::new();
        let doc = store
            .insert_vertex_named(["ReceiptDoc"], Vec::<(&str, Value)>::new())
            .expect("insert doc");

        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: Str::from("d"),
                label: Some(NodeLabelRef::from("ReceiptDoc")),
                property_projection: None,
            },
            search_op("d", "distance"),
            search_op("d", "score"),
            PlanOp::Project {
                columns: vec![project(var("d"), "d")],
                distinct: false,
            },
        ]);

        let result = pollster::block_on(execute_plan_query_bindings_with_outcome(
            &store,
            &plan,
            &params(),
            None,
            resolved_search("d", "distance", &[u32::from(doc)]),
            vec![empty_row_for_plan(&plan)],
            false,
        ));
        assert!(
            result.is_err(),
            "a second SEARCH arm must fail closed on the filled receipt slot"
        );
    }
}
