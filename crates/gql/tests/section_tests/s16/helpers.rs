//! §16 — Shared graph-pattern test helpers.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

/// Extract the graph pattern from the first match statement.
pub(crate) fn graph_pat(prog: &GqlProgram) -> &GraphPattern {
    let b = body(prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part {
                    return &m.pattern;
                }
            }
            panic!("no Match found");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

/// Extract the first edge direction from a match pattern.
pub(crate) fn first_edge_dir(input: &str) -> EdgeDirection {
    let prog = p(input);
    let gp = graph_pat(&prog);
    let path = &gp.paths[0];
    match &path.expr {
        PathPatternExpr::Term(term) => {
            for f in &term.factors {
                if let PathPrimary::Edge(e) = &f.primary {
                    return e.direction;
                }
            }
            panic!("no edge found");
        }
        _ => panic!("expected Term"),
    }
}
