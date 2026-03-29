# GraphType Schema Extension (§12)

## Status: Complete (all 5 phases + CONSTRAINT enforcement done)

> Property type definitions (PropertyDef, StoredPropertyDef), property type validation on
> CREATE/MERGE/SET, edge type definitions (EdgeTypeDef, StoredEdgeType), edge endpoint
> validation (`enforce_edge_type_endpoints`). Open schema by default.
> `DESCRIBE GRAPH TYPE` introspection query implemented (Phase 5).
> Named constraints (`CREATE CONSTRAINT ... ASSERT ... IS UNIQUE|NOT NULL`) with enforcement
> on INSERT/MERGE, existing-data validation on creation, `SHOW CONSTRAINTS`, `DROP CONSTRAINT`.

## Motivation

Current `GraphTypeDefinition` validates only node/edge labels. Extending it to include
property type constraints and edge type definitions (with from/to node type restrictions)
enables full schema enforcement.

## Current State

```rust
// crates/gql/src/ast.rs
pub struct GraphTypeDefinition {
    pub node_labels: Vec<String>,
    pub edge_labels: Vec<String>,
    pub node_types: Vec<NodeTypeDef>,
}

pub struct NodeTypeDef {
    pub name: String,
    pub labels: Vec<String>,
}
```

- `enforce_graph_type_labels` in `gql_bridge.rs` validates that mutations only use
  labels declared in the graph type.
- No property type checking.
- No edge endpoint constraints.

## Design

### Extended AST Types

```rust
pub struct StoredPropertyDef {
    pub name: String,
    pub value_type: ValueType,
    pub required: bool,       // NOT NULL constraint
    pub default: Option<Value>,
}

pub struct NodeTypeDef {
    pub name: String,
    pub labels: Vec<String>,
    pub properties: Vec<StoredPropertyDef>,  // NEW
}

pub struct EdgeTypeDef {
    pub name: String,
    pub label: String,
    pub from_types: Vec<String>,  // allowed source node types
    pub to_types: Vec<String>,    // allowed destination node types
    pub properties: Vec<StoredPropertyDef>,
}

pub struct GraphTypeDefinition {
    pub node_labels: Vec<String>,
    pub edge_labels: Vec<String>,
    pub node_types: Vec<NodeTypeDef>,
    pub edge_types: Vec<EdgeTypeDef>,  // NEW
}
```

### DDL Syntax Extension

```sql
CREATE GRAPH TYPE SocialGraph {
  -- Node types with property constraints
  DEFINE PersonType AS (:Person {
    name :: TEXT NOT NULL,
    age :: INT,
    email :: TEXT
  }),

  -- Edge types with endpoint constraints
  DEFINE KnowsType AS (:Person)-[:KNOWS {
    since :: INT NOT NULL
  }]->(:Person),

  DEFINE WorksAtType AS (:Person)-[:WORKS_AT {
    role :: TEXT
  }]->(:Company),

  -- Standalone labels (backward compatible)
  (:Company),
  -[:LOCATED_IN]->
}
```

### Validation Rules

`enforce_graph_type_schema` (replaces `enforce_graph_type_labels`):

1. **Label validation** (existing): Reject unknown labels.
2. **Property type validation** (new):
   - On INSERT/SET: check property value matches declared type.
   - Required properties must be present on INSERT.
   - Unknown properties are allowed (open schema by default).
3. **Edge endpoint validation** (new):
   - On INSERT edge: check source/destination node labels match declared from/to types.
   - If no edge type defined for the label, any endpoints allowed (open schema).

### Storage

`StoredGraphType` in `state.rs` — serialized in `RuntimePersistSnapshot`:
```rust
pub struct StoredGraphType {
    pub node_labels: HashSet<String>,
    pub edge_labels: HashSet<String>,
    pub node_types: HashMap<String, StoredNodeType>,
    pub edge_types: HashMap<String, StoredEdgeType>,
}

pub struct StoredNodeType {
    pub labels: Vec<String>,
    pub properties: Vec<StoredPropertyDef>,
}

pub struct StoredEdgeType {
    pub label: String,
    pub from_types: Vec<String>,
    pub to_types: Vec<String>,
    pub properties: Vec<StoredPropertyDef>,
}
```

### Error Messages

```
GraphType 'SocialGraph': property 'age' on :Person expects INT, got TEXT
GraphType 'SocialGraph': required property 'name' missing on :Person
GraphType 'SocialGraph': edge :KNOWS not allowed from :Company to :Person
```

### Implementation Phases

1. **Phase 1**: Property type definitions in DDL + AST
2. **Phase 2**: Property type validation in `enforce_graph_type_schema`
3. **Phase 3**: Edge type definitions in DDL + AST
4. **Phase 4**: Edge endpoint validation
5. **Phase 5**: `DESCRIBE GRAPH TYPE` query for introspection

### Integration with V4 (Static Type System)

When a graph type is active, the TypeEnv can use property definitions to
infer types for `n.property` accesses — enabling more precise static type checking.

### Test Plan

- Parse extended DDL with property types
- Validate property types on INSERT
- Validate required property enforcement
- Validate edge endpoint constraints
- Backward compat: existing CREATE GRAPH TYPE without properties still works
- PocketIC: persist and restore extended graph types across upgrades
