//! ADR 0059 canonical index-export benches (Graph-owned export pages).
//!
//! Each fixture seeds the indexed facts and registers the immutable physical scope
//! OUTSIDE the measured closure, then measures one bounded `export_page` call with
//! `MAX_CANONICAL_EXPORT_PAGE_ITEMS`. Fixture construction never enters the closure.
//!
//! Run from `crates/graph`: `canbench index_export` (see `canbench.yml`).

use crate::facade::GraphStore;
use crate::facade::mutation_executor::GraphMutationExecutor;
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_graph_kernel::canonical_export::{
    CanonicalExportRequest, CanonicalExportScope, CanonicalExportTarget, CanonicalInlineProjection,
    MAX_CANONICAL_EXPORT_PAGE_ITEMS,
};
use gleaph_graph_kernel::entry::{
    EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, GraphId, IndexNameId,
};
use gleaph_graph_kernel::index::{EdgeIndexDirection, PhysicalIndexId};
use ic_stable_lara::labeled::LabeledOrientation;
use std::hint::black_box;

/// Seeded canonical facts per export page. Int64 facts are far below the page byte
/// budget, so one bounded call emits the full 256-fact page.
const INDEX_EXPORT_PAGE_COUNT: u32 = 256;

fn export_request(
    physical_index_id: PhysicalIndexId,
    target: CanonicalExportTarget,
) -> CanonicalExportRequest {
    CanonicalExportRequest {
        graph_id: GraphId::from_raw(1),
        index_name_id: IndexNameId::from_raw(1),
        physical_index_id,
        catalog_epoch: 1,
        target,
        cursor: None,
        limit: MAX_CANONICAL_EXPORT_PAGE_ITEMS,
    }
}

fn export_scope(target: CanonicalExportTarget) -> CanonicalExportScope {
    CanonicalExportScope {
        graph_id: GraphId::from_raw(1),
        index_name_id: IndexNameId::from_raw(1),
        catalog_epoch: 1,
        target,
        inline: None,
    }
}

/// Vertex page: 256 vertices carrying the indexed property on the indexed label.
#[bench(raw)]
fn bench_graph_index_export_vertex_page_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let label = crate::test_labels::vertex_label_id_for_name("bench_index_export_vertex_label");
    let property = crate::test_labels::property_id_for_name("bench_index_export_vertex_property");
    for vertex in 0..INDEX_EXPORT_PAGE_COUNT {
        store
            .insert_vertex_with([label], [(property, Value::Int64(vertex as i64))], 0)
            .expect("indexed vertex");
    }
    let physical_index_id = PhysicalIndexId::new(901_001).expect("bench physical id");
    let target = CanonicalExportTarget::Vertex {
        label_id: label.raw(),
        property_id: property,
    };
    crate::index::canonical_export::register_scope(physical_index_id, export_scope(target.clone()))
        .expect("register vertex scope");
    let request = export_request(physical_index_id, target);
    let sanity = crate::index::canonical_export::export_page(request.clone())
        .expect("sanity vertex export page");
    assert_eq!(
        sanity.facts.len(),
        INDEX_EXPORT_PAGE_COUNT as usize,
        "sanity vertex fact count"
    );
    assert!(
        sanity.done,
        "sanity vertex page reaches the end of the source"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("index_export_vertex_page_256");
        let page = crate::index::canonical_export::export_page(black_box(request.clone()))
            .expect("canonical export page");
        black_box(page);
    })
}

/// Edge sidecar page: 256 directed edges on one hub owner, each with the indexed
/// sidecar property on the indexed label.
#[bench(raw)]
fn bench_graph_index_export_edge_sidecar_page_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let label = crate::test_labels::edge_label_id_for_name("bench_index_export_sidecar_label");
    let property = crate::test_labels::property_id_for_name("bench_index_export_sidecar_property");
    let owner = store.insert_vertex().expect("sidecar hub owner");
    let targets = (0..INDEX_EXPORT_PAGE_COUNT)
        .map(|_| store.insert_vertex().expect("sidecar target"))
        .collect::<Vec<_>>();
    for (slot, target) in targets.iter().copied().enumerate() {
        let edge = store
            .insert_directed_edge(owner, target, Some(label))
            .expect("sidecar edge");
        store
            .set_edge_property(
                edge.occurrence(LabeledOrientation::Forward),
                property,
                Value::Int64(slot as i64),
            )
            .expect("sidecar property");
    }
    let physical_index_id = PhysicalIndexId::new(901_002).expect("bench physical id");
    let target = CanonicalExportTarget::Edge {
        label_id: label,
        property_id: property,
        direction: EdgeIndexDirection::Any,
    };
    crate::index::canonical_export::register_scope(physical_index_id, export_scope(target.clone()))
        .expect("register sidecar scope");
    let request = export_request(physical_index_id, target);
    let sanity = crate::index::canonical_export::export_page(request.clone())
        .expect("sanity sidecar export page");
    assert_eq!(
        sanity.facts.len(),
        INDEX_EXPORT_PAGE_COUNT as usize,
        "sanity sidecar fact count"
    );
    assert!(
        sanity.done,
        "sanity sidecar page reaches the end of the source"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("index_export_edge_sidecar_page_256");
        let page = crate::index::canonical_export::export_page(black_box(request.clone()))
            .expect("canonical export page");
        black_box(page);
    })
}

/// Edge inline page: 256 directed edges on one hub owner with F32 inline property
/// bytes projected by the registered inline scope.
#[bench(raw)]
fn bench_graph_index_export_edge_inline_page_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let label = crate::test_labels::edge_label_id_for_name("bench_index_export_inline_label");
    let property = crate::test_labels::property_id_for_name("bench_index_export_inline_property");
    let profile = EdgeInlinePropertyProfile {
        byte_width: 4,
        encoding: EdgeInlinePropertyEncoding::F32,
    };
    crate::test_labels::install_test_edge_inline_property_profile(label, profile.clone());
    crate::test_labels::install_test_edge_inline_property(label, property);
    let owner = store.insert_vertex().expect("inline hub owner");
    let targets = (0..INDEX_EXPORT_PAGE_COUNT)
        .map(|_| store.insert_vertex().expect("inline target"))
        .collect::<Vec<_>>();
    for target in targets {
        store
            .insert_directed_edge_with_inline_property_bytes(
                owner,
                target,
                Some(label),
                &1.25f32.to_le_bytes(),
            )
            .expect("inline edge");
    }
    let physical_index_id = PhysicalIndexId::new(901_003).expect("bench physical id");
    let target = CanonicalExportTarget::Edge {
        label_id: label,
        property_id: property,
        direction: EdgeIndexDirection::Any,
    };
    let mut scope = export_scope(target.clone());
    scope.inline = Some(CanonicalInlineProjection {
        source_property_id: property,
        byte_offset: 0,
        source_profile: profile.clone(),
        value_profile: profile,
    });
    crate::index::canonical_export::register_scope(physical_index_id, scope)
        .expect("register inline scope");
    let request = export_request(physical_index_id, target);
    let sanity = crate::index::canonical_export::export_page(request.clone())
        .expect("sanity inline export page");
    assert_eq!(
        sanity.facts.len(),
        INDEX_EXPORT_PAGE_COUNT as usize,
        "sanity inline fact count"
    );
    assert!(
        sanity.done,
        "sanity inline page reaches the end of the source"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("index_export_edge_inline_page_256");
        let page = crate::index::canonical_export::export_page(black_box(request.clone()))
            .expect("canonical export page");
        black_box(page);
    })
}
