//! Stable graph type catalog (CREATE GRAPH TYPE / CREATE GRAPH).
//!
//! This crate intentionally contains only pure catalog concerns: DDL application,
//! schema resolution, and persistence codecs. Planner/executor bridge helpers stay
//! in `gleaph-graph` for now.
//!
//! **Graph type binding:** `CREATE GRAPH g { ... }` is always an inline graph type (including
//! `{}` with no elements), so the catalog stores a binding and [`GraphCatalog::try_property_schema_for_graph`]
//! may return `Some` (possibly empty). For an open graph with no stored schema, use `CREATE GRAPH g ANY`
//! or omit a graph type per the parser (no `LIKE` / `TYPED` / `{` / `ANY` / type reference).

#[macro_use(concat_string)]
extern crate concat_string;

use derive_more::{AsRef, From, Into};
use gleaph_gql::ast::{
    CreateGraphStatement, CreateGraphTypeStatement, DropGraphStatement, DropGraphTypeStatement,
    GraphTypeDefinition, GraphTypeSpec, ObjectName, Statement, StatementBlock,
};
use gleaph_gql::type_check::GraphTypePropertySchema;
use ic_stable_structures::{
    Memory, StableBTreeMap,
    storable::{Bound, Storable},
};
use std::borrow::Cow;

#[cfg(feature = "canbench")]
mod bench;

type CatalogTypeKey = String;
type CatalogBindingKey = String;

/// Returns a single stable string key for [`ObjectName`] by joining [`ObjectName::parts`] with `.`.
///
/// Used for map keys so qualified names (e.g. `schema.gt`) round-trip consistently with simple names (`gt`).
pub fn object_name_key(name: &ObjectName) -> String {
    name.parts.join(".")
}

/// In-canister catalog: named graph type definitions and per-property-graph schema bindings.
///
/// - `type_map` holds definitions from `CREATE GRAPH TYPE` (keyed by [`object_name_key`]).
/// - `binding_map` holds each property graph’s binding: inline [`GraphTypeDefinition`] or a reference to a named type (`TYPED`).
///
/// `MT` / `MB` are separate [`Memory`] regions for [`StableBTreeMap`] (e.g. split stable memory slots on IC).
pub struct GraphCatalog<MT: Memory, MB: Memory> {
    type_map: StableBTreeMap<CatalogTypeKey, StorableGraphTypeDefinition, MT>,
    binding_map: StableBTreeMap<CatalogBindingKey, GraphSchemaBinding, MB>,
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
    pub fn apply_statement_block(&mut self, block: &StatementBlock) -> Result<(), CatalogError> {
        for stmt in block.iter_statements() {
            self.apply_statement(stmt.clone())?;
        }
        Ok(())
    }

    /// Dispatches a single top-level [`Statement`] to the corresponding catalog handler.
    fn apply_statement(&mut self, stmt: Statement) -> Result<(), CatalogError> {
        match stmt {
            Statement::CreateGraphType(c) => self.apply_create_graph_type(c),
            Statement::CreateGraph(c) => self.apply_create_graph(c),
            Statement::DropGraphType(d) => {
                self.apply_drop_graph_type(d)?;
                Ok(())
            }
            Statement::DropGraph(d) => {
                self.apply_drop_graph(d);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Inserts or replaces a named graph type; supports `IF NOT EXISTS` and `OR REPLACE`. `COPY OF` is rejected.
    fn apply_create_graph_type(&mut self, c: CreateGraphTypeStatement) -> Result<(), CatalogError> {
        if c.copy_of.is_some() {
            return Err(CatalogError::Unsupported(
                "CREATE GRAPH TYPE ... COPY OF is not supported yet".into(),
            ));
        }
        let key: CatalogTypeKey = object_name_key(&c.name);
        if c.if_not_exists && self.type_map.get(&key).is_some() {
            return Ok(());
        }
        if self.type_map.contains_key(&key) {
            if c.or_replace {
                self.type_map.insert(key, c.definition.into());
                return Ok(());
            }
            return Err(CatalogError::GraphTypeExists(key.to_string()));
        }
        self.type_map.insert(key, c.definition.into());
        Ok(())
    }

    /// Binds a property graph name to an inline definition, a named type (`TYPED`), `ANY`, or no stored binding. Rejects `LIKE` and `AS COPY OF`.
    fn apply_create_graph(&mut self, c: CreateGraphStatement) -> Result<(), CatalogError> {
        if c.copy_of.is_some() {
            return Err(CatalogError::Unsupported(
                "CREATE GRAPH ... AS COPY OF is not supported yet".into(),
            ));
        }
        let key: CatalogBindingKey = object_name_key(&c.name);
        if c.if_not_exists && self.binding_map.get(&key).is_some() {
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
                if self.type_map.get(&key).is_none() {
                    return Err(CatalogError::GraphTypeNotFound(key.to_string()));
                }
                Some(GraphSchemaBinding::TypeRef(key.to_string()))
            }
            Some(GraphTypeSpec::Inline(def)) => Some(GraphSchemaBinding::Inline(def.clone())),
        };

        if self.binding_map.contains_key(&key) {
            if c.or_replace {
                match binding {
                    Some(b) => {
                        self.binding_map.insert(key, b);
                    }
                    None => {
                        self.binding_map.remove(&key);
                    }
                };
                return Ok(());
            }
            return Err(CatalogError::Unsupported(concat_string!(
                "graph ",
                key.to_string(),
                " already exists (use OR REPLACE or IF NOT EXISTS)"
            )));
        }

        if let Some(b) = binding {
            self.binding_map.insert(key, b);
        }
        Ok(())
    }

    /// Removes a graph type and any property graphs that referenced it by `TYPED` ([`GraphSchemaBinding::TypeRef`]).
    fn apply_drop_graph_type(&mut self, d: DropGraphTypeStatement) -> Result<(), CatalogError> {
        let key: CatalogTypeKey = object_name_key(&d.name);
        self.type_map.remove(&key);

        let mut binding_keys_to_remove = Vec::new();
        for entry in self.binding_map.iter() {
            if let GraphSchemaBinding::TypeRef(t) = entry.value()
                && t == key
            {
                binding_keys_to_remove.push(entry.key().clone());
            }
        }
        for binding_key in binding_keys_to_remove {
            self.binding_map.remove(&binding_key);
        }
        Ok(())
    }

    /// Removes the binding for a property graph name only (does not delete named graph types).
    fn apply_drop_graph(&mut self, d: DropGraphStatement) {
        let key: CatalogBindingKey = object_name_key(&d.name);
        self.binding_map.remove(&key);
    }

    /// Returns the property-graph schema for planning when `graph_name` identifies the current property graph.
    ///
    /// - `None` or `Some("")` → [`None`] (no active graph).
    /// - Unknown graph name → [`None`] (no binding).
    /// - Inline or `TYPED` binding → [`Some`] schema, unless the definition fails validation ([`CatalogError::InvalidDefinition`])
    ///   or a `TypeRef` target is missing ([`CatalogError::GraphTypeNotFound`]).
    pub fn try_property_schema_for_graph(
        &self,
        graph_name: Option<&str>,
    ) -> Result<Option<GraphTypePropertySchema>, CatalogError> {
        let Some(g) = graph_name.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let Some(binding) = self.binding_map.get(&g.into()) else {
            return Ok(None);
        };
        let def = match binding {
            GraphSchemaBinding::TypeRef(k) => {
                let Some(value) = self.type_map.get(&k) else {
                    return Err(CatalogError::GraphTypeNotFound(k));
                };
                value.into()
            }
            GraphSchemaBinding::Inline(def) => def,
        };
        GraphTypePropertySchema::try_from_definition(&def)
            .map(Some)
            .map_err(CatalogError::InvalidDefinition)
    }
}

/// rkyv’s checked [`rkyv::from_bytes`] places the archived root at [`rkyv::api::root_position`]
/// (aligned); buffers read back from stable memory are plain [`Vec<u8>`] and may not satisfy
/// that alignment on wasm32, so we copy into [`rkyv::util::AlignedVec`] before decoding.
fn rkyv_from_bytes_aligned_graph_def(
    bytes: &[u8],
) -> Result<GraphTypeDefinition, rkyv::rancor::Error> {
    let mut aligned = rkyv::util::AlignedVec::<16>::new();
    aligned.extend_from_slice(bytes);
    rkyv::from_bytes::<GraphTypeDefinition, rkyv::rancor::Error>(&aligned)
}

/// Newtype for [`GraphTypeDefinition`] stored in [`StableBTreeMap`] (rkyv [`Storable`] payload).
#[derive(Clone, Debug, PartialEq, AsRef, From, Into)]
struct StorableGraphTypeDefinition(GraphTypeDefinition);

/// Encodes [`GraphTypeDefinition`] with rkyv (archived AST without spans).
impl Storable for StorableGraphTypeDefinition {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&self.0)
            .expect("graph type definition rkyv encode should not fail");
        Cow::Owned(bytes.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        rkyv::to_bytes::<rkyv::rancor::Error>(&self.0)
            .expect("graph type definition rkyv encode should not fail")
            .to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(
            rkyv_from_bytes_aligned_graph_def(&bytes[..])
                .expect("graph type definition rkyv decode should not fail"),
        )
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// How a property graph name resolves: either a shared named type or an inline definition.
#[derive(Clone, Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
enum GraphSchemaBinding {
    /// `CREATE GRAPH ... TYPED <name>` — key matches [`object_name_key`] of the graph type.
    TypeRef(String),
    /// `CREATE GRAPH ... { ... }` — graph-specific inline body.
    Inline(GraphTypeDefinition),
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
        c.apply_statement_block(&block).expect("first");
        let err = c.apply_statement_block(&block).expect_err("duplicate type");
        assert!(matches!(err, CatalogError::GraphTypeExists(ref k) if k == "gt"));
    }

    /// Second `CREATE GRAPH TYPE IF NOT EXISTS` for an existing type succeeds without error.
    #[test]
    fn create_graph_type_if_not_exists_skips() {
        let body = "NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person)";
        let mut c = catalog();
        c.apply_statement_block(&block_from(&format!("CREATE GRAPH TYPE gt {{ {body} }}")))
            .expect("first");
        c.apply_statement_block(&block_from(&format!(
            "CREATE GRAPH TYPE IF NOT EXISTS gt {{ {body} }}"
        )))
        .expect("if not exists");
    }

    /// `CREATE OR REPLACE GRAPH TYPE` updates the stored definition; a graph `TYPED` that name sees the new schema (here: undirected `KNOWS`).
    #[test]
    fn create_graph_type_or_replace_updates_definition() {
        let mut c = catalog();
        c.apply_statement_block(&block_from(
            "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }",
        ))
        .expect("create");
        c.apply_statement_block(&block_from(
            "CREATE OR REPLACE GRAPH TYPE gt { NODE Person LABEL Person, UNDIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person ~ Person) }",
        ))
        .expect("replace");
        let schema = c.try_property_schema_for_graph(Some("g")).expect("resolve");
        assert!(schema.is_none());
        c.apply_statement_block(&block_from("CREATE GRAPH g TYPED gt"))
            .expect("graph");
        let schema = c.try_property_schema_for_graph(Some("g")).expect("schema");
        let s = schema.expect("typed graph has schema");
        assert_eq!(s.edge_is_undirected("KNOWS"), Some(true));
    }

    /// `CREATE GRAPH ... TYPED <name>` fails with [`CatalogError::GraphTypeNotFound`] when the type was never created.
    #[test]
    fn create_graph_typed_missing_type_errors() {
        let mut c = catalog();
        let err = c
            .apply_statement_block(&block_from("CREATE GRAPH g TYPED missing"))
            .expect_err("missing type");
        assert!(matches!(
            err,
            CatalogError::GraphTypeNotFound(ref k) if k == "missing"
        ));
    }

    /// `CREATE GRAPH ... ANY` does not store a binding, so [`GraphCatalog::try_property_schema_for_graph`] returns [`None`].
    #[test]
    fn create_graph_any_has_no_property_schema() {
        let mut c = catalog();
        c.apply_statement_block(&block_from("CREATE GRAPH g ANY"))
            .expect("apply");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_none()
        );
    }

    /// `CREATE GRAPH g {}` is an inline graph type with zero elements; the catalog still records a binding and schema resolution returns [`Some`].
    #[test]
    fn create_graph_empty_inline_body_still_binds() {
        let mut c = catalog();
        c.apply_statement_block(&block_from("CREATE GRAPH g {}"))
            .expect("apply");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_some()
        );
    }

    /// After `CREATE GRAPH TYPE` and `CREATE GRAPH ... TYPED`, [`GraphCatalog::try_property_schema_for_graph`] returns a schema for that graph.
    #[test]
    fn create_graph_typed_resolves_schema() {
        let mut c = catalog();
        let ddl = "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) } NEXT CREATE GRAPH g TYPED gt";
        c.apply_statement_block(&block_from(ddl)).expect("apply");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_some()
        );
    }

    /// `DROP GRAPH` removes the graph binding; subsequent schema lookup for that name returns [`None`].
    #[test]
    fn drop_graph_removes_binding() {
        let mut c = catalog();
        c.apply_statement_block(&block_from(
            "CREATE GRAPH g { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }",
        ))
        .expect("create");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_some()
        );
        c.apply_statement_block(&block_from("DROP GRAPH g"))
            .expect("drop");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_none()
        );
    }

    /// `DROP GRAPH TYPE` deletes the type and removes graphs that referenced it via `TYPED` (TypeRef cascade).
    #[test]
    fn drop_graph_type_removes_type_and_type_ref_bindings() {
        let mut c = catalog();
        c.apply_statement_block(&block_from(
            "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) } NEXT CREATE GRAPH g TYPED gt",
        ))
        .expect("setup");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_some()
        );
        c.apply_statement_block(&block_from("DROP GRAPH TYPE gt"))
            .expect("drop type");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_none()
        );
    }

    /// `CREATE GRAPH TYPE ... COPY OF` is rejected with [`CatalogError::Unsupported`].
    #[test]
    fn unsupported_graph_type_copy_of() {
        let mut c = catalog();
        let err = c
            .apply_statement_block(&block_from(
                "CREATE GRAPH TYPE gt COPY OF other { NODE Person LABEL Person }",
            ))
            .expect_err("copy of");
        assert!(matches!(err, CatalogError::Unsupported(_)));
    }

    /// `CREATE GRAPH ... LIKE` is rejected with [`CatalogError::Unsupported`].
    #[test]
    fn unsupported_create_graph_like() {
        let mut c = catalog();
        let err = c
            .apply_statement_block(&block_from("CREATE GRAPH g LIKE other"))
            .expect_err("like");
        assert!(matches!(err, CatalogError::Unsupported(_)));
    }

    /// `CREATE GRAPH ... AS COPY OF` is rejected with [`CatalogError::Unsupported`].
    #[test]
    fn unsupported_create_graph_as_copy_of() {
        let mut c = catalog();
        let err = c
            .apply_statement_block(&block_from("CREATE GRAPH g {} AS COPY OF other"))
            .expect_err("copy of graph");
        assert!(matches!(err, CatalogError::Unsupported(_)));
    }

    /// Creating the same property graph twice (with a stored binding) without `OR REPLACE` / `IF NOT EXISTS` yields [`CatalogError::Unsupported`].
    #[test]
    fn duplicate_graph_errors() {
        let mut c = catalog();
        let setup = "CREATE GRAPH g { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }";
        c.apply_statement_block(&block_from(setup)).expect("first");
        let err = c
            .apply_statement_block(&block_from(setup))
            .expect_err("dup");
        assert!(matches!(err, CatalogError::Unsupported(ref m) if m.contains("already exists")));
    }

    /// Statements outside catalog DDL (e.g. `CREATE SCHEMA`) are ignored and do not fail [`GraphCatalog::apply_statement_block`].
    #[test]
    fn non_catalog_statement_is_ignored() {
        let mut c = catalog();
        c.apply_statement_block(&block_from(
            "CREATE GRAPH TYPE gt { NODE Person LABEL Person } NEXT CREATE SCHEMA /x",
        ))
        .expect("mixed block");
    }

    /// [`GraphCatalog::try_property_schema_for_graph`] returns [`None`] when the active graph is unset or the name is empty.
    #[test]
    fn try_property_schema_empty_or_none_graph_name() {
        let mut c = catalog();
        c.apply_statement_block(&block_from(
            "CREATE GRAPH g { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }",
        ))
        .expect("create");
        assert!(c.try_property_schema_for_graph(None).expect("ok").is_none());
        assert!(
            c.try_property_schema_for_graph(Some(""))
                .expect("ok")
                .is_none()
        );
    }

    /// `CREATE OR REPLACE GRAPH ... ANY` replaces a typed binding with an open graph: schema resolution becomes [`None`].
    #[test]
    fn create_or_replace_graph_switches_binding() {
        let mut c = catalog();
        c.apply_statement_block(&block_from(
            "CREATE GRAPH TYPE gt { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) } NEXT CREATE GRAPH g TYPED gt",
        ))
        .expect("typed");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_some()
        );
        c.apply_statement_block(&block_from("CREATE OR REPLACE GRAPH g ANY"))
            .expect("replace with any");
        assert!(
            c.try_property_schema_for_graph(Some("g"))
                .expect("ok")
                .is_none()
        );
    }

    /// Conflicting directedness for the same edge label in an inline type surfaces as [`CatalogError::InvalidDefinition`] when resolving the schema.
    #[test]
    fn invalid_definition_on_property_schema_resolution() {
        let mut c = catalog();
        let ddl = "CREATE GRAPH g { NODE A LABEL A, NODE B LABEL B, DIRECTED EDGE E1 LABELS R CONNECTING (A -> B), UNDIRECTED EDGE E2 LABELS R CONNECTING (A ~ B) }";
        c.apply_statement_block(&block_from(ddl)).expect("apply");
        let err = c
            .try_property_schema_for_graph(Some("g"))
            .expect_err("invalid schema");
        assert!(matches!(err, CatalogError::InvalidDefinition(_)));
    }
}
