use std::fs;
use std::path::{Path, PathBuf};

use candid::{Principal, decode_one, encode_args};
use gleaph_types::{
    AccessLevel, AclEntry, GleaphError, MutationResult, QueryResult, QueryResultWithContinuation,
    Value,
};
use pocket_ic::PocketIc;

fn wasm_path(crate_stem: &str) -> PathBuf {
    let release = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(format!("{crate_stem}.wasm"));
    if release.exists() {
        return release;
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("debug")
        .join(format!("{crate_stem}.wasm"))
}

fn load_wasm(crate_stem: &str) -> Vec<u8> {
    fs::read(wasm_path(crate_stem)).unwrap_or_else(|e| {
        panic!(
            "failed to read wasm for {crate_stem}; build it first with `cargo build -p {} --target wasm32-unknown-unknown --release` (preferred) or `cargo build -p {} --target wasm32-unknown-unknown`: {e}",
            crate_stem.replace('_', "-"),
            crate_stem.replace('_', "-")
        )
    })
}

struct GqlHarness {
    pic: PocketIc,
    graph_id: Principal,
    sender: Principal,
}

impl GqlHarness {
    fn new() -> Self {
        let pic = PocketIc::new();
        let sender = Principal::anonymous();
        let graph_id = pic.create_canister();
        pic.add_cycles(graph_id, 2_000_000_000_000);
        pic.install_canister(
            graph_id,
            load_wasm("gleaph_graph"),
            encode_args((Some(64u32), Some(0u64))).expect("graph init arg"),
            Some(sender),
        );
        Self {
            pic,
            graph_id,
            sender,
        }
    }

    fn query(&self, gql: &str) -> Result<QueryResult, GleaphError> {
        self.query_as(self.sender, gql)
    }

    fn query_as(&self, sender: Principal, gql: &str) -> Result<QueryResult, GleaphError> {
        let bytes = self
            .pic
            .query_call(
                self.graph_id,
                sender,
                "query",
                encode_args((gql.to_string(),)).expect("encode query"),
            )
            .unwrap_or_else(|e| panic!("graph query failed: {e:?}"));
        let result: Result<QueryResultWithContinuation, GleaphError> =
            decode_one(&bytes).expect("decode query response");
        result.map(|r| r.result)
    }

    fn mutate(&self, gql: &str) -> Result<MutationResult, GleaphError> {
        self.mutate_as(self.sender, gql)
    }

    fn mutate_as(&self, sender: Principal, gql: &str) -> Result<MutationResult, GleaphError> {
        let bytes = self
            .pic
            .update_call(
                self.graph_id,
                sender,
                "mutate",
                encode_args((gql.to_string(),)).expect("encode mutate"),
            )
            .unwrap_or_else(|e| panic!("graph mutate failed: {e:?}"));
        decode_one(&bytes).expect("decode mutate response")
    }

    fn batch_mutate(&self, gqls: Vec<&str>) -> Vec<Result<MutationResult, GleaphError>> {
        let gqls = gqls.into_iter().map(str::to_string).collect::<Vec<_>>();
        let bytes = self
            .pic
            .update_call(
                self.graph_id,
                self.sender,
                "batch_mutate",
                encode_args((gqls,)).expect("encode batch_mutate"),
            )
            .unwrap_or_else(|e| panic!("graph batch_mutate failed: {e:?}"));
        decode_one(&bytes).expect("decode batch_mutate response")
    }

    fn upgrade(&self) {
        self.pic
            .upgrade_canister(
                self.graph_id,
                load_wasm("gleaph_graph"),
                encode_args((Option::<u32>::None, Option::<u64>::None)).expect("upgrade arg"),
                Some(self.sender),
            )
            .expect("upgrade graph canister");
    }

    fn set_acl(&self, principal: Principal, level: AccessLevel) -> Result<(), GleaphError> {
        let bytes = self
            .pic
            .update_call(
                self.graph_id,
                self.sender,
                "set_acl_entry",
                encode_args((principal, level)).expect("encode set_acl_entry"),
            )
            .unwrap_or_else(|e| panic!("set_acl_entry failed: {e:?}"));
        decode_one(&bytes).expect("decode set_acl_entry response")
    }

    fn list_acl(&self) -> Result<Vec<AclEntry>, GleaphError> {
        let bytes = self
            .pic
            .query_call(
                self.graph_id,
                self.sender,
                "list_acl_entries",
                encode_args(()).expect("encode list_acl_entries"),
            )
            .unwrap_or_else(|e| panic!("list_acl_entries failed: {e:?}"));
        decode_one(&bytes).expect("decode list_acl_entries response")
    }
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn deploy_graph_canister_for_gql() {
    let _ = GqlHarness::new();
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_query_mutate_round_trip_via_candid() {
    let h = GqlHarness::new();

    let m = h
        .mutate(r#"INSERT (:User {name: 'A'})-[:KNOWS]->(:User {name: 'B'})"#)
        .expect("mutation ok");
    assert_eq!(m.affected_edges, 1);

    let q = h
        .query("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name LIMIT 10")
        .expect("query ok");
    assert_eq!(q.columns, vec!["b.name"]);
    assert_eq!(q.rows.len(), 1);
    assert_eq!(q.rows[0], vec![Value::Text("B".into())]);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_bulk_seed_and_batch_mutate() {
    let h = GqlHarness::new();
    let results = h.batch_mutate(vec![
        r#"INSERT (:User {name: 'A'})"#,
        r#"INSERT (:User {name: 'B'})-[:KNOWS]->(:User {name: 'C'})"#,
    ]);
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.is_ok()));

    let q = h
        .query("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name")
        .expect("query ok");
    assert_eq!(q.rows, vec![vec![Value::Text("C".into())]]);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_batch_mutate_validation_failure_rejects_entire_batch() {
    let h = GqlHarness::new();
    let results = h.batch_mutate(vec![
        r#"INSERT (:User {name: 'A'})-[:KNOWS]->(:User {name: 'B'})"#,
        r#"MATCH (a)-[:X]->(b) RETURN a LIMIT 5000000000"#,
    ]);

    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|r| matches!(r, Err(GleaphError::ParseError(_)))),
        "expected all ParseError results, got: {results:?}"
    );

    // Validation is all-or-none: the first CREATE must not have executed.
    let q = h
        .query("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name")
        .expect("query ok");
    assert!(q.rows.is_empty());
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_batch_mutate_returns_per_item_execution_results_after_validation() {
    let h = GqlHarness::new();
    let results = h.batch_mutate(vec![
        r#"INSERT (:User {name: 'A'})-[:KNOWS]->(:User {name: 'B'})"#,
        r#"MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name"#,
    ]);

    assert_eq!(results.len(), 2);
    assert!(
        results[0].is_ok(),
        "unexpected first result: {:?}",
        results[0]
    );
    assert!(
        matches!(results[1], Err(GleaphError::ValidationError(_))),
        "unexpected second result: {:?}",
        results[1]
    );

    // The CREATE still commits because execution errors are reported per-item after validation.
    let q = h
        .query("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name")
        .expect("query ok");
    assert_eq!(q.rows, vec![vec![Value::Text("B".into())]]);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_upgrade_persistence_property_index() {
    let h = GqlHarness::new();
    let _ = h
        .mutate(r#"INSERT (:User {name: 'PersistA'})-[:KNOWS]->(:User {name: 'PersistB'})"#)
        .expect("seed before upgrade");

    let before = h
        .query(
            r#"MATCH (u:User)-[:KNOWS]->(v:User) WHERE v.name = 'PersistB' RETURN u.name, v.name"#,
        )
        .expect("query before upgrade");
    assert_eq!(
        before.rows,
        vec![vec![
            Value::Text("PersistA".into()),
            Value::Text("PersistB".into())
        ]]
    );

    h.upgrade();

    let after = h
        .query(
            r#"MATCH (u:User)-[:KNOWS]->(v:User) WHERE v.name = 'PersistB' RETURN u.name, v.name"#,
        )
        .expect("query after upgrade");
    assert_eq!(after.rows, before.rows);
}

// ── Wave D: Phase 3.5 GQL features ──────────────────────────────────────────

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_set_remove_mutations() {
    let h = GqlHarness::new();
    h.mutate(r#"INSERT (:Person {name: 'Alice', age: 25})"#)
        .expect("create");

    h.mutate(r#"MATCH (p:Person) WHERE p.name = 'Alice' SET p.age = 30"#)
        .expect("set");
    let q = h
        .query(r#"MATCH (p:Person) WHERE p.name = 'Alice' RETURN p.age"#)
        .expect("query after set");
    assert_eq!(q.rows, vec![vec![Value::Int64(30)]]);

    h.mutate(r#"MATCH (p:Person) WHERE p.name = 'Alice' REMOVE p.age"#)
        .expect("remove");
    let q2 = h
        .query(r#"MATCH (p:Person) WHERE p.name = 'Alice' RETURN p.age"#)
        .expect("query after remove");
    assert_eq!(q2.rows, vec![vec![Value::Null]]);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_optional_match() {
    let h = GqlHarness::new();
    h.mutate(r#"INSERT (:Person {name: 'A'})-[:KNOWS]->(:Person {name: 'B'})"#)
        .expect("seed");
    h.mutate(r#"INSERT (:Person {name: 'C'})"#)
        .expect("lonely node");

    let q = h
        .query(
            r#"MATCH (p:Person) OPTIONAL MATCH (p)-[:KNOWS]->(f:Person) RETURN p.name, f.name ORDER BY p.name"#,
        )
        .expect("optional match");
    assert_eq!(q.rows.len(), 3);
    // A->B, B->null, C->null
    assert_eq!(
        q.rows[0],
        vec![Value::Text("A".into()), Value::Text("B".into())]
    );
    assert_eq!(q.rows[1][1], Value::Null); // B has no outgoing KNOWS
    assert_eq!(q.rows[2][1], Value::Null); // C has no outgoing KNOWS
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_aggregation_group_by_having() {
    let h = GqlHarness::new();
    h.batch_mutate(vec![
        r#"INSERT (:Cat {name: 'Whiskers'})"#,
        r#"INSERT (:Cat {name: 'Mittens'})"#,
        r#"INSERT (:Dog {name: 'Rex'})"#,
    ]);

    let q = h
        .query("MATCH (n) RETURN labels(n)[0] AS label, count(*) AS cnt ORDER BY label")
        .expect("group by");
    assert_eq!(q.rows.len(), 2);
    assert_eq!(q.rows[0], vec![Value::Text("Cat".into()), Value::Int64(2)]);
    assert_eq!(q.rows[1], vec![Value::Text("Dog".into()), Value::Int64(1)]);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_shortest_path() {
    let h = GqlHarness::new();
    h.batch_mutate(vec![
        r#"INSERT (:Node {id: 1})-[:LINK]->(:Node {id: 2})"#,
        r#"MATCH (a:Node {id: 2}) INSERT (a)-[:LINK]->(:Node {id: 3})"#,
        r#"MATCH (a:Node {id: 1}), (c:Node {id: 3}) INSERT (a)-[:LINK]->(c)"#,
    ]);

    let q = h
        .query(
            r#"MATCH SHORTEST (a:Node {id: 1})-[*]->(b:Node {id: 3}) RETURN length(path) AS len"#,
        )
        .expect("shortest path");
    assert_eq!(q.rows.len(), 1);
    assert_eq!(q.rows[0], vec![Value::Int64(1)]); // direct edge, length 1
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn gql_union_except_intersect() {
    let h = GqlHarness::new();
    h.batch_mutate(vec![
        r#"INSERT (:Fruit {name: 'Apple'})"#,
        r#"INSERT (:Fruit {name: 'Banana'})"#,
        r#"INSERT (:Veggie {name: 'Carrot'})"#,
    ]);

    let q = h
        .query(
            r#"MATCH (f:Fruit) RETURN f.name AS item UNION MATCH (v:Veggie) RETURN v.name AS item"#,
        )
        .expect("union");
    assert_eq!(q.rows.len(), 3);
    let items: Vec<_> = q.rows.iter().map(|r| &r[0]).collect();
    assert!(items.contains(&&Value::Text("Apple".into())));
    assert!(items.contains(&&Value::Text("Banana".into())));
    assert!(items.contains(&&Value::Text("Carrot".into())));
}

// ── Wave D: ACL tests ────────────────────────────────────────────────────────

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn acl_denies_write_for_unpermitted_principal() {
    let h = GqlHarness::new();
    let stranger = Principal::from_slice(&[1, 2, 3, 4, 5]);

    // Stranger has no ACL entry → default Read-only.
    let err = h
        .mutate_as(stranger, r#"INSERT (:Node {val: 1})"#)
        .expect_err("mutation should be denied");
    assert!(
        matches!(err, GleaphError::ExecutionError(_)),
        "expected ExecutionError, got: {err:?}"
    );

    // Stranger CAN query (Read access).
    let q = h.query_as(stranger, "MATCH (n) RETURN count(*)");
    assert!(q.is_ok(), "stranger query should succeed: {q:?}");
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn acl_allows_controller_all_ops() {
    let h = GqlHarness::new();

    // Controller (anonymous in PocketIC) can mutate.
    h.mutate(r#"INSERT (:Node {val: 42})"#)
        .expect("controller mutation");

    // Controller can query.
    let q = h
        .query("MATCH (n:Node) RETURN n.val")
        .expect("controller query");
    assert_eq!(q.rows, vec![vec![Value::Int64(42)]]);

    // Controller can manage ACL.
    let writer = Principal::from_slice(&[10, 20, 30]);
    h.set_acl(writer, AccessLevel::Write).expect("set acl");
    let entries = h.list_acl().expect("list acl");
    assert!(
        entries
            .iter()
            .any(|e| e.principal == writer && e.level == AccessLevel::Write)
    );

    // Writer can now mutate.
    h.mutate_as(writer, r#"INSERT (:Node {val: 99})"#)
        .expect("writer mutation");
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn acl_survives_upgrade() {
    let h = GqlHarness::new();

    let writer = Principal::from_slice(&[7, 8, 9]);
    h.set_acl(writer, AccessLevel::Write).expect("set acl");

    h.upgrade();

    // After upgrade, the ACL entry should still be present.
    let entries = h.list_acl().expect("list acl after upgrade");
    assert!(
        entries
            .iter()
            .any(|e| e.principal == writer && e.level == AccessLevel::Write),
        "ACL entry missing after upgrade: {entries:?}"
    );

    // Writer can still mutate.
    h.mutate_as(writer, r#"INSERT (:Node {val: 1})"#)
        .expect("writer mutation after upgrade");
}
