//! Stable graph type catalog (CREATE GRAPH TYPE / CREATE GRAPH).
//!
//! This crate intentionally contains only pure catalog concerns: DDL application,
//! schema resolution, and persistence codecs. Planner/executor bridge helpers stay
//! in `gleaph-graph` for now.
//!
//! **Graph type binding:** `CREATE GRAPH g { ... }` stores a binding at the resolved
//! [`GraphId`] (see [`GraphNameLookup`]). [`GraphCatalog::try_property_schema_for_graph_id`]
//! may return `Some` (possibly empty). For an open graph with no stored schema, use
//! `CREATE GRAPH g ANY` or omit a graph type per the parser.

#[macro_use(concat_string)]
extern crate concat_string;

use gleaph_gql::ast::{
    CreateGraphStatement, CreateGraphTypeStatement, DropGraphStatement, DropGraphTypeStatement,
    GraphTypeDefinition, GraphTypeSpec, ObjectName, Statement, StatementBlock,
};
use gleaph_gql::type_check::GraphTypePropertySchema;
use gleaph_graph_kernel::entry::{GraphId, GraphTypeId};
use ic_stable_structures::{
    Memory, StableBTreeMap,
    storable::{Bound, Storable},
};
use std::borrow::Cow;

#[cfg(feature = "canbench")]
mod bench;

type CatalogTypeKey = String;

/// Resolves a property graph name from catalog DDL to a federation [`GraphId`].
pub trait GraphNameLookup {
    fn lookup_graph_id(&self, graph_name: &str) -> Option<GraphId>;
}

/// Resolves and interns GQL graph **type** names to [`GraphTypeId`] (ADR 0014).
pub trait GraphTypeLookup {
    fn lookup_graph_type_id(&self, type_name: &str) -> Option<GraphTypeId>;

    fn intern_graph_type_id(&mut self, type_name: &str) -> Result<GraphTypeId, CatalogError>;

    fn remove_graph_type_by_name(&mut self, type_name: &str) -> Option<GraphTypeId>;
}

fn resolve_graph_id_for_name(
    name: &ObjectName,
    lookup: &impl GraphNameLookup,
) -> Result<GraphId, CatalogError> {
    let key = object_name_key(name);
    lookup
        .lookup_graph_id(&key)
        .ok_or(CatalogError::GraphNotRegistered(key))
}

/// Returns a single stable string key for [`ObjectName`] by joining [`ObjectName::parts`] with `.`.
///
/// Used for map keys so qualified names (e.g. `schema.gt`) round-trip consistently with simple names (`gt`).
pub fn object_name_key(name: &ObjectName) -> String {
    name.parts.join(".")
}

/// In-canister catalog: named graph type definitions and per-property-graph schema bindings.
///
/// - `type_map` holds definitions from `CREATE GRAPH TYPE` (keyed by [`GraphTypeId`]).
/// - `binding_map` holds each property graph’s binding at **`GraphId`**: inline [`GraphTypeDefinition`] or a reference to a named type (`TYPED`).
pub struct GraphCatalog<MT: Memory, MB: Memory> {
    type_map: StableBTreeMap<GraphTypeId, StorableGraphTypeDefinition, MT>,
    binding_map: StableBTreeMap<GraphId, GraphSchemaBinding, MB>,
}

impl<MT: Memory, MB: Memory> std::fmt::Debug for GraphCatalog<MT, MB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphCatalog")
            .field("graph_type_count", &self.type_map.len())
            .field("graph_binding_count", &self.binding_map.len())
            .finish()
    }
}

/// Failure applying catalog DDL or resolving a graph type for planning.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// `CREATE GRAPH TYPE` for a name that already exists (without `OR REPLACE`).
    #[error("graph type `{0}` already exists")]
    GraphTypeExists(String),
    /// Named graph type missing, or `TYPED` reference points to a removed type.
    #[error("graph type `{0}` not found")]
    GraphTypeNotFound(String),
    /// Property graph name is not registered in the federation graph name catalog.
    #[error("graph `{0}` is not registered")]
    GraphNotRegistered(String),
    /// DDL construct not implemented in this catalog (e.g. `COPY OF`, `LIKE`) or duplicate graph without `OR REPLACE` / `IF NOT EXISTS`.
    #[error("unsupported catalog DDL: {0}")]
    Unsupported(String),
    /// [`GraphTypePropertySchema::try_from_definition`] rejected the stored [`GraphTypeDefinition`] (e.g. conflicting edge directedness).
    #[error("graph type definition invalid: {0}")]
    InvalidDefinition(String),
}

impl<MT: Memory, MB: Memory> GraphCatalog<MT, MB> {
    /// Builds an empty catalog using the given stable memories for type definitions and graph bindings.
    pub fn init(type_memory: MT, binding_memory: MB) -> Self {
        Self {
            type_map: StableBTreeMap::init(type_memory),
            binding_map: StableBTreeMap::init(binding_memory),
        }
    }

    /// Applies every catalog-related statement in `block` in order (the block’s first statement plus each `NEXT` statement).
    ///
    /// Handles `CREATE` / `DROP` for graph types and property graphs. Other [`Statement`] variants are ignored.
    pub fn apply_statement_block(
        &mut self,
        block: &StatementBlock,
        graph_lookup: &impl GraphNameLookup,
        type_lookup: &mut impl GraphTypeLookup,
    ) -> Result<(), CatalogError> {
        for stmt in block.iter_statements() {
            self.apply_statement(stmt, graph_lookup, type_lookup)?;
        }
        Ok(())
    }

    fn apply_statement(
        &mut self,
        stmt: &Statement,
        graph_lookup: &impl GraphNameLookup,
        type_lookup: &mut impl GraphTypeLookup,
    ) -> Result<(), CatalogError> {
        match stmt {
            Statement::CreateGraphType(c) => self.apply_create_graph_type(c, type_lookup),
            Statement::CreateGraph(c) => self.apply_create_graph(c, graph_lookup, type_lookup),
            Statement::DropGraphType(d) => {
                self.apply_drop_graph_type(d, type_lookup)?;
                Ok(())
            }
            Statement::DropGraph(d) => {
                self.apply_drop_graph(d, graph_lookup);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Inserts or replaces a named graph type; supports `IF NOT EXISTS` and `OR REPLACE`. `COPY OF` is rejected.
    fn apply_create_graph_type(
        &mut self,
        c: &CreateGraphTypeStatement,
        type_lookup: &mut impl GraphTypeLookup,
    ) -> Result<(), CatalogError> {
        if c.copy_of.is_some() {
            return Err(CatalogError::Unsupported(
                "CREATE GRAPH TYPE ... COPY OF is not supported yet".into(),
            ));
        }
        let key: CatalogTypeKey = object_name_key(&c.name);
        if c.if_not_exists && type_lookup.lookup_graph_type_id(&key).is_some() {
            return Ok(());
        }
        if let Some(type_id) = type_lookup.lookup_graph_type_id(&key) {
            if c.or_replace {
                self.type_map.insert(type_id, c.definition.clone().into());
                return Ok(());
            }
            return Err(CatalogError::GraphTypeExists(key));
        }
        if c.if_not_exists {
            return Ok(());
        }
        let type_id = type_lookup.intern_graph_type_id(&key)?;
        self.type_map.insert(type_id, c.definition.clone().into());
        Ok(())
    }

    /// Binds a property graph name to an inline definition, a named type (`TYPED`), `ANY`, or no stored binding. Rejects `LIKE` and `AS COPY OF`.
    fn apply_create_graph(
        &mut self,
        c: &CreateGraphStatement,
        graph_lookup: &impl GraphNameLookup,
        type_lookup: &impl GraphTypeLookup,
    ) -> Result<(), CatalogError> {
        if c.copy_of.is_some() {
            return Err(CatalogError::Unsupported(
                "CREATE GRAPH ... AS COPY OF is not supported yet".into(),
            ));
        }
        let graph_name = object_name_key(&c.name);
        let graph_id = resolve_graph_id_for_name(&c.name, graph_lookup)?;
        if c.if_not_exists && self.binding_map.get(&graph_id).is_some() {
            return Ok(());
        }
        let binding = match &c.graph_type {
            None => None,
            Some(GraphTypeSpec::Any { .. }) => None,
            Some(GraphTypeSpec::Like(_)) => {
                return Err(CatalogError::Unsupported(
                    "CREATE GRAPH ... LIKE is not supported yet".into(),
                ));
            }
            Some(GraphTypeSpec::Typed { name, .. }) => {
                let key: CatalogTypeKey = object_name_key(name);
                let type_id = type_lookup
                    .lookup_graph_type_id(&key)
                    .ok_or(CatalogError::GraphTypeNotFound(key))?;
                Some(GraphSchemaBinding::type_ref(type_id))
            }
            Some(GraphTypeSpec::Inline(def)) => Some(GraphSchemaBinding::inline(def.clone())),
        };

        if self.binding_map.contains_key(&graph_id) {
            if c.or_replace {
                match binding {
                    Some(b) => {
                        self.binding_map.insert(graph_id, b);
                    }
                    None => {
                        self.binding_map.remove(&graph_id);
                    }
                };
                return Ok(());
            }
            return Err(CatalogError::Unsupported(concat_string!(
                "graph ",
                graph_name,
                " already exists (use OR REPLACE or IF NOT EXISTS)"
            )));
        }

        if let Some(b) = binding {
            self.binding_map.insert(graph_id, b);
        }
        Ok(())
    }

    /// Removes a graph type and any property graphs that referenced it by `TYPED`.
    fn apply_drop_graph_type(
        &mut self,
        d: &DropGraphTypeStatement,
        type_lookup: &mut impl GraphTypeLookup,
    ) -> Result<(), CatalogError> {
        let key: CatalogTypeKey = object_name_key(&d.name);
        let Some(type_id) = type_lookup.remove_graph_type_by_name(&key) else {
            return Ok(());
        };
        self.type_map.remove(&type_id);

        let mut binding_keys_to_remove = Vec::new();
        for entry in self.binding_map.iter() {
            if binding_type_ref_id(&entry.value()) == Some(type_id) {
                binding_keys_to_remove.push(entry.key().clone());
            }
        }
        for binding_key in binding_keys_to_remove {
            self.binding_map.remove(&binding_key);
        }
        Ok(())
    }

    fn apply_drop_graph(&mut self, d: &DropGraphStatement, lookup: &impl GraphNameLookup) {
        if let Ok(graph_id) = resolve_graph_id_for_name(&d.name, lookup) {
            self.binding_map.remove(&graph_id);
        }
    }

    /// Returns the property-graph schema for planning when `graph_id` identifies the logical graph.
    ///
    /// - Reserved / unknown `GraphId` → [`None`] (no binding).
    /// - Inline or `TYPED` binding → [`Some`] schema, unless the definition fails validation
    ///   ([`CatalogError::InvalidDefinition`]) or a `TypeRef` target is missing
    ///   ([`CatalogError::GraphTypeNotFound`]).
    pub fn try_property_schema_for_graph_id(
        &self,
        graph_id: GraphId,
    ) -> Result<Option<GraphTypePropertySchema>, CatalogError> {
        if graph_id.is_reserved() {
            return Ok(None);
        }
        let Some(binding) = self.binding_map.get(&graph_id) else {
            return Ok(None);
        };
        let def = self.definition_for_binding(&binding)?;
        GraphTypePropertySchema::try_from_definition(&def)
            .map(Some)
            .map_err(CatalogError::InvalidDefinition)
    }

    fn definition_for_binding(
        &self,
        binding: &GraphSchemaBinding,
    ) -> Result<GraphTypeDefinition, CatalogError> {
        match binding {
            GraphSchemaBinding::V2(v2) => match v2 {
                GraphSchemaBindingV2::TypeRef(raw) => {
                    let type_id = GraphTypeId::from_raw(*raw);
                    let Some(value) = self.type_map.get(&type_id) else {
                        return Err(CatalogError::GraphTypeNotFound(type_id.to_string()));
                    };
                    Ok(value.into())
                }
                GraphSchemaBindingV2::Inline(def) => Ok(def.clone()),
            },
            GraphSchemaBinding::V1(v1) => match v1 {
                GraphSchemaBindingV1::Inline(def) => Ok(def.clone()),
                GraphSchemaBindingV1::TypeRef(name) => Err(CatalogError::Unsupported(format!(
                    "legacy graph schema TypeRef `{name}` requires catalog migration (ADR 0014)"
                ))),
            },
        }
    }
}

/// Version 1 graph schema binding payload (legacy string [`TypeRef`]).
#[derive(Clone, Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
enum GraphSchemaBindingV1 {
    /// Legacy `CREATE GRAPH ... TYPED` — graph type name string (ADR 0013).
    TypeRef(String),
    Inline(GraphTypeDefinition),
}

/// Version 2 graph schema binding payload (ADR 0014).
#[derive(Clone, Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
enum GraphSchemaBindingV2 {
    /// `CREATE GRAPH ... TYPED <name>` — resolved [`GraphTypeId`] (`raw()` in stable storage).
    TypeRef(u32),
    Inline(GraphTypeDefinition),
}

/// Versioned graph schema binding for stable storage and upgrade-safe evolution.
#[derive(Clone, Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
enum GraphSchemaBinding {
    V1(GraphSchemaBindingV1),
    V2(GraphSchemaBindingV2),
}

impl GraphSchemaBinding {
    fn type_ref(type_id: GraphTypeId) -> Self {
        Self::V2(GraphSchemaBindingV2::TypeRef(type_id.raw()))
    }

    fn inline(def: GraphTypeDefinition) -> Self {
        Self::V2(GraphSchemaBindingV2::Inline(def))
    }
}

fn binding_type_ref_id(binding: &GraphSchemaBinding) -> Option<GraphTypeId> {
    match binding {
        GraphSchemaBinding::V2(GraphSchemaBindingV2::TypeRef(raw)) => {
            Some(GraphTypeId::from_raw(*raw))
        }
        GraphSchemaBinding::V1(_) | GraphSchemaBinding::V2(GraphSchemaBindingV2::Inline(_)) => None,
    }
}

/// Versioned graph type definition stored in [`StableBTreeMap`] (rkyv [`Storable`] payload).
#[derive(Clone, Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
enum StorableGraphTypeDefinition {
    V1(GraphTypeDefinition),
}

impl From<GraphTypeDefinition> for StorableGraphTypeDefinition {
    fn from(def: GraphTypeDefinition) -> Self {
        Self::V1(def)
    }
}

impl From<StorableGraphTypeDefinition> for GraphTypeDefinition {
    fn from(value: StorableGraphTypeDefinition) -> Self {
        match value {
            StorableGraphTypeDefinition::V1(def) => def,
        }
    }
}

/// Encodes [`GraphTypeDefinition`] with rkyv (archived AST without spans).
impl Storable for StorableGraphTypeDefinition {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .expect("graph type definition rkyv encode should not fail");
        Cow::Owned(bytes.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        rkyv::to_bytes::<rkyv::rancor::Error>(&self)
            .expect("graph type definition rkyv encode should not fail")
            .to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut aligned = rkyv::util::AlignedVec::<16>::new();
        aligned.extend_from_slice(&bytes);
        rkyv::from_bytes::<Self, rkyv::rancor::Error>(&aligned)
            .expect("graph type definition rkyv decode should not fail")
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// Encodes [`GraphSchemaBinding`] with rkyv for stable storage.
impl Storable for GraphSchemaBinding {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(self)
            .expect("graph type definition rkyv encode should not fail");
        Cow::Owned(bytes.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        rkyv::to_bytes::<rkyv::rancor::Error>(&self)
            .expect("graph type definition rkyv encode should not fail")
            .to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut aligned = rkyv::util::AlignedVec::<16>::new();
        aligned.extend_from_slice(&bytes);
        rkyv::from_bytes::<Self, rkyv::rancor::Error>(&aligned)
            .expect("graph schema binding rkyv decode should not fail")
    }

    const BOUND: Bound = Bound::Unbounded;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::{ast::ObjectName, parser, type_check::PropertySchema};
    use ic_stable_structures::VectorMemory;
    use std::collections::BTreeMap;

    const G: GraphId = GraphId::from_raw(1);

    struct TestGraphLookup(BTreeMap<String, GraphId>);

    impl TestGraphLookup {
        fn with_graphs(names: &[(&str, u32)]) -> Self {
            Self(
                names
                    .iter()
                    .map(|(name, id)| (name.to_string(), GraphId::from_raw(*id)))
                    .collect(),
            )
        }
    }

    impl Default for TestGraphLookup {
        fn default() -> Self {
            Self(BTreeMap::new())
        }
    }

    impl GraphNameLookup for TestGraphLookup {
        fn lookup_graph_id(&self, graph_name: &str) -> Option<GraphId> {
            self.0.get(graph_name).copied()
        }
    }

    struct TestGraphTypeLookup {
        names: BTreeMap<String, GraphTypeId>,
        next: u32,
    }

    impl Default for TestGraphTypeLookup {
        fn default() -> Self {
            Self {
                names: BTreeMap::new(),
                next: 1,
            }
        }
    }

    impl GraphTypeLookup for TestGraphTypeLookup {
        fn lookup_graph_type_id(&self, type_name: &str) -> Option<GraphTypeId> {
            self.names.get(type_name).copied()
        }

        fn intern_graph_type_id(&mut self, type_name: &str) -> Result<GraphTypeId, CatalogError> {
            if let Some(id) = self.names.get(type_name) {
                return Ok(*id);
            }
            let id = GraphTypeId::from_raw(self.next);
            self.next += 1;
            self.names.insert(type_name.to_string(), id);
            Ok(id)
        }

        fn remove_graph_type_by_name(&mut self, type_name: &str) -> Option<GraphTypeId> {
            self.names.remove(type_name)
        }
    }

    struct TestLookups {
        graphs: TestGraphLookup,
        types: TestGraphTypeLookup,
    }

    impl TestLookups {
        fn with_graphs(names: &[(&str, u32)]) -> Self {
            Self {
                graphs: TestGraphLookup::with_graphs(names),
                types: TestGraphTypeLookup::default(),
            }
        }

        fn default() -> Self {
            Self {
                graphs: TestGraphLookup::default(),
                types: TestGraphTypeLookup::default(),
            }
        }
    }

    fn apply_block(
        catalog: &mut GraphCatalog<VectorMemory, VectorMemory>,
        block: &StatementBlock,
        lookups: &mut TestLookups,
    ) {
        catalog
            .apply_statement_block(block, &lookups.graphs, &mut lookups.types)
            .expect("apply");
    }

    /// Empty catalog backed by in-memory [`VectorMemory`] for unit tests.
    fn catalog() -> GraphCatalog<VectorMemory, VectorMemory> {
        GraphCatalog::init(VectorMemory::default(), VectorMemory::default())
    }

    /// Parses `gql` as a full program and returns the transaction [`StatementBlock`] body.
    fn block_from(gql: &str) -> StatementBlock {
        let program = parser::parse(gql).expect("parse");
        program
            .transaction_activity
            .expect("tx")
            .body
            .expect("body")
    }

    /// [`object_name_key`] joins [`ObjectName`] segments with `.` (e.g. schema-qualified names).
    #[test]
    fn object_name_key_joins_parts() {
        let name = ObjectName::qualified(vec!["a".into(), "b".into()]);
        assert_eq!(object_name_key(&name), "a.b");
    }

    /// Applying the same `CREATE GRAPH TYPE` twice without `IF NOT EXISTS` or `OR REPLACE` yields [`CatalogError::GraphTypeExists`].
    #[test]
    fn create_graph_type_duplicate_errors() {
        let ddl = "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }";
        let block = block_from(ddl);
        let mut c = catalog();
        let mut lookups = TestLookups::default();
        apply_block(&mut c, &block, &mut lookups);
        let err = c
            .apply_statement_block(&block, &lookups.graphs, &mut lookups.types)
            .expect_err("duplicate type");
        assert!(matches!(err, CatalogError::GraphTypeExists(ref k) if k == "gt"));
    }

    /// Second `CREATE GRAPH TYPE IF NOT EXISTS` for an existing type succeeds without error.
    #[test]
    fn create_graph_type_if_not_exists_skips() {
        let body = "NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person)";
        let mut c = catalog();
        let mut lookups = TestLookups::default();
        apply_block(
            &mut c,
            &block_from(&format!("CREATE GRAPH TYPE gt {{ {body} }}")),
            &mut lookups,
        );
        apply_block(
            &mut c,
            &block_from(&format!("CREATE GRAPH TYPE IF NOT EXISTS gt {{ {body} }}")),
            &mut lookups,
        );
    }

    /// `CREATE OR REPLACE GRAPH TYPE` updates the stored definition; a graph `TYPED` that name sees the new schema (here: undirected `KNOWS`).
    #[test]
    fn create_graph_type_or_replace_updates_definition() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        apply_block(
            &mut c,
            &block_from(
                "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }",
            ),
            &mut lookups,
        );
        apply_block(
            &mut c,
            &block_from(
                "CREATE OR REPLACE GRAPH TYPE gt { NODE Person LABEL Person, UNDIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person ~ Person) }",
            ),
            &mut lookups,
        );
        assert!(
            c.try_property_schema_for_graph_id(G)
                .expect("resolve")
                .is_none()
        );
        apply_block(&mut c, &block_from("CREATE GRAPH g TYPED gt"), &mut lookups);
        let schema = c
            .try_property_schema_for_graph_id(G)
            .expect("schema")
            .expect("typed graph has schema");
        assert_eq!(schema.edge_is_undirected("KNOWS"), Some(true));
    }

    /// `CREATE GRAPH ... TYPED <name>` fails with [`CatalogError::GraphTypeNotFound`] when the type was never created.
    #[test]
    fn create_graph_typed_missing_type_errors() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        let err = c
            .apply_statement_block(
                &block_from("CREATE GRAPH g TYPED missing"),
                &lookups.graphs,
                &mut lookups.types,
            )
            .expect_err("missing type");
        assert!(matches!(
            err,
            CatalogError::GraphTypeNotFound(ref k) if k == "missing"
        ));
    }

    /// `CREATE GRAPH` without a registered federation graph id fails with [`CatalogError::GraphNotRegistered`].
    #[test]
    fn create_graph_unregistered_name_errors() {
        let mut c = catalog();
        let mut lookups = TestLookups::default();
        let err = c
            .apply_statement_block(
                &block_from("CREATE GRAPH g {}"),
                &lookups.graphs,
                &mut lookups.types,
            )
            .expect_err("unregistered");
        assert!(matches!(err, CatalogError::GraphNotRegistered(ref n) if n == "g"));
    }

    /// `CREATE GRAPH ... ANY` does not store a binding, so [`GraphCatalog::try_property_schema_for_graph_id`] returns [`None`].
    #[test]
    fn create_graph_any_has_no_property_schema() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        apply_block(&mut c, &block_from("CREATE GRAPH g ANY"), &mut lookups);
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_none());
    }

    /// `CREATE GRAPH g {}` is an inline graph type with zero elements; the catalog still records a binding and schema resolution returns [`Some`].
    #[test]
    fn create_graph_empty_inline_body_still_binds() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        apply_block(&mut c, &block_from("CREATE GRAPH g {}"), &mut lookups);
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_some());
    }

    /// After `CREATE GRAPH TYPE` and `CREATE GRAPH ... TYPED`, [`GraphCatalog::try_property_schema_for_graph_id`] returns a schema for that graph.
    #[test]
    fn create_graph_typed_resolves_schema() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        let ddl = "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) } NEXT CREATE GRAPH g TYPED gt";
        apply_block(&mut c, &block_from(ddl), &mut lookups);
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_some());
    }

    /// `DROP GRAPH` removes the graph binding; subsequent schema lookup for that `GraphId` returns [`None`].
    #[test]
    fn drop_graph_removes_binding() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        apply_block(
            &mut c,
            &block_from(
                "CREATE GRAPH g { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }",
            ),
            &mut lookups,
        );
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_some());
        apply_block(&mut c, &block_from("DROP GRAPH g"), &mut lookups);
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_none());
    }

    /// `DROP GRAPH TYPE` deletes the type and removes graphs that referenced it via `TYPED` (TypeRef cascade).
    #[test]
    fn drop_graph_type_removes_type_and_type_ref_bindings() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        apply_block(
            &mut c,
            &block_from(
                "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) } NEXT CREATE GRAPH g TYPED gt",
            ),
            &mut lookups,
        );
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_some());
        apply_block(&mut c, &block_from("DROP GRAPH TYPE gt"), &mut lookups);
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_none());
    }

    /// `CREATE GRAPH TYPE ... COPY OF` is rejected with [`CatalogError::Unsupported`].
    #[test]
    fn unsupported_graph_type_copy_of() {
        let mut c = catalog();
        let mut lookups = TestLookups::default();
        let err = c
            .apply_statement_block(
                &block_from("CREATE GRAPH TYPE gt COPY OF other { NODE Person LABEL Person }"),
                &lookups.graphs,
                &mut lookups.types,
            )
            .expect_err("copy of");
        assert!(matches!(err, CatalogError::Unsupported(_)));
    }

    /// `CREATE GRAPH ... LIKE` is rejected with [`CatalogError::Unsupported`].
    #[test]
    fn unsupported_create_graph_like() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        let err = c
            .apply_statement_block(
                &block_from("CREATE GRAPH g LIKE other"),
                &lookups.graphs,
                &mut lookups.types,
            )
            .expect_err("like");
        assert!(matches!(err, CatalogError::Unsupported(_)));
    }

    /// `CREATE GRAPH ... AS COPY OF` is rejected with [`CatalogError::Unsupported`].
    #[test]
    fn unsupported_create_graph_as_copy_of() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        let err = c
            .apply_statement_block(
                &block_from("CREATE GRAPH g {} AS COPY OF other"),
                &lookups.graphs,
                &mut lookups.types,
            )
            .expect_err("copy of graph");
        assert!(matches!(err, CatalogError::Unsupported(_)));
    }

    /// Creating the same property graph twice (with a stored binding) without `OR REPLACE` / `IF NOT EXISTS` yields [`CatalogError::Unsupported`].
    #[test]
    fn duplicate_graph_errors() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        let setup = "CREATE GRAPH g { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }";
        apply_block(&mut c, &block_from(setup), &mut lookups);
        let err = c
            .apply_statement_block(&block_from(setup), &lookups.graphs, &mut lookups.types)
            .expect_err("dup");
        assert!(matches!(err, CatalogError::Unsupported(ref m) if m.contains("already exists")));
    }

    /// Statements outside catalog DDL (e.g. `CREATE SCHEMA`) are ignored and do not fail [`GraphCatalog::apply_statement_block`].
    #[test]
    fn non_catalog_statement_is_ignored() {
        let mut c = catalog();
        let mut lookups = TestLookups::default();
        apply_block(
            &mut c,
            &block_from("CREATE GRAPH TYPE gt { NODE Person LABEL Person } NEXT CREATE SCHEMA /x"),
            &mut lookups,
        );
    }

    /// [`GraphCatalog::try_property_schema_for_graph_id`] returns [`None`] when no binding exists for the id.
    #[test]
    fn try_property_schema_missing_binding_returns_none() {
        let c = catalog();
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_none());
        assert!(
            c.try_property_schema_for_graph_id(GraphId::from_raw(0))
                .expect("ok")
                .is_none()
        );
    }

    /// `CREATE OR REPLACE GRAPH ... ANY` replaces a typed binding with an open graph: schema resolution becomes [`None`].
    #[test]
    fn create_or_replace_graph_switches_binding() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        apply_block(
            &mut c,
            &block_from(
                "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) } NEXT CREATE GRAPH g TYPED gt",
            ),
            &mut lookups,
        );
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_some());
        apply_block(
            &mut c,
            &block_from("CREATE OR REPLACE GRAPH g ANY"),
            &mut lookups,
        );
        assert!(c.try_property_schema_for_graph_id(G).expect("ok").is_none());
    }

    /// Conflicting directedness for the same edge label in an inline type surfaces as [`CatalogError::InvalidDefinition`] when resolving the schema.
    #[test]
    fn invalid_definition_on_property_schema_resolution() {
        let mut c = catalog();
        let mut lookups = TestLookups::with_graphs(&[("g", 1)]);
        let ddl = "CREATE GRAPH g { NODE A LABEL A, NODE B LABEL B, DIRECTED EDGE E1 LABELS R CONNECTING (A -> B), UNDIRECTED EDGE E2 LABELS R CONNECTING (A ~ B) }";
        apply_block(&mut c, &block_from(ddl), &mut lookups);
        let err = c
            .try_property_schema_for_graph_id(G)
            .expect_err("invalid schema");
        assert!(matches!(err, CatalogError::InvalidDefinition(_)));
    }

    #[test]
    fn versioned_catalog_records_round_trip_through_storable() {
        let block = block_from("CREATE GRAPH TYPE gt { NODE Person LABEL Person }");
        let stmt = block.iter_statements().next().expect("stmt");
        let Statement::CreateGraphType(c) = stmt else {
            panic!("expected create graph type");
        };
        let def = c.definition.clone();

        let type_record = StorableGraphTypeDefinition::from(def.clone());
        let decoded_def: GraphTypeDefinition =
            StorableGraphTypeDefinition::from_bytes(Cow::Owned(type_record.into_bytes())).into();
        GraphTypePropertySchema::try_from_definition(&def).expect("original schema");
        GraphTypePropertySchema::try_from_definition(&decoded_def).expect("decoded schema");

        let binding = GraphSchemaBinding::inline(def);
        let decoded_binding =
            GraphSchemaBinding::from_bytes(Cow::Owned(binding.clone().into_bytes()));
        match decoded_binding {
            GraphSchemaBinding::V2(GraphSchemaBindingV2::Inline(decoded_def)) => {
                GraphTypePropertySchema::try_from_definition(&decoded_def)
                    .expect("decoded binding schema");
            }
            GraphSchemaBinding::V1(_)
            | GraphSchemaBinding::V2(GraphSchemaBindingV2::TypeRef(_)) => {
                panic!("expected inline V2 binding")
            }
        }
    }
}
