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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{NullOrder, SortDirection};
    use std::cmp::Ordering;

    #[test]
    fn nulls_first_ascending_places_null_before_value() {
        let ord = compare_sort_values(&Value::Null, &Value::Int64(1), None, Some(NullOrder::First))
            .unwrap();
        assert_eq!(ord, Ordering::Less);
    }

    #[test]
    fn nulls_last_ascending_places_null_after_value() {
        let ord = compare_sort_values(&Value::Null, &Value::Int64(1), None, Some(NullOrder::Last))
            .unwrap();
        assert_eq!(ord, Ordering::Greater);
    }

    #[test]
    fn nulls_first_descending_places_null_before_value() {
        let ord = compare_sort_values(
            &Value::Null,
            &Value::Int64(1),
            Some(SortDirection::Descending),
            Some(NullOrder::First),
        )
        .unwrap();
        assert_eq!(ord, Ordering::Less);
    }

    #[test]
    fn nulls_last_descending_places_null_after_value() {
        let ord = compare_sort_values(
            &Value::Null,
            &Value::Int64(1),
            Some(SortDirection::Descending),
            Some(NullOrder::Last),
        )
        .unwrap();
        assert_eq!(ord, Ordering::Greater);
    }

    #[test]
    fn default_null_order_follows_descending_direction() {
        let asc_null_vs_int =
            compare_sort_values(&Value::Null, &Value::Int64(1), None, None).unwrap();
        assert_eq!(asc_null_vs_int, Ordering::Greater);

        let desc_null_vs_int = compare_sort_values(
            &Value::Null,
            &Value::Int64(1),
            Some(SortDirection::Desc),
            None,
        )
        .unwrap();
        assert_eq!(desc_null_vs_int, Ordering::Less);
    }

    #[test]
    fn compare_sort_keys_uses_first_non_equal_key() {
        use gleaph_gql::ast::{OrderByClause, SortItem};
        use gleaph_gql::token::Span;

        let order_by = OrderByClause {
            span: Span::DUMMY,
            items: vec![
                SortItem {
                    span: Span::DUMMY,
                    expr: gleaph_gql::ast::Expr::var("a"),
                    direction: None,
                    null_order: None,
                },
                SortItem {
                    span: Span::DUMMY,
                    expr: gleaph_gql::ast::Expr::var("b"),
                    direction: None,
                    null_order: None,
                },
            ],
        };
        let left = vec![Value::Int64(1), Value::Int64(10)];
        let right = vec![Value::Int64(2), Value::Int64(1)];
        assert_eq!(
            compare_sort_keys(&left, &right, &order_by).unwrap(),
            Ordering::Less
        );
    }
}
