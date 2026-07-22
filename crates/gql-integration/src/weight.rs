//! Gleaph-specific `GLEAPH.WEIGHT` expression-shape classification.
//!
//! Pure helpers consumed by Graph execution to recognize and extract edge-variable references from
//! `GLEAPH.WEIGHT(...)` expressions.

use gleaph_gql::ast::{Expr, ExprKind, ObjectName};
use gleaph_graph_kernel::gql_dialect::GLEAPH_WEIGHT;

/// How [`GLEAPH.WEIGHT`] names an edge in an expression.
#[derive(Clone, Debug, PartialEq)]
pub enum GleaphWeightEdgeRef {
    /// Single-hop expand or shortest-path relax step.
    SingletonVar(String),
    /// Indexed element of a variable-length edge group (`e[-1]`, `e[0]`, …).
    /// Reachable only through the cypher list-index expression.
    #[cfg(feature = "cypher")]
    GroupElement { group_var: String, index: Box<Expr> },
}

/// True when `name` is an unqualified `GLEAPH.WEIGHT` function reference.
pub fn is_gleaph_weight_call(name: &ObjectName, distinct: bool) -> bool {
    !distinct && GLEAPH_WEIGHT.matches_ascii_case_insensitive(&name.parts)
}

/// Returns the single argument of a `GLEAPH.WEIGHT` call, if exactly one is present.
pub fn gleaph_weight_single_arg(args: &[Expr]) -> Option<&Expr> {
    if args.len() == 1 {
        Some(&args[0])
    } else {
        None
    }
}

/// Resolves the edge variable referenced by a `GLEAPH.WEIGHT` argument expression.
pub fn gleaph_weight_edge_ref(expr: &Expr) -> Option<GleaphWeightEdgeRef> {
    match &expr.kind {
        ExprKind::Paren(inner) => gleaph_weight_edge_ref(inner),
        ExprKind::Variable(v) => Some(GleaphWeightEdgeRef::SingletonVar(v.clone())),
        #[cfg(feature = "cypher")]
        ExprKind::ListIndex { list, index } => {
            let ExprKind::Variable(v) = &list.kind else {
                return None;
            };
            Some(GleaphWeightEdgeRef::GroupElement {
                group_var: v.clone(),
                index: index.clone(),
            })
        }
        _ => None,
    }
}

/// Returns the edge variable name referenced by a `GLEAPH.WEIGHT` argument, if any.
pub fn gleaph_weight_arg_edge_var(expr: &Expr) -> Option<String> {
    match gleaph_weight_edge_ref(expr)? {
        GleaphWeightEdgeRef::SingletonVar(v) => Some(v),
        #[cfg(feature = "cypher")]
        GleaphWeightEdgeRef::GroupElement { group_var, .. } => Some(group_var),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn is_gleaph_weight_call_recognizes_weight() {
        let name = ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]);
        assert!(is_gleaph_weight_call(&name, false));
        assert!(!is_gleaph_weight_call(&name, true));
    }

    #[cfg(feature = "cypher")]
    #[test]
    fn gleaph_weight_edge_ref_recognizes_group_element() {
        use gleaph_gql::value::Value;
        let list = Expr::var("e");
        let index = Expr::new(ExprKind::Literal(Value::Int64(-1)));
        let expr = Expr::new(ExprKind::ListIndex {
            list: Box::new(list),
            index: Box::new(index),
        });
        assert!(
            matches!(
                gleaph_weight_edge_ref(&expr),
                Some(GleaphWeightEdgeRef::GroupElement { group_var, .. })
                if group_var == "e"
            ),
            "expected e[-1] to resolve to group element edge ref"
        );
    }
}
