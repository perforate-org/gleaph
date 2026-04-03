//! §13 — Data modification statements.
//!
//! GQL rules: insertStatement, setStatement, setItem, removeStatement,
//! removeItem, deleteStatement, deleteItem.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

// ── insertStatement ─────────────────────────────────────────────────────
//   : INSERT insertGraphPattern
//   ;
mod insert_statement {
    use super::*;

    /// INSERT node with label and properties → Statement::Insert
    #[test]
    fn node_with_label_and_properties() {
        let prog = p("INSERT (:Person {name: 'Alice'})");
        let b = body(&prog);
        if let Statement::Insert(ins) = &b.first {
            assert_eq!(ins.patterns.len(), 1);
            let pat = &ins.patterns[0];
            assert_eq!(pat.elements.len(), 1);
            if let InsertElement::Node(node) = &pat.elements[0] {
                assert_eq!(node.labels, vec!["Person".to_string()]);
                assert_eq!(node.properties.len(), 1);
                assert_eq!(node.properties[0].name, "name");
            } else {
                panic!("expected InsertElement::Node");
            }
        } else {
            panic!("expected Statement::Insert, got {:?}", b.first);
        }
    }

    /// INSERT edge pattern: node-edge-node
    #[test]
    fn edge_pattern_node_edge_node() {
        let prog = p("INSERT (:Person {name: 'Alice'})-[:KNOWS]->(:Person {name: 'Bob'})");
        let b = body(&prog);
        if let Statement::Insert(ins) = &b.first {
            assert_eq!(ins.patterns.len(), 1);
            let elems = &ins.patterns[0].elements;
            assert_eq!(elems.len(), 3);
            assert!(matches!(&elems[0], InsertElement::Node(_)));
            if let InsertElement::Edge(edge) = &elems[1] {
                assert_eq!(edge.labels, vec!["KNOWS".to_string()]);
                assert_eq!(edge.direction, EdgeDirection::PointingRight);
            } else {
                panic!("expected InsertElement::Edge");
            }
            assert!(matches!(&elems[2], InsertElement::Node(_)));
        } else {
            panic!("expected Statement::Insert, got {:?}", b.first);
        }
    }

    /// INSERT with graph name is None for standalone
    #[test]
    fn standalone_insert_no_graph_name() {
        let prog = p("INSERT (:Label)");
        let b = body(&prog);
        if let Statement::Insert(ins) = &b.first {
            assert!(ins.graph_name.is_none());
        } else {
            panic!("expected Statement::Insert");
        }
    }
}

// ── setStatement ────────────────────────────────────────────────────────
//   : SET setItemList
//   ;
mod set_statement {
    use super::*;

    /// SET property: n.age = 30
    #[test]
    fn set_property() {
        let prog = p("MATCH (n) SET n.age = 30 RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let set_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Set(_)));
            if let Some(SimpleQueryStatement::Set(set)) = set_part {
                assert_eq!(set.items.len(), 1);
                if let SetItem::Property {
                    variable,
                    property,
                    value,
                    ..
                } = &set.items[0]
                {
                    assert_eq!(variable, "n");
                    assert_eq!(property, "age");
                    assert_eq!(*value, Expr::int(30));
                } else {
                    panic!("expected SetItem::Property");
                }
            } else {
                panic!("expected SimpleQueryStatement::Set in parts");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// SET all properties: n = {name: 'Alice'}
    #[test]
    fn set_all_properties() {
        let prog = p("MATCH (n) SET n = {name: 'Alice'} RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let set_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Set(_)));
            if let Some(SimpleQueryStatement::Set(set)) = set_part {
                assert_eq!(set.items.len(), 1);
                if let SetItem::AllProperties { variable, .. } = &set.items[0] {
                    assert_eq!(variable, "n");
                } else {
                    panic!("expected SetItem::AllProperties");
                }
            } else {
                panic!("expected SimpleQueryStatement::Set in parts");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// SET label: n IS Person
    #[test]
    fn set_label_is() {
        let prog = p("MATCH (n) SET n IS Person RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let set_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Set(_)));
            if let Some(SimpleQueryStatement::Set(set)) = set_part {
                assert_eq!(set.items.len(), 1);
                if let SetItem::Label {
                    variable, label, ..
                } = &set.items[0]
                {
                    assert_eq!(variable, "n");
                    assert_eq!(label, "Person");
                } else {
                    panic!("expected SetItem::Label");
                }
            } else {
                panic!("expected SimpleQueryStatement::Set in parts");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// SET multiple items: n.x = 1, n.y = 2
    #[test]
    fn set_multiple_items() {
        let prog = p("MATCH (n) SET n.x = 1, n.y = 2 RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let set_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Set(_)));
            if let Some(SimpleQueryStatement::Set(set)) = set_part {
                assert_eq!(set.items.len(), 2);
                assert!(
                    matches!(&set.items[0], SetItem::Property { property, .. } if property == "x")
                );
                assert!(
                    matches!(&set.items[1], SetItem::Property { property, .. } if property == "y")
                );
            } else {
                panic!("expected SimpleQueryStatement::Set in parts");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }
}

// ── setItem ─────────────────────────────────────────────────────────────
//   : setPropertyItem | setAllPropertiesItem | setLabelItem
//   ;
mod set_item {
    use super::*;

    /// setPropertyItem: v.prop = expr
    #[test]
    fn property_item() {
        let prog = p("MATCH (n) SET n.name = 'Bob' RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let set_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Set(_)));
            if let Some(SimpleQueryStatement::Set(set)) = set_part {
                if let SetItem::Property {
                    variable,
                    property,
                    value,
                    ..
                } = &set.items[0]
                {
                    assert_eq!(variable, "n");
                    assert_eq!(property, "name");
                    assert_eq!(*value, Expr::string("Bob"));
                } else {
                    panic!("expected SetItem::Property");
                }
            } else {
                panic!("expected Set");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// setAllPropertiesItem: v = {key: val}
    #[test]
    fn all_properties_item() {
        let prog = p("MATCH (n) SET n = {age: 25} RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let set_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Set(_)));
            if let Some(SimpleQueryStatement::Set(set)) = set_part {
                assert!(
                    matches!(&set.items[0], SetItem::AllProperties { variable, .. } if variable == "n")
                );
            } else {
                panic!("expected Set");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// setLabelItem: v IS Label
    #[test]
    fn label_item() {
        let prog = p("MATCH (n) SET n IS Employee RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let set_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Set(_)));
            if let Some(SimpleQueryStatement::Set(set)) = set_part {
                if let SetItem::Label {
                    variable, label, ..
                } = &set.items[0]
                {
                    assert_eq!(variable, "n");
                    assert_eq!(label, "Employee");
                } else {
                    panic!("expected SetItem::Label");
                }
            } else {
                panic!("expected Set");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── removeStatement ─────────────────────────────────────────────────────
//   : REMOVE removeItemList
//   ;
mod remove_statement {
    use super::*;

    /// REMOVE property: n.age
    #[test]
    fn remove_property() {
        let prog = p("MATCH (n) REMOVE n.age RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let rm = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Remove(_)));
            if let Some(SimpleQueryStatement::Remove(rem)) = rm {
                assert_eq!(rem.items.len(), 1);
                if let RemoveItem::Property {
                    variable, property, ..
                } = &rem.items[0]
                {
                    assert_eq!(variable, "n");
                    assert_eq!(property, "age");
                } else {
                    panic!("expected RemoveItem::Property");
                }
            } else {
                panic!("expected SimpleQueryStatement::Remove in parts");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// REMOVE label: n IS Person
    #[test]
    fn remove_label() {
        let prog = p("MATCH (n) REMOVE n IS Person RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let rm = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Remove(_)));
            if let Some(SimpleQueryStatement::Remove(rem)) = rm {
                assert_eq!(rem.items.len(), 1);
                if let RemoveItem::Label {
                    variable, label, ..
                } = &rem.items[0]
                {
                    assert_eq!(variable, "n");
                    assert_eq!(label, "Person");
                } else {
                    panic!("expected RemoveItem::Label");
                }
            } else {
                panic!("expected SimpleQueryStatement::Remove in parts");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }
}

// ── removeItem ──────────────────────────────────────────────────────────
//   : removePropertyItem | removeLabelItem
//   ;
mod remove_item {
    use super::*;

    /// removePropertyItem: v.prop
    #[test]
    fn property_item() {
        let prog = p("MATCH (n) REMOVE n.name RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let rm = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Remove(_)));
            if let Some(SimpleQueryStatement::Remove(rem)) = rm {
                assert!(
                    matches!(&rem.items[0], RemoveItem::Property { variable, property, .. } if variable == "n" && property == "name")
                );
            } else {
                panic!("expected Remove");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// removeLabelItem: v IS Label
    #[test]
    fn label_item() {
        let prog = p("MATCH (n) REMOVE n IS Employee RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let rm = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Remove(_)));
            if let Some(SimpleQueryStatement::Remove(rem)) = rm {
                assert!(
                    matches!(&rem.items[0], RemoveItem::Label { variable, label, .. } if variable == "n" && label == "Employee")
                );
            } else {
                panic!("expected Remove");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── deleteStatement ─────────────────────────────────────────────────────
//   : (DETACH | NODETACH)? DELETE deleteItemList
//   ;
mod delete_statement {
    use super::*;

    /// DELETE n (unspecified detach)
    #[test]
    fn delete_unspecified() {
        let prog = p("MATCH (n) DELETE n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let del = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Delete(_)));
            if let Some(SimpleQueryStatement::Delete(d)) = del {
                assert_eq!(d.detach, DeleteDetach::Unspecified);
                assert_eq!(d.items.len(), 1);
                assert_eq!(d.items[0], Expr::new(ExprKind::Variable("n".into())));
            } else {
                panic!("expected SimpleQueryStatement::Delete in parts");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// DETACH DELETE n
    #[test]
    fn detach_delete() {
        let prog = p("MATCH (n) DETACH DELETE n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let del = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Delete(_)));
            if let Some(SimpleQueryStatement::Delete(d)) = del {
                assert_eq!(d.detach, DeleteDetach::Detach);
                assert_eq!(d.items.len(), 1);
            } else {
                panic!("expected Delete");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// NODETACH DELETE n
    #[test]
    fn nodetach_delete() {
        let prog = p("MATCH (n) NODETACH DELETE n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let del = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Delete(_)));
            if let Some(SimpleQueryStatement::Delete(d)) = del {
                assert_eq!(d.detach, DeleteDetach::NoDetach);
                assert_eq!(d.items.len(), 1);
            } else {
                panic!("expected Delete");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// DELETE multiple items: n, m
    #[test]
    fn delete_multiple_items() {
        let prog = p("MATCH (n)-[e]->(m) DELETE n, m");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let del = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Delete(_)));
            if let Some(SimpleQueryStatement::Delete(d)) = del {
                assert_eq!(d.detach, DeleteDetach::Unspecified);
                assert_eq!(d.items.len(), 2);
                assert_eq!(d.items[0], Expr::new(ExprKind::Variable("n".into())));
                assert_eq!(d.items[1], Expr::new(ExprKind::Variable("m".into())));
            } else {
                panic!("expected Delete");
            }
        } else {
            panic!("expected Query");
        }
    }
}
