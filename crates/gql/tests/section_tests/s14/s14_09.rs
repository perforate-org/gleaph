//! §14.9 — Order by and page statement.
//!
//! GQL rules: orderByAndPageStatement, orderByClause, sortSpecificationList,
//! sortSpecification, orderingSpecification, nullOrdering, offsetClause,
//! limitClause.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── orderByAndPageStatement ────────────────────────────────────────────
//   : orderByClause offsetClause? limitClause?
//   | offsetClause limitClause?
//   | limitClause
//   ;
mod order_by_and_page_statement {
    use super::*;

    /// ORDER BY n.age as a standalone query step
    #[test]
    fn order_by_as_query_step() {
        let prog = p("MATCH (n) ORDER BY n.age RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let ob = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::OrderBy(_)));
            assert!(
                ob.is_some(),
                "expected SimpleQueryStatement::OrderBy in parts"
            );
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// ORDER BY in a RETURN clause
    #[test]
    fn order_by_in_return() {
        let prog = p("MATCH (n) RETURN n.age AS a ORDER BY a");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    assert!(order_by.is_some(), "expected order_by in ReturnBody::Items");
                    let ob = order_by.as_ref().unwrap();
                    assert_eq!(ob.items.len(), 1);
                } else {
                    panic!("expected ReturnBody::Items");
                }
            } else {
                panic!("expected ResultStatement::Return");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// LIMIT in a RETURN clause
    #[test]
    fn limit_in_return() {
        let prog = p("MATCH (n) RETURN n LIMIT 5");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { limit, .. } = &ret.body {
                    assert!(limit.is_some(), "expected limit in ReturnBody::Items");
                    assert_eq!(limit.as_ref().unwrap().count, Expr::int(5));
                } else {
                    panic!("expected ReturnBody::Items");
                }
            } else {
                panic!("expected ResultStatement::Return");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// OFFSET in a RETURN clause
    #[test]
    fn offset_in_return() {
        let prog = p("MATCH (n) RETURN n OFFSET 10");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { offset, .. } = &ret.body {
                    assert!(offset.is_some(), "expected offset in ReturnBody::Items");
                    assert_eq!(offset.as_ref().unwrap().count, Expr::int(10));
                } else {
                    panic!("expected ReturnBody::Items");
                }
            } else {
                panic!("expected ResultStatement::Return");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }
}

// ── orderByClause ──────────────────────────────────────────────────────
//   : ORDER BY sortSpecificationList
//   ;
mod order_by_clause {
    use super::*;

    /// ORDER BY single expression
    #[test]
    fn single_sort_key() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    let ob = order_by.as_ref().expect("expected order_by");
                    assert_eq!(ob.items.len(), 1);
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

    /// ORDER BY multiple expressions
    #[test]
    fn multiple_sort_keys() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age, n.name");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    let ob = order_by.as_ref().expect("expected order_by");
                    assert_eq!(ob.items.len(), 2);
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
}

// ── sortSpecification ──────────────────────────────────────────────────
//   : sortKey orderingSpecification? nullOrdering?
//   ;
mod sort_specification {
    use super::*;

    /// Sort key only — no direction, no null ordering
    #[test]
    fn key_only() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    let item = &order_by.as_ref().unwrap().items[0];
                    assert!(item.direction.is_none());
                    assert!(item.null_order.is_none());
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

    /// Sort key with DESC direction
    #[test]
    fn with_desc() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age DESC");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    let item = &order_by.as_ref().unwrap().items[0];
                    assert_eq!(item.direction, Some(SortDirection::Desc));
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

    /// Sort key with ASC direction
    #[test]
    fn with_asc() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age ASC");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    let item = &order_by.as_ref().unwrap().items[0];
                    assert_eq!(item.direction, Some(SortDirection::Asc));
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

    /// Sort key with DESC NULLS FIRST
    #[test]
    fn desc_nulls_first() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age DESC NULLS FIRST");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    let item = &order_by.as_ref().unwrap().items[0];
                    assert_eq!(item.direction, Some(SortDirection::Desc));
                    assert_eq!(item.null_order, Some(NullOrder::First));
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

    /// Sort key with NULLS LAST (no explicit direction)
    #[test]
    fn nulls_last() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age NULLS LAST");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    let item = &order_by.as_ref().unwrap().items[0];
                    assert!(item.direction.is_none());
                    assert_eq!(item.null_order, Some(NullOrder::Last));
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

// ── offsetClause ───────────────────────────────────────────────────────
//   : OFFSET nonNegativeIntegerSpecification
//   ;
mod offset_clause {
    use super::*;

    /// OFFSET integer
    #[test]
    fn offset_integer() {
        let prog = p("MATCH (n) RETURN n OFFSET 10");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { offset, .. } = &ret.body {
                    let off = offset.as_ref().expect("expected offset");
                    assert_eq!(off.count, Expr::int(10));
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

// ── limitClause ────────────────────────────────────────────────────────
//   : LIMIT nonNegativeIntegerSpecification
//   ;
mod limit_clause {
    use super::*;

    /// LIMIT integer
    #[test]
    fn limit_integer() {
        let prog = p("MATCH (n) RETURN n LIMIT 5");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { limit, .. } = &ret.body {
                    let lim = limit.as_ref().expect("expected limit");
                    assert_eq!(lim.count, Expr::int(5));
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

    /// Combined ORDER BY, OFFSET, LIMIT
    #[test]
    fn combined_order_offset_limit() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age OFFSET 10 LIMIT 5");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items {
                    order_by,
                    offset,
                    limit,
                    ..
                } = &ret.body
                {
                    assert!(order_by.is_some(), "expected order_by");
                    assert_eq!(offset.as_ref().unwrap().count, Expr::int(10));
                    assert_eq!(limit.as_ref().unwrap().count, Expr::int(5));
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
