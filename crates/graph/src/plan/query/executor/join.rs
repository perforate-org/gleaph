//! Hash join and cartesian product operators.

use std::hash::Hasher;
use std::pin::Pin;
use std::sync::Arc;

use gleaph_gql_planner::plan::{PlanOp, Str};
use gleaph_gql::{Value, hash_value_for_join};
use ic_stable_lara::VertexId;
use nohash_hasher::IntMap;
use rapidhash::fast::RapidHasher;

use super::context::ExecuteCtx;
use super::ops::execute_ops_from;
use super::PlanBinding;
use super::super::error::PlanQueryError;
use super::super::row::PlanRow;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

/// Bindings for each hash-join key column (planner order), used for equality and hashing.
type HashJoinKey = Vec<PlanBinding>;

/// Left subplan rows that share the same exact [`HashJoinKey`] within one hash bucket.
type HashJoinBucketEntry = (HashJoinKey, Vec<PlanRow>);

type HashJoinBuckets = IntMap<u64, Vec<HashJoinBucketEntry>>;

pub(crate) async fn execute_cartesian_product(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    left: &[PlanOp],
    right: &[PlanOp],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        let left_rows = execute_ops_from(ctx, left, vec![row.clone()]).await?;
        let right_rows = execute_ops_from(ctx, right, vec![row]).await?;
        for left_row in &left_rows {
            for right_row in &right_rows {
                if let Some(merged) = merge_rows(left_row, right_row) {
                    out.push(merged);
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn merge_rows(left: &PlanRow, right: &PlanRow) -> Option<PlanRow> {
    left.try_merge(right, &[])
}

/// Like [`merge_rows`], but caller guarantees join-key columns already match between `left` and `right`.
/// Skips re-checking join-key bindings (hot path for [`execute_hash_join`] after key equality).
fn merge_rows_with_known_join_keys(
    left: &PlanRow,
    right: &PlanRow,
    join_keys: &[Str],
) -> Option<PlanRow> {
    match join_keys {
        [only] => left.try_merge_skip_one(right, only.as_ref()),
        keys => {
            let skip: Vec<&str> = keys.iter().map(|k| k.as_ref()).collect();
            left.try_merge(right, &skip)
        }
    }
}

fn merge_rows_with_known_join_keys_pooled(
    _arena: &mut super::super::arena::QueryArena,
    left: &PlanRow,
    right: &PlanRow,
    join_keys: &[Str],
) -> Option<PlanRow> {
    // Merge stays on `try_merge_skip_one` (slot clone). Arena is for row recycle after join.
    merge_rows_with_known_join_keys(left, right, join_keys)
}

pub(crate) async fn execute_hash_join(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    left: &[PlanOp],
    right: &[PlanOp],
    join_keys: &[Str],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    if join_keys.is_empty() {
        return Err(PlanQueryError::UnsupportedOp("HashJoin(empty join_keys)"));
    }

    let mut out = Vec::new();
    for row in rows {
        let left_rows = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_left_subplan");
            execute_ops_from(ctx, left, vec![row.clone()]).await?
        };
        let right_rows = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_right_subplan");
            execute_ops_from(ctx, right, vec![row]).await?
        };

        let join_key_fast_vertex = join_keys.len() == 1 && {
            let jk = join_keys[0].as_ref();
            left_rows
                .iter()
                .all(|r| matches!(r.get(jk), Some(PlanBinding::Vertex(_))))
                && right_rows
                    .iter()
                    .all(|r| matches!(r.get(jk), Some(PlanBinding::Vertex(_))))
        };

        if join_key_fast_vertex {
            let jk = join_keys[0].as_ref();
            let left_by_vertex = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("hash_join_vertex_partition");
                let mut left_by_vertex: IntMap<u32, Vec<PlanRow>> = IntMap::default();
                for lr in left_rows {
                    let PlanBinding::Vertex(vid) = lr.get(jk).expect("join key binding") else {
                        unreachable!("join_key_fast_vertex pre-scan should guarantee Vertex");
                    };
                    left_by_vertex.entry(u32::from(*vid)).or_default().push(lr);
                }
                left_by_vertex
            };
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_vertex_probe_merge");
            super::super::arena::QueryArena::with(|arena| {
                for rr in &right_rows {
                    let Some(PlanBinding::Vertex(vid)) = rr.get(jk) else {
                        continue;
                    };
                    let Some(left_matches) = left_by_vertex.get(&u32::from(*vid)) else {
                        continue;
                    };
                    for lr in left_matches {
                        if let Some(merged) =
                            merge_rows_with_known_join_keys_pooled(arena, lr, rr, join_keys)
                        {
                            out.push(merged);
                        }
                    }
                }
                arena.recycle_rows(right_rows);
                for (_, bucket) in left_by_vertex {
                    arena.recycle_rows(bucket);
                }
            });
        } else {
            let buckets = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("hash_join_bucket_partition");
                let mut buckets: HashJoinBuckets = IntMap::default();
                for lr in left_rows {
                    let key = extract_join_key(&lr, join_keys)?;
                    insert_join_bucket(&mut buckets, key, lr);
                }
                buckets
            };

            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_bucket_probe_merge");
            super::super::arena::QueryArena::with(|arena| -> Result<(), PlanQueryError> {
                for rr in &right_rows {
                    let key = extract_join_key(rr, join_keys)?;
                    let h = hash_join_mix(&key);
                    let Some(bucket) = buckets.get(&h) else {
                        continue;
                    };
                    for (left_key, left_matches) in bucket {
                        if left_key == &key {
                            for lr in left_matches {
                                if let Some(merged) =
                                    merge_rows_with_known_join_keys_pooled(arena, lr, rr, join_keys)
                                {
                                    out.push(merged);
                                }
                            }
                        }
                    }
                }
                arena.recycle_rows(right_rows);
                for bucket in buckets.into_values() {
                    for (_, rows) in bucket {
                        arena.recycle_rows(rows);
                    }
                }
                Ok(())
            })?;
        }
    }
    Ok(out)
}

fn extract_join_key(row: &PlanRow, join_keys: &[Str]) -> Result<HashJoinKey, PlanQueryError> {
    join_keys
        .iter()
        .map(|k| {
            row.get(k.as_ref())
                .cloned()
                .ok_or_else(|| PlanQueryError::MissingBinding {
                    variable: k.as_ref().to_owned(),
                })
        })
        .collect()
}

fn insert_join_bucket(buckets: &mut HashJoinBuckets, key: HashJoinKey, row: PlanRow) {
    let h = hash_join_mix(&key);
    let bucket = buckets.entry(h).or_default();
    if let Some((_, rows)) = bucket.iter_mut().find(|(k, _)| k == &key) {
        rows.push(row);
    } else {
        bucket.push((key, vec![row]));
    }
}

/// Mix join-key bindings for hash buckets; must satisfy `a == b ⇒ mix(a) == mix(b)` for [`PlanBinding`].
fn hash_join_mix(bindings: &[PlanBinding]) -> u64 {
    let mut hasher = RapidHasher::default();
    for b in bindings {
        hash_plan_binding_for_join(b, &mut hasher);
    }
    hasher.finish()
}

fn hash_plan_binding_for_join(binding: &PlanBinding, hasher: &mut RapidHasher<'_>) {
    match binding {
        PlanBinding::Vertex(v) => {
            hasher.write_u8(1);
            hasher.write_u32(u32::from(*v));
        }
        PlanBinding::Edge(e) => {
            hasher.write_u8(2);
            hasher.write_u32(u32::from(e.handle.owner_vertex_id));
            hasher.write_u32(e.handle.slot_index);
            hasher.write_u8(e.value_len());
            hasher.write(e.value_bytes_slice());
        }
        PlanBinding::Value(v) => {
            hasher.write_u8(3);
            hash_value_for_join(v, hasher);
        }
        PlanBinding::Path(pb) => {
            hasher.write_u8(4);
            hasher.write_u32(pb.shard_id);
            hasher.write_usize(pb.leaf_state_idx);
            hasher.write_usize(Arc::as_ptr(&pb.states) as usize);
            hasher.write_usize(pb.states.len());
        }
        PlanBinding::RemoteVertex(logical) => {
            hasher.write_u8(5);
            hasher.write_u64(*logical);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use pollster;
    #[test]
    fn cartesian_product_combines_independent_subplans() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryCartesianLeft"],
                [("name", Value::Text("Left A".into()))],
            )
            .expect("insert left a");
        store
            .insert_vertex_named(
                ["QueryCartesianLeft"],
                [("name", Value::Text("Left B".into()))],
            )
            .expect("insert left b");
        store
            .insert_vertex_named(
                ["QueryCartesianRight"],
                [("name", Value::Text("Right A".into()))],
            )
            .expect("insert right a");
        store
            .insert_vertex_named(
                ["QueryCartesianRight"],
                [("name", Value::Text("Right B".into()))],
            )
            .expect("insert right b");
        let plan = plan(vec![
            PlanOp::CartesianProduct {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryCartesianLeft".into()),
                    property_projection: None,
                }],
                right: vec![PlanOp::NodeScan {
                    variable: "b".into(),
                    label: Some("QueryCartesianRight".into()),
                    property_projection: None,
                }],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "left"),
                    project(prop("b", "name"), "right"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute cartesian product");

        assert_eq!(result.rows.len(), 4);
    }
    #[test]
    fn cartesian_product_drops_conflicting_bindings() {
        let left = PlanRow::from(BTreeMap::from([(
            "x".to_owned(),
            PlanBinding::Value(Value::Int64(1)),
        )]));
        let same = PlanRow::from(BTreeMap::from([(
            "x".to_owned(),
            PlanBinding::Value(Value::Int64(1)),
        )]));
        let different = PlanRow::from(BTreeMap::from([(
            "x".to_owned(),
            PlanBinding::Value(Value::Int64(2)),
        )]));

        assert_eq!(merge_rows(&left, &same), Some(left.clone()));
        assert_eq!(merge_rows(&left, &different), None);
    }
    #[test]
    fn hash_join_matches_planned_two_match() {
        let store = GraphStore::new();
        let alice = store
            .insert_vertex_named(
                ["QueryHashJoinUser"],
                [("name", Value::Text("HJ Alice".into()))],
            )
            .expect("insert alice");
        let bob = store
            .insert_vertex_named(
                ["QueryHashJoinTarget"],
                [("name", Value::Text("HJ Bob".into()))],
            )
            .expect("insert bob");
        store
            .insert_directed_edge_named(
                alice,
                bob,
                Some("QueryHashJoinKnows"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");

        let gql = "MATCH (a:QueryHashJoinUser) MATCH (a)-[r:QueryHashJoinKnows]->(b:QueryHashJoinTarget) \
                   RETURN a.name AS an, b.name AS bn";
        let sequential = plan_gql(gql);
        let seq_result = store
            .execute_plan_query(&sequential, &params(), GqlExecutionContext::default())
            .expect("sequential two-match");

        let hash_plan = plan(vec![
            PlanOp::HashJoin {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryHashJoinUser".into()),
                    property_projection: None,
                }],
                right: vec![
                    PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("QueryHashJoinUser".into()),
                        property_projection: None,
                    },
                    PlanOp::Expand {
                        src: "a".into(),
                        edge: "r".into(),
                        dst: "b".into(),
                        direction: EdgeDirection::PointingRight,
                        label: Some("QueryHashJoinKnows".into()),
                        label_expr: None,
                        var_len: None,
                        indexed_edge_equality: None,
                        edge_property_projection: None,
                        dst_property_projection: None,
                        hop_aux_binding: None,
                        emit_edge_binding: true,
                    },
                ],
                join_keys: vec!["a".into()],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "an"),
                    project(prop("b", "name"), "bn"),
                ],
                distinct: false,
            },
        ]);

        let hj_result = store
            .execute_plan_query(&hash_plan, &params(), GqlExecutionContext::default())
            .expect("hash join");

        assert_eq!(hj_result.rows.len(), seq_result.rows.len());
        assert_eq!(hj_result.rows, seq_result.rows);
    }
    #[test]
    fn hash_join_joins_equivalent_decimal_scales() {
        use gleaph_gql::types::Decimal;

        let lit_decimal = |s: &str| {
            Expr::new(ExprKind::Literal(Value::Decimal(
                Decimal::parse(s).expect("decimal literal"),
            )))
        };
        let lit_text = |t: &str| Expr::new(ExprKind::Literal(Value::Text(t.into())));

        let plan = plan(vec![PlanOp::HashJoin {
            left: vec![PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: lit_decimal("1.0"),
                        alias: Some("k".into()),
                    },
                    ProjectColumn {
                        expr: lit_text("L"),
                        alias: Some("left_tag".into()),
                    },
                ],
                distinct: false,
            }],
            right: vec![PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: lit_decimal("1.00"),
                        alias: Some("k".into()),
                    },
                    ProjectColumn {
                        expr: lit_text("R"),
                        alias: Some("right_tag".into()),
                    },
                ],
                distinct: false,
            }],
            join_keys: vec!["k".into()],
        }]);

        let store = GraphStore::new();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("decimal hash join");

        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert_eq!(row.get("left_tag"), Some(&Value::Text("L".into())));
        assert_eq!(row.get("right_tag"), Some(&Value::Text("R".into())));
        assert_eq!(
            row.get("k"),
            Some(&Value::Decimal(Decimal::parse("1.0").expect("k")))
        );
    }

    /// Two left + three right rows share the same join key → six merged rows (L×R multiplicity).
    /// Also checks `right_id` survives `merge_rows` (right-only binding) alongside `left_id`.    #[test]
    fn hash_join_same_key_row_multiplicity_2x3() {
        let store = GraphStore::new();
        for (left_id, tag) in [(0i64, "L0"), (1, "L1")] {
            store
                .insert_vertex_named(
                    ["QueryHashJoinDupL"],
                    [
                        ("jk", Value::Int64(7)),
                        ("left_id", Value::Int64(left_id)),
                        ("left_tag", Value::Text(tag.into())),
                    ],
                )
                .expect("insert left");
        }
        for (right_id, tag) in [(0i64, "R0"), (1, "R1"), (2, "R2")] {
            store
                .insert_vertex_named(
                    ["QueryHashJoinDupR"],
                    [
                        ("jk", Value::Int64(7)),
                        ("right_id", Value::Int64(right_id)),
                        ("right_tag", Value::Text(tag.into())),
                    ],
                )
                .expect("insert right");
        }

        let scan_project_l = || {
            vec![
                PlanOp::NodeScan {
                    variable: "nl".into(),
                    label: Some("QueryHashJoinDupL".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("nl", "jk"), "jk"),
                        project(prop("nl", "left_id"), "left_id"),
                        project(prop("nl", "left_tag"), "left_tag"),
                    ],
                    distinct: false,
                },
            ]
        };
        let scan_project_r = || {
            vec![
                PlanOp::NodeScan {
                    variable: "nr".into(),
                    label: Some("QueryHashJoinDupR".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("nr", "jk"), "jk"),
                        project(prop("nr", "right_id"), "right_id"),
                        project(prop("nr", "right_tag"), "right_tag"),
                    ],
                    distinct: false,
                },
            ]
        };

        let plan = plan(vec![PlanOp::HashJoin {
            left: scan_project_l(),
            right: scan_project_r(),
            join_keys: vec!["jk".into()],
        }]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("hash join multiplicity");

        assert_eq!(result.rows.len(), 6);
        let mut pairs: Vec<(i64, i64)> = result
            .rows
            .iter()
            .map(|row| {
                let li = match row.get("left_id") {
                    Some(Value::Int64(x)) => *x,
                    other => panic!("expected int left_id, got {other:?}"),
                };
                let ri = match row.get("right_id") {
                    Some(Value::Int64(x)) => *x,
                    other => panic!("expected int right_id, got {other:?}"),
                };
                (li, ri)
            })
            .collect();
        pairs.sort();
        assert_eq!(pairs, vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2),]);
        for row in &result.rows {
            assert_eq!(row.get("jk"), Some(&Value::Int64(7)));
            assert!(row.get("left_tag").is_some());
            assert!(row.get("right_tag").is_some());
        }
    }
    #[test]
    fn hash_join_two_join_keys_excludes_partial_match() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyL"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(2)),
                    ("lt", Value::Text("L12".into())),
                ],
            )
            .expect("insert L12");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyL"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(3)),
                    ("lt", Value::Text("L13".into())),
                ],
            )
            .expect("insert L13");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyR"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(2)),
                    ("rt", Value::Text("R12".into())),
                ],
            )
            .expect("insert R12");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyR"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(99)),
                    ("rt", Value::Text("R199".into())),
                ],
            )
            .expect("insert R199");

        let plan = plan(vec![PlanOp::HashJoin {
            left: vec![
                PlanOp::NodeScan {
                    variable: "l".into(),
                    label: Some("QueryHashJoin2KeyL".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("l", "ka"), "ka"),
                        project(prop("l", "kb"), "kb"),
                        project(prop("l", "lt"), "lt"),
                    ],
                    distinct: false,
                },
            ],
            right: vec![
                PlanOp::NodeScan {
                    variable: "r".into(),
                    label: Some("QueryHashJoin2KeyR".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("r", "ka"), "ka"),
                        project(prop("r", "kb"), "kb"),
                        project(prop("r", "rt"), "rt"),
                    ],
                    distinct: false,
                },
            ],
            join_keys: vec!["ka".into(), "kb".into()],
        }]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("two-key hash join");

        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert_eq!(row.get("ka"), Some(&Value::Int64(1)));
        assert_eq!(row.get("kb"), Some(&Value::Int64(2)));
        assert_eq!(row.get("lt"), Some(&Value::Text("L12".into())));
        assert_eq!(row.get("rt"), Some(&Value::Text("R12".into())));
    }
    #[test]
    fn hash_join_matches_sequential_on_branching_graph() {
        let store = GraphStore::new();
        let alice = store
            .insert_vertex_named(
                ["QueryHashJoinBranchUser"],
                [("name", Value::Text("Branch Alice".into()))],
            )
            .expect("insert user");
        let bob = store
            .insert_vertex_named(
                ["QueryHashJoinBranchTarget"],
                [("name", Value::Text("Branch Bob".into()))],
            )
            .expect("insert bob");
        let carol = store
            .insert_vertex_named(
                ["QueryHashJoinBranchTarget"],
                [("name", Value::Text("Branch Carol".into()))],
            )
            .expect("insert carol");
        store
            .insert_directed_edge_named(
                alice,
                bob,
                Some("QueryHashJoinBranchRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge to bob");
        store
            .insert_directed_edge_named(
                alice,
                carol,
                Some("QueryHashJoinBranchRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge to carol");

        let gql = "MATCH (a:QueryHashJoinBranchUser) MATCH (a)-[r:QueryHashJoinBranchRel]->(b:QueryHashJoinBranchTarget) \
                   RETURN a.name AS an, b.name AS bn";
        let sequential = plan_gql(gql);
        let seq_result = store
            .execute_plan_query(&sequential, &params(), GqlExecutionContext::default())
            .expect("sequential two-match branching");

        let hash_plan = plan(vec![
            PlanOp::HashJoin {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryHashJoinBranchUser".into()),
                    property_projection: None,
                }],
                right: vec![
                    PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("QueryHashJoinBranchUser".into()),
                        property_projection: None,
                    },
                    PlanOp::Expand {
                        src: "a".into(),
                        edge: "r".into(),
                        dst: "b".into(),
                        direction: EdgeDirection::PointingRight,
                        label: Some("QueryHashJoinBranchRel".into()),
                        label_expr: None,
                        var_len: None,
                        indexed_edge_equality: None,
                        edge_property_projection: None,
                        dst_property_projection: None,
                        hop_aux_binding: None,
                        emit_edge_binding: true,
                    },
                ],
                join_keys: vec!["a".into()],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "an"),
                    project(prop("b", "name"), "bn"),
                ],
                distinct: false,
            },
        ]);

        let hj_result = store
            .execute_plan_query(&hash_plan, &params(), GqlExecutionContext::default())
            .expect("hash join branching");

        assert_eq!(hj_result.rows.len(), seq_result.rows.len());
        fn pair_key(row: &std::collections::BTreeMap<String, Value>) -> (String, String) {
            let an = match row.get("an") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("expected text an, got {other:?}"),
            };
            let bn = match row.get("bn") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("expected text bn, got {other:?}"),
            };
            (an, bn)
        }
        let mut hj_keys: Vec<_> = hj_result.rows.iter().map(pair_key).collect();
        hj_keys.sort();
        let mut seq_keys: Vec<_> = seq_result.rows.iter().map(pair_key).collect();
        seq_keys.sort();
        assert_eq!(hj_keys, seq_keys);
    }
}
