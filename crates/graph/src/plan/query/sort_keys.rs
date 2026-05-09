//! Shared ORDER BY key comparison for query execution and aggregate ordering.

use super::error::PlanQueryError;
use gleaph_gql::Value;
use gleaph_gql::ast::{NullOrder, OrderByClause, SortDirection};
use gleaph_gql::value_cmp::compare_values;
use std::cmp::Ordering;

pub(crate) fn compare_sort_keys(
    left_keys: &[Value],
    right_keys: &[Value],
    order_by: &OrderByClause,
) -> Result<Ordering, PlanQueryError> {
    for ((left, right), item) in left_keys
        .iter()
        .zip(right_keys.iter())
        .zip(order_by.items.iter())
    {
        let ordering = compare_sort_values(left, right, item.direction, item.null_order)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

pub(crate) fn compare_sort_values(
    left: &Value,
    right: &Value,
    direction: Option<SortDirection>,
    null_order: Option<NullOrder>,
) -> Result<Ordering, PlanQueryError> {
    let descending = matches!(
        direction,
        Some(SortDirection::Desc | SortDirection::Descending)
    );
    let nulls_first = match null_order {
        Some(NullOrder::First) => true,
        Some(NullOrder::Last) => false,
        None => descending,
    };

    match (left == &Value::Null, right == &Value::Null) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => {
            return Ok(if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        (false, true) => {
            return Ok(if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            });
        }
        (false, false) => {}
    }

    let ordering =
        compare_values(left, right).ok_or_else(|| PlanQueryError::IncomparableSortValues {
            left: left.clone(),
            right: right.clone(),
        })?;
    Ok(if descending {
        ordering.reverse()
    } else {
        ordering
    })
}
