//! Additional statement.rs coverage tests.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── Composite query set operations ──────────────────────────────────────

mod set_operations {
    use super::*;

    fn set_op(prog: &GqlProgram) -> &SetOp {
        let b = body(prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(!cq.rest.is_empty(), "expected composite query with rest");
                &cq.rest[0].0
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn union_distinct() {
        let prog = p("MATCH (n) RETURN n.x UNION DISTINCT MATCH (m) RETURN m.x");
        assert_eq!(*set_op(&prog), SetOp::UnionDistinct);
    }

    #[test]
    fn except_all() {
        let prog = p("MATCH (n) RETURN n.x EXCEPT ALL MATCH (m) RETURN m.x");
        assert_eq!(*set_op(&prog), SetOp::ExceptAll);
    }

    #[test]
    fn except_distinct() {
        let prog = p("MATCH (n) RETURN n.x EXCEPT DISTINCT MATCH (m) RETURN m.x");
        assert_eq!(*set_op(&prog), SetOp::ExceptDistinct);
    }

    #[test]
    fn intersect_all() {
        let prog = p("MATCH (n) RETURN n.x INTERSECT ALL MATCH (m) RETURN m.x");
        assert_eq!(*set_op(&prog), SetOp::IntersectAll);
    }

    #[test]
    fn intersect_distinct() {
        let prog = p("MATCH (n) RETURN n.x INTERSECT DISTINCT MATCH (m) RETURN m.x");
        assert_eq!(*set_op(&prog), SetOp::IntersectDistinct);
    }

    #[test]
    fn except_bare() {
        let prog = p("MATCH (n) RETURN n.x EXCEPT MATCH (m) RETURN m.x");
        assert_eq!(*set_op(&prog), SetOp::Except);
    }

    #[test]
    fn intersect_bare() {
        let prog = p("MATCH (n) RETURN n.x INTERSECT MATCH (m) RETURN m.x");
        assert_eq!(*set_op(&prog), SetOp::Intersect);
    }
}

// ── Schema references ───────────────────────────────────────────────────

mod schema_references {
    use super::*;

    #[test]
    fn relative_schema_ref() {
        let prog = p("AT ../other MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(cq.left.at_schema.is_some());
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn absolute_schema_ref_multi_segment() {
        let prog = p("CREATE SCHEMA /db/schema1");
        let b = body(&prog);
        match &b.first {
            Statement::CreateSchema(cs) => {
                assert!(cs.name.parts.len() >= 2);
            }
            other => panic!("expected CreateSchema, got {other:?}"),
        }
    }
}

// ── SELECT variants ─────────────────────────────────────────────────────

mod select_variants {
    use super::*;

    #[test]
    fn select_all() {
        let prog = p("SELECT ALL n.name FROM myGraph MATCH (n)");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
                ResultStatement::Select(sel) => {
                    assert_eq!(sel.set_quantifier, SetQuantifier::All);
                }
                other => panic!("expected Select, got {other:?}"),
            },
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn select_without_from() {
        let prog = p("SELECT 1 + 2");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
                ResultStatement::Select(sel) => {
                    assert!(sel.source.is_none());
                }
                other => panic!("expected Select, got {other:?}"),
            },
            other => panic!("expected Query, got {other:?}"),
        }
    }
}

// ── SET/REMOVE label variants ───────────────────────────────────────────

mod set_remove_labels {
    use super::*;

    #[test]
    fn set_label_with_is() {
        let prog = p("MATCH (n) SET n IS Person");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(
                    cq.left
                        .parts
                        .iter()
                        .any(|p| matches!(p, SimpleQueryStatement::Set(_)))
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn set_label_with_colon() {
        let prog = p("MATCH (n) SET n :Person");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(
                    cq.left
                        .parts
                        .iter()
                        .any(|p| matches!(p, SimpleQueryStatement::Set(_)))
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn remove_label_with_is() {
        let prog = p("MATCH (n) REMOVE n IS Person");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(
                    cq.left
                        .parts
                        .iter()
                        .any(|p| matches!(p, SimpleQueryStatement::Remove(_)))
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn remove_label_with_colon() {
        let prog = p("MATCH (n) REMOVE n :Person");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(
                    cq.left
                        .parts
                        .iter()
                        .any(|p| matches!(p, SimpleQueryStatement::Remove(_)))
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}

// ── CALL procedure ──────────────────────────────────────────────────────

mod call_procedure {
    use super::*;

    #[test]
    fn call_no_args() {
        let prog = p("CALL myProc");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(matches!(
                    &cq.left.parts[0],
                    SimpleQueryStatement::CallProcedure(_)
                ));
            }
            other => panic!("expected Query with CallProcedure, got {other:?}"),
        }
    }
}

// ── Object name with slash-dot ──────────────────────────────────────────

mod object_names {
    use super::*;

    #[test]
    fn slash_dot_name() {
        let prog = p("USE /db/schema.graph MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(matches!(
                    &cq.left.parts[0],
                    SimpleQueryStatement::Focused { .. }
                ));
            }
            other => panic!("expected Query with Focused, got {other:?}"),
        }
    }
}

// ── DELETE variants ─────────────────────────────────────────────────────

mod delete_variants {
    use super::*;

    #[test]
    fn detach_delete() {
        let prog = p("MATCH (n) DETACH DELETE n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(
                    cq.left
                        .parts
                        .iter()
                        .any(|p| matches!(p, SimpleQueryStatement::Delete(_)))
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn nodetach_delete() {
        let prog = p("MATCH (n) NODETACH DELETE n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                assert!(
                    cq.left
                        .parts
                        .iter()
                        .any(|p| matches!(p, SimpleQueryStatement::Delete(_)))
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
