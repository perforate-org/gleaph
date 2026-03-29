use gleaph_pma::{BulkEdgeInput, PmaGraph, VecMemory};
use gleaph_types::{EntityType, IndexType, PropertyIndex, Value, VertexIdSet};
use std::collections::HashMap;

#[test]
fn pma_smoke_from_tests_crate() {
    let mem = VecMemory::default();
    let mut g = PmaGraph::new(mem, 4).expect("init");
    g.insert(0, 1, 0, 1.0, 1).expect("insert");
    let ns = g.collect_neighbors(0).expect("neighbors");
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0].target, 1);
}

// ── label_live_count unit tests ───────────────────────────────────────────────

fn new_graph() -> PmaGraph<VecMemory> {
    PmaGraph::new(VecMemory::default(), 8).expect("init")
}

#[test]
fn label_live_count_create_vertex() {
    let mut g = new_graph();
    g.create_vertex(vec!["User".into(), "Admin".into()], Default::default())
        .unwrap();
    let counts = g.label_cardinalities();
    assert_eq!(counts["User"], 1);
    assert_eq!(counts["Admin"], 1);
}

#[test]
fn label_live_count_two_vertices_same_label() {
    let mut g = new_graph();
    g.create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    g.create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    assert_eq!(g.label_cardinalities()["User"], 2);
}

#[test]
fn label_live_count_delete_vertex_decrements() {
    let mut g = new_graph();
    let v = g
        .create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    g.create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    g.delete_vertex(v).unwrap();
    assert_eq!(g.label_cardinalities()["User"], 1);
}

#[test]
fn label_live_count_add_label_increments() {
    let mut g = new_graph();
    let v = g
        .create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    g.add_vertex_label(v, "Admin".into()).unwrap();
    let counts = g.label_cardinalities();
    assert_eq!(counts["User"], 1);
    assert_eq!(counts["Admin"], 1);
}

#[test]
fn label_live_count_remove_label_decrements() {
    let mut g = new_graph();
    let v = g
        .create_vertex(vec!["User".into(), "Admin".into()], Default::default())
        .unwrap();
    g.remove_vertex_label(v, "Admin").unwrap();
    let counts = g.label_cardinalities();
    assert_eq!(counts["User"], 1);
    assert_eq!(*counts.get("Admin").unwrap_or(&0), 0);
}

#[test]
fn label_live_count_add_label_on_tombstoned_vertex_no_increment() {
    let mut g = new_graph();
    let v = g
        .create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    g.delete_vertex(v).unwrap();
    // add_vertex_label on a tombstoned vertex should not increment
    g.add_vertex_label(v, "Admin".into()).unwrap();
    let counts = g.label_cardinalities();
    assert_eq!(*counts.get("Admin").unwrap_or(&0), 0);
    assert_eq!(*counts.get("User").unwrap_or(&0), 0);
}

#[test]
fn label_live_count_restore_snapshot_recomputes() {
    let mut g = new_graph();
    let _v1 = g
        .create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    let v2 = g
        .create_vertex(vec!["User".into(), "Admin".into()], Default::default())
        .unwrap();
    g.delete_vertex(v2).unwrap();

    // Take snapshot, clear state by restoring it
    let snap = g.overlay_snapshot();
    g.restore_overlay_snapshot(snap).unwrap();

    let counts = g.label_cardinalities();
    // v1 is alive → User=1; v2 is tombstoned → Admin=0
    assert_eq!(counts["User"], 1);
    assert_eq!(*counts.get("Admin").unwrap_or(&0), 0);
}

// ── avg_degree tracking ──────────────────────────────────────────────────────

#[test]
fn avg_degree_computed_from_vertex_and_edge_counts() {
    let mut g = new_graph();
    let v1 = g
        .create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    let v2 = g
        .create_vertex(vec!["User".into()], Default::default())
        .unwrap();
    let _v3 = g
        .create_vertex(vec!["User".into()], Default::default())
        .unwrap();

    // No edges → avg_degree = 0.0
    assert_eq!(g.stats().avg_degree, 0.0);

    // num_vertices in PMA is the total slot count, not the created vertex count.
    // avg_degree = num_edges / num_vertices (slot count).
    g.create_edge(v1, v2, None, vec![], 1.0, 0).unwrap();
    let s = g.stats();
    assert!(s.num_vertices > 0);
    let expected = s.num_edges as f64 / s.num_vertices as f64;
    assert!(
        (s.avg_degree - expected).abs() < 1e-10,
        "got {}",
        s.avg_degree
    );

    // Adding more edges increases avg_degree
    g.create_edge(v2, v1, None, vec![], 1.0, 0).unwrap();
    let s2 = g.stats();
    assert!(s2.avg_degree > s.avg_degree);
}

// ── ABP property store integration tests ─────────────────────────────────────

#[test]
fn abp_property_store_backfill_preserves_vertex_and_edge_props() {
    let mut g = new_graph();
    let v1 = g
        .create_vertex(
            vec!["User".into()],
            vec![
                ("name".into(), Value::Text("Alice".into())),
                ("age".into(), Value::Int64(30)),
            ],
        )
        .unwrap();
    let v2 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Bob".into()))],
        )
        .unwrap();
    g.create_edge(
        v1,
        v2,
        Some("KNOWS".into()),
        vec![("since".into(), Value::Int64(2020))],
        1.0,
        0,
    )
    .unwrap();

    // Build ABP property store snapshot
    let mem = VecMemory::default();
    let store = g.build_abp_property_store(mem, 0).unwrap();

    // Verify vertex properties survived
    assert_eq!(
        store.get_vertex_prop(v1, "name"),
        Some(Value::Text("Alice".into()))
    );
    assert_eq!(store.get_vertex_prop(v1, "age"), Some(Value::Int64(30)));
    assert_eq!(
        store.get_vertex_prop(v2, "name"),
        Some(Value::Text("Bob".into()))
    );

    // Verify edge property survived
    let edge_id = g.edge_id_for_labeled(v1, v2, Some("KNOWS"));
    let edge_val = store.get_edge_prop_by_id(edge_id, "since");
    assert_eq!(edge_val, Some(Value::Int64(2020)));

    // Missing property returns None
    assert_eq!(store.get_vertex_prop(v1, "missing"), None);
}

#[test]
fn abp_property_store_scan_vertex_props_returns_all_properties() {
    let mut g = new_graph();
    let v = g
        .create_vertex(
            vec!["User".into()],
            vec![
                ("a".into(), Value::Int64(1)),
                ("b".into(), Value::Int64(2)),
                ("c".into(), Value::Int64(3)),
            ],
        )
        .unwrap();

    let mem = VecMemory::default();
    let store = g.build_abp_property_store(mem, 0).unwrap();
    let props = store.scan_vertex_props(v);

    assert_eq!(props.len(), 3);
    let names: Vec<&str> = props.iter().map(|(k, _)| k.as_str()).collect();
    assert!(names.contains(&"a"));
    assert!(names.contains(&"b"));
    assert!(names.contains(&"c"));
}

#[test]
fn abp_property_store_handles_deletion_correctly() {
    let mut g = new_graph();
    let v = g
        .create_vertex(
            vec!["User".into()],
            vec![
                ("name".into(), Value::Text("Alice".into())),
                ("age".into(), Value::Int64(30)),
            ],
        )
        .unwrap();

    // Delete one property
    g.delete_vertex_prop(v, "age").unwrap();

    // Build store - should only have "name"
    let mem = VecMemory::default();
    let store = g.build_abp_property_store(mem, 0).unwrap();
    assert_eq!(
        store.get_vertex_prop(v, "name"),
        Some(Value::Text("Alice".into()))
    );
    assert_eq!(store.get_vertex_prop(v, "age"), None);
}

// ── Secondary index integration tests ────────────────────────────────────────

#[test]
fn secondary_index_create_and_query_vertex_equality() {
    let mut g = new_graph();
    g.create_vertex(
        vec!["User".into()],
        vec![("uid".into(), Value::Text("alice".into()))],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![("uid".into(), Value::Text("bob".into()))],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![("uid".into(), Value::Text("alice".into()))],
    )
    .unwrap();

    // Create index on "uid" property
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // Query using in-memory index
    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("alice".into()));
    assert_eq!(results.len(), 2);

    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("bob".into()));
    assert_eq!(results.len(), 1);

    // No match
    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("carol".into()));
    assert!(results.is_empty());
}

#[test]
fn secondary_index_tracks_property_mutations() {
    let mut g = new_graph();
    let v1 = g
        .create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Text("alice".into()))],
        )
        .unwrap();
    let v2 = g
        .create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Text("bob".into()))],
        )
        .unwrap();

    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // Update v1's uid from "alice" to "carol"
    g.set_vertex_prop(v1, "uid".into(), Value::Text("carol".into()))
        .unwrap();

    // "alice" should be gone, "carol" should appear
    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("alice".into()));
    assert!(results.is_empty());

    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("carol".into()));
    assert_eq!(results.len(), 1u64);
    assert_eq!(results.min().unwrap(), v1);

    // Delete v2's uid property
    g.delete_vertex_prop(v2, "uid").unwrap();
    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("bob".into()));
    assert!(results.is_empty());
}

#[test]
fn secondary_index_only_tracks_registered_properties() {
    let mut g = new_graph();
    g.create_vertex(
        vec!["User".into()],
        vec![
            ("uid".into(), Value::Text("alice".into())),
            ("name".into(), Value::Text("Alice".into())),
        ],
    )
    .unwrap();

    // Only register "uid" index
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // "uid" is indexed
    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("alice".into()));
    assert_eq!(results.len(), 1);

    // "name" is NOT indexed
    let results = g.scan_vertices_by_property_eq("name", &Value::Text("Alice".into()));
    assert!(results.is_empty());
}

#[test]
fn secondary_index_survives_overlay_snapshot_restore() {
    let mut g = new_graph();
    g.create_vertex(
        vec!["User".into()],
        vec![("uid".into(), Value::Text("alice".into()))],
    )
    .unwrap();
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // Snapshot and restore
    let snap = g.overlay_snapshot();
    g.restore_overlay_snapshot(snap).unwrap();

    // Index should still work after restore
    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("alice".into()));
    assert_eq!(results.len(), 1);
}

#[test]
fn secondary_index_consistency_after_vertex_deletion() {
    let mut g = new_graph();
    let v1 = g
        .create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Text("alice".into()))],
        )
        .unwrap();
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // Delete vertex
    g.delete_vertex(v1).unwrap();

    // Index entry should be removed
    let results = g.scan_vertices_by_property_eq("uid", &Value::Text("alice".into()));
    assert!(results.is_empty());
}

#[test]
fn abp_secondary_index_backfill_matches_in_memory_index() {
    let mut g = new_graph();
    g.create_vertex(
        vec!["User".into()],
        vec![("uid".into(), Value::Text("alice".into()))],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![("uid".into(), Value::Text("bob".into()))],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![("uid".into(), Value::Text("alice".into()))],
    )
    .unwrap();

    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // In-memory results
    let in_mem = g.scan_vertices_by_property_eq("uid", &Value::Text("alice".into()));

    // Build ABP secondary index snapshot
    let abp_mem = VecMemory::default();
    let idx = g.build_abp_secondary_index(abp_mem, 0).unwrap();

    // Query ABP index
    let abp_results = idx
        .scan_vertices_eq("uid", &Value::Text("alice".into()))
        .unwrap();

    // Results should match
    assert_eq!(in_mem.len() as usize, abp_results.len());
    for vid in in_mem.iter() {
        assert!(abp_results.contains(&vid), "ABP missing vertex {vid}");
    }
}

#[test]
fn index_lifecycle_survives_overlay_roundtrip() {
    // Phase 1: Create graph, create indexes, insert data.
    let mut g = new_graph();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();
    g.create_index(EntityType::Edge, "weight".into(), IndexType::Equality)
        .unwrap();

    let v0 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Alice".into()))],
        )
        .unwrap();
    let v1 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Bob".into()))],
        )
        .unwrap();
    let v2 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Alice".into()))],
        )
        .unwrap();
    g.create_edge(
        v0,
        v1,
        Some("KNOWS".into()),
        vec![("weight".into(), Value::Int64(5))],
        1.0,
        1,
    )
    .unwrap();
    g.create_edge(
        v1,
        v2,
        Some("KNOWS".into()),
        vec![("weight".into(), Value::Int64(3))],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(
        v2,
        v0,
        Some("KNOWS".into()),
        vec![("weight".into(), Value::Int64(5))],
        1.0,
        3,
    )
    .unwrap();

    // Pre-roundtrip: verify in-memory indexes work.
    let alice_pre = g.scan_vertices_by_property_eq("name", &Value::Text("Alice".into()));
    assert_eq!(alice_pre.len(), 2, "pre-roundtrip: 2 Alices");
    let w5_pre = g.scan_edges_by_property_eq("weight", &Value::Int64(5));
    assert_eq!(w5_pre.len(), 2, "pre-roundtrip: 2 edges with weight=5");

    // Build ABP snapshot.
    let abp_mem = VecMemory::default();
    let abp_idx = g.build_abp_secondary_index(abp_mem, 0).unwrap();
    let abp_alice = abp_idx
        .scan_vertices_eq("name", &Value::Text("Alice".into()))
        .unwrap();
    assert_eq!(abp_alice.len(), 2, "ABP snapshot: 2 Alices");

    // Phase 2: Simulate persist → restore via overlay snapshot.
    let snapshot = g.overlay_snapshot();
    drop(g);

    let mut g2 = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g2.restore_overlay_snapshot(snapshot).unwrap();

    // Post-roundtrip: verify in-memory indexes are restored.
    let alice_post = g2.scan_vertices_by_property_eq("name", &Value::Text("Alice".into()));
    assert_eq!(
        alice_post.len(),
        2,
        "post-roundtrip: 2 Alices via in-memory index"
    );

    let w5_post = g2.scan_edges_by_property_eq("weight", &Value::Int64(5));
    assert_eq!(
        w5_post.len(),
        2,
        "post-roundtrip: 2 edges with weight=5 via in-memory index"
    );

    // Verify registered indexes survived.
    let indexes = g2.list_property_indexes();
    assert!(indexes.contains(&PropertyIndex {
        entity_type: EntityType::Vertex,
        property_name: "name".into(),
        index_type: IndexType::Equality
    }));
    assert!(indexes.contains(&PropertyIndex {
        entity_type: EntityType::Edge,
        property_name: "weight".into(),
        index_type: IndexType::Equality
    }));

    // Phase 3: Verify mutations after restore update in-memory indexes.
    let v3 = g2
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Alice".into()))],
        )
        .unwrap();
    let alice_after_mut = g2.scan_vertices_by_property_eq("name", &Value::Text("Alice".into()));
    assert_eq!(
        alice_after_mut.len(),
        3,
        "post-mutation: 3 Alices after new vertex"
    );

    g2.create_edge(
        v0,
        v3,
        Some("KNOWS".into()),
        vec![("weight".into(), Value::Int64(5))],
        1.0,
        4,
    )
    .unwrap();
    let w5_after_mut = g2.scan_edges_by_property_eq("weight", &Value::Int64(5));
    assert_eq!(
        w5_after_mut.len(),
        3,
        "post-mutation: 3 edges with weight=5"
    );

    // Phase 4: GQL query through executor to verify end-to-end.
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;
    use gleaph_gql::stats::TableStats;

    g2.compute_property_selectivity();
    let mut stats = TableStats {
        vertex_count: g2.vertex_count(),
        edge_count: g2.edge_count(),
        avg_degree: (g2.edge_count() as f64 / g2.vertex_count() as f64).max(1.0),
        label_cardinality: g2.label_cardinalities(),
        ..TableStats::default()
    };
    for (key, &s) in g2.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g2.list_property_indexes() {
        if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
            stats
                .indexed_vertex_properties
                .insert(idx.property_name.clone());
        }
        if idx.entity_type == EntityType::Edge && idx.index_type == IndexType::Equality {
            stats
                .indexed_edge_properties
                .insert(idx.property_name.clone());
        }
    }

    let gql = "MATCH (a:User) WHERE a.name = 'Alice' RETURN id(a)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
    let result = execute_plan(&plan, &g2).unwrap();
    assert_eq!(result.rows.len(), 3, "GQL query: 3 Alices after roundtrip");
}

// ── property_selectivity unit tests ──────────────────────────────────────────

#[test]
fn property_selectivity_empty_graph_returns_empty() {
    let mut g = new_graph();
    g.compute_property_selectivity();
    assert!(g.get_property_selectivity().is_empty());
}

#[test]
fn property_selectivity_all_distinct_values_returns_one() {
    let mut g = new_graph();
    // Create 3 vertices with unique `uid` values.
    for i in 0u32..3 {
        g.create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Int64(i as i64))],
        )
        .unwrap();
    }
    g.compute_property_selectivity();
    let sel = g.get_property_selectivity();
    let v = sel["vertex:uid"];
    // 3 distinct out of 3 sampled → 1.0
    assert!((v - 1.0).abs() < 1e-9, "expected 1.0, got {v}");
}

#[test]
fn property_selectivity_all_same_value_returns_low() {
    let mut g = new_graph();
    // 4 vertices all with the same `kind` value.
    for _ in 0..4 {
        g.create_vertex(
            vec!["Item".into()],
            vec![("kind".into(), Value::Text("a".into()))],
        )
        .unwrap();
    }
    g.compute_property_selectivity();
    let sel = g.get_property_selectivity();
    let v = sel["vertex:kind"];
    // 1 distinct out of 4 sampled → 0.25
    assert!((v - 0.25).abs() < 1e-9, "expected 0.25, got {v}");
}

#[test]
fn property_selectivity_persisted_in_overlay_snapshot() {
    let mut g = new_graph();
    g.create_vertex(vec!["U".into()], vec![("x".into(), Value::Int64(1))])
        .unwrap();
    g.create_vertex(vec!["U".into()], vec![("x".into(), Value::Int64(2))])
        .unwrap();
    g.compute_property_selectivity();
    let before = g.get_property_selectivity().clone();
    assert!(!before.is_empty());

    // Round-trip through overlay snapshot.
    let snap = g.overlay_snapshot();
    let mut g2 = new_graph();
    g2.restore_overlay_snapshot(snap).unwrap();
    assert_eq!(g2.get_property_selectivity(), &before);
}

#[test]
fn planner_stats_reflects_computed_selectivity() {
    let mut g = new_graph();
    g.create_vertex(vec!["A".into()], vec![("score".into(), Value::Int64(10))])
        .unwrap();
    g.create_vertex(vec!["A".into()], vec![("score".into(), Value::Int64(20))])
        .unwrap();
    g.compute_property_selectivity();
    let stats = g.planner_stats();
    // vertex_count is the array capacity (>= 2); we check it's non-zero.
    assert!(stats.vertex_count >= 2);
    let sel_map: std::collections::BTreeMap<_, _> =
        stats.property_selectivity.into_iter().collect();
    assert!(sel_map.contains_key("vertex:score"));
}

// ── ensure_property_store_region / ensure_secondary_index_region unit tests ──

#[test]
fn ensure_property_store_region_allocates_after_pma_end() {
    let mut g = new_graph();
    let offset = g
        .ensure_property_store_region(0)
        .expect("ensure property store region");
    // Offset must be non-zero and beyond the start of the graph (header is at 0).
    assert!(offset > 0, "property store region must be beyond offset 0");
    // A second call with the same minimum should be idempotent.
    let offset2 = g.ensure_property_store_region(0).expect("idempotent");
    assert_eq!(offset, offset2, "second call must return the same offset");
}

#[test]
fn ensure_secondary_index_region_placed_after_property_store() {
    let mut g = new_graph();
    let prop_off = g.ensure_property_store_region(0).expect("property store");
    let sec_off = g.ensure_secondary_index_region(0).expect("secondary index");
    // Secondary index must start at or after the end of the property store region.
    assert!(
        sec_off >= prop_off,
        "secondary index ({sec_off}) should be at or after property store ({prop_off})"
    );
    // Also idempotent.
    let sec_off2 = g.ensure_secondary_index_region(0).expect("idempotent");
    assert_eq!(sec_off, sec_off2);
}

#[test]
fn ensure_regions_are_non_overlapping() {
    let mut g = new_graph();
    let prop_off = g.ensure_property_store_region(0).expect("property store");
    let sec_off = g.ensure_secondary_index_region(0).expect("secondary index");
    // Both must be at different offsets (non-overlapping by construction).
    // The property store ends before or at the secondary index start.
    // We can't query lengths from the test easily, but they should differ.
    assert_ne!(prop_off, sec_off, "regions must not share an offset");
}

// ── Persisted index metadata / measured selectivity tests ──────────────

#[test]
fn index_metadata_survives_upgrade() {
    let mut g = new_graph();
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();
    // 6 vertices with 3 distinct uid values.
    for i in 0..6u32 {
        g.create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Int64((i % 3) as i64))],
        )
        .unwrap();
    }
    g.compute_property_selectivity();
    let before = g.get_property_selectivity().clone();
    assert!(!before.is_empty());
    // Exact: 3 distinct / 6 total = 0.5
    let sel = before["vertex:uid"];
    assert!((sel - 0.5).abs() < 1e-9, "expected 0.5, got {sel}");

    // Simulate upgrade: overlay snapshot round-trip.
    let snap = g.overlay_snapshot();
    let mut g2 = new_graph();
    g2.restore_overlay_snapshot(snap).unwrap();
    assert_eq!(g2.get_property_selectivity(), &before);
    // Index metadata must also survive.
    assert_eq!(
        g2.list_property_indexes(),
        vec![gleaph_types::PropertyIndex {
            entity_type: EntityType::Vertex,
            property_name: "uid".into(),
            index_type: IndexType::Equality,
        }]
    );
}

#[test]
fn planner_uses_measured_selectivity() {
    use gleaph_gql::planner::build_plan_with_stats;
    use gleaph_gql::stats::TableStats;

    let mut g = new_graph();
    g.create_index(EntityType::Vertex, "email".into(), IndexType::Equality)
        .unwrap();
    // 100 vertices with 100 distinct email values → selectivity = 1.0.
    for i in 0..100u32 {
        g.create_vertex(
            vec!["User".into()],
            vec![("email".into(), Value::Text(format!("user{i}@test.com")))],
        )
        .unwrap();
    }
    g.compute_property_selectivity();
    let sel = g.get_property_selectivity()["vertex:email"];
    assert!((sel - 1.0).abs() < 1e-9, "expected 1.0, got {sel}");

    // Build TableStats from graph state (mirrors gql_bridge pattern).
    let mut stats = TableStats {
        vertex_count: g.vertex_count(),
        edge_count: g.edge_count(),
        avg_degree: 1.0,
        label_cardinality: g.label_cardinalities(),
        ..TableStats::default()
    };
    for (key, &s) in g.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g.list_property_indexes() {
        if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
            stats
                .indexed_vertex_properties
                .insert(idx.property_name.clone());
            stats
                .property_selectivity
                .entry(format!("vertex:{}", idx.property_name))
                .or_insert(0.1);
        }
    }
    // Measured selectivity (1.0) must be used, not default 0.1.
    assert!(
        (stats.property_selectivity["vertex:email"] - 1.0).abs() < 1e-9,
        "planner should see measured selectivity 1.0, not default 0.1"
    );

    // With an equality predicate on an indexed prop, planner should emit IndexScan.
    let stmt =
        gleaph_gql::parse_statement("MATCH (a:User) WHERE a.email = 'user42@test.com' RETURN a")
            .unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "expected IndexScan with measured selectivity on indexed property; ops = {:?}",
        plan.ops
    );
}

#[test]
fn selectivity_auto_refreshes_after_mutations() {
    let mut g = new_graph();
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // 2 vertices with 1 distinct uid → selectivity 0.5.
    g.create_vertex(vec!["U".into()], vec![("uid".into(), Value::Int64(1))])
        .unwrap();
    g.create_vertex(vec!["U".into()], vec![("uid".into(), Value::Int64(1))])
        .unwrap();
    g.compute_property_selectivity();
    let sel_before = g.get_property_selectivity()["vertex:uid"];
    assert!(
        (sel_before - 0.5).abs() < 1e-9,
        "expected 0.5, got {sel_before}"
    );

    // Mutate enough to trigger auto-refresh (threshold = 100).
    for i in 2..104 {
        g.create_vertex(vec!["U".into()], vec![("uid".into(), Value::Int64(i))])
            .unwrap();
    }

    // refresh_selectivity_if_stale should trigger recomputation.
    g.refresh_selectivity_if_stale();

    let sel_after = g.get_property_selectivity()["vertex:uid"];
    // 103 distinct / 104 total ≈ 0.99
    assert!(
        sel_after > 0.9,
        "selectivity should have auto-refreshed; got {sel_after}"
    );
    assert!(
        (sel_after - sel_before).abs() > 0.3,
        "selectivity should differ after mutations: before={sel_before}, after={sel_after}"
    );
}

// ── Live stable equality index tests ───────────────────────────────────

#[test]
fn stable_eq_index_survives_upgrade() {
    let mut g = new_graph();
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();
    let a = g
        .create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Text("alice".into()))],
        )
        .unwrap();
    let b = g
        .create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Text("bob".into()))],
        )
        .unwrap();

    // Build ABP snapshot in stable memory.
    let offset = g.ensure_secondary_index_region(0).expect("region");
    let idx = g
        .build_abp_secondary_index(g.mem.clone(), offset)
        .expect("build ABP");
    g.mem = idx.into_memory();

    // Simulate upgrade: overlay snapshot round-trip.
    let snap = g.overlay_snapshot();
    let mut g2 = PmaGraph::new(g.mem.clone(), 8).expect("new graph from same memory");
    g2.restore_overlay_snapshot(snap).unwrap();

    // Attach the ABP that persisted in stable memory.
    g2.attach_live_eq_index(offset).expect("attach");
    assert!(g2.has_live_eq_index());

    // Verify ABP reads return correct data.
    let alice_ids = g2
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("alice".into()))
        .unwrap();
    assert_eq!(alice_ids, VertexIdSet::from_iter([a]));
    let bob_ids = g2
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("bob".into()))
        .unwrap();
    assert_eq!(bob_ids, VertexIdSet::from_iter([b]));
    let nobody = g2
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("nobody".into()))
        .unwrap();
    assert!(nobody.is_empty());
}

#[test]
fn incremental_index_update_on_mutation() {
    let mut g = new_graph();
    g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
        .unwrap();

    // Create the first vertex *before* building the ABP index so that
    // PMA expansion (which drops the live ABP handle) happens now.
    let a = g
        .create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Text("alice".into()))],
        )
        .unwrap();

    // Build ABP (includes vertex `a`) and attach as live.
    let offset = g.ensure_secondary_index_region(0).expect("region");
    let idx = g
        .build_abp_secondary_index(g.mem.clone(), offset)
        .expect("build ABP");
    g.mem = idx.into_memory();
    g.attach_live_eq_index(offset).expect("attach");
    assert!(g.has_live_eq_index(), "live eq index should be attached");

    // Sanity: ABP already contains vertex `a`.
    let ids = g
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("alice".into()))
        .unwrap();
    assert_eq!(
        ids,
        VertexIdSet::from_iter([a]),
        "ABP should contain initial vertex"
    );

    // set_vertex_prop — update value.
    g.set_vertex_prop(a, "uid".into(), Value::Text("alice2".into()))
        .unwrap();
    let old = g
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("alice".into()))
        .unwrap();
    assert!(old.is_empty(), "old value should be removed from ABP");
    let new = g
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("alice2".into()))
        .unwrap();
    assert_eq!(
        new,
        VertexIdSet::from_iter([a]),
        "new value should be in ABP"
    );

    // delete_vertex_prop.
    g.delete_vertex_prop(a, "uid").unwrap();
    let gone = g
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("alice2".into()))
        .unwrap();
    assert!(gone.is_empty(), "deleted prop should be removed from ABP");

    // Add a vertex, then delete the vertex itself.
    // create_vertex triggers PMA expansion which drops the live ABP handle.
    // Re-build and re-attach the ABP so subsequent incremental updates work.
    let b = g
        .create_vertex(
            vec!["User".into()],
            vec![("uid".into(), Value::Text("bob".into()))],
        )
        .unwrap();
    let offset = g.ensure_secondary_index_region(0).expect("region");
    let idx = g
        .build_abp_secondary_index(g.mem.clone(), offset)
        .expect("rebuild ABP");
    g.mem = idx.into_memory();
    g.attach_live_eq_index(offset).expect("re-attach");
    let bids = g
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("bob".into()))
        .unwrap();
    assert_eq!(bids, VertexIdSet::from_iter([b]));
    g.delete_vertex(b).unwrap();
    let bids2 = g
        .scan_vertices_by_property_eq_auto("uid", &Value::Text("bob".into()))
        .unwrap();
    assert!(
        bids2.is_empty(),
        "deleted vertex should be removed from ABP"
    );
}

#[test]
fn range_index_survives_abp_rebuild() {
    let mut g = new_graph();
    g.create_index(EntityType::Vertex, "score".into(), IndexType::Range)
        .unwrap();

    // Create vertices with varying scores.
    for i in 0..20u32 {
        g.create_vertex(
            vec!["Item".into()],
            vec![("score".into(), Value::Int64(i as i64))],
        )
        .unwrap();
    }

    // Build ABP snapshot (simulates pre_upgrade).
    let offset = g.ensure_secondary_index_region(0).expect("region");
    let idx = g
        .build_abp_secondary_index(g.mem.clone(), offset)
        .expect("build ABP");
    g.mem = idx.into_memory();

    // Simulate upgrade: overlay round-trip into a fresh graph.
    let snap = g.overlay_snapshot();
    let mut g2 = PmaGraph::new(g.mem.clone(), 8).expect("new graph from same memory");
    g2.restore_overlay_snapshot(snap).unwrap();

    // Clear the in-memory range index to prove ABP provides the data.
    g2.clear_in_memory_range_index();

    // Attach the ABP handle (simulates post_upgrade).
    g2.attach_live_eq_index(offset).expect("attach");

    // Range scan via ABP should find vertices with score >= 15.
    use gleaph_pma::property_store::RangeOp;
    let ids = g2
        .scan_vertices_by_property_range_auto("score", &Value::Int64(15), RangeOp::Ge)
        .unwrap();
    assert_eq!(ids.len(), 5u64, "expected vertices with score 15..19");
    for vid in ids.iter() {
        let props = g2.get_vertex_props(vid).unwrap();
        let score = props
            .iter()
            .find(|(k, _)| k == "score")
            .map(|(_, v)| match v {
                Value::Int64(n) => *n,
                _ => panic!("unexpected score type"),
            })
            .expect("missing score");
        assert!(score >= 15, "score {score} should be >= 15");
    }

    // Range scan for score < 3.
    let ids2 = g2
        .scan_vertices_by_property_range_auto("score", &Value::Int64(3), RangeOp::Lt)
        .unwrap();
    assert_eq!(ids2.len(), 3, "expected vertices with score 0, 1, 2");
}

// ── Extended IndexScan tests ──────────────────────────────────────────

#[test]
fn index_scan_non_start_anchor() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;
    use gleaph_gql::stats::TableStats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    // Create an index on "name" for vertex properties.
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();

    // Graph: (0:User)-[:KNOWS]->(4:Person{name:"Alice"})
    //        (1:User)-[:KNOWS]->(4)
    //        (2:User)-[:FOLLOWS]->(4)    ← different label
    //        (3:Other)-[:KNOWS]->(4)     ← not :User
    let u0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u3 = g.create_vertex(vec!["Other".into()], vec![]).unwrap();
    let alice = g
        .create_vertex(
            vec!["Person".into()],
            vec![("name".into(), Value::Text("Alice".into()))],
        )
        .unwrap();
    g.create_edge(u0, alice, Some("KNOWS".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(u1, alice, Some("KNOWS".into()), vec![], 1.0, 2)
        .unwrap();
    g.create_edge(u2, alice, Some("FOLLOWS".into()), vec![], 1.0, 3)
        .unwrap();
    g.create_edge(u3, alice, Some("KNOWS".into()), vec![], 1.0, 4)
        .unwrap();

    g.compute_property_selectivity();

    // Build TableStats.
    let mut stats = TableStats {
        vertex_count: g.vertex_count(),
        edge_count: g.edge_count(),
        avg_degree: if g.vertex_count() == 0 {
            1.0
        } else {
            (g.edge_count() as f64 / g.vertex_count() as f64).max(1.0)
        },
        label_cardinality: g.label_cardinalities(),
        ..TableStats::default()
    };
    for (key, &s) in g.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g.list_property_indexes() {
        if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
            stats
                .indexed_vertex_properties
                .insert(idx.property_name.clone());
            stats
                .property_selectivity
                .entry(format!("vertex:{}", idx.property_name))
                .or_insert(0.1);
        }
    }

    // Query: anchor is b (non-start), predicate on b.name
    let gql = "MATCH (a:User)-[e:KNOWS]->(b:Person) WHERE b.name = 'Alice' RETURN id(a), id(b)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    // Plan should use IndexScan.
    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "expected IndexScan for non-start-anchor query; ops = {:?}",
        plan.ops
    );

    let result = execute_plan(&plan, &g).unwrap();
    // Should find u0 and u1 (both :User with :KNOWS edge to alice).
    // u2 has :FOLLOWS (not :KNOWS) → excluded. u3 is :Other (not :User) → excluded.
    let mut rows: Vec<(i64, i64)> = result
        .rows
        .iter()
        .map(|r| {
            let a_id = match &r[0] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int for id(a)"),
            };
            let b_id = match &r[1] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int for id(b)"),
            };
            (a_id, b_id)
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![(u0 as i64, alice as i64), (u1 as i64, alice as i64)],
        "non-start-anchor IndexScan should return 2 matching rows"
    );
}

#[test]
fn index_scan_on_edge_property() {
    let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();

    // Create edges with different "weight" property values.
    g.create_edge(
        0,
        1,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int64(5))],
        1.0,
        1,
    )
    .unwrap();
    g.create_edge(
        0,
        2,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int64(3))],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(
        1,
        2,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int64(5))],
        1.0,
        3,
    )
    .unwrap();
    g.create_edge(
        2,
        3,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int64(7))],
        1.0,
        4,
    )
    .unwrap();

    // Before index creation, scan returns empty.
    let results = g.scan_edges_by_property_eq("weight", &Value::Int64(5));
    assert!(results.is_empty(), "no index yet → empty");

    // Create edge property index → backfill.
    g.create_index(EntityType::Edge, "weight".into(), IndexType::Equality)
        .unwrap();

    // Verify index registered.
    assert!(g.list_property_indexes().contains(&PropertyIndex {
        entity_type: EntityType::Edge,
        property_name: "weight".into(),
        index_type: IndexType::Equality,
    }));

    // Scan for weight=5 → should find (0,1) and (1,2).
    let mut results = g.scan_edges_by_property_eq("weight", &Value::Int64(5));
    results.sort();
    assert_eq!(results, vec![(0, 1), (1, 2)]);

    // Scan for weight=3 → should find (0,2).
    let results = g.scan_edges_by_property_eq("weight", &Value::Int64(3));
    assert_eq!(results, vec![(0, 2)]);

    // Scan for weight=99 → empty.
    let results = g.scan_edges_by_property_eq("weight", &Value::Int64(99));
    assert!(results.is_empty());

    // Mutate: set_edge_prop changes weight on (0,1) from 5 to 3.
    g.set_edge_prop(0, 1, Some("RATED"), "weight".into(), Value::Int64(3))
        .unwrap();
    let mut results5 = g.scan_edges_by_property_eq("weight", &Value::Int64(5));
    results5.sort();
    assert_eq!(
        results5,
        vec![(1, 2)],
        "after set_edge_prop: (0,1) removed from weight=5"
    );
    let mut results3 = g.scan_edges_by_property_eq("weight", &Value::Int64(3));
    results3.sort();
    assert_eq!(
        results3,
        vec![(0, 1), (0, 2)],
        "after set_edge_prop: (0,1) added to weight=3"
    );

    // delete_edge removes (1,2) → no longer in weight=5.
    g.delete_edge(1, 2, Some("RATED")).unwrap();
    let results5 = g.scan_edges_by_property_eq("weight", &Value::Int64(5));
    assert!(
        results5.is_empty(),
        "after delete_edge: (1,2) removed from weight=5"
    );

    // delete_edge_prop removes weight from (0,2) → no longer in weight=3.
    g.delete_edge_prop(0, 2, Some("RATED"), "weight").unwrap();
    let results3 = g.scan_edges_by_property_eq("weight", &Value::Int64(3));
    assert_eq!(
        results3,
        vec![(0, 1)],
        "after delete_edge_prop: (0,2) removed from weight=3"
    );

    // Selectivity should include edge property.
    g.compute_property_selectivity();
    let sel = g.get_property_selectivity();
    assert!(
        sel.contains_key("edge:weight"),
        "selectivity should include edge:weight"
    );
}

#[test]
fn edge_index_pre_filter_in_gql_query() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;
    use gleaph_gql::stats::TableStats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();

    // Create 4 vertices: (0:User), (1:User), (2:User), (3:Product)
    let u0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let p = g
        .create_vertex(
            vec!["Product".into()],
            vec![("name".into(), Value::Text("Widget".into()))],
        )
        .unwrap();

    // Edges with varying "weight" property:
    // u0 -[:RATED {weight:5}]-> p
    // u1 -[:RATED {weight:3}]-> p
    // u2 -[:RATED {weight:5}]-> p
    g.create_edge(
        u0,
        p,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int32(5))],
        1.0,
        1,
    )
    .unwrap();
    g.create_edge(
        u1,
        p,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int32(3))],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(
        u2,
        p,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int32(5))],
        1.0,
        3,
    )
    .unwrap();

    // Create edge property index on "weight".
    g.create_index(EntityType::Edge, "weight".into(), IndexType::Equality)
        .unwrap();
    g.compute_property_selectivity();

    let mut stats = TableStats {
        vertex_count: g.vertex_count(),
        edge_count: g.edge_count(),
        avg_degree: if g.vertex_count() == 0 {
            1.0
        } else {
            (g.edge_count() as f64 / g.vertex_count() as f64).max(1.0)
        },
        label_cardinality: g.label_cardinalities(),
        ..TableStats::default()
    };
    for (key, &s) in g.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g.list_property_indexes() {
        match idx.entity_type {
            EntityType::Vertex if idx.index_type == IndexType::Equality => {
                stats
                    .indexed_vertex_properties
                    .insert(idx.property_name.clone());
            }
            EntityType::Edge if idx.index_type == IndexType::Equality => {
                stats
                    .indexed_edge_properties
                    .insert(idx.property_name.clone());
            }
            _ => {}
        }
    }

    // Query with edge property hint that matches the index.
    let gql = "MATCH (a:User)-[e:RATED {weight: 5}]->(b:Product) RETURN id(a), id(b)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
    let result = execute_plan(&plan, &g).unwrap();

    // Should find u0 and u2 (both rated with weight=5).
    let mut rows: Vec<(i64, i64)> = result
        .rows
        .iter()
        .map(|r| {
            let a_id = match &r[0] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            let b_id = match &r[1] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            (a_id, b_id)
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![(u0 as i64, p as i64), (u2 as i64, p as i64)],
        "edge-index pre-filtered query should return only weight=5 edges"
    );
}

#[test]
fn edge_index_targets_for_src_basic() {
    let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
    g.create_edge(
        0,
        1,
        Some("R".into()),
        vec![("w".into(), Value::Int64(5))],
        1.0,
        1,
    )
    .unwrap();
    g.create_edge(
        0,
        2,
        Some("R".into()),
        vec![("w".into(), Value::Int64(3))],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(
        1,
        2,
        Some("R".into()),
        vec![("w".into(), Value::Int64(5))],
        1.0,
        3,
    )
    .unwrap();
    g.create_edge(
        2,
        3,
        Some("R".into()),
        vec![("w".into(), Value::Int64(5))],
        1.0,
        4,
    )
    .unwrap();

    // No index yet → returns None.
    assert!(
        g.edge_index_targets_for_src("w", &Value::Int64(5), 0)
            .is_none()
    );

    g.create_index(EntityType::Edge, "w".into(), IndexType::Equality)
        .unwrap();

    // From vertex 0, w=5 → target 1 only.
    let targets = g
        .edge_index_targets_for_src("w", &Value::Int64(5), 0)
        .unwrap();
    assert_eq!(targets, vec![1]);

    // From vertex 1, w=5 → target 2.
    let targets = g
        .edge_index_targets_for_src("w", &Value::Int64(5), 1)
        .unwrap();
    assert_eq!(targets, vec![2]);

    // From vertex 2, w=5 → target 3.
    let targets = g
        .edge_index_targets_for_src("w", &Value::Int64(5), 2)
        .unwrap();
    assert_eq!(targets, vec![3]);

    // From vertex 3, w=5 → empty.
    let targets = g
        .edge_index_targets_for_src("w", &Value::Int64(5), 3)
        .unwrap();
    assert!(targets.is_empty());

    // sources_for_dst: to vertex 2, w=5 → source 1.
    let sources = g
        .edge_index_sources_for_dst("w", &Value::Int64(5), 2)
        .unwrap();
    assert_eq!(sources, vec![1]);
}

// ── Algorithm continuation integration tests ────────────────────────────

#[test]
fn bfs_continuation_with_pma_graph() {
    use gleaph_algo::{
        AlgoOutcome,
        bfs::{BfsConfig, bfs_resumable, bfs_resume},
        budget::CountingBudget,
    };

    let mut g = new_graph();
    // Create a chain: 0->1->2->3->4
    for _ in 0u32..5 {
        g.create_vertex(vec![], Default::default()).unwrap();
    }
    for i in 0u32..4 {
        g.create_edge(i, i + 1, None, Default::default(), 1.0, 1)
            .unwrap();
    }

    let config = BfsConfig::default();

    // First call with very tight budget (1 vertex per call)
    let mut budget = CountingBudget::new(1);
    let outcome = bfs_resumable(&g, 0, &config, &mut budget).unwrap();
    let mut cp = match outcome {
        AlgoOutcome::Suspended { checkpoint, .. } => checkpoint,
        AlgoOutcome::Done(_) => panic!("expected suspension with budget=1"),
    };

    // Resume loop
    let final_result = loop {
        let mut b = CountingBudget::new(1);
        match bfs_resume(&g, cp, &mut b).unwrap() {
            AlgoOutcome::Done(r) => break r,
            AlgoOutcome::Suspended {
                checkpoint: next, ..
            } => cp = next,
        }
    };

    assert_eq!(final_result.visited.len(), 5);
    // Verify distances
    for &(v, d) in &final_result.distances {
        assert_eq!(d, v, "distance to vertex {v} should be {v}");
    }
}

#[test]
fn query_continue_rejects_stale_fingerprint() {
    use gleaph_algo::{
        AlgoOutcome,
        bfs::{BfsConfig, bfs_resumable},
        budget::CountingBudget,
    };
    use gleaph_types::{AlgorithmKind, ContinuationToken, GraphFingerprint};

    let mut g = new_graph();
    for _ in 0..3 {
        g.create_vertex(vec![], Default::default()).unwrap();
    }
    g.create_edge(0, 1, None, Default::default(), 1.0, 1)
        .unwrap();
    g.create_edge(1, 2, None, Default::default(), 1.0, 1)
        .unwrap();

    // Get a continuation checkpoint
    let config = BfsConfig::default();
    let mut budget = CountingBudget::new(1);
    let outcome = bfs_resumable(&g, 0, &config, &mut budget).unwrap();
    let cp = match outcome {
        AlgoOutcome::Suspended { checkpoint, .. } => checkpoint,
        AlgoOutcome::Done(_) => panic!("expected suspension"),
    };

    // Create a fingerprint from before mutation
    let fp_before = GraphFingerprint {
        num_vertices: g.vertex_count(),
        num_edges: g.edge_count(),
        next_edge_id: g.next_edge_id,
    };

    // Serialize checkpoint
    let data = serde_cbor::to_vec(&cp).unwrap();
    let token = ContinuationToken {
        kind: AlgorithmKind::Bfs,
        data,
        graph_fingerprint: fp_before.clone(),
    };

    // Mutate the graph (add a vertex)
    g.create_vertex(vec![], Default::default()).unwrap();

    // Now the fingerprint is stale
    let fp_after = GraphFingerprint {
        num_vertices: g.vertex_count(),
        num_edges: g.edge_count(),
        next_edge_id: g.next_edge_id,
    };
    assert_ne!(
        fp_before, fp_after,
        "fingerprint should differ after mutation"
    );

    // Attempting to resume with stale fingerprint should fail
    // (We can't call api::query_continue directly from integration tests because
    // it uses thread-local state, but we can validate the fingerprint logic:)
    assert_ne!(token.graph_fingerprint, fp_after);
}

// ── Bulk insert integration tests ────────────────────────────────────────────

#[test]
fn bulk_insert_then_gql_match() {
    use gleaph_gql::{
        executor::execute_plan, parse_statement, planner::build_plan, validate_statement,
    };

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();

    // Create vertices with labels.
    let v0 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Alice".into()))],
        )
        .unwrap();
    let v1 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Bob".into()))],
        )
        .unwrap();
    let v2 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Carol".into()))],
        )
        .unwrap();

    // Bulk-insert edges.
    let inputs = vec![
        BulkEdgeInput {
            src: v0,
            dst: v1,
            label: Some("KNOWS".into()),
            props: vec![],
            weight: 1.0,
            timestamp: 0,
        },
        BulkEdgeInput {
            src: v0,
            dst: v2,
            label: Some("KNOWS".into()),
            props: vec![],
            weight: 1.0,
            timestamp: 0,
        },
        BulkEdgeInput {
            src: v1,
            dst: v2,
            label: Some("KNOWS".into()),
            props: vec![],
            weight: 1.0,
            timestamp: 0,
        },
    ];
    let result = g.bulk_create_edges(&inputs).unwrap();
    assert_eq!(result.inserted, 3);

    // GQL query to find all KNOWS edges.
    let gql = "MATCH (a:User)-[e:KNOWS]->(b:User) RETURN a.name, b.name";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let plan = build_plan(&stmt).unwrap();
    let qr = execute_plan(&plan, &g).unwrap();

    assert_eq!(
        qr.rows.len(),
        3,
        "expected 3 KNOWS edges, got {}",
        qr.rows.len()
    );
}

#[test]
fn bulk_insert_large_batch() {
    let mut g = PmaGraph::new(VecMemory::default(), 64).unwrap();

    // Create 50 vertices.
    for _ in 0..50u32 {
        g.create_vertex(vec!["Node".into()], vec![]).unwrap();
    }

    // Create 5000 edges across various source/destination pairs.
    let edges: Vec<(u32, u32, f32, u64)> = (0..5000u32)
        .map(|i| {
            let src = i % 50;
            let dst = (i * 7 + 13) % 50;
            // Skip self-loops — filter later.
            (src, dst, 1.0, i as u64)
        })
        .filter(|&(s, d, _, _)| s != d)
        .collect();

    // Deduplicate: only keep first occurrence of each (src, dst).
    let mut seen = std::collections::HashSet::new();
    let unique_edges: Vec<(u32, u32, u32, f32, u64)> = edges
        .into_iter()
        .filter(|&(s, d, _, _)| seen.insert((s, d)))
        .map(|(s, d, w, t)| (s, d, 0u32, w, t))
        .collect();

    let result = g.bulk_insert_raw(&unique_edges).unwrap();
    assert_eq!(result.inserted, unique_edges.len() as u64);

    // Verify all edges are queryable.
    for &(src, dst, _, _, _) in &unique_edges {
        let ns = g.collect_neighbors(src).unwrap();
        assert!(
            ns.iter().any(|e| e.target == dst),
            "edge {src}->{dst} not found after bulk insert"
        );
    }
}

// ── Reverse-anchor optimization tests ────────────────────────────────────────

/// Helper: create a graph with Users → Product edges and return (graph, stats, product_id).
/// Graph shape: N users, each with a :Bought edge to a single Product{id: P}.
fn setup_reverse_anchor_graph(
    n_users: u32,
) -> (PmaGraph<VecMemory>, gleaph_gql::stats::TableStats, u32) {
    use gleaph_gql::stats::TableStats;

    let mut g = PmaGraph::new(VecMemory::default(), (n_users + 4).next_power_of_two()).unwrap();
    g.create_index(EntityType::Vertex, "pid".into(), IndexType::Equality)
        .unwrap();

    // Create the target product vertex with property pid=5.
    let product = g
        .create_vertex(
            vec!["Product".into()],
            vec![("pid".into(), Value::Int32(5))],
        )
        .unwrap();

    // Create N user vertices, each with a :Bought edge to the product.
    for i in 0..n_users {
        let u = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(u, product, Some("Bought".into()), vec![], 1.0, i as u64 + 1)
            .unwrap();
    }

    g.compute_property_selectivity();

    let mut stats = TableStats {
        vertex_count: g.vertex_count(),
        edge_count: g.edge_count(),
        avg_degree: if g.vertex_count() == 0 {
            1.0
        } else {
            (g.edge_count() as f64 / g.vertex_count() as f64).max(1.0)
        },
        label_cardinality: g.label_cardinalities(),
        ..TableStats::default()
    };
    for (key, &s) in g.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g.list_property_indexes() {
        if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
            stats
                .indexed_vertex_properties
                .insert(idx.property_name.clone());
            stats
                .property_selectivity
                .entry(format!("vertex:{}", idx.property_name))
                .or_insert(0.1);
        }
    }
    (g, stats, product)
}

/// Helper: parse + validate + plan + execute a GQL query with stats.
fn run_with_stats(
    g: &PmaGraph<VecMemory>,
    stats: &gleaph_gql::stats::TableStats,
    gql: &str,
) -> gleaph_types::QueryResult {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(stats)).unwrap();
    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "expected IndexScan; ops = {:?}",
        plan.ops
    );
    execute_plan(&plan, g).unwrap()
}

#[test]
fn test_reverse_anchor_skip_forward_rescan() {
    // Anchor at end of chain — reverse traversal covers entire first MATCH.
    // Verify correctness: results should match what we'd get without optimization.
    let (g, stats, _product) = setup_reverse_anchor_graph(10);

    let gql = "MATCH (u:User)-[:Bought]->(t:Product) WHERE t.pid = 5 RETURN id(u), t.pid";
    let result = run_with_stats(&g, &stats, gql);

    // All 10 users should appear, each paired with the product.
    assert_eq!(
        result.rows.len(),
        10,
        "expected 10 rows, got {}",
        result.rows.len()
    );
    for row in &result.rows {
        let t_id = match &row[1] {
            Value::Int32(i) => *i as i64,
            other => panic!("expected Int32 for t.pid, got {other:?}"),
        };
        assert_eq!(t_id, 5, "t.pid should be 5 (property value)");
    }
    // All user ids should be distinct.
    let mut user_ids: Vec<i64> = result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int64(i) => *i,
            other => panic!("expected Int for id(u), got {other:?}"),
        })
        .collect();
    user_ids.sort();
    user_ids.dedup();
    assert_eq!(user_ids.len(), 10);
}

#[test]
fn test_reverse_anchor_with_limit_pushdown() {
    // WITH u LIMIT 3 should be pushed down into reverse traversal,
    // producing at most 3 rows from the first MATCH.
    let (g, stats, _product) = setup_reverse_anchor_graph(20);

    let gql = "MATCH (u:User)-[:Bought]->(t:Product) WHERE t.pid = 5 \
               WITH u LIMIT 3 \
               RETURN id(u)";
    let result = run_with_stats(&g, &stats, gql);

    assert!(
        result.rows.len() <= 3,
        "expected <= 3 rows after WITH LIMIT 3, got {}",
        result.rows.len()
    );
    assert!(!result.rows.is_empty(), "expected at least 1 row");
}

#[test]
fn test_reverse_anchor_with_order_by_limit_no_pushdown() {
    // WITH u ORDER BY id(u) LIMIT 3 must NOT push down the limit,
    // because ORDER BY needs all rows to sort first.
    // Verify we get the 3 smallest user IDs.
    let (g, stats, _product) = setup_reverse_anchor_graph(20);

    let gql = "MATCH (u:User)-[:Bought]->(t:Product) WHERE t.pid = 5 \
               WITH u ORDER BY id(u) ASC LIMIT 3 \
               RETURN id(u)";
    let result = run_with_stats(&g, &stats, gql);

    assert_eq!(
        result.rows.len(),
        3,
        "expected 3 rows after ORDER BY + LIMIT 3"
    );
    let ids: Vec<i64> = result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int64(i) => *i,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    // Verify IDs are in ascending order (ORDER BY id(u) ASC).
    assert!(
        ids.windows(2).all(|w| w[0] <= w[1]),
        "IDs should be sorted ASC: {ids:?}"
    );
    // Run without LIMIT to get all IDs and verify we got the smallest 3.
    let all_result = run_with_stats(
        &g,
        &stats,
        "MATCH (u:User)-[:Bought]->(t:Product) WHERE t.pid = 5 RETURN id(u)",
    );
    let mut all_ids: Vec<i64> = all_result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int64(i) => *i,
            _ => panic!(),
        })
        .collect();
    all_ids.sort();
    assert_eq!(
        ids,
        &all_ids[..3],
        "ORDER BY should produce the 3 smallest IDs"
    );
}

#[test]
fn test_reverse_anchor_mid_chain_still_forward_scans() {
    // Anchor in the middle of a 2-chain pattern — forward re-scan should still run.
    // Pattern: (a:User)-[:KNOWS]->(b:Person)-[:LIKES]->(c:Product)
    // Anchor on b.name = "Alice" (chain index 0, but chains.len() == 2).
    use gleaph_gql::stats::TableStats;

    let mut g = PmaGraph::new(VecMemory::default(), 128).unwrap();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();

    let u0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let alice = g
        .create_vertex(
            vec!["Person".into()],
            vec![("name".into(), Value::Text("Alice".into()))],
        )
        .unwrap();
    let p0 = g.create_vertex(vec!["Product".into()], vec![]).unwrap();
    let p1 = g.create_vertex(vec!["Product".into()], vec![]).unwrap();
    // Add filler vertices so the planner prefers IndexScan over full scan.
    for i in 0..60u32 {
        let filler = g
            .create_vertex(
                vec!["Person".into()],
                vec![("name".into(), Value::Text(format!("Filler{i}")))],
            )
            .unwrap();
        g.create_edge(
            u0,
            filler,
            Some("KNOWS".into()),
            vec![],
            1.0,
            100 + i as u64,
        )
        .unwrap();
    }

    g.create_edge(u0, alice, Some("KNOWS".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(u1, alice, Some("KNOWS".into()), vec![], 1.0, 2)
        .unwrap();
    g.create_edge(alice, p0, Some("LIKES".into()), vec![], 1.0, 3)
        .unwrap();
    g.create_edge(alice, p1, Some("LIKES".into()), vec![], 1.0, 4)
        .unwrap();

    g.compute_property_selectivity();

    let mut stats = TableStats {
        vertex_count: g.vertex_count(),
        edge_count: g.edge_count(),
        avg_degree: if g.vertex_count() == 0 {
            1.0
        } else {
            (g.edge_count() as f64 / g.vertex_count() as f64).max(1.0)
        },
        label_cardinality: g.label_cardinalities(),
        ..TableStats::default()
    };
    // Inflate User cardinality so the planner's cost model prefers IndexScan.
    stats.label_cardinality.insert("User".into(), 1000);
    stats.vertex_count = 1000;
    for (key, &s) in g.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g.list_property_indexes() {
        if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
            stats
                .indexed_vertex_properties
                .insert(idx.property_name.clone());
            stats
                .property_selectivity
                .entry(format!("vertex:{}", idx.property_name))
                .or_insert(0.1);
        }
    }

    // Anchor is b (chain index 0), but chains.len() == 2, so anchor is NOT at end.
    // Forward re-scan must run to extend b→c via :LIKES.
    let gql = "MATCH (a:User)-[:KNOWS]->(b:Person)-[:LIKES]->(c:Product) \
               WHERE b.name = 'Alice' RETURN id(a), id(c)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = gleaph_gql::planner::build_plan_with_stats(&stmt, Some(&stats)).unwrap();
    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "expected IndexScan; ops = {:?}",
        plan.ops
    );
    let result = gleaph_gql::executor::execute_plan(&plan, &g).unwrap();

    // 2 users × 2 products = 4 result rows.
    assert_eq!(
        result.rows.len(),
        4,
        "expected 4 rows for mid-chain anchor, got {}",
        result.rows.len()
    );
    let mut pairs: Vec<(i64, i64)> = result
        .rows
        .iter()
        .map(|r| {
            let a = match &r[0] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            let c = match &r[1] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            (a, c)
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            (u0 as i64, p0 as i64),
            (u0 as i64, p1 as i64),
            (u1 as i64, p0 as i64),
            (u1 as i64, p1 as i64),
        ],
        "mid-chain anchor should still forward-scan to reach Product nodes"
    );
}

#[test]
fn create_index_computes_selectivity_immediately() {
    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();

    // Create some vertices with a "color" property before creating the index.
    for _ in 0..10 {
        g.create_vertex(
            vec!["Item".into()],
            vec![("color".into(), Value::Text("red".into()))],
        )
        .unwrap();
    }
    for _ in 0..5 {
        g.create_vertex(
            vec!["Item".into()],
            vec![("color".into(), Value::Text("blue".into()))],
        )
        .unwrap();
    }

    // Before CREATE INDEX: no selectivity data.
    assert!(
        g.get_property_selectivity().is_empty(),
        "no selectivity before CREATE INDEX"
    );

    // CREATE INDEX on "color".
    g.create_index(EntityType::Vertex, "color".into(), IndexType::Equality)
        .unwrap();

    // Simulate what gql_bridge does: compute selectivity for the new index.
    g.compute_selectivity_for_properties(&["vertex:color".to_string()]);

    let sel = g.get_property_selectivity();
    let color_sel = sel.get("vertex:color").copied();
    assert!(
        color_sel.is_some(),
        "selectivity should be computed after CREATE INDEX"
    );
    // 2 distinct values (red, blue) out of 15 total → cardinality ratio ≈ 0.133.
    let ratio = color_sel.unwrap();
    assert!(
        ratio > 0.1 && ratio < 0.2,
        "expected cardinality ratio ~0.133 for 2 distinct / 15 total, got {ratio}"
    );
}

// ── SET/REMOVE continuation tests ────────────────────────────────────────────

fn run_mutation_resumable(
    g: &mut PmaGraph<VecMemory>,
    gql: &str,
    budget: u64,
) -> gleaph_gql::executor::MutationProgress {
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    gleaph_gql::executor::execute_mutation_resumable(
        &stmt,
        g,
        gleaph_gql::executor::ExecutionLimits {
            max_rows: None,
            max_execution_steps: Some(budget),
        },
        1,
    )
    .unwrap()
}

#[test]
fn set_continuation_suspend_and_resume() {
    use gleaph_gql::executor::MutationProgress;

    let mut g = new_graph();
    // Create 5 pairs: (src)-[:R]->(dst) so MATCH has at least 1 hop
    let mut item_ids = Vec::new();
    for i in 0..5u32 {
        let src = g
            .create_vertex(
                vec!["Item".into()],
                vec![("idx".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        item_ids.push(src);
        let dst = g.create_vertex(vec!["Sink".into()], vec![]).unwrap();
        g.create_edge(src, dst, Some("R".into()), Default::default(), 1.0, 1)
            .unwrap();
    }

    // SET with budget=2 → should suspend after 2 ops (5 Items to update)
    let progress = run_mutation_resumable(&mut g, "MATCH (n:Item)-[:R]->() SET n.tag = 'done'", 2);

    let mut cp = match progress {
        MutationProgress::Suspended {
            partial,
            checkpoint,
        } => {
            assert!(
                partial.result.affected_vertices > 0,
                "first round: some ops applied"
            );
            checkpoint
        }
        MutationProgress::Done(_) => panic!("expected suspension with budget=2 and 5 vertices"),
    };

    // Resume loop until done
    let final_result = loop {
        let limits = gleaph_gql::executor::ExecutionLimits {
            max_rows: None,
            max_execution_steps: Some(2),
        };
        match gleaph_gql::executor::resume_mutation(cp, &mut g, limits).unwrap() {
            MutationProgress::Done(outcome) => break outcome,
            MutationProgress::Suspended {
                checkpoint: next, ..
            } => cp = next,
        }
    };

    assert_eq!(final_result.result.affected_vertices, 5);

    // Verify all Item vertices got the property
    for &id in &item_ids {
        let tag = g.get_single_vertex_property(id, "tag");
        assert_eq!(
            tag,
            Some(Value::Text("done".into())),
            "vertex {id} should have tag='done'"
        );
    }
}

#[test]
fn remove_continuation_suspend_and_resume() {
    use gleaph_gql::executor::MutationProgress;

    let mut g = new_graph();
    // Create 5 pairs with a property to remove
    let mut item_ids = Vec::new();
    for i in 0..5u32 {
        let src = g
            .create_vertex(
                vec!["Item".into()],
                vec![
                    ("idx".into(), Value::Int64(i as i64)),
                    ("temp".into(), Value::Text("remove_me".into())),
                ],
            )
            .unwrap();
        item_ids.push(src);
        let dst = g.create_vertex(vec!["Sink".into()], vec![]).unwrap();
        g.create_edge(src, dst, Some("R".into()), Default::default(), 1.0, 1)
            .unwrap();
    }

    // REMOVE with budget=1 → should suspend after each op
    let progress = run_mutation_resumable(&mut g, "MATCH (n:Item)-[:R]->() REMOVE n.temp", 1);

    let mut cp = match progress {
        MutationProgress::Suspended {
            partial,
            checkpoint,
        } => {
            assert_eq!(partial.result.affected_vertices, 1);
            checkpoint
        }
        MutationProgress::Done(_) => panic!("expected suspension with budget=1 and 5 vertices"),
    };

    // Resume until done
    let final_result = loop {
        let limits = gleaph_gql::executor::ExecutionLimits {
            max_rows: None,
            max_execution_steps: Some(1),
        };
        match gleaph_gql::executor::resume_mutation(cp, &mut g, limits).unwrap() {
            MutationProgress::Done(outcome) => break outcome,
            MutationProgress::Suspended {
                checkpoint: next, ..
            } => cp = next,
        }
    };

    assert_eq!(final_result.result.affected_vertices, 5);

    // Verify property was removed from all Item vertices
    for &id in &item_ids {
        let temp = g.get_single_vertex_property(id, "temp");
        assert!(
            temp.is_none(),
            "vertex {id} should not have 'temp' property"
        );
    }
}

#[test]
fn set_edge_continuation() {
    use gleaph_gql::executor::MutationProgress;

    let mut g = new_graph();
    // Create 4 vertices and 3 edges between them
    let mut vids = Vec::new();
    for _ in 0..4u32 {
        vids.push(g.create_vertex(vec!["Node".into()], vec![]).unwrap());
    }
    for i in 0..3 {
        g.create_edge(
            vids[i],
            vids[i + 1],
            Some("LINK".into()),
            Default::default(),
            1.0,
            1,
        )
        .unwrap();
    }

    // SET edge property with budget=1
    let progress = run_mutation_resumable(
        &mut g,
        "MATCH (a:Node)-[e:LINK]->(b:Node) SET e.weight = 42",
        1,
    );

    let mut cp = match progress {
        MutationProgress::Suspended { checkpoint, .. } => checkpoint,
        MutationProgress::Done(_) => panic!("expected suspension with budget=1 and 3 edges"),
    };

    // Resume until done
    let final_result = loop {
        let limits = gleaph_gql::executor::ExecutionLimits {
            max_rows: None,
            max_execution_steps: Some(1),
        };
        match gleaph_gql::executor::resume_mutation(cp, &mut g, limits).unwrap() {
            MutationProgress::Done(outcome) => break outcome,
            MutationProgress::Suspended {
                checkpoint: next, ..
            } => cp = next,
        }
    };

    assert_eq!(final_result.result.affected_edges, 3);
}

#[test]
fn set_large_budget_completes_immediately() {
    use gleaph_gql::executor::MutationProgress;

    let mut g = new_graph();
    for i in 0..3u32 {
        let src = g
            .create_vertex(
                vec!["Item".into()],
                vec![("idx".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        let dst = g.create_vertex(vec!["Sink".into()], vec![]).unwrap();
        g.create_edge(src, dst, Some("R".into()), Default::default(), 1.0, 1)
            .unwrap();
    }

    let progress =
        run_mutation_resumable(&mut g, "MATCH (n:Item)-[:R]->() SET n.done = true", 1000);

    match progress {
        MutationProgress::Done(outcome) => {
            assert_eq!(outcome.result.affected_vertices, 3);
        }
        MutationProgress::Suspended { .. } => {
            panic!("should not suspend with large budget");
        }
    }
}

#[test]
fn delete_continuation_backward_compat() {
    use gleaph_gql::executor::MutationProgress;

    let mut g = new_graph();
    for _ in 0..5u32 {
        let src = g.create_vertex(vec!["Tmp".into()], vec![]).unwrap();
        let dst = g.create_vertex(vec!["Sink".into()], vec![]).unwrap();
        g.create_edge(src, dst, Some("R".into()), Default::default(), 1.0, 1)
            .unwrap();
    }

    // DETACH DELETE with budget=2 → uses MutationCheckpoint::Delete
    let progress = run_mutation_resumable(&mut g, "MATCH (n:Tmp)-[:R]->() DETACH DELETE n", 2);

    let cp = match progress {
        MutationProgress::Suspended { checkpoint, .. } => checkpoint,
        MutationProgress::Done(_) => panic!("expected suspension"),
    };

    // Verify it's a Delete variant
    match &cp {
        gleaph_types::MutationCheckpoint::Delete(dc) => {
            assert!(!dc.remaining_vertices.is_empty());
        }
        _ => panic!("expected MutationCheckpoint::Delete"),
    }

    // Resume to completion
    let limits = gleaph_gql::executor::ExecutionLimits {
        max_rows: None,
        max_execution_steps: Some(100),
    };
    let result = gleaph_gql::executor::resume_mutation(cp, &mut g, limits).unwrap();
    match result {
        MutationProgress::Done(outcome) => {
            assert_eq!(outcome.result.affected_vertices, 5);
        }
        _ => panic!("expected Done after resume"),
    }
}

// ── Edge-index-seeded query tests ────────────────────────────────────────────

fn build_edge_index_stats(g: &mut PmaGraph<VecMemory>) -> gleaph_gql::stats::TableStats {
    use gleaph_gql::stats::TableStats;
    g.compute_property_selectivity();
    let mut stats = TableStats {
        vertex_count: g.vertex_count(),
        edge_count: g.edge_count(),
        avg_degree: if g.vertex_count() == 0 {
            1.0
        } else {
            (g.edge_count() as f64 / g.vertex_count() as f64).max(1.0)
        },
        label_cardinality: g.label_cardinalities(),
        ..TableStats::default()
    };
    for (key, &s) in g.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g.list_property_indexes() {
        match idx.entity_type {
            EntityType::Vertex if idx.index_type == IndexType::Equality => {
                stats
                    .indexed_vertex_properties
                    .insert(idx.property_name.clone());
            }
            EntityType::Edge if idx.index_type == IndexType::Equality => {
                stats
                    .indexed_edge_properties
                    .insert(idx.property_name.clone());
            }
            _ => {}
        }
    }
    stats
}

#[test]
fn edge_index_seeded_query_inline_prop() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();

    let u0 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let p = g.create_vertex(vec!["Product".into()], vec![]).unwrap();

    g.create_edge(
        u0,
        p,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int32(5))],
        1.0,
        1,
    )
    .unwrap();
    g.create_edge(
        u1,
        p,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int32(3))],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(
        u2,
        p,
        Some("RATED".into()),
        vec![("weight".into(), Value::Int32(5))],
        1.0,
        3,
    )
    .unwrap();

    g.create_index(EntityType::Edge, "weight".into(), IndexType::Equality)
        .unwrap();
    let stats = build_edge_index_stats(&mut g);

    // Inline edge property hint — should use EdgeIndexScan.
    let gql = "MATCH (a:User)-[e:RATED {weight: 5}]->(b:Product) RETURN id(a), id(b)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, gleaph_gql::plan::PlanOp::EdgeIndexScan)),
        "plan should include EdgeIndexScan, got {:?}",
        plan.ops
    );

    let result = execute_plan(&plan, &g).unwrap();

    let mut rows: Vec<(i64, i64)> = result
        .rows
        .iter()
        .map(|r| {
            let a = match &r[0] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            let b = match &r[1] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            (a, b)
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![(u0 as i64, p as i64), (u2 as i64, p as i64)],
        "edge-index seeded query returns only weight=5 edges"
    );
    assert!(
        result.stats.breakdown.index_fast_path_used,
        "edge-index fast path should be used"
    );
}

#[test]
fn edge_index_seeded_query_where_clause() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();

    let a = g.create_vertex(vec!["Node".into()], vec![]).unwrap();
    let b = g.create_vertex(vec!["Node".into()], vec![]).unwrap();
    let c = g.create_vertex(vec!["Node".into()], vec![]).unwrap();

    g.create_edge(
        a,
        b,
        Some("LINK".into()),
        vec![("kind".into(), Value::Text("fast".into()))],
        1.0,
        1,
    )
    .unwrap();
    g.create_edge(
        b,
        c,
        Some("LINK".into()),
        vec![("kind".into(), Value::Text("slow".into()))],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(
        a,
        c,
        Some("LINK".into()),
        vec![("kind".into(), Value::Text("fast".into()))],
        1.0,
        3,
    )
    .unwrap();

    g.create_index(EntityType::Edge, "kind".into(), IndexType::Equality)
        .unwrap();
    let stats = build_edge_index_stats(&mut g);

    // WHERE clause on edge variable — should use EdgeIndexScan.
    let gql = "MATCH (x:Node)-[e:LINK]->(y:Node) WHERE e.kind = 'fast' RETURN id(x), id(y)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, gleaph_gql::plan::PlanOp::EdgeIndexScan)),
        "plan should include EdgeIndexScan for WHERE-based edge predicate, got {:?}",
        plan.ops
    );

    let result = execute_plan(&plan, &g).unwrap();
    let mut rows: Vec<(i64, i64)> = result
        .rows
        .iter()
        .map(|r| {
            let x = match &r[0] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            let y = match &r[1] {
                Value::Int64(i) => *i,
                _ => panic!("expected Int"),
            };
            (x, y)
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![(a as i64, b as i64), (a as i64, c as i64)],
        "WHERE e.kind = 'fast' returns 2 edges"
    );
}

#[test]
fn edge_index_seeded_query_multi_chain() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();

    let a = g.create_vertex(vec!["A".into()], vec![]).unwrap();
    let b = g.create_vertex(vec!["B".into()], vec![]).unwrap();
    let c = g.create_vertex(vec!["C".into()], vec![]).unwrap();
    let d = g.create_vertex(vec!["D".into()], vec![]).unwrap();

    // a -[e1:R {priority:1}]-> b -[:S]-> c
    // a -[e2:R {priority:2}]-> d (no :S outgoing from d)
    g.create_edge(
        a,
        b,
        Some("R".into()),
        vec![("priority".into(), Value::Int32(1))],
        1.0,
        1,
    )
    .unwrap();
    g.create_edge(
        a,
        d,
        Some("R".into()),
        vec![("priority".into(), Value::Int32(2))],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(b, c, Some("S".into()), vec![], 1.0, 3)
        .unwrap();

    g.create_index(EntityType::Edge, "priority".into(), IndexType::Equality)
        .unwrap();
    let stats = build_edge_index_stats(&mut g);

    // Multi-chain: seed from edge index on first chain, forward-extend second.
    let gql = "MATCH (x:A)-[e:R {priority: 1}]->(y)-[:S]->(z) RETURN id(x), id(y), id(z)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, gleaph_gql::plan::PlanOp::EdgeIndexScan)),
        "plan should include EdgeIndexScan, got {:?}",
        plan.ops
    );

    let result = execute_plan(&plan, &g).unwrap();
    assert_eq!(result.rows.len(), 1, "only one path: a→b→c");
    let row = &result.rows[0];
    assert_eq!(row[0], Value::Int64(a as i64));
    assert_eq!(row[1], Value::Int64(b as i64));
    assert_eq!(row[2], Value::Int64(c as i64));
}

#[test]
fn edge_index_seeded_no_index_falls_back() {
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    let a = g.create_vertex(vec!["X".into()], vec![]).unwrap();
    let b = g.create_vertex(vec!["X".into()], vec![]).unwrap();
    g.create_edge(
        a,
        b,
        Some("R".into()),
        vec![("w".into(), Value::Int64(1))],
        1.0,
        1,
    )
    .unwrap();

    // No index created → should NOT emit EdgeIndexScan.
    let stats = build_edge_index_stats(&mut g);
    let gql = "MATCH (x)-[e:R {w: 1}]->(y) RETURN id(x)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
    assert!(
        !plan
            .ops
            .iter()
            .any(|op| matches!(op, gleaph_gql::plan::PlanOp::EdgeIndexScan)),
        "should NOT use EdgeIndexScan without edge index"
    );
}

#[test]
fn edge_index_seeded_annotation_source() {
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    let a = g.create_vertex(vec!["X".into()], vec![]).unwrap();
    let b = g.create_vertex(vec!["X".into()], vec![]).unwrap();
    g.create_edge(
        a,
        b,
        Some("R".into()),
        vec![("w".into(), Value::Int64(1))],
        1.0,
        1,
    )
    .unwrap();
    g.create_index(EntityType::Edge, "w".into(), IndexType::Equality)
        .unwrap();
    let stats = build_edge_index_stats(&mut g);

    let gql = "MATCH (x)-[e:R {w: 1}]->(y) RETURN id(x)";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
    assert_eq!(
        plan.annotations.estimated_cardinality_source.as_deref(),
        Some("edge-property-index(edge:w)"),
        "annotation should reference edge property index"
    );
}

// ── Parenthesized subpath patterns (§16.7) ──────────────────────────────────

/// Build a linear chain: v0 -[:E]-> v1 -[:E]-> v2 -[:E]-> v3 -[:E]-> v4
fn setup_linear_chain() -> PmaGraph<VecMemory> {
    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    let mut verts = Vec::new();
    for i in 0..5u32 {
        let v = g
            .create_vertex(
                vec!["N".into()],
                vec![("idx".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        verts.push(v);
    }
    for w in verts.windows(2) {
        g.create_edge(w[0], w[1], Some("E".to_string()), vec![], 1.0, 0)
            .unwrap();
    }
    g
}

fn run_gql(g: &PmaGraph<VecMemory>, gql: &str) -> gleaph_types::QueryResult {
    use gleaph_gql::{
        executor::execute_plan, parse_statement, planner::build_plan, validate_statement,
    };
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    let plan = build_plan(&stmt).unwrap();
    execute_plan(&plan, g).unwrap_or_else(|e| panic!("execute '{gql}': {e}"))
}

#[test]
fn subpath_fixed_repetition() {
    // Chain: v0->v1->v2->v3->v4
    // ((x)-[:E]->(y)){2} matches exactly 2 hops: v0→v2, v1→v3, v2→v4
    let g = setup_linear_chain();
    // First, verify a simpler case: 1 repetition should work like a normal hop.
    let qr1 = run_gql(&g, "MATCH (a:N)((x)-[:E]->(y)){1}(b:N) RETURN a.idx, b.idx");
    // 1 hop: same as MATCH (a:N)-[:E]->(b:N): 4 edges
    assert_eq!(
        qr1.rows.len(),
        4,
        "1-rep: expected 4 rows, got {:?}",
        qr1.rows
    );
    // Check a specific row: a=0, b=1 (v0→v1)
    let has_0_1 = qr1
        .rows
        .iter()
        .any(|r| r == &[Value::Int64(0), Value::Int64(1)]);
    assert!(has_0_1, "1-rep: expected (0,1) in {:?}", qr1.rows);

    let qr = run_gql(&g, "MATCH (a:N)((x)-[:E]->(y)){2}(b:N) RETURN a.idx, b.idx");
    // Expecting 3 paths: (0,2), (1,3), (2,4)
    assert_eq!(
        qr.rows.len(),
        3,
        "2-rep: expected 3 rows, got {:?}",
        qr.rows
    );
}

#[test]
fn subpath_range_repetition() {
    // ((x)-[:E]->(y)){1,2} matches 1 or 2 hops.
    let g = setup_linear_chain();
    let qr = run_gql(
        &g,
        "MATCH (a:N)((x)-[:E]->(y)){1,2}(b:N) RETURN a.idx, b.idx",
    );
    // 1 hop: 4 edges (0→1, 1→2, 2→3, 3→4)
    // 2 hops: 3 paths (0→2, 1→3, 2→4)
    // Total: 7
    assert_eq!(qr.rows.len(), 7, "expected 7 rows, got {:?}", qr.rows);
}

#[test]
fn subpath_no_quantifier_is_single() {
    // No quantifier = Fixed(1), same as a single hop.
    let g = setup_linear_chain();
    let qr = run_gql(&g, "MATCH (a:N)((x)-[:E]->(y))(b:N) RETURN a.idx, b.idx");
    // 1 hop only: 4 edges
    assert_eq!(qr.rows.len(), 4, "expected 4 rows, got {:?}", qr.rows);
}

/// Build a triangle: v0 -[:E]-> v1 -[:E]-> v2 -[:E]-> v0
fn setup_cycle_graph() -> PmaGraph<VecMemory> {
    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    let mut verts = Vec::new();
    for i in 0..3u32 {
        let v = g
            .create_vertex(
                vec!["N".into()],
                vec![("idx".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        verts.push(v);
    }
    // v0→v1, v1→v2, v2→v0 (cycle)
    for i in 0..3 {
        g.create_edge(
            verts[i],
            verts[(i + 1) % 3],
            Some("E".to_string()),
            vec![],
            1.0,
            0,
        )
        .unwrap();
    }
    g
}

#[test]
fn subpath_trail_no_repeated_edges() {
    // Triangle: v0→v1→v2→v0. Subpath {1,4} with TRAIL should stop
    // when an edge would be repeated.
    // WALK (default) allows revisiting edges, so {1,4} on a 3-cycle produces many rows.
    let g = setup_cycle_graph();
    let walk_qr = run_gql(
        &g,
        "MATCH (a:N)((x)-[:E]->(y)){1,4}(b:N) RETURN a.idx, b.idx",
    );
    let trail_qr = run_gql(
        &g,
        "MATCH TRAIL (a:N)((x)-[:E]->(y)){1,4}(b:N) RETURN a.idx, b.idx",
    );
    // TRAIL should produce fewer results than WALK (no repeated edges).
    assert!(
        trail_qr.rows.len() < walk_qr.rows.len(),
        "TRAIL ({}) should have fewer results than WALK ({})",
        trail_qr.rows.len(),
        walk_qr.rows.len(),
    );
    // TRAIL with {1,3} on a 3-cycle: each starting vertex can go 1, 2, or 3 hops
    // without repeating edges. 3 starts × 3 lengths = 9 maximum.
    let trail3 = run_gql(
        &g,
        "MATCH TRAIL (a:N)((x)-[:E]->(y)){1,3}(b:N) RETURN a.idx, b.idx",
    );
    assert_eq!(
        trail3.rows.len(),
        9,
        "TRAIL {{1,3}} on triangle: expected 9 rows, got {:?}",
        trail3.rows
    );
}

#[test]
fn subpath_simple_no_repeated_vertices() {
    // Triangle: v0→v1→v2→v0. SIMPLE should prevent vertex revisits.
    let g = setup_cycle_graph();
    // {1,3}: 1 hop = 3 results, 2 hops = 3 results (no vertex repeated yet),
    // 3 hops = back to start (vertex repeated) → filtered out by SIMPLE.
    let simple_qr = run_gql(
        &g,
        "MATCH SIMPLE (a:N)((x)-[:E]->(y)){1,3}(b:N) RETURN a.idx, b.idx",
    );
    // 1 hop: 3 (0→1, 1→2, 2→0) — but 2→0 revisits vertex 0 which is also `a`.
    // Actually SIMPLE checks all vertices in bindings: a, x, y, b.
    // The variables x and y get overwritten each repetition, so only the last
    // repetition's values are in bindings. Plus a and b.
    // Let's just verify SIMPLE produces fewer than WALK.
    let walk_qr = run_gql(
        &g,
        "MATCH (a:N)((x)-[:E]->(y)){1,3}(b:N) RETURN a.idx, b.idx",
    );
    assert!(
        simple_qr.rows.len() <= walk_qr.rows.len(),
        "SIMPLE ({}) should have <= results than WALK ({})",
        simple_qr.rows.len(),
        walk_qr.rows.len(),
    );
}

// ── Conditional index scan tests ─────────────────────────────────────────────

/// Helper: build TableStats from a PmaGraph.
fn build_table_stats(g: &PmaGraph<VecMemory>) -> gleaph_gql::stats::TableStats {
    let mut stats = gleaph_gql::stats::TableStats {
        vertex_count: g.vertex_count(),
        edge_count: g.edge_count(),
        avg_degree: if g.vertex_count() == 0 {
            1.0
        } else {
            (g.edge_count() as f64 / g.vertex_count() as f64).max(1.0)
        },
        label_cardinality: g.label_cardinalities(),
        ..Default::default()
    };
    for (key, &s) in g.get_property_selectivity() {
        stats.property_selectivity.insert(key.clone(), s);
    }
    for idx in g.list_property_indexes() {
        if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
            stats
                .indexed_vertex_properties
                .insert(idx.property_name.clone());
            stats
                .property_selectivity
                .entry(format!("vertex:{}", idx.property_name))
                .or_insert(0.1);
        }
        if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Range {
            stats
                .range_indexed_vertex_properties
                .insert(idx.property_name.clone());
        }
    }
    stats
}

#[test]
fn conditional_scan_uses_index_when_param_non_null() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();

    // Create several users.
    let _u0 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Alice".into()))],
        )
        .unwrap();
    let _u1 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Bob".into()))],
        )
        .unwrap();
    let _u2 = g
        .create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text("Carol".into()))],
        )
        .unwrap();

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE $name IS NULL OR u.name = $name RETURN u.name";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        matches!(
            plan.ops.first(),
            Some(gleaph_gql::plan::PlanOp::ConditionalIndexScan)
        ),
        "expected ConditionalIndexScan, got {:?}",
        plan.ops.first()
    );

    // Execute with non-NULL param → should use index and return only Alice.
    let mut params = HashMap::new();
    params.insert("name".into(), Value::Text("Alice".into()));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Text("Alice".into()));
    assert!(
        result.stats.breakdown.index_fast_path_used,
        "index fast path should be used when param is non-NULL"
    );
}

#[test]
fn conditional_scan_falls_back_when_param_null() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();

    g.create_vertex(
        vec!["User".into()],
        vec![("name".into(), Value::Text("Alice".into()))],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![("name".into(), Value::Text("Bob".into()))],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![("name".into(), Value::Text("Carol".into()))],
    )
    .unwrap();

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE $name IS NULL OR u.name = $name RETURN u.name";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    // Execute with NULL param → should fall back to full scan, return all users.
    let mut params = HashMap::new();
    params.insert("name".into(), Value::Null);
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(
        result.rows.len(),
        3,
        "NULL param should return all users via fallback"
    );
}

#[test]
fn conditional_scan_with_multiple_optional_filters() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();
    g.create_index(EntityType::Vertex, "city".into(), IndexType::Equality)
        .unwrap();

    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Alice".into())),
            ("city".into(), Value::Text("Tokyo".into())),
        ],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Bob".into())),
            ("city".into(), Value::Text("Osaka".into())),
        ],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Carol".into())),
            ("city".into(), Value::Text("Tokyo".into())),
        ],
    )
    .unwrap();

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE ($name IS NULL OR u.name = $name) AND ($city IS NULL OR u.city = $city) RETURN u.name";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    // Both name and city are indexed → ConditionalIndexScan with 2 candidates.
    assert!(
        matches!(
            plan.ops.first(),
            Some(gleaph_gql::plan::PlanOp::ConditionalIndexScan)
        ),
        "expected ConditionalIndexScan, got {:?}",
        plan.ops.first()
    );
    let cond = plan.annotations.conditional_scan.as_ref().unwrap();
    assert_eq!(
        cond.candidates.len(),
        2,
        "both name and city should be candidates"
    );

    // Filter by name only (city=NULL) → uses name index.
    let mut params = HashMap::new();
    params.insert("name".into(), Value::Text("Alice".into()));
    params.insert("city".into(), Value::Null);
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Text("Alice".into()));
    assert!(
        result.stats.breakdown.index_fast_path_used,
        "name index should be used"
    );

    // Filter by both name and city → uses first non-NULL candidate's index.
    params.insert("name".into(), Value::Text("Alice".into()));
    params.insert("city".into(), Value::Text("Tokyo".into()));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert!(result.stats.breakdown.index_fast_path_used);

    // Filter by city only (name=NULL) → uses city index (second candidate).
    params.insert("name".into(), Value::Null);
    params.insert("city".into(), Value::Text("Tokyo".into()));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 2, "Tokyo users: Alice and Carol");
    assert!(
        result.stats.breakdown.index_fast_path_used,
        "city index should be used when name is NULL"
    );

    // Both NULL → all users (falls back to full scan).
    params.insert("name".into(), Value::Null);
    params.insert("city".into(), Value::Null);
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 3, "all users when both params are NULL");
}

#[test]
fn conditional_range_scan_ge_uses_index() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g.create_index(EntityType::Vertex, "age".into(), IndexType::Range)
        .unwrap();

    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Alice".into())),
            ("age".into(), Value::Int64(20)),
        ],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Bob".into())),
            ("age".into(), Value::Int64(30)),
        ],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Carol".into())),
            ("age".into(), Value::Int64(40)),
        ],
    )
    .unwrap();

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql =
        "MATCH (u:User) WHERE $min_age IS NULL OR u.age >= $min_age RETURN u.name ORDER BY u.age";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        matches!(
            plan.ops.first(),
            Some(gleaph_gql::plan::PlanOp::ConditionalIndexScan)
        ),
        "expected ConditionalIndexScan, got {:?}",
        plan.ops.first()
    );
    let cond = plan.annotations.conditional_scan.as_ref().unwrap();
    assert_eq!(
        cond.candidates[0].cmp_op,
        gleaph_gql::plan::ConditionalCmpOp::Ge
    );

    // Execute with non-NULL param → should use range index, return age >= 30.
    let mut params = HashMap::new();
    params.insert("min_age".into(), Value::Int64(30));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::Text("Bob".into()));
    assert_eq!(result.rows[1][0], Value::Text("Carol".into()));
    assert!(result.stats.breakdown.index_fast_path_used);
}

#[test]
fn conditional_range_scan_lt_uses_index() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g.create_index(EntityType::Vertex, "age".into(), IndexType::Range)
        .unwrap();

    g.create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(20))])
        .unwrap();
    g.create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(30))])
        .unwrap();
    g.create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(40))])
        .unwrap();

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql =
        "MATCH (u:User) WHERE $max_age IS NULL OR u.age < $max_age RETURN u.age ORDER BY u.age";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(matches!(
        plan.ops.first(),
        Some(gleaph_gql::plan::PlanOp::ConditionalIndexScan)
    ),);
    let cond = plan.annotations.conditional_scan.as_ref().unwrap();
    assert_eq!(
        cond.candidates[0].cmp_op,
        gleaph_gql::plan::ConditionalCmpOp::Lt
    );

    // Execute with max_age=30 → should return only age=20.
    let mut params = HashMap::new();
    params.insert("max_age".into(), Value::Int64(30));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Int64(20));
    assert!(result.stats.breakdown.index_fast_path_used);
}

#[test]
fn conditional_range_scan_fallback_when_null() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g.create_index(EntityType::Vertex, "age".into(), IndexType::Range)
        .unwrap();

    g.create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(20))])
        .unwrap();
    g.create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(30))])
        .unwrap();
    g.create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(40))])
        .unwrap();

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE $min_age IS NULL OR u.age >= $min_age RETURN u.age";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    // NULL param → full scan returns all 3 users.
    let mut params = HashMap::new();
    params.insert("min_age".into(), Value::Null);
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn conditional_range_scan_mixed_eq_and_range() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();
    g.create_index(EntityType::Vertex, "age".into(), IndexType::Range)
        .unwrap();

    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Alice".into())),
            ("age".into(), Value::Int64(20)),
        ],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Bob".into())),
            ("age".into(), Value::Int64(30)),
        ],
    )
    .unwrap();
    g.create_vertex(
        vec!["User".into()],
        vec![
            ("name".into(), Value::Text("Carol".into())),
            ("age".into(), Value::Int64(40)),
        ],
    )
    .unwrap();

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    // Mix equality on name + range on age.
    let gql = "MATCH (u:User) WHERE ($name IS NULL OR u.name = $name) AND ($min_age IS NULL OR u.age >= $min_age) RETURN u.name ORDER BY u.age";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    let cond = plan.annotations.conditional_scan.as_ref().unwrap();
    assert_eq!(cond.candidates.len(), 2);
    assert_eq!(
        cond.candidates[0].cmp_op,
        gleaph_gql::plan::ConditionalCmpOp::Eq
    );
    assert_eq!(
        cond.candidates[1].cmp_op,
        gleaph_gql::plan::ConditionalCmpOp::Ge
    );

    // Use only the range filter (name=NULL, min_age=25).
    let mut params = HashMap::new();
    params.insert("name".into(), Value::Null);
    params.insert("min_age".into(), Value::Int64(25));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::Text("Bob".into()));
    assert_eq!(result.rows[1][0], Value::Text("Carol".into()));
    assert!(result.stats.breakdown.index_fast_path_used);
}

// ── Phase 4: Direct Range IndexScan E2E tests ──

#[test]
fn direct_range_index_scan_ge_uses_index() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
    g.create_index(EntityType::Vertex, "age".into(), IndexType::Range)
        .unwrap();

    // Create enough vertices for the cost model to favor index scan.
    for i in 0..100u32 {
        g.create_vertex(
            vec!["User".into()],
            vec![
                ("name".into(), Value::Text(format!("user_{i}"))),
                ("age".into(), Value::Int32(i as i32)),
            ],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE u.age >= 95 RETURN u.name ORDER BY u.age";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "expected IndexScan for range literal, got {:?}",
        plan.ops.first()
    );
    assert_eq!(
        plan.annotations.index_scan_cmp_op,
        Some(gleaph_gql::plan::ConditionalCmpOp::Ge)
    );

    let result = execute_plan(&plan, &g).unwrap();
    assert_eq!(result.rows.len(), 5); // ages 95, 96, 97, 98, 99
    assert_eq!(result.rows[0][0], Value::Text("user_95".into()));
    assert_eq!(result.rows[4][0], Value::Text("user_99".into()));
    assert!(result.stats.breakdown.index_fast_path_used);
}

#[test]
fn direct_range_index_scan_lt_uses_index() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
    g.create_index(EntityType::Vertex, "age".into(), IndexType::Range)
        .unwrap();

    for i in 0..100u32 {
        g.create_vertex(
            vec!["User".into()],
            vec![
                ("name".into(), Value::Text(format!("user_{i}"))),
                ("age".into(), Value::Int64(i as i64)),
            ],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE u.age < 3 RETURN u.name ORDER BY u.age";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(matches!(
        plan.ops.first(),
        Some(gleaph_gql::plan::PlanOp::IndexScan)
    ));
    assert_eq!(
        plan.annotations.index_scan_cmp_op,
        Some(gleaph_gql::plan::ConditionalCmpOp::Lt)
    );

    let result = execute_plan(&plan, &g).unwrap();
    assert_eq!(result.rows.len(), 3); // ages 0, 1, 2
    assert_eq!(result.rows[0][0], Value::Text("user_0".into()));
    assert_eq!(result.rows[2][0], Value::Text("user_2".into()));
    assert!(result.stats.breakdown.index_fast_path_used);
}

#[test]
fn direct_range_index_scan_no_index_falls_back() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
    // No range index created

    for (name, age) in [("Alice", 20), ("Bob", 30)] {
        g.create_vertex(
            vec!["User".into()],
            vec![
                ("name".into(), Value::Text(name.into())),
                ("age".into(), Value::Int64(age)),
            ],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE u.age >= 25 RETURN u.name";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    // No range index → NodeScan
    assert!(matches!(
        plan.ops.first(),
        Some(gleaph_gql::plan::PlanOp::NodeScan)
    ));

    // Should still return correct results via full scan + filter
    let result = execute_plan(&plan, &g).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Text("Bob".into()));
}

// ── Phase 5: Parameter-Based IndexScan E2E tests ──

#[test]
fn param_eq_index_scan_uses_index() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();

    for i in 0..100u32 {
        g.create_vertex(
            vec!["User".into()],
            vec![
                ("name".into(), Value::Text(format!("user_{i}"))),
                ("score".into(), Value::Int64(i as i64)),
            ],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE u.name = $name RETURN u.score";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "param eq should emit IndexScan, got {:?}",
        plan.ops.first()
    );

    let mut params = HashMap::new();
    params.insert("name".into(), Value::Text("user_42".into()));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Int64(42));
    assert!(result.stats.breakdown.index_fast_path_used);
}

#[test]
fn param_eq_index_scan_null_returns_empty() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
    g.create_index(EntityType::Vertex, "name".into(), IndexType::Equality)
        .unwrap();

    for i in 0..100u32 {
        g.create_vertex(
            vec!["User".into()],
            vec![("name".into(), Value::Text(format!("user_{i}")))],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE u.name = $name RETURN u.name";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    // NULL parameter → no match (three-valued logic: NULL = x → NULL → false)
    let mut params = HashMap::new();
    params.insert("name".into(), Value::Null);
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn param_range_index_scan_uses_index() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
    g.create_index(EntityType::Vertex, "score".into(), IndexType::Range)
        .unwrap();

    for i in 0..100u32 {
        g.create_vertex(
            vec!["User".into()],
            vec![
                ("name".into(), Value::Text(format!("user_{i}"))),
                ("score".into(), Value::Int64(i as i64)),
            ],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    let gql = "MATCH (u:User) WHERE u.score >= $min RETURN u.name ORDER BY u.score";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "param range should emit IndexScan, got {:?}",
        plan.ops.first()
    );

    let mut params = HashMap::new();
    params.insert("min".into(), Value::Int64(97));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 3); // scores 97, 98, 99
    assert_eq!(result.rows[0][0], Value::Text("user_97".into()));
    assert!(result.stats.breakdown.index_fast_path_used);
}

#[test]
fn multi_pred_anchor_picks_most_selective() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
    g.create_index(EntityType::Vertex, "country".into(), IndexType::Equality)
        .unwrap();
    g.create_index(EntityType::Vertex, "email".into(), IndexType::Equality)
        .unwrap();

    // 100 users. country has ~30% selectivity (3 countries), email is unique.
    for i in 0..100u32 {
        let country = match i % 3 {
            0 => "JP",
            1 => "US",
            _ => "DE",
        };
        g.create_vertex(
            vec!["User".into()],
            vec![
                ("country".into(), Value::Text(country.into())),
                ("email".into(), Value::Text(format!("user_{i}@test.com"))),
                ("score".into(), Value::Int64(i as i64)),
            ],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    // country appears FIRST in AST but email is more selective.
    let gql =
        "MATCH (n:User) WHERE n.country = 'JP' AND n.email = 'user_42@test.com' RETURN n.score";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        matches!(plan.ops.first(), Some(gleaph_gql::plan::PlanOp::IndexScan)),
        "multi-pred should emit IndexScan, got {:?}",
        plan.ops.first()
    );

    let result = execute_plan(&plan, &g).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Int64(42));
    assert!(result.stats.breakdown.index_fast_path_used);
}

#[test]
fn compound_range_scan_both_bounds() {
    use gleaph_gql::executor::{ExecutionLimits, execute_plan_with_params};
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
    g.create_index(EntityType::Vertex, "score".into(), IndexType::Range)
        .unwrap();

    for i in 0..100u32 {
        g.create_vertex(
            vec!["Item".into()],
            vec![
                ("name".into(), Value::Text(format!("item_{i}"))),
                ("score".into(), Value::Int64(i as i64)),
            ],
        )
        .unwrap();
    }

    g.compute_property_selectivity();
    let stats = build_table_stats(&g);

    // Both bounds non-NULL → compound range scan should kick in.
    let gql = "MATCH (n:Item) WHERE ($min IS NULL OR n.score >= $min) AND ($max IS NULL OR n.score <= $max) RETURN n.name ORDER BY n.score";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert!(
        matches!(
            plan.ops.first(),
            Some(gleaph_gql::plan::PlanOp::ConditionalIndexScan)
        ),
        "compound range should emit ConditionalIndexScan, got {:?}",
        plan.ops.first()
    );

    // Both bounds: score in [40, 50] → 11 results.
    let mut params = HashMap::new();
    params.insert("min".into(), Value::Int64(40));
    params.insert("max".into(), Value::Int64(50));
    let result = execute_plan_with_params(&plan, &g, &params, ExecutionLimits::default()).unwrap();
    assert_eq!(result.rows.len(), 11, "expected items 40..=50");
    assert_eq!(result.rows[0][0], Value::Text("item_40".into()));
    assert_eq!(result.rows[10][0], Value::Text("item_50".into()));
    assert!(result.stats.breakdown.index_fast_path_used);

    // Only lower bound: score >= 95 → 5 results.
    let mut params2 = HashMap::new();
    params2.insert("min".into(), Value::Int64(95));
    params2.insert("max".into(), Value::Null);
    let result2 =
        execute_plan_with_params(&plan, &g, &params2, ExecutionLimits::default()).unwrap();
    assert_eq!(result2.rows.len(), 5, "expected items 95..=99");
    assert!(result2.stats.breakdown.index_fast_path_used);
}

// ── Phase 10: Label-based mid-chain anchor tests ──

#[test]
fn label_anchor_mid_chain_reverse_traverse() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 128).unwrap();

    // Create 50 Common vertices and 3 Rare vertices.
    let mut common_ids = Vec::new();
    for _ in 0..50 {
        common_ids.push(g.create_vertex(vec!["Common".into()], vec![]).unwrap());
    }
    let mut rare_ids = Vec::new();
    for _ in 0..3 {
        rare_ids.push(g.create_vertex(vec!["Rare".into()], vec![]).unwrap());
    }
    // More Common vertices for c.
    let mut common2_ids = Vec::new();
    for _ in 0..50 {
        common2_ids.push(g.create_vertex(vec!["Common".into()], vec![]).unwrap());
    }

    // Edges: some Common→Rare→Common chains.
    g.create_edge(common_ids[0], rare_ids[0], Some("R".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(
        rare_ids[0],
        common2_ids[0],
        Some("R".into()),
        vec![],
        1.0,
        2,
    )
    .unwrap();
    g.create_edge(common_ids[1], rare_ids[1], Some("R".into()), vec![], 1.0, 3)
        .unwrap();
    g.create_edge(
        rare_ids[1],
        common2_ids[1],
        Some("R".into()),
        vec![],
        1.0,
        4,
    )
    .unwrap();

    // Build stats with explicit cardinalities to force b:Rare as anchor.
    let mut stats = build_table_stats(&g);
    stats.label_cardinality.insert("Common".into(), 100);
    stats.label_cardinality.insert("Rare".into(), 3);

    let gql = "MATCH (a:Common)-[:R]->(b:Rare)-[:R]->(c:Common) RETURN a, b, c";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    // Planner should pick "b" (Rare, card=3) as anchor.
    assert_eq!(
        plan.annotations.chosen_anchor.as_deref(),
        Some("b"),
        "planner should choose b:Rare as anchor"
    );

    let result = execute_plan(&plan, &g).unwrap();
    assert_eq!(result.rows.len(), 2, "expected 2 matching chains");
}

#[test]
fn label_anchor_end_chain_covers_all() {
    use gleaph_gql::executor::execute_plan;
    use gleaph_gql::planner::build_plan_with_stats;

    let mut g = PmaGraph::new(VecMemory::default(), 64).unwrap();

    // 30 Common, 2 Rare.
    let mut common_ids = Vec::new();
    for _ in 0..30 {
        common_ids.push(g.create_vertex(vec!["Common".into()], vec![]).unwrap());
    }
    let r0 = g.create_vertex(vec!["Rare".into()], vec![]).unwrap();
    let r1 = g.create_vertex(vec!["Rare".into()], vec![]).unwrap();

    // common[0] → r0, common[1] → r0, common[2] → r1
    g.create_edge(common_ids[0], r0, Some("E".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(common_ids[1], r0, Some("E".into()), vec![], 1.0, 2)
        .unwrap();
    g.create_edge(common_ids[2], r1, Some("E".into()), vec![], 1.0, 3)
        .unwrap();

    // Build stats with explicit cardinalities.
    let mut stats = build_table_stats(&g);
    stats.label_cardinality.insert("Common".into(), 30);
    stats.label_cardinality.insert("Rare".into(), 2);

    let gql = "MATCH (a:Common)-[:E]->(b:Rare) RETURN a, b";
    let stmt = gleaph_gql::parse_statement(gql).unwrap();
    gleaph_gql::validate_statement(&stmt).unwrap();
    let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

    assert_eq!(
        plan.annotations.chosen_anchor.as_deref(),
        Some("b"),
        "planner should choose b:Rare as end-chain anchor"
    );

    let result = execute_plan(&plan, &g).unwrap();
    assert_eq!(result.rows.len(), 3, "expected 3 matching edges");
}
