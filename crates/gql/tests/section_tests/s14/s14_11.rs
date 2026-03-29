//! §14.11 — Return statement.
//!
//! GQL rules: returnStatement, returnStatementBody, returnItemList,
//! returnItem, returnItemAlias, groupByClause.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── returnStatement ────────────────────────────────────────────────────
//   : RETURN returnStatementBody
//   ;
mod return_statement {
    use super::*;

    /// RETURN * — star body
    #[test]
    fn return_star() {
        let prog = p("MATCH (n) RETURN *");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                assert!(
                    matches!(ret.body, ReturnBody::Star),
                    "expected ReturnBody::Star, got {:?}",
                    ret.body
                );
            } else {
                panic!("expected ResultStatement::Return");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// RETURN n — single item, no alias
    #[test]
    fn return_single_item() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { items, .. } = &ret.body {
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].expr, Expr::var("n"));
                    assert!(items[0].alias.is_none());
                } else {
                    panic!("expected ReturnBody::Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// RETURN DISTINCT n — distinct flag
    #[test]
    fn return_distinct() {
        let prog = p("MATCH (n) RETURN DISTINCT n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                assert_eq!(
                    ret.set_quantifier,
                    SetQuantifier::Distinct,
                    "expected distinct"
                );
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// RETURN n — distinct is false by default
    #[test]
    fn return_not_distinct() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                assert_eq!(
                    ret.set_quantifier,
                    SetQuantifier::None,
                    "expected no quantifier"
                );
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── returnStatementBody ────────────────────────────────────────────────
//   : setQuantifier? (ASTERISK | returnItemList) groupByClause?
//   ;
mod return_statement_body {
    use super::*;

    /// RETURN * — asterisk variant
    #[test]
    fn asterisk() {
        let prog = p("MATCH (n) RETURN *");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                assert!(matches!(ret.body, ReturnBody::Star));
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// RETURN n, m — item list variant
    #[test]
    fn item_list() {
        let prog = p("MATCH (n)-[]->(m) RETURN n, m");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { items, .. } = &ret.body {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0].expr, Expr::var("n"));
                    assert_eq!(items[1].expr, Expr::var("m"));
                } else {
                    panic!("expected ReturnBody::Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// RETURN DISTINCT n — set quantifier DISTINCT
    #[test]
    fn distinct_quantifier() {
        let prog = p("MATCH (n) RETURN DISTINCT n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                assert_eq!(ret.set_quantifier, SetQuantifier::Distinct);
                assert!(matches!(ret.body, ReturnBody::Items { .. }));
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── returnItemList ─────────────────────────────────────────────────────
//   : returnItem (COMMA returnItem)*
//   ;
mod return_item_list {
    use super::*;

    /// Single return item
    #[test]
    fn single_item() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { items, .. } = &ret.body {
                    assert_eq!(items.len(), 1);
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// Multiple comma-separated return items
    #[test]
    fn multiple_items() {
        let prog = p("MATCH (n) RETURN n.name, n.age, n.city");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { items, .. } = &ret.body {
                    assert_eq!(items.len(), 3);
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── returnItem ─────────────────────────────────────────────────────────
//   : aggregatingValueExpression returnItemAlias?
//   ;
mod return_item {
    use super::*;

    /// Return item without alias
    #[test]
    fn without_alias() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { items, .. } = &ret.body {
                    assert_eq!(items[0].expr, Expr::var("n"));
                    assert!(items[0].alias.is_none());
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// Return item with alias
    #[test]
    fn with_alias() {
        let prog = p("MATCH (n) RETURN n AS name");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { items, .. } = &ret.body {
                    assert_eq!(items[0].expr, Expr::var("n"));
                    assert_eq!(items[0].alias, Some("name".to_string()));
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── groupByClause ──────────────────────────────────────────────────────
//   : GROUP BY groupingElementList
//   ;
mod group_by_clause {
    use super::*;

    /// RETURN with GROUP BY
    #[test]
    fn group_by_single() {
        let prog = p("MATCH (n) RETURN n.label AS lbl GROUP BY n.label");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { group_by, .. } = &ret.body {
                    let gb = group_by.as_ref().expect("expected group_by");
                    assert_eq!(gb.items.len(), 1);
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// RETURN with ORDER BY, LIMIT, OFFSET
    #[test]
    fn order_limit_offset() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age LIMIT 5 OFFSET 10");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items {
                    order_by,
                    limit,
                    offset,
                    ..
                } = &ret.body
                {
                    assert!(order_by.is_some(), "expected order_by");
                    assert!(limit.is_some(), "expected limit");
                    assert!(offset.is_some(), "expected offset");
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Return");
            }
        } else {
            panic!("expected Query");
        }
    }
}
