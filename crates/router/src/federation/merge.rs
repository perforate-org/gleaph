//! Merge partial results from per-shard graph execution.
//!
//! Federation v1 unions independent shard-local query fragments by concatenating row batches
//! (`rows_blob`) and summing row counts. Queries with mergeable `PlanOp::Aggregate` use
//! group-key merge instead (see `aggregate_merge.rs`).

use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::plan_exec::{ExecutePlanResult, LabelTelemetryEventWire};

use super::aggregate_merge::{FederatedMergeMode, merge_optional_aggregate_blobs};

/// Sum shard-local row counts for independent query fragments.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "public federation merge helper; exercised in unit tests"
    )
)]
pub fn merge_row_counts(shard_row_counts: impl IntoIterator<Item = u64>) -> u64 {
    shard_row_counts
        .into_iter()
        .fold(0u64, |total, rows| total.saturating_add(rows))
}

/// Accumulate one shard's row count into a running total.
#[inline]
pub fn merge_add_row_count(total: u64, shard_rows: u64) -> u64 {
    total.saturating_add(shard_rows)
}

/// Merge one shard [`ExecutePlanResult`] into an accumulator.
pub fn merge_execute_plan_result(
    acc: &mut ExecutePlanResult,
    shard: ExecutePlanResult,
    mode: FederatedMergeMode,
) -> Result<(), String> {
    acc.label_telemetry_events
        .extend(shard.label_telemetry_events);
    acc.rows_blob = match &mode {
        FederatedMergeMode::UnionRows => {
            acc.row_count = merge_add_row_count(acc.row_count, shard.row_count);
            IcWirePlanQueryResult::merge_optional_batch_blobs(acc.rows_blob.take(), shard.rows_blob)
                .map_err(|e| e.to_string())?
        }
        FederatedMergeMode::Aggregate(spec) => {
            let merged =
                merge_optional_aggregate_blobs(acc.rows_blob.take(), shard.rows_blob, spec)?;
            acc.row_count = merged
                .as_ref()
                .map(|blob| {
                    IcWirePlanQueryResult::decode_blob(blob)
                        .map(|decoded| decoded.rows.len() as u64)
                        .map_err(|e| e.to_string())
                })
                .transpose()?
                .unwrap_or(0);
            merged
        }
    };
    Ok(())
}

/// Empty query accumulator before merging shard fragments.
pub fn empty_execute_plan_result() -> ExecutePlanResult {
    ExecutePlanResult {
        row_count: 0,
        label_telemetry_events: Vec::<LabelTelemetryEventWire>::new(),
        rows_blob: None,
    }
}

#[cfg(test)]
mod tests {
    use gleaph_gql::Value;
    use gleaph_gql_ic::{IcWirePlanQueryResult, IcWirePlanQueryRow, IcWireValue};

    use gleaph_graph_kernel::plan_exec::ExecutePlanResult;

    use super::{
        empty_execute_plan_result, merge_add_row_count, merge_execute_plan_result, merge_row_counts,
    };
    use crate::federation::aggregate_merge::{
        AggregateMergeColumn, FederatedAggregateMerge, FederatedMergeMode,
    };
    use gleaph_gql::ast::AggregateFunc;

    fn sample_rows_blob(values: &[i64]) -> Vec<u8> {
        IcWirePlanQueryResult {
            rows: values
                .iter()
                .map(|n| IcWirePlanQueryRow {
                    columns: vec![("n".into(), IcWireValue::Int64(*n))],
                })
                .collect(),
        }
        .encode_blob()
        .expect("encode")
    }

    #[test]
    fn merge_row_counts_saturates_on_overflow() {
        assert_eq!(merge_row_counts([1, 2, 3]), 6);
        assert_eq!(merge_row_counts(std::iter::empty::<u64>()), 0);
        assert_eq!(merge_row_counts([u64::MAX, 1]), u64::MAX,);
    }

    #[test]
    fn merge_add_row_count_matches_fold() {
        let total = merge_add_row_count(merge_add_row_count(0, 4), 9);
        assert_eq!(total, 13);
    }

    #[test]
    fn merge_execute_plan_result_unions_rows_and_sums_counts() {
        let mut acc = empty_execute_plan_result();
        merge_execute_plan_result(
            &mut acc,
            ExecutePlanResult {
                row_count: 1,
                label_telemetry_events: Vec::new(),
                rows_blob: Some(sample_rows_blob(&[1])),
            },
            FederatedMergeMode::UnionRows,
        )
        .expect("first shard");
        merge_execute_plan_result(
            &mut acc,
            ExecutePlanResult {
                row_count: 2,
                label_telemetry_events: Vec::new(),
                rows_blob: Some(sample_rows_blob(&[2, 3])),
            },
            FederatedMergeMode::UnionRows,
        )
        .expect("second shard");
        assert_eq!(acc.row_count, 3);
        let merged = IcWirePlanQueryResult::decode_blob(acc.rows_blob.as_ref().unwrap())
            .expect("decode merged");
        assert_eq!(merged.rows.len(), 3);
        let values = merged
            .try_into_value_rows()
            .expect("values")
            .into_iter()
            .map(|row| match row.get("n") {
                Some(Value::Int64(v)) => *v,
                other => panic!("unexpected column: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[test]
    fn merge_execute_plan_result_merges_aggregate_rows_and_row_count() {
        let spec = FederatedAggregateMerge {
            group_key_columns: vec![],
            aggregate_columns: vec![AggregateMergeColumn {
                name: "cnt".into(),
                func: AggregateFunc::CountStar,
            }],
            having: None,
        };
        let mut acc = empty_execute_plan_result();
        let count_blob = |n: i64| {
            IcWirePlanQueryResult {
                rows: vec![IcWirePlanQueryRow {
                    columns: vec![("cnt".into(), IcWireValue::Int64(n))],
                }],
            }
            .encode_blob()
            .expect("encode")
        };
        merge_execute_plan_result(
            &mut acc,
            ExecutePlanResult {
                row_count: 1,
                label_telemetry_events: Vec::new(),
                rows_blob: Some(count_blob(5)),
            },
            FederatedMergeMode::Aggregate(spec.clone()),
        )
        .expect("first shard");
        merge_execute_plan_result(
            &mut acc,
            ExecutePlanResult {
                row_count: 1,
                label_telemetry_events: Vec::new(),
                rows_blob: Some(count_blob(3)),
            },
            FederatedMergeMode::Aggregate(spec),
        )
        .expect("second shard");
        assert_eq!(acc.row_count, 1);
        let merged =
            IcWirePlanQueryResult::decode_blob(acc.rows_blob.as_ref().unwrap()).expect("decode");
        assert_eq!(merged.rows.len(), 1);
        assert_eq!(merged.rows[0].columns[0].1, IcWireValue::Int64(8));
    }
}
