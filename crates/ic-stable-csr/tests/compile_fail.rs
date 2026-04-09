#[test]
fn row_tombstone_graph_has_no_logical_iter_api() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/row_tombstone_logical_iter.rs");
}
