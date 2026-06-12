//! §16 — Node property maps in patterns.

use super::helpers::graph_pat;
use crate::section_tests::p;

// ── Property map in node pattern ────────────────────────────────────────

#[test]
fn node_with_properties() {
    let prog = p("MATCH (n {name: 'Alice', age: 30}) RETURN n");
    let gp = graph_pat(&prog);
    // Should parse with a single path containing a node with 2 properties
    assert!(!gp.paths.is_empty());
}
