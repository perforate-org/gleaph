//! In-memory graph type catalog (CREATE GRAPH TYPE / CREATE GRAPH) and persistence helpers.
//!
//! Resolved [`gleaph_gql::type_check::GraphTypePropertySchema`] is used at plan time via
//! [`build_block_plan_output_with_schema`](gleaph_gql_planner::build_block_plan_output_with_schema).

use gleaph_gql::ast::{
    CreateGraphStatement, CreateGraphTypeStatement, DropGraphStatement, DropGraphTypeStatement,
    GraphTypeDefinition, GraphTypeSpec, ObjectName, Statement, StatementBlock,
};
use gleaph_gql::type_check::{GraphTypePropertySchema, NoSchema, PropertySchema};
use ic_stable_structures::storable::Bound;
use ic_stable_structures::{StableBTreeMap, Storable, VectorMemory};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

fn no_schema() -> &'static NoSchema {
    static NO: NoSchema = NoSchema;
    &NO
}

/// Normalized catalog key for [`ObjectName`] (e.g. `schema.type`).
pub fn object_name_key(name: &ObjectName) -> String {
    name.parts.join(".")
}

#[derive(Clone, Debug, PartialEq)]
enum GraphSchemaBinding {
    /// References `graph_type_definitions` by key.
    TypeRef(String),
    /// Inline graph type body for this graph only.
    Inline(GraphTypeDefinition),
}

/// Catalog of named graph types and per-graph schema bindings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphCatalog {
    graph_type_definitions: BTreeMap<String, GraphTypeDefinition>,
    graph_bindings: BTreeMap<String, GraphSchemaBinding>,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("graph type `{0}` already exists")]
    GraphTypeExists(String),
    #[error("graph type `{0}` not found")]
    GraphTypeNotFound(String),
    #[error("unsupported catalog DDL: {0}")]
    Unsupported(String),
    #[error("graph type definition invalid: {0}")]
    InvalidDefinition(String),
}

impl GraphCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply all catalog-relevant DDL statements in block order (first + NEXT chain).
    pub fn apply_statement_block(&mut self, block: &StatementBlock) -> Result<(), CatalogError> {
        for stmt in block.iter_statements() {
            self.apply_statement(stmt)?;
        }
        Ok(())
    }

    fn apply_statement(&mut self, stmt: &Statement) -> Result<(), CatalogError> {
        match stmt {
            Statement::CreateGraphType(c) => self.apply_create_graph_type(c),
            Statement::CreateGraph(c) => self.apply_create_graph(c),
            Statement::DropGraphType(d) => {
                self.apply_drop_graph_type(d);
                Ok(())
            }
            Statement::DropGraph(d) => {
                self.apply_drop_graph(d);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn apply_create_graph_type(&mut self, c: &CreateGraphTypeStatement) -> Result<(), CatalogError> {
        if c.copy_of.is_some() {
            return Err(CatalogError::Unsupported(
                "CREATE GRAPH TYPE ... COPY OF is not supported yet".into(),
            ));
        }
        let key = object_name_key(&c.name);
        if c.if_not_exists && self.graph_type_definitions.contains_key(&key) {
            return Ok(());
        }
        if self.graph_type_definitions.contains_key(&key) {
            if c.or_replace {
                self.graph_type_definitions.insert(key, c.definition.clone());
                return Ok(());
            }
            return Err(CatalogError::GraphTypeExists(key));
        }
        self.graph_type_definitions
            .insert(key, c.definition.clone());
        Ok(())
    }

    fn apply_create_graph(&mut self, c: &CreateGraphStatement) -> Result<(), CatalogError> {
        if c.copy_of.is_some() {
            return Err(CatalogError::Unsupported(
                "CREATE GRAPH ... AS COPY OF is not supported yet".into(),
            ));
        }
        let gkey = object_name_key(&c.name);
        if c.if_not_exists && self.graph_bindings.contains_key(&gkey) {
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
                let tkey = object_name_key(name);
                if !self.graph_type_definitions.contains_key(&tkey) {
                    return Err(CatalogError::GraphTypeNotFound(tkey));
                }
                Some(GraphSchemaBinding::TypeRef(tkey))
            }
            Some(GraphTypeSpec::Inline(def)) => Some(GraphSchemaBinding::Inline(def.clone())),
        };

        if self.graph_bindings.contains_key(&gkey) {
            if c.or_replace {
                match binding {
                    Some(b) => self.graph_bindings.insert(gkey, b),
                    None => self.graph_bindings.remove(&gkey),
                };
                return Ok(());
            }
            return Err(CatalogError::Unsupported(format!(
                "graph `{gkey}` already exists (use OR REPLACE or IF NOT EXISTS)"
            )));
        }

        if let Some(b) = binding {
            self.graph_bindings.insert(gkey, b);
        }
        Ok(())
    }

    fn apply_drop_graph_type(&mut self, d: &DropGraphTypeStatement) {
        let key = object_name_key(&d.name);
        self.graph_type_definitions.remove(&key);
        self.graph_bindings.retain(|_, b| match b {
            GraphSchemaBinding::TypeRef(t) => t != &key,
            GraphSchemaBinding::Inline(_) => true,
        });
    }

    fn apply_drop_graph(&mut self, d: &DropGraphStatement) {
        let key = object_name_key(&d.name);
        self.graph_bindings.remove(&key);
    }

    /// Resolve a [`GraphTypePropertySchema`] for planning when `graph_name` is the active property graph.
    pub fn try_property_schema_for_graph(
        &self,
        graph_name: Option<&str>,
    ) -> Result<Option<GraphTypePropertySchema>, CatalogError> {
        let Some(g) = graph_name.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let Some(binding) = self.graph_bindings.get(g) else {
            return Ok(None);
        };
        let def: &GraphTypeDefinition = match binding {
            GraphSchemaBinding::TypeRef(k) => self
                .graph_type_definitions
                .get(k)
                .ok_or_else(|| CatalogError::GraphTypeNotFound(k.clone()))?,
            GraphSchemaBinding::Inline(def) => def,
        };
        GraphTypePropertySchema::try_from_definition(def)
            .map(Some)
            .map_err(CatalogError::InvalidDefinition)
    }

    /// JSON encoding helper (debugging / tooling). Canister persistence uses [`Self::to_stable_blob`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let p = self.to_persisted();
        serde_json::to_string(&p)
    }

    /// Restore from JSON (empty string = empty catalog).
    pub fn from_json(s: &str) -> Result<Self, CatalogRestoreError> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }
        let p: PersistedCatalog = serde_json::from_str(s)?;
        Self::from_persisted(p)
    }

    fn to_persisted(&self) -> PersistedCatalog {
        PersistedCatalog {
            graph_types: self
                .graph_type_definitions
                .iter()
                .map(|(k, v)| (k.clone(), GraphTypeDefinitionPersisted::from_ast(v)))
                .collect(),
            graph_bindings: self
                .graph_bindings
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        match v {
                            GraphSchemaBinding::TypeRef(t) => PersistedBinding::TypeRef(t.clone()),
                            GraphSchemaBinding::Inline(d) => {
                                PersistedBinding::Inline(GraphTypeDefinitionPersisted::from_ast(d))
                            }
                        },
                    )
                })
                .collect(),
        }
    }

    fn from_persisted(p: PersistedCatalog) -> Result<Self, CatalogRestoreError> {
        let mut c = GraphCatalog::default();
        for (k, def) in p.graph_types {
            c.graph_type_definitions.insert(
                k,
                def.into_ast().map_err(CatalogRestoreError::Invalid)?,
            );
        }
        for (k, b) in p.graph_bindings {
            let binding = match b {
                PersistedBinding::TypeRef(t) => GraphSchemaBinding::TypeRef(t),
                PersistedBinding::Inline(d) => {
                    GraphSchemaBinding::Inline(d.into_ast().map_err(CatalogRestoreError::Invalid)?)
                }
            };
            c.graph_bindings.insert(k, binding);
        }
        Ok(c)
    }

    /// Encode as `StableBTreeMap` wire bytes (prefix keys `t:` graph type, `b:` graph binding).
    pub fn to_stable_blob(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mem = VectorMemory::default();
        let mut map: StableBTreeMap<StableCatalogKey, StableCatalogValue, _> =
            StableBTreeMap::init(mem);
        for (k, def) in &self.graph_type_definitions {
            let key = StableCatalogKey(format!("t:{k}").into_bytes());
            let payload = serde_json::to_vec(&GraphTypeDefinitionPersisted::from_ast(def))?;
            map.insert(key, StableCatalogValue(payload));
        }
        for (k, b) in &self.graph_bindings {
            let key = StableCatalogKey(format!("b:{k}").into_bytes());
            let persisted = match b {
                GraphSchemaBinding::TypeRef(t) => PersistedBinding::TypeRef(t.clone()),
                GraphSchemaBinding::Inline(d) => {
                    PersistedBinding::Inline(GraphTypeDefinitionPersisted::from_ast(d))
                }
            };
            let payload = serde_json::to_vec(&persisted)?;
            map.insert(key, StableCatalogValue(payload));
        }
        Ok(clone_stable_catalog_map(&map).into_memory().borrow().clone())
    }

    /// Decode [`Self::to_stable_blob`] output. Empty slice yields an empty catalog.
    pub fn from_stable_blob(bytes: &[u8]) -> Result<Self, CatalogRestoreError> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let mut padded = bytes.to_vec();
        pad_stable_catalog_bytes(&mut padded);
        let mem = Rc::new(RefCell::new(padded));
        let map: StableBTreeMap<StableCatalogKey, StableCatalogValue, _> =
            StableBTreeMap::init(mem);
        let mut c = GraphCatalog::default();
        for e in map.iter() {
            let key_bytes = &e.key().0;
            let key_str =
                std::str::from_utf8(key_bytes).map_err(|e| CatalogRestoreError::Invalid(e.to_string()))?;
            if let Some(name) = key_str.strip_prefix("t:") {
                let def: GraphTypeDefinitionPersisted = serde_json::from_slice(&e.value().0)?;
                c.graph_type_definitions.insert(
                    name.to_owned(),
                    def.into_ast()
                        .map_err(|msg| CatalogRestoreError::Invalid(msg.clone()))?,
                );
            } else if let Some(name) = key_str.strip_prefix("b:") {
                let b: PersistedBinding = serde_json::from_slice(&e.value().0)?;
                let binding = match b {
                    PersistedBinding::TypeRef(t) => GraphSchemaBinding::TypeRef(t),
                    PersistedBinding::Inline(d) => GraphSchemaBinding::Inline(
                        d.into_ast()
                            .map_err(|msg| CatalogRestoreError::Invalid(msg.clone()))?,
                    ),
                };
                c.graph_bindings.insert(name.to_owned(), binding);
            } else {
                return Err(CatalogRestoreError::Invalid(format!(
                    "unknown stable catalog key prefix: {key_str:?}"
                )));
            }
        }
        Ok(c)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StableCatalogKey(Vec<u8>);

impl Storable for StableCatalogKey {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }

    const BOUND: Bound = Bound::Unbounded;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StableCatalogValue(Vec<u8>);

impl Storable for StableCatalogValue {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }

    const BOUND: Bound = Bound::Unbounded;
}

fn pad_stable_catalog_bytes(v: &mut Vec<u8>) {
    const WASM_PAGE: usize = 65536;
    if v.is_empty() {
        return;
    }
    let padded_len = v.len().div_ceil(WASM_PAGE) * WASM_PAGE;
    v.resize(padded_len, 0);
}

fn clone_stable_catalog_map(
    src: &StableBTreeMap<StableCatalogKey, StableCatalogValue, VectorMemory>,
) -> StableBTreeMap<StableCatalogKey, StableCatalogValue, VectorMemory> {
    let mem = VectorMemory::default();
    let mut dst = StableBTreeMap::init(mem);
    for e in src.iter() {
        dst.insert(e.key().clone(), e.value().clone());
    }
    dst
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogRestoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid persisted catalog: {0}")]
    Invalid(String),
}

#[derive(Serialize, Deserialize)]
struct PersistedCatalog {
    graph_types: Vec<(String, GraphTypeDefinitionPersisted)>,
    graph_bindings: Vec<(String, PersistedBinding)>,
}

#[derive(Serialize, Deserialize)]
enum PersistedBinding {
    TypeRef(String),
    Inline(GraphTypeDefinitionPersisted),
}

#[derive(Serialize, Deserialize)]
struct GraphTypeDefinitionPersisted {
    elements: Vec<GraphTypeElementPersisted>,
}

#[derive(Serialize, Deserialize)]
enum GraphTypeElementPersisted {
    Node(NodeTypeDefPersisted),
    Edge(EdgeTypeDefPersisted),
}

#[derive(Serialize, Deserialize)]
struct NodeTypeDefPersisted {
    name: Option<String>,
    alias: Option<String>,
    label_keyword_plural: bool,
    labels: Vec<String>,
    properties: Vec<PropertyDefPersisted>,
}

#[derive(Serialize, Deserialize)]
struct EdgeTypeDefPersisted {
    name: Option<String>,
    /// `true` = undirected in DDL sense (`UNDIRECTED` / `~[...]~` connector).
    undirected: bool,
    source: EdgeEndpointPersisted,
    destination: EdgeEndpointPersisted,
    label_keyword_plural: bool,
    labels: Vec<String>,
    properties: Vec<PropertyDefPersisted>,
}

#[derive(Serialize, Deserialize)]
struct EdgeEndpointPersisted {
    label: Option<String>,
    type_name: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PropertyDefPersisted {
    name: String,
    value_type: String,
    not_null: bool,
}

impl GraphTypeDefinitionPersisted {
    fn from_ast(d: &GraphTypeDefinition) -> Self {
        Self {
            elements: d
                .elements
                .iter()
                .map(GraphTypeElementPersisted::from_ast)
                .collect(),
        }
    }

    fn into_ast(self) -> Result<GraphTypeDefinition, String> {
        use gleaph_gql::ast::GraphTypeDefinition;
        use gleaph_gql::token::Span;

        let elements: Result<Vec<_>, _> = self
            .elements
            .into_iter()
            .map(|e| e.into_ast())
            .collect();
        Ok(GraphTypeDefinition {
            span: Span::DUMMY,
            elements: elements?,
        })
    }
}

impl GraphTypeElementPersisted {
    fn from_ast(e: &gleaph_gql::ast::GraphTypeElement) -> Self {
        match e {
            gleaph_gql::ast::GraphTypeElement::Node(n) => {
                Self::Node(NodeTypeDefPersisted::from_ast(n))
            }
            gleaph_gql::ast::GraphTypeElement::Edge(ed) => {
                Self::Edge(EdgeTypeDefPersisted::from_ast(ed))
            }
        }
    }

    fn into_ast(self) -> Result<gleaph_gql::ast::GraphTypeElement, String> {
        use gleaph_gql::ast::GraphTypeElement;
        match self {
            Self::Node(n) => Ok(GraphTypeElement::Node(n.into_ast()?)),
            Self::Edge(e) => Ok(GraphTypeElement::Edge(e.into_ast()?)),
        }
    }
}

impl NodeTypeDefPersisted {
    fn from_ast(n: &gleaph_gql::ast::NodeTypeDef) -> Self {
        let labels = n
            .label_set
            .as_ref()
            .map(|ls| ls.labels.clone())
            .unwrap_or_default();
        let label_keyword_plural = n
            .label_set
            .as_ref()
            .map(|ls| ls.label_keyword_plural)
            .unwrap_or(false);
        Self {
            name: n.name.clone(),
            alias: n.alias.clone(),
            label_keyword_plural,
            labels,
            properties: n.properties.iter().map(PropertyDefPersisted::from_ast).collect(),
        }
    }

    fn into_ast(self) -> Result<gleaph_gql::ast::NodeTypeDef, String> {
        use gleaph_gql::ast::{Keyword, KeyLabelSet, NodeTypeDef};
        use gleaph_gql::token::Span;

        let label_set = if self.labels.is_empty() {
            None
        } else {
            Some(KeyLabelSet {
                span: Span::DUMMY,
                label_keyword_plural: self.label_keyword_plural,
                labels: self.labels,
            })
        };
        let properties: Result<Vec<_>, _> = self
            .properties
            .into_iter()
            .map(|p| p.into_ast())
            .collect();
        Ok(NodeTypeDef {
            span: Span::DUMMY,
            keyword: Keyword::new("NODE"),
            name: self.name,
            alias: self.alias,
            label_set,
            properties: properties?,
        })
    }
}

impl EdgeTypeDefPersisted {
    fn from_ast(e: &gleaph_gql::ast::EdgeTypeDef) -> Self {
        let undirected = matches!(e.direction, gleaph_gql::types::EdgeDirection::Undirected);
        let (label_keyword_plural, labels) = match &e.label_set {
            Some(ls) => (ls.label_keyword_plural, ls.labels.clone()),
            None => (false, Vec::new()),
        };
        Self {
            name: e.name.clone(),
            undirected,
            source: EdgeEndpointPersisted::from_ast(&e.source),
            destination: EdgeEndpointPersisted::from_ast(&e.destination),
            label_keyword_plural,
            labels,
            properties: e.properties.iter().map(PropertyDefPersisted::from_ast).collect(),
        }
    }

    fn into_ast(self) -> Result<gleaph_gql::ast::EdgeTypeDef, String> {
        use gleaph_gql::ast::{EdgeTypeDef, Keyword, KeyLabelSet};
        use gleaph_gql::token::Span;
        use gleaph_gql::types::EdgeDirection;

        let direction = if self.undirected {
            EdgeDirection::Undirected
        } else {
            EdgeDirection::PointingRight
        };
        let label_set = if self.labels.is_empty() {
            None
        } else {
            Some(KeyLabelSet {
                span: Span::DUMMY,
                label_keyword_plural: self.label_keyword_plural,
                labels: self.labels,
            })
        };
        let properties: Result<Vec<_>, _> = self
            .properties
            .into_iter()
            .map(|p| p.into_ast())
            .collect();
        Ok(EdgeTypeDef {
            span: Span::DUMMY,
            keyword: Keyword::new("EDGE"),
            name: self.name,
            direction,
            source: self.source.into_ast(),
            destination: self.destination.into_ast(),
            label_set,
            properties: properties?,
        })
    }
}

impl EdgeEndpointPersisted {
    fn from_ast(e: &gleaph_gql::ast::EdgeEndpoint) -> Self {
        Self {
            label: e.label.clone(),
            type_name: e.type_name.clone(),
        }
    }

    fn into_ast(self) -> gleaph_gql::ast::EdgeEndpoint {
        use gleaph_gql::ast::EdgeEndpoint;
        use gleaph_gql::token::Span;
        EdgeEndpoint {
            span: Span::DUMMY,
            label: self.label,
            type_name: self.type_name,
        }
    }
}

impl PropertyDefPersisted {
    fn from_ast(p: &gleaph_gql::ast::PropertyDef) -> Self {
        Self {
            name: p.name.clone(),
            value_type: value_type_tag(&p.value_type),
            not_null: p.not_null,
        }
    }

    fn into_ast(self) -> Result<gleaph_gql::ast::PropertyDef, String> {
        use gleaph_gql::ast::PropertyDef;
        use gleaph_gql::token::Span;

        Ok(PropertyDef {
            span: Span::DUMMY,
            name: self.name,
            value_type: parse_value_type_tag(&self.value_type)?,
            not_null: self.not_null,
            default_value: None,
        })
    }
}

fn value_type_tag(v: &gleaph_gql::ast::ValueType) -> String {
    use gleaph_gql::ast::ValueType;
    match v {
        ValueType::Bool { .. } => "BOOL".into(),
        ValueType::String { .. } => "STRING".into(),
        ValueType::Int32 { .. } => "INT32".into(),
        ValueType::Int64 { .. } => "INT64".into(),
        ValueType::Date => "DATE".into(),
        ValueType::DateTime => "DATETIME".into(),
        ValueType::Timestamp => "TIMESTAMP".into(),
        _ => format!("OTHER:{v:?}"),
    }
}

fn parse_value_type_tag(s: &str) -> Result<gleaph_gql::ast::ValueType, String> {
    use gleaph_gql::ast::{Keyword, ValueType};
    Ok(match s {
        "BOOL" => ValueType::Bool {
            keyword: Keyword::new("BOOL"),
        },
        "STRING" => ValueType::String {
            min_length: None,
            max_length: None,
        },
        "INT32" => ValueType::Int32 {
            keyword: Keyword::new("INT32"),
        },
        "INT64" => ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        },
        "DATE" => ValueType::Date,
        "DATETIME" => ValueType::DateTime,
        "TIMESTAMP" => ValueType::Timestamp,
        _ => return Err(format!("unknown persisted value type: {s}")),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum PlanBlockError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Planner(#[from] gleaph_gql_planner::PlannerError),
}

/// Plan a block with optional per-graph [`PropertySchema`] from a catalog.
pub fn plan_block_with_catalog(
    block: &StatementBlock,
    stats: Option<&dyn gleaph_gql_planner::GraphStats>,
    catalog: &GraphCatalog,
    active_graph: Option<&str>,
) -> Result<gleaph_gql_planner::PlanBuildOutput, PlanBlockError> {
    use gleaph_gql_planner::build_block_plan_output_with_schema;

    let no = no_schema();
    let schema_owned = catalog.try_property_schema_for_graph(active_graph)?;
    let schema: &dyn PropertySchema = match &schema_owned {
        Some(s) => s,
        None => no,
    };
    Ok(build_block_plan_output_with_schema(block, stats, schema)?)
}

pub fn execute_block_with_catalog<G: gleaph_graph_kernel::GraphRead + gleaph_graph_kernel::GraphWrite>(
    graph: &mut G,
    block: &StatementBlock,
    stats: Option<&dyn gleaph_gql_planner::GraphStats>,
    ctx: &gleaph_gql_executor::ExecutionContext,
    catalog: &GraphCatalog,
) -> Result<crate::QueryRunOutput, crate::GleaphError> {
    use gleaph_gql_planner::build_block_plan_output_for_execute_with_schema;

    let no = no_schema();
    let schema_owned = catalog
        .try_property_schema_for_graph(ctx.selected_graph.as_deref())
        .map_err(|e| crate::GleaphError::Catalog(e.to_string()))?;
    let schema: &dyn PropertySchema = match &schema_owned {
        Some(s) => s,
        None => no,
    };
    let plan = build_block_plan_output_for_execute_with_schema(block, stats, schema)?;
    crate::ensure_plan_supported_by_executor(&plan.plan)?;
    let execution = gleaph_gql_executor::execute_plan_with_context(graph, &plan.plan, ctx)?;
    Ok(crate::QueryRunOutput { plan, execution })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::parser;

    fn block_from(gql: &str) -> StatementBlock {
        let program = parser::parse(gql).expect("parse");
        program
            .transaction_activity
            .expect("tx")
            .body
            .expect("body")
    }

    #[test]
    fn catalog_roundtrip_json() {
        let mut c = GraphCatalog::default();
        let block = block_from(
            "CREATE GRAPH TYPE Social { NODE Person LABEL Person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }",
        );
        c.apply_statement_block(&block).expect("apply");
        let j = c.to_json().expect("json");
        let c2 = GraphCatalog::from_json(&j).expect("restore");
        assert_eq!(j, c2.to_json().expect("re-json"), "json round-trip must stabilize");
    }

    #[test]
    fn catalog_stable_blob_roundtrip() {
        let mut c = GraphCatalog::default();
        let block = block_from(
            "CREATE GRAPH TYPE gt { NODE Person LABEL Person, UNDIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person ~ Person) } NEXT CREATE GRAPH g TYPED gt",
        );
        c.apply_statement_block(&block).expect("apply");
        let blob = c.to_stable_blob().expect("stable blob");
        let c2 = GraphCatalog::from_stable_blob(&blob).expect("restore blob");
        assert_eq!(
            blob,
            c2.to_stable_blob().expect("re-encode"),
            "stable blob round-trip must stabilize"
        );
    }

    #[test]
    fn typed_graph_catalog_triggers_dml006_for_directed_match() {
        let mut c = GraphCatalog::default();
        let ddl = block_from(
            "CREATE GRAPH TYPE gt { NODE Person LABEL Person, UNDIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person ~ Person) } NEXT CREATE GRAPH g TYPED gt",
        );
        c.apply_statement_block(&ddl).expect("ddl");
        let q = block_from("MATCH (a)-[:KNOWS]->(b) RETURN a");
        let out = crate::plan_block_with_catalog(&q, None, &c, Some("g")).expect("plan");
        assert!(
            out.plan
                .diagnostics
                .dml_warnings
                .iter()
                .any(|w| w.code == "DML006"),
            "expected DML006, got {:?}",
            out.plan.diagnostics
        );
    }
}
