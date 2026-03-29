//! §14.12 — Select statement.
//!
//! GQL rules: selectStatement, selectItemList, selectItem,
//! havingClause, selectStatementBody, selectGraphMatchList,
//! selectGraphMatch, selectQuerySpecification.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── selectStatement ────────────────────────────────────────────────────
//   : SELECT setQuantifier? (ASTERISK | selectItemList)
//     (selectStatementBody whereClause? groupByClause? havingClause?
//      orderByClause? offsetClause? limitClause?)?
//   ;
mod select_statement {
    use super::*;

    /// SELECT * FROM g MATCH (n) — star, source is GraphMatchList
    #[test]
    fn select_star_from_graph_match() {
        let prog = p("SELECT * FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                assert!(matches!(sel.body, SelectBody::Star { .. }));
                assert!(matches!(&sel.source, Some(SelectSource::GraphMatchList(_))));
            } else {
                panic!("expected ResultStatement::Select, got {:?}", q.left.result);
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// SELECT n.name AS name FROM g MATCH (n) — items variant
    #[test]
    fn select_items_from_graph_match() {
        let prog = p("SELECT n.name AS name FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                if let SelectBody::Items { items, .. } = &sel.body {
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].alias, Some("name".to_string()));
                } else {
                    panic!("expected SelectBody::Items");
                }
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// SELECT DISTINCT n FROM g MATCH (n) — distinct flag
    #[test]
    fn select_distinct() {
        let prog = p("SELECT DISTINCT n FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                assert_eq!(
                    sel.set_quantifier,
                    SetQuantifier::Distinct,
                    "expected distinct"
                );
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// SELECT n FROM g MATCH (n) HAVING n.age > 0 — having clause
    #[test]
    fn select_with_having() {
        let prog = p("SELECT n FROM g MATCH (n) HAVING n.age > 0");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                match &sel.body {
                    SelectBody::Items { having, .. } => {
                        assert!(having.is_some(), "expected having clause");
                    }
                    SelectBody::Star { having, .. } => {
                        assert!(having.is_some(), "expected having clause");
                    }
                }
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// SELECT n FROM { MATCH (n) RETURN n } — source is QuerySpecification(Nested)
    #[test]
    fn select_from_nested_query() {
        let prog = p("SELECT n FROM { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                assert!(
                    matches!(
                        &sel.source,
                        Some(SelectSource::QuerySpecification(
                            SelectQuerySpecification::Nested(_)
                        ))
                    ),
                    "expected SelectSource::QuerySpecification(Nested), got {:?}",
                    sel.source
                );
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── selectItemList ─────────────────────────────────────────────────────
//   : selectItem (COMMA selectItem)*
//   ;
mod select_item_list {
    use super::*;

    /// Single select item
    #[test]
    fn single_item() {
        let prog = p("SELECT n FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                if let SelectBody::Items { items, .. } = &sel.body {
                    assert_eq!(items.len(), 1);
                } else {
                    panic!("expected SelectBody::Items");
                }
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// Multiple comma-separated select items
    #[test]
    fn multiple_items() {
        let prog = p("SELECT n.name, n.age FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                if let SelectBody::Items { items, .. } = &sel.body {
                    assert_eq!(items.len(), 2);
                } else {
                    panic!("expected SelectBody::Items");
                }
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── selectItem ─────────────────────────────────────────────────────────
//   : aggregatingValueExpression selectItemAlias?
//   ;
mod select_item {
    use super::*;

    /// Select item without alias
    #[test]
    fn without_alias() {
        let prog = p("SELECT n FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                if let SelectBody::Items { items, .. } = &sel.body {
                    assert_eq!(items[0].expr, Expr::var("n"));
                    assert!(items[0].alias.is_none());
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// Select item with alias
    #[test]
    fn with_alias() {
        let prog = p("SELECT n AS node FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                if let SelectBody::Items { items, .. } = &sel.body {
                    assert_eq!(items[0].alias, Some("node".to_string()));
                } else {
                    panic!("expected Items");
                }
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── selectGraphMatch ───────────────────────────────────────────────────
//   : graphExpression matchStatement
//   ;
mod select_graph_match {
    use super::*;

    /// FROM g MATCH (n) — graph expression + match
    #[test]
    fn graph_and_match() {
        let prog = p("SELECT n FROM g MATCH (n)");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                if let Some(SelectSource::GraphMatchList(list)) = &sel.source {
                    assert_eq!(list.len(), 1);
                    assert!(!list[0].graph.parts.is_empty());
                } else {
                    panic!("expected GraphMatchList, got {:?}", sel.source);
                }
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── selectQuerySpecification ───────────────────────────────────────────
//   : nestedQuerySpecification
//   | graphExpression nestedQuerySpecification
//   ;
mod select_query_specification {
    use super::*;

    /// FROM { MATCH (n) RETURN n } — bare nested query
    #[test]
    fn nested_query() {
        let prog = p("SELECT n FROM { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                assert!(matches!(
                    &sel.source,
                    Some(SelectSource::QuerySpecification(
                        SelectQuerySpecification::Nested(_)
                    ))
                ));
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// FROM g { MATCH (n) RETURN n } — graph + nested query
    #[test]
    fn graph_nested_query() {
        let prog = p("SELECT n FROM g { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Select(sel)) = &q.left.result {
                assert!(
                    matches!(
                        &sel.source,
                        Some(SelectSource::QuerySpecification(
                            SelectQuerySpecification::GraphNested { .. }
                        ))
                    ),
                    "expected GraphNested, got {:?}",
                    sel.source
                );
            } else {
                panic!("expected Select");
            }
        } else {
            panic!("expected Query");
        }
    }
}
