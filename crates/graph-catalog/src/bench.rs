//! PocketIC / `canbench` targets for [`GraphCatalog`] (`StableBTreeMap` + rkyv).
//!
//! Run from `crates/graph-catalog`: `canbench` (see `canbench.yml`).

use super::{GraphCatalog, GraphNameLookup, GraphTypeLookup, object_name_key};
use canbench_rs::bench;
use gleaph_gql::{
    ast::{Statement, StatementBlock},
    parser,
    type_check::{GraphTypePropertySchema, PropertySchema},
};
use gleaph_graph_kernel::entry::{GraphId, GraphTypeId};
use ic_stable_structures::{
    DefaultMemoryImpl,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::hint::black_box;

fn catalog_new() -> GraphCatalog<VirtualMemory<DefaultMemoryImpl>, VirtualMemory<DefaultMemoryImpl>>
{
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let type_mem = manager.get(MemoryId::new(0));
    let bind_mem = manager.get(MemoryId::new(1));
    GraphCatalog::init(type_mem, bind_mem)
}

fn block_from(gql: &str) -> StatementBlock {
    let program = parser::parse(gql).expect("parse");
    program
        .transaction_activity
        .expect("tx")
        .body
        .expect("body")
}

struct BenchGraphLookup(RefCell<BTreeMap<String, GraphId>>);

impl BenchGraphLookup {
    fn new() -> Self {
        Self(RefCell::new(BTreeMap::new()))
    }

    fn ensure_graphs_in_block(&self, block: &StatementBlock) {
        let mut map = self.0.borrow_mut();
        for stmt in block.iter_statements() {
            let Statement::CreateGraph(c) = stmt else {
                continue;
            };
            let name = object_name_key(&c.name);
            if map.contains_key(&name) {
                continue;
            }
            let id = GraphId::from_raw(map.len() as u32 + 1);
            map.insert(name, id);
        }
    }
}

impl GraphNameLookup for BenchGraphLookup {
    fn lookup_graph_id(&self, graph_name: &str) -> Option<GraphId> {
        self.0.borrow().get(graph_name).copied()
    }
}

struct BenchGraphTypeLookup(RefCell<BTreeMap<String, GraphTypeId>>);

impl BenchGraphTypeLookup {
    fn new() -> Self {
        Self(RefCell::new(BTreeMap::new()))
    }
}

impl GraphTypeLookup for BenchGraphTypeLookup {
    fn lookup_graph_type_id(&self, type_name: &str) -> Option<GraphTypeId> {
        self.0.borrow().get(type_name).copied()
    }

    fn intern_graph_type_id(
        &mut self,
        type_name: &str,
    ) -> Result<GraphTypeId, super::CatalogError> {
        let mut map = self.0.borrow_mut();
        if let Some(id) = map.get(type_name) {
            return Ok(*id);
        }
        let id = GraphTypeId::from_raw(map.len() as u32 + 1);
        map.insert(type_name.to_string(), id);
        Ok(id)
    }

    fn remove_graph_type_by_name(&mut self, type_name: &str) -> Option<GraphTypeId> {
        self.0.borrow_mut().remove(type_name)
    }
}

struct BenchLookups {
    graphs: BenchGraphLookup,
    types: BenchGraphTypeLookup,
}

impl BenchLookups {
    fn new() -> Self {
        Self {
            graphs: BenchGraphLookup::new(),
            types: BenchGraphTypeLookup::new(),
        }
    }

    fn ensure_graphs_in_block(&self, block: &StatementBlock) {
        self.graphs.ensure_graphs_in_block(block);
    }
}

const PERSON_KNOWS: &str =
    "NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person)";

fn ddl_or_replace_type_and_typed_graph() -> String {
    format!(
        "CREATE OR REPLACE GRAPH TYPE gt {{ {PERSON_KNOWS} }} NEXT CREATE OR REPLACE GRAPH g TYPED gt"
    )
}

fn ddl_one_type_and_n_typed_graphs(n: u32) -> String {
    let mut s = format!("CREATE GRAPH TYPE gt {{ {PERSON_KNOWS} }}");
    for i in 0..n {
        s.push_str(" NEXT CREATE GRAPH g");
        s.push_str(&i.to_string());
        s.push_str(" TYPED gt");
    }
    s
}

fn ddl_n_graph_types(n: u32) -> String {
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push_str(" NEXT ");
        }
        s.push_str(&format!(
            "CREATE GRAPH TYPE gt{i} {{ NODE N LABEL N, DIRECTED EDGE E LABEL E CONNECTING (N -> N) }}"
        ));
    }
    s
}

/// Inline graph type with several labels and edges (rkyv decode + schema build).
const DDL_INLINE_MEDIUM: &str = "CREATE GRAPH g {
    NODE Person LABEL Person,
    NODE Company LABEL Company,
    NODE Project LABEL Project,
    DIRECTED EDGE WORKS_AT LABEL WORKS_AT CONNECTING (Person -> Company),
    DIRECTED EDGE OWNS LABEL OWNS CONNECTING (Company -> Project),
    DIRECTED EDGE LEADS LABEL LEADS CONNECTING (Person -> Project),
    UNDIRECTED EDGE COLLEAGUE LABEL COLLEAGUE CONNECTING (Person ~ Person)
}";

fn black_box_schema_ops(schema: &GraphTypePropertySchema) {
    black_box((
        schema.edge_is_undirected(black_box("KNOWS")),
        schema.edge_is_undirected(black_box("COLLEAGUE")),
    ));
}

#[bench(raw)]
fn bench_catalog_parse_small_ddl() -> canbench_rs::BenchResult {
    let ddl = ddl_or_replace_type_and_typed_graph();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("catalog_parse");
        let block = block_from(black_box(ddl.as_str()));
        black_box(block.iter_statements().count())
    })
}

/// Steady-state DDL apply: same maps, `OR REPLACE` updates definitions each sample.
#[bench(raw)]
fn bench_catalog_apply_or_replace_type_and_graph() -> canbench_rs::BenchResult {
    let block = block_from(&ddl_or_replace_type_and_typed_graph());
    let mut lookups = BenchLookups::new();
    lookups.ensure_graphs_in_block(&block);
    let mut cat = catalog_new();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("catalog_apply");
        cat.apply_statement_block(black_box(&block), &lookups.graphs, &mut lookups.types)
            .expect("apply");
        black_box(
            cat.try_property_schema_for_graph_id(GraphId::from_raw(1))
                .expect("resolve")
                .is_some(),
        );
    })
}

/// Hot path: schema resolution for `TYPED` (BTree get + rkyv + [`GraphTypePropertySchema`]).
#[bench(raw)]
fn bench_catalog_resolve_typed_schema() -> canbench_rs::BenchResult {
    let block = block_from(&ddl_one_type_and_n_typed_graphs(1));
    let mut lookups = BenchLookups::new();
    lookups.ensure_graphs_in_block(&block);
    let mut cat = catalog_new();
    cat.apply_statement_block(&block, &lookups.graphs, &mut lookups.types)
        .expect("setup");
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("catalog_resolve");
        let schema = cat
            .try_property_schema_for_graph_id(black_box(GraphId::from_raw(1)))
            .expect("resolve")
            .expect("schema");
        black_box_schema_ops(&schema);
    })
}

/// Resolution cost grows with binding map size (lookup + shared type decode).
#[bench(raw)]
fn bench_catalog_resolve_typed_schema_among_128_graphs() -> canbench_rs::BenchResult {
    let block = block_from(&ddl_one_type_and_n_typed_graphs(128));
    let mut lookups = BenchLookups::new();
    lookups.ensure_graphs_in_block(&block);
    let mut cat = catalog_new();
    cat.apply_statement_block(&block, &lookups.graphs, &mut lookups.types)
        .expect("setup");
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("catalog_resolve");
        let schema = cat
            .try_property_schema_for_graph_id(black_box(GraphId::from_raw(65)))
            .expect("resolve")
            .expect("schema");
        black_box_schema_ops(&schema);
    })
}

/// Inline binding: decode stored [`GraphTypeDefinition`] and validate schema.
#[bench(raw)]
fn bench_catalog_resolve_inline_medium() -> canbench_rs::BenchResult {
    let block = block_from(DDL_INLINE_MEDIUM);
    let mut lookups = BenchLookups::new();
    lookups.ensure_graphs_in_block(&block);
    let mut cat = catalog_new();
    cat.apply_statement_block(&block, &lookups.graphs, &mut lookups.types)
        .expect("setup");
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("catalog_resolve");
        let schema = cat
            .try_property_schema_for_graph_id(black_box(GraphId::from_raw(1)))
            .expect("resolve")
            .expect("schema");
        black_box_schema_ops(&schema);
    })
}

/// Batch catalog DDL: many new graph types in one block (encode + BTree inserts).
#[bench(raw)]
fn bench_catalog_apply_32_new_graph_types() -> canbench_rs::BenchResult {
    let block = block_from(&ddl_n_graph_types(32));
    canbench_rs::bench_fn(|| {
        let mut lookups = BenchLookups::new();
        let mut cat = catalog_new();
        let _scope = canbench_rs::bench_scope("catalog_apply");
        cat.apply_statement_block(black_box(&block), &lookups.graphs, &mut lookups.types)
            .expect("apply");
        black_box(
            cat.try_property_schema_for_graph_id(GraphId::from_raw(1))
                .expect("resolve")
                .is_none(),
        );
    })
}

/// `DROP GRAPH TYPE` walks all bindings to remove `TypeRef` dependents (cascade).
#[bench(raw)]
fn bench_catalog_drop_graph_type_cascade_32_graphs() -> canbench_rs::BenchResult {
    let setup = block_from(&ddl_one_type_and_n_typed_graphs(32));
    let drop = block_from("DROP GRAPH TYPE gt");
    canbench_rs::bench_fn(|| {
        let mut lookups = BenchLookups::new();
        lookups.ensure_graphs_in_block(&setup);
        let mut cat = catalog_new();
        cat.apply_statement_block(&setup, &lookups.graphs, &mut lookups.types)
            .expect("setup");
        let _scope = canbench_rs::bench_scope("catalog_drop_type");
        cat.apply_statement_block(black_box(&drop), &lookups.graphs, &mut lookups.types)
            .expect("drop");
        black_box(
            cat.try_property_schema_for_graph_id(GraphId::from_raw(1))
                .expect("post")
                .is_none(),
        );
    })
}
