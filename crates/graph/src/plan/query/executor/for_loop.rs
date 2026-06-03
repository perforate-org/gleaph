//! §14.8 FOR statement: unnest a list expression into rows.

use gleaph_gql::Value;
use gleaph_gql::ast::Expr;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::PlanBinding;
use super::context::QueryExprEvaluator;

pub(crate) fn execute_for(
    evaluator: &QueryExprEvaluator<'_>,
    rows: Vec<PlanRow>,
    variable: &str,
    list: &Expr,
    ordinality: Option<&str>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        unnest_row(evaluator, &row, variable, list, ordinality, &mut out)?;
    }
    Ok(out)
}

fn unnest_row(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    variable: &str,
    list: &Expr,
    ordinality: Option<&str>,
    out: &mut Vec<PlanRow>,
) -> Result<(), PlanQueryError> {
    let value = evaluator.eval_expr(row, list)?;
    let Value::List(elements) = value else {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: format!("FOR ... IN {value:?}"),
        });
    };
    for (i, element) in elements.into_iter().enumerate() {
        let mut expanded = row.clone();
        expanded.insert(variable.to_owned(), PlanBinding::Value(element));
        if let Some(ord_var) = ordinality {
            expanded.insert(
                ord_var.to_owned(),
                PlanBinding::Value(Value::Int64((i as i64) + 1)),
            );
        }
        out.push(expanded);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{Expr, ExprKind};

    use crate::facade::GraphStore;

    use super::super::test_support::params;

    fn eval_for(list: Expr, ordinality: Option<&str>) -> Vec<PlanRow> {
        let store = GraphStore::new();
        let evaluator = QueryExprEvaluator {
            store: &store,
            parameters: &params(),
            aggregate_specs: None,
            caller: None,
            gleaph_weight_decoders: None,
        };
        execute_for(&evaluator, vec![PlanRow::new()], "x", &list, ordinality).expect("execute_for")
    }

    #[test]
    fn for_literal_list_expands() {
        let list = Expr::new(ExprKind::Literal(Value::List(vec![
            Value::Int64(1),
            Value::Int64(2),
            Value::Int64(3),
        ])));
        let rows = eval_for(list, None);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn for_with_ordinality_is_one_based() {
        let list = Expr::new(ExprKind::Literal(Value::List(vec![
            Value::Int64(10),
            Value::Int64(20),
        ])));
        let rows = eval_for(list, Some("i"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("i"), Some(&PlanBinding::Value(Value::Int64(1))));
        assert_eq!(rows[1].get("i"), Some(&PlanBinding::Value(Value::Int64(2))));
    }

    #[test]
    fn for_null_list_produces_no_rows() {
        let list = Expr::new(ExprKind::Literal(Value::Null));
        let rows = eval_for(list, None);
        assert!(rows.is_empty());
    }

    #[test]
    fn for_non_list_errors() {
        let store = GraphStore::new();
        let evaluator = QueryExprEvaluator {
            store: &store,
            parameters: &params(),
            aggregate_specs: None,
            caller: None,
            gleaph_weight_decoders: None,
        };
        let list = Expr::new(ExprKind::Literal(Value::Int64(1)));
        let err =
            execute_for(&evaluator, vec![PlanRow::new()], "x", &list, None).expect_err("non-list");
        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }
}
