pub use gleaph_types::LabelExpr;
use gleaph_types::Value;

/// Scalar element type for typed lists (`LIST<INT>`, `LIST<TEXT>`, etc.).
///
/// This is `Copy`-safe — no `Box` indirection needed — because lists can only
/// contain scalar elements, not nested lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarType {
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Uint128,
    Uint256,
    Float32,
    Float64,
    Text,
    Bool,
    Timestamp,
    Bytes,
    Date,
    Time,
    DateTime,
    Duration,
    Principal,
    Decimal,
}

/// GQL §21.3 value type for parameter type annotations and CAST/IS ::type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Uint128,
    Uint256,
    Float32,
    Float64,
    Text,
    Bool,
    Timestamp,
    List,
    /// Typed list with known element type: `LIST<INT>`, `LIST<TEXT>`, etc.
    TypedList(ScalarType),
    Null,
    Bytes,
    Date,
    Time,
    DateTime,
    Duration,
    Decimal,
    /// Character string with length constraints: `STRING(max)`, `STRING(min,max)`,
    /// `VARCHAR(max)`, `CHAR(n)`.  The underlying value is still `Value::Text`;
    /// constraints are enforced at schema-validation and CAST time.
    ///
    /// * `fixed == true` ⇒ `CHAR(n)`: `min_length == max_length == n`.
    /// * `fixed == false` ⇒ `STRING(…)` or `VARCHAR(…)`.
    TextConstrained {
        min_length: u32,
        max_length: u32,
        fixed: bool,
    },
    /// Byte string with length constraints: `BYTES(max)`, `BYTES(min,max)`,
    /// `VARBINARY(max)`, `BINARY(n)`.  The underlying value is still `Value::Bytes`;
    /// constraints are enforced at schema-validation and CAST time.
    ///
    /// * `fixed == true` ⇒ `BINARY(n)`: `min_length == max_length == n`.
    /// * `fixed == false` ⇒ `BYTES(…)` or `VARBINARY(…)`.
    BytesConstrained {
        min_length: u32,
        max_length: u32,
        fixed: bool,
    },
}

impl ScalarType {
    /// Convert to the public `PreparedScalarType` for Candid serialisation.
    pub fn to_prepared(self) -> gleaph_types::PreparedScalarType {
        use gleaph_types::PreparedScalarType;
        match self {
            Self::Int8 => PreparedScalarType::Int8,
            Self::Int16 => PreparedScalarType::Int16,
            Self::Int32 => PreparedScalarType::Int32,
            Self::Int64 => PreparedScalarType::Int64,
            Self::Int128 => PreparedScalarType::Int128,
            Self::Int256 => PreparedScalarType::Int256,
            Self::Uint8 => PreparedScalarType::Uint8,
            Self::Uint16 => PreparedScalarType::Uint16,
            Self::Uint32 => PreparedScalarType::Uint32,
            Self::Uint64 => PreparedScalarType::Uint64,
            Self::Uint128 => PreparedScalarType::Uint128,
            Self::Uint256 => PreparedScalarType::Uint256,
            Self::Float32 => PreparedScalarType::Float32,
            Self::Float64 => PreparedScalarType::Float64,
            Self::Text => PreparedScalarType::Text,
            Self::Bool => PreparedScalarType::Bool,
            Self::Timestamp => PreparedScalarType::Timestamp,
            Self::Bytes => PreparedScalarType::Bytes,
            Self::Date => PreparedScalarType::Date,
            Self::Time => PreparedScalarType::Time,
            Self::DateTime => PreparedScalarType::DateTime,
            Self::Duration => PreparedScalarType::Duration,
            Self::Principal => PreparedScalarType::Principal,
            Self::Decimal => PreparedScalarType::Decimal,
        }
    }

    /// Convert a non-list `ValueType` to its scalar counterpart.
    pub fn from_value_type(vt: ValueType) -> Option<Self> {
        match vt {
            ValueType::Int8 => Some(Self::Int8),
            ValueType::Int16 => Some(Self::Int16),
            ValueType::Int32 => Some(Self::Int32),
            ValueType::Int64 => Some(Self::Int64),
            ValueType::Int128 => Some(Self::Int128),
            ValueType::Int256 => Some(Self::Int256),
            ValueType::Uint8 => Some(Self::Uint8),
            ValueType::Uint16 => Some(Self::Uint16),
            ValueType::Uint32 => Some(Self::Uint32),
            ValueType::Uint64 => Some(Self::Uint64),
            ValueType::Uint128 => Some(Self::Uint128),
            ValueType::Uint256 => Some(Self::Uint256),
            ValueType::Float32 => Some(Self::Float32),
            ValueType::Float64 => Some(Self::Float64),
            ValueType::Text => Some(Self::Text),
            ValueType::Bool => Some(Self::Bool),
            ValueType::Timestamp => Some(Self::Timestamp),
            ValueType::Bytes => Some(Self::Bytes),
            ValueType::Date => Some(Self::Date),
            ValueType::Time => Some(Self::Time),
            ValueType::DateTime => Some(Self::DateTime),
            ValueType::Duration => Some(Self::Duration),
            ValueType::Decimal => Some(Self::Decimal),
            ValueType::TextConstrained { .. } => Some(Self::Text),
            ValueType::BytesConstrained { .. } => Some(Self::Bytes),
            ValueType::List | ValueType::TypedList(_) | ValueType::Null => None,
        }
    }

    /// Convert to the corresponding `ValueType`.
    pub fn to_value_type(self) -> ValueType {
        match self {
            Self::Int8 => ValueType::Int8,
            Self::Int16 => ValueType::Int16,
            Self::Int32 => ValueType::Int32,
            Self::Int64 => ValueType::Int64,
            Self::Int128 => ValueType::Int128,
            Self::Int256 => ValueType::Int256,
            Self::Uint8 => ValueType::Uint8,
            Self::Uint16 => ValueType::Uint16,
            Self::Uint32 => ValueType::Uint32,
            Self::Uint64 => ValueType::Uint64,
            Self::Uint128 => ValueType::Uint128,
            Self::Uint256 => ValueType::Uint256,
            Self::Float32 => ValueType::Float32,
            Self::Float64 => ValueType::Float64,
            Self::Text => ValueType::Text,
            Self::Bool => ValueType::Bool,
            Self::Timestamp => ValueType::Timestamp,
            Self::Bytes => ValueType::Bytes,
            Self::Date => ValueType::Date,
            Self::Time => ValueType::Time,
            Self::DateTime => ValueType::DateTime,
            Self::Duration => ValueType::Duration,
            Self::Principal => ValueType::Int64, // no ValueType::Principal yet; unused in practice
            Self::Decimal => ValueType::Decimal,
        }
    }
}

/// Parse a type name (case-insensitive) into a [`ValueType`].
///
/// Accepts common SQL/GQL aliases (INTEGER, INT, BIGINT, FLOAT, DOUBLE, REAL,
/// STRING, VARCHAR, TEXT, BOOLEAN, BOOL, TIMESTAMP, LIST, NULL).
/// Returns `None` for unrecognised names (caller decides whether that is an error).
pub fn parse_value_type(ident: &str) -> Option<ValueType> {
    match ident.to_ascii_lowercase().as_str() {
        "int8" | "tinyint" | "integer8" => Some(ValueType::Int8),
        "int16" | "smallint" | "integer16" => Some(ValueType::Int16),
        "int" | "int32" | "integer" | "integer32" => Some(ValueType::Int32),
        "int64" | "bigint" | "integer64" => Some(ValueType::Int64),
        "int128" | "integer128" => Some(ValueType::Int128),
        "int256" | "integer256" => Some(ValueType::Int256),
        "uint8" => Some(ValueType::Uint8),
        "uint16" | "usmallint" => Some(ValueType::Uint16),
        "uint" | "uint32" => Some(ValueType::Uint32),
        "uint64" | "ubigint" => Some(ValueType::Uint64),
        "uint128" => Some(ValueType::Uint128),
        "uint256" => Some(ValueType::Uint256),
        "float" | "float32" | "real" => Some(ValueType::Float32),
        "float64" | "double" => Some(ValueType::Float64),
        "string" | "varchar" | "char" | "text" => Some(ValueType::Text),
        "boolean" | "bool" => Some(ValueType::Bool),
        "timestamp" => Some(ValueType::Timestamp),
        "list" => Some(ValueType::List),
        "null" => Some(ValueType::Null),
        "bytes" | "binary" | "varbinary" | "blob" => Some(ValueType::Bytes),
        "date" => Some(ValueType::Date),
        "time" => Some(ValueType::Time),
        "datetime" | "localdatetime" => Some(ValueType::DateTime),
        "duration" => Some(ValueType::Duration),
        "decimal" | "dec" | "numeric" => Some(ValueType::Decimal),
        _ => None,
    }
}

/// Parse a scalar type name (for use as element type in `LIST<SCALAR>`).
pub fn parse_scalar_type(ident: &str) -> Option<ScalarType> {
    match ident.to_ascii_lowercase().as_str() {
        "int8" | "tinyint" | "integer8" => Some(ScalarType::Int8),
        "int16" | "smallint" | "integer16" => Some(ScalarType::Int16),
        "int" | "int32" | "integer" | "integer32" => Some(ScalarType::Int32),
        "int64" | "bigint" | "integer64" => Some(ScalarType::Int64),
        "int128" | "integer128" => Some(ScalarType::Int128),
        "int256" | "integer256" => Some(ScalarType::Int256),
        "uint8" => Some(ScalarType::Uint8),
        "uint16" | "usmallint" => Some(ScalarType::Uint16),
        "uint" | "uint32" => Some(ScalarType::Uint32),
        "uint64" | "ubigint" => Some(ScalarType::Uint64),
        "uint128" => Some(ScalarType::Uint128),
        "uint256" => Some(ScalarType::Uint256),
        "float" | "float32" | "real" => Some(ScalarType::Float32),
        "float64" | "double" => Some(ScalarType::Float64),
        "string" | "varchar" | "char" | "text" => Some(ScalarType::Text),
        "boolean" | "bool" => Some(ScalarType::Bool),
        "timestamp" => Some(ScalarType::Timestamp),
        "bytes" | "binary" | "varbinary" | "blob" => Some(ScalarType::Bytes),
        "date" => Some(ScalarType::Date),
        "time" => Some(ScalarType::Time),
        "datetime" | "localdatetime" => Some(ScalarType::DateTime),
        "duration" => Some(ScalarType::Duration),
        "principal" => Some(ScalarType::Principal),
        "decimal" | "dec" | "numeric" => Some(ScalarType::Decimal),
        _ => None,
    }
}

/// Truth value for IS TRUE / IS FALSE / IS UNKNOWN predicates (§20.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruthValue {
    True,
    False,
    Unknown,
}

/// Match mode for the entire query: `MATCH REPEATABLE ELEMENTS` / `MATCH DIFFERENT EDGES` (§16.4).
///
/// - `RepeatableElements`: no restriction (default).
/// - `DifferentEdges`: no edge may appear more than once across all match entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    RepeatableElements,
    DifferentEdges,
}

/// Path traversal mode for `MATCH WALK / TRAIL / SIMPLE / ACYCLIC` (§16.6).
///
/// - `Walk`: no restrictions (default).
/// - `Trail`: no repeated edges.
/// - `Simple`: no repeated vertices.
/// - `Acyclic`: no repeated vertices AND start vertex ≠ end vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMode {
    Walk,
    Trail,
    Simple,
    Acyclic,
}

/// Shortest-path collection mode for `MATCH SHORTEST` / `MATCH ALL SHORTEST` (§16.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortestMode {
    /// `SHORTEST` — return one path at minimum distance.
    One,
    /// `ALL SHORTEST` — return all paths at minimum distance.
    All,
    /// `SHORTEST k N` — return up to N shortest paths (Yen-style).
    K(u32),
    /// `SHORTEST GROUP` — return one shortest path per (source, destination) endpoint pair.
    Group,
}

/// A top-level GQL statement that can be executed against the graph.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    /// A read-only traversal query (`MATCH … RETURN`).
    Query(QueryStmt),
    /// Compound read-only query (`UNION`, `EXCEPT`, `INTERSECT`).
    Compound {
        op: SetOp,
        left: Box<Statement>,
        right: Box<Statement>,
    },
    /// A mutation that creates nodes or edges (`INSERT`).
    /// Multiple patterns separated by commas: `INSERT (a:X), (b:Y)-[:E]->(c:Z)`.
    Create(Vec<CreateStmt>),
    /// `MERGE pattern [ON CREATE SET ...] [ON MATCH SET ...]` — upsert pattern.
    Merge(MergeStmt),
    /// A mutation that removes nodes or edges (`MATCH … DELETE`).
    Delete(DeleteStmt),
    /// A mutation that updates properties/labels (`MATCH … SET`).
    Set(SetStmt),
    /// A mutation that removes properties/labels (`MATCH … REMOVE`).
    Remove(RemoveStmt),
    /// `FINISH` — execute mutations, return empty result (§14.10).
    Finish,
    /// `FILTER [WHERE] condition` — inline filter stage (§14.6).
    Filter(FilterStmt),
    /// `LET var = expr` — computed binding added to binding table (§14.7).
    Let(LetStmt),
    /// `FOR item IN list_expr RETURN ...` — row expansion over a list (§14.8).
    For(ForStmt),
    /// `CALL (<vars>) { <body> }` — inline subquery with outer scope seeding (§15.2).
    Call(CallStmt),
    /// `CALL proc_name(args...) YIELD col [, col]... [WHERE ...] [RETURN ...]`.
    CallProcedure(CallProcedureStmt),
    /// `USE [GRAPH] name` — select the active graph (§16.2).
    UseGraph(String),
    /// `CREATE GRAPH [IF NOT EXISTS] name` — create a named graph via registry (§12).
    CreateGraph { name: String, if_not_exists: bool },
    /// `DROP GRAPH [IF EXISTS] name` — drop a named graph via registry (§12).
    DropGraph { name: String, if_exists: bool },
    /// `CREATE [OR REPLACE] [PROPERTY] GRAPH TYPE [IF NOT EXISTS] name { ... | LIKE source | COPY OF source }` — define a graph type schema (§12).
    CreateGraphType {
        name: String,
        definition: GraphTypeDefinition,
        if_not_exists: bool,
        or_replace: bool,
        /// `LIKE source` or `COPY OF source` — copy definition from an existing graph type.
        source: Option<String>,
    },
    /// `DROP GRAPH TYPE [IF EXISTS] name` — remove a graph type schema (§12).
    DropGraphType { name: String, if_exists: bool },
    /// `CREATE SCHEMA [IF NOT EXISTS] name` — create a schema namespace (§12).
    CreateSchema { name: String, if_not_exists: bool },
    /// `DROP SCHEMA [IF EXISTS] name` — remove a schema namespace (§12).
    DropSchema { name: String, if_exists: bool },
    /// `DESCRIBE GRAPH TYPE name` — introspect a graph type schema (W-D5).
    DescribeGraphType(String),
    /// `CREATE INDEX ON :Label(property)` or `CREATE INDEX ON -[:Label](property)`.
    CreateIndex {
        entity_type: gleaph_types::EntityType,
        property_name: String,
    },
    /// `DROP INDEX ON :Label(property)` or `DROP INDEX ON -[:Label](property)`.
    DropIndex {
        entity_type: gleaph_types::EntityType,
        property_name: String,
    },
    /// `SHOW STATS` / `SHOW INDEXES` / `SHOW GRANTS` / `SHOW METRICS` / `SHOW PLANNER STATS`
    /// / `SHOW SCHEMAS` / `SHOW GRAPH TYPES` / `SHOW QUOTA` / `SHOW ALIASES`.
    Show(ShowTarget),
    /// `GRANT READ|WRITE|ADMIN ON GRAPH TO 'principal'`.
    Grant {
        level: gleaph_types::AccessLevel,
        principal: String,
    },
    /// `REVOKE ACCESS ON GRAPH FROM 'principal'`.
    Revoke { principal: String },
    /// `ANALYZE` — recompute planner statistics.
    Analyze,
    /// `SET TYPE CHECK STRICT|WARNING` — toggle error-mode type checking (§18.9 Phase 3).
    SetTypeCheck(TypeCheckMode),
    /// `CREATE CONSTRAINT name ON (:Label) ASSERT property IS UNIQUE|NOT NULL` (§12).
    CreateConstraint(ConstraintDef),
    /// `DROP CONSTRAINT name` (§12).
    DropConstraint(String),
}

/// Type-check strictness level (§18.9 Phase 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeCheckMode {
    /// Emit type mismatches as informational warnings (default).
    Warning,
    /// Reject queries with provable type mismatches.
    Strict,
}

/// Kind of schema constraint (§12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    /// Property value must be unique among all vertices/edges with the given label.
    Unique,
    /// Property must be present and non-null on all vertices/edges with the given label.
    NotNull,
}

/// Definition of a named constraint (§12).
#[derive(Clone, Debug, PartialEq)]
pub struct ConstraintDef {
    pub name: String,
    pub label: String,
    pub property: String,
    pub kind: ConstraintKind,
}

/// Target of a `SHOW` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShowTarget {
    Stats,
    PlannerStats,
    Indexes,
    Grants,
    Metrics,
    Schemas,
    GraphTypes,
    Quota,
    Aliases,
    Prepared,
    Settings,
    Constraints,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetOp {
    Union,
    UnionAll,
    Except,
    Intersect,
    /// `OTHERWISE` — execute left; if empty rows, execute right (§14.2).
    Otherwise,
    /// `NEXT [YIELD cols]` — pipe left result rows as seeds into right statement (§9.2/§16.14).
    /// `None` = no YIELD clause or `YIELD *` (pass all bindings).
    /// `Some(cols)` = project only the named columns.
    Next(Option<Vec<String>>),
}

/// A read-only query statement: `MATCH … [WHERE …] RETURN … [ORDER BY …] [LIMIT …]`.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryStmt {
    pub match_clauses: Vec<MatchEntry>,
    pub where_clause: Option<WhereClause>,
    pub with_clauses: Vec<WithClause>,
    pub return_clause: ReturnClause,
    pub group_by: Option<Vec<Expr>>,
    pub having: Option<Expr>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<Limit>,
    pub offset: Option<u32>,
    /// Match mode for the entire query (§16.4). `None` means REPEATABLE ELEMENTS (default).
    pub match_mode: Option<MatchMode>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchEntry {
    pub optional: bool,
    pub shortest: bool,
    pub path_variable: Option<String>,
    pub pattern: MatchClause,
    /// Shortest-path mode; `None` when `shortest` is false (§16.6).
    pub shortest_mode: Option<ShortestMode>,
    /// Path traversal mode (§16.6). `None` means WALK (default, no restrictions).
    pub path_mode: Option<PathMode>,
    /// `ANY n PATHS` limit (§16.6). `None` means no limit.
    pub any_paths: Option<u32>,
    /// `KEEP *` or `KEEP var1, var2` — restrict variable scope after pattern matching (§16.4).
    pub keep_clause: Option<KeepClause>,
}

/// Restricts the binding table to specified variables after pattern matching (§16.4).
#[derive(Clone, Debug, PartialEq)]
pub enum KeepClause {
    /// `KEEP *` — retain all bindings (no-op, but explicit).
    All,
    /// `KEEP var1, var2` — retain only the named variables.
    Vars(Vec<String>),
}

/// A `CREATE` statement — either a single node or a node-edge-node triple.
#[derive(Clone, Debug, PartialEq)]
pub enum CreateStmt {
    Node(NodeCreate),
    Edge(Box<EdgeCreate>),
}

/// Creates a single vertex with optional labels and property hints.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeCreate {
    pub node: NodePattern,
}

/// Creates two vertices connected by a directed edge.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeCreate {
    pub left: NodePattern,
    pub edge: EdgePattern,
    pub right: NodePattern,
}

/// `MERGE (n:Label {props}) [ON CREATE SET ...] [ON MATCH SET ...]` — upsert.
#[derive(Clone, Debug, PartialEq)]
pub struct MergeStmt {
    /// The pattern to match or create (node only for now; edge patterns deferred).
    pub create: CreateStmt,
    /// SET items applied only when the pattern is newly created.
    pub on_create_set: Vec<SetItem>,
    /// SET items applied only when the pattern already existed.
    pub on_match_set: Vec<SetItem>,
}

/// A `MATCH … DELETE <var>` statement.
///
/// Requires a WHERE clause; unbounded deletes are rejected during validation.
#[derive(Clone, Debug, PartialEq)]
pub struct DeleteStmt {
    pub match_clause: MatchClause,
    pub where_clause: Option<WhereClause>,
    /// `DETACH DELETE` — also delete incident edges.
    pub detach: bool,
    /// `NODETACH DELETE` — explicitly require no incident edges (error if any exist).
    pub nodetach: bool,
    /// The variable names of the nodes or edges to delete.
    pub target_vars: Vec<String>,
}

/// A `MATCH … [WHERE …] SET …` mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct SetStmt {
    pub match_clause: MatchClause,
    pub where_clause: Option<WhereClause>,
    pub set_clause: SetClause,
}

/// A `MATCH … [WHERE …] REMOVE …` mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoveStmt {
    pub match_clause: MatchClause,
    pub where_clause: Option<WhereClause>,
    pub remove_clause: RemoveClause,
}

/// A `FILTER [WHERE] condition` — inline WHERE filter stage (§14.6).
#[derive(Clone, Debug, PartialEq)]
pub struct FilterStmt {
    pub match_clause: MatchClause,
    pub where_clause: Option<WhereClause>,
    pub filter_expr: WhereClause,
}

/// A `LET var = expr` — computed binding addition (§14.7).
#[derive(Clone, Debug, PartialEq)]
pub struct LetStmt {
    pub match_clause: MatchClause,
    pub where_clause: Option<WhereClause>,
    pub bindings: Vec<(String, Expr)>,
    pub return_clause: ReturnClause,
}

/// A `CALL (<scope_vars>) { <body> }` — inline subquery seeded with outer scope (§15.2).
///
/// The `scope_vars` are variable names from the outer binding table that are made available
/// inside `body`. The result rows are joined back with the outer scope.
#[derive(Clone, Debug, PartialEq)]
pub struct CallStmt {
    /// Variables imported from outer scope into the inner query.
    pub scope_vars: Vec<String>,
    /// The inner statement body.
    pub body: Box<Statement>,
    /// `OPTIONAL CALL` — return empty result on error instead of failing.
    pub optional: bool,
}

/// `CALL proc_name(args...) YIELD col [, col]...` — built-in procedure invocation.
///
/// Optionally followed by MATCH/WHERE/RETURN to pipe results into a GQL query.
#[derive(Clone, Debug, PartialEq)]
pub struct CallProcedureStmt {
    /// Procedure name (e.g. `bfs`, `sssp`, `pagerank`, `recommend`).
    pub procedure: String,
    /// Positional arguments (expressions).
    pub args: Vec<Expr>,
    /// YIELD column names — determines which columns appear in the result.
    /// `None` means YIELD was omitted, so all available columns are returned.
    pub yield_cols: Option<Vec<String>>,
}

/// A `FOR item IN list_expr RETURN ...` — per-element row expansion (§14.8).
///
/// Evaluates `list_expr`, then for each element binds `var` and (optionally) `ordinality_var`
/// (1-based index), then projects the `return_clause`.
#[derive(Clone, Debug, PartialEq)]
pub struct ForStmt {
    /// Variable bound to each element of the list.
    pub var: String,
    /// Expression that evaluates to a `Value::List`.
    pub list_expr: Expr,
    /// Optional variable bound to the 1-based index of each element.
    pub ordinality_var: Option<String>,
    pub return_clause: ReturnClause,
}

/// The pattern after `MATCH`: a starting node followed by zero or more edge-node chains.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchClause {
    pub start: NodePattern,
    pub elements: Vec<PatternElement>,
}

impl MatchClause {
    /// Returns an iterator over only the `Hop` elements (ignoring subpaths).
    /// Useful for code that only cares about simple hop chains.
    pub fn hops(&self) -> impl Iterator<Item = &MatchChain> {
        self.elements.iter().filter_map(|e| match e {
            PatternElement::Hop(c) => Some(c),
            PatternElement::SubPath { .. } => None,
        })
    }

    /// Returns true if all elements are simple hops (no subpaths).
    pub fn is_flat(&self) -> bool {
        self.elements
            .iter()
            .all(|e| matches!(e, PatternElement::Hop(_)))
    }

    /// Returns flat chain references by index. Panics if element at idx is not a Hop.
    pub fn chain(&self, idx: usize) -> &MatchChain {
        match &self.elements[idx] {
            PatternElement::Hop(c) => c,
            PatternElement::SubPath { .. } => panic!("expected Hop at index {idx}, found SubPath"),
        }
    }
}

/// A single element in a MATCH pattern: either a simple hop or a parenthesized subpath group.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternElement {
    /// A simple edge→node hop.
    Hop(MatchChain),
    /// A parenthesized subpath pattern with quantifier (§16.7).
    /// `MATCH (a)((x)-[:E]->(y)){2,4}(b)` — the inner pattern is repeated.
    ///
    /// **WARNING — Computational complexity**: Subpath expansion is O(fan_out^(hops×max)),
    /// which can be exponential. Use with small quantifier ranges on sparse graphs only.
    SubPath {
        /// Start node of the inner pattern.
        inner_start: NodePattern,
        /// The hops inside the subpath.
        inner_elements: Vec<PatternElement>,
        /// Repetition quantifier.
        quantifier: PathLength,
        /// Optional variable binding for the subpath.
        var: Option<String>,
        /// Optional trailing node pattern after the subpath group: `...){2}(b)`.
        /// Constrains/binds the endpoint of the last repetition.
        trailing_node: Option<NodePattern>,
    },
}

/// One hop in a MATCH pattern: an edge and the node it leads to.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchChain {
    pub edge: EdgePattern,
    pub node: NodePattern,
}

/// A node pattern such as `(a:User {id: 1})` in a MATCH or CREATE clause.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodePattern {
    /// Optional variable name bound to this node.
    pub var: Option<String>,
    /// Label constraints (all must match) — legacy colon-separated form.
    pub labels: Vec<String>,
    /// Inline property filters; each value must be a literal.
    pub props_hint: Vec<(String, Expr)>,
    /// Label expression for advanced label matching (§16.8).
    /// When set, takes precedence over `labels`.
    pub label_expr: Option<LabelExpr>,
    /// Inline WHERE predicate inside the node pattern: `(a:Person WHERE a.age > 25)`.
    /// Evaluated after full bindings are built (same semantics as outer WHERE).
    pub where_clause: Option<Box<Expr>>,
    /// Type annotation: `(n :: PersonType)` or `(n :: Person | Company)`.
    /// Mutually exclusive with `labels`/`label_expr`.
    pub type_annotation: Option<TypeExpr>,
}

/// An edge pattern such as `-[e:KNOWS {since: 2020}]->` in a MATCH or CREATE clause.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgePattern {
    /// Optional variable name bound to this edge.
    pub var: Option<String>,
    /// Simple single-label constraint (`[e:KNOWS]`).
    /// Used for CREATE and as the BFS filter label for variable-length paths.
    pub label: Option<String>,
    /// Full label expression for OR/AND/NOT/wildcard matching in MATCH (`[e:A|B]`).
    /// When set, takes precedence over `label` for matching.
    /// Mutually exclusive with `label`: parser sets at most one of the two.
    pub label_expr: Option<LabelExpr>,
    pub direction: Direction,
    pub length: PathLength,
    /// Inline property filters on the edge (§16.8).
    pub properties: Vec<(String, Expr)>,
    /// Inline WHERE predicate inside the edge pattern: `[e:KNOWS WHERE e.since > 2020]`.
    pub where_clause: Option<Box<Expr>>,
    /// Type annotation: `-[e :: KnowsType]->`.
    /// Mutually exclusive with `label`/`label_expr`.
    pub type_annotation: Option<TypeExpr>,
}

/// Traversal direction of an edge pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// `->` pattern.
    Outgoing,
    /// `<-` pattern.
    Incoming,
    /// `-` pattern (undirected / either direction).
    Either,
}

/// Path length specification for an edge pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathLength {
    Fixed(u32),
    Range { min: u32, max: u32 },
}

/// Temporal edge fields available in WHERE predicates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemporalField {
    Timestamp,
}

/// A `WHERE` clause expression.
pub type WhereClause = Expr;

/// String predicate kind for infix string comparisons (`STARTS WITH`, `ENDS WITH`, `CONTAINS`, `LIKE`, `ILIKE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringPredicateKind {
    StartsWith,
    EndsWith,
    Contains,
    /// SQL-style wildcard: `%` = any chars, `_` = exactly one char.
    Like,
    /// Case-insensitive variant of `Like`.
    ILike,
}

/// Comparison operator used in WHERE predicates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A `RETURN` clause listing the expressions to project.
#[derive(Clone, Debug, PartialEq)]
pub struct ReturnClause {
    pub distinct: bool,
    pub items: Vec<ReturnItem>,
    /// `RETURN *` — return all currently-bound variables.
    pub star: bool,
    /// `RETURN NO BINDINGS` — return empty schema (§14.10).
    pub no_bindings: bool,
    /// `FINISH` — execute but return empty result (§14.10).
    pub finish: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WithClause {
    pub items: Vec<ReturnItem>,
    pub distinct: bool,
    /// `WITH *` — pass all currently-bound variables through unchanged.
    pub star: bool,
    /// `WHERE` filter applied to the projected rows immediately after the WITH projection.
    pub where_clause: Option<Expr>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<Limit>,
    pub offset: Option<u32>,
    /// Optional follow-on `MATCH`/`OPTIONAL MATCH` clauses that execute using
    /// the projected rows as seed bindings (`WITH … MATCH …` continuation).
    pub match_clauses: Vec<MatchEntry>,
    /// `WHERE` clause applied after the follow-on match clauses (if any).
    pub post_match_where: Option<Expr>,
}

/// One item in a RETURN clause, optionally renamed with `AS <alias>`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

/// An `ORDER BY` clause with one or more sort keys.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderBy {
    pub items: Vec<OrderByItem>,
}

/// A single sort key in an ORDER BY clause.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    /// `true` for `DESC`, `false` for `ASC` (default).
    pub descending: bool,
    /// `Some(true)` = NULLS FIRST, `Some(false)` = NULLS LAST, `None` = default.
    /// Default: NULLS LAST for ASC, NULLS FIRST for DESC (SQL semantics).
    pub nulls_first: Option<bool>,
}

/// A `LIMIT <n>` clause; the value is capped at `u32::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit(pub u32);

#[derive(Clone, Debug, PartialEq)]
pub struct SetClause {
    pub items: Vec<SetItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SetItem {
    Property {
        var: String,
        property: String,
        value: Expr,
    },
    /// `SET n = { key: expr, ... }` — replace all properties.
    AllProperties {
        var: String,
        properties: Vec<(String, Expr)>,
    },
    Label {
        var: String,
        label: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoveClause {
    pub items: Vec<RemoveItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RemoveItem {
    Property { var: String, property: String },
    Label { var: String, label: String },
}

/// An expression tree for WHERE, RETURN, ORDER BY, and inline property maps.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Value),
    Variable(String),
    PropertyAccess {
        target: Box<Expr>,
        property: String,
    },
    Parameter {
        name: String,
        /// Single type or union of types: `$x :: INT` or `$x :: INT | TEXT`.
        type_annotation: Option<Vec<ValueType>>,
    },
    BinaryOp {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Xor(Box<Expr>, Box<Expr>),
    Compare {
        left: Box<Expr>,
        op: CmpOp,
        right: Box<Expr>,
    },
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `expr STARTS WITH str` / `expr ENDS WITH str` / `expr CONTAINS str`
    StringPredicate {
        expr: Box<Expr>,
        kind: StringPredicateKind,
        pattern: Box<Expr>,
    },
    Case(CaseExpr),
    Coalesce(Vec<Expr>),
    NullIf {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Aggregate(AggregateExpr),
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
    Exists(Box<Statement>),
    Concat(Box<Expr>, Box<Expr>),
    PathVar(String),
    PathLength(Box<Expr>),
    ListLiteral(Vec<Expr>),
    ListIndex {
        list: Box<Expr>,
        index: Box<Expr>,
    },
    /// `CAST(expr AS type)` — type conversion (§20.8).
    Cast {
        expr: Box<Expr>,
        target_type: ValueType,
    },
    /// `expr IS [NOT] TRUE/FALSE/UNKNOWN` — three-valued logic (§20.1).
    IsTruth {
        expr: Box<Expr>,
        negated: bool,
        truth: TruthValue,
    },
    /// `n IS [NOT] LABELED labelExpr` — label predicate (§19.9).
    IsLabeled {
        expr: Box<Expr>,
        negated: bool,
        label_expr: LabelExpr,
    },
    /// `n IS [NOT] SOURCE OF e` — source endpoint predicate (§19.10).
    IsSourceOf {
        node: Box<Expr>,
        negated: bool,
        edge: Box<Expr>,
    },
    /// `n IS [NOT] DESTINATION OF e` — destination endpoint predicate (§19.10).
    IsDestOf {
        node: Box<Expr>,
        negated: bool,
        edge: Box<Expr>,
    },
    /// `e IS [NOT] DIRECTED` — directed edge predicate (§19.8).
    /// All edges in this engine are directed, so IS DIRECTED is always true for edges.
    IsDirected {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `ALL_DIFFERENT(e1, e2, ...)` — all elements are distinct (§19.11).
    AllDifferent(Vec<Expr>),
    /// `SAME(e1, e2, ...)` — all elements are the same (§19.11).
    Same(Vec<Expr>),
    /// `PROPERTY_EXISTS(n, "prop")` — property existence check (§19.13).
    PropertyExists {
        target: Box<Expr>,
        property: String,
    },
    /// Record literal `{key: value, ...}` (§20.18).
    RecordLiteral(Vec<(String, Expr)>),
    /// `expr IS [NOT] :: typename` — runtime type predicate (§19.6).
    IsType {
        expr: Box<Expr>,
        negated: bool,
        /// `Some` for built-in value types, `None` for node type names.
        value_type: Option<ValueType>,
        /// Original type name — used for node type resolution when `value_type` is `None`.
        type_name: String,
    },
    /// `VALUE { query }` — scalar subquery returning one value (§20.6).
    ValueSubquery(Box<Statement>),
    /// `LET x = e1, y = e2 IN body END` — value-expression binding (§20.5).
    LetIn {
        bindings: Vec<(String, Expr)>,
        body: Box<Expr>,
    },
    /// `PATH [n1, e1, n2, ...]` — explicit path constructor (§20.14).
    PathConstructor(Vec<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    /// Concatenate strings with a separator (`STRING_AGG`).
    StringAgg,
    /// Continuous percentile interpolation (`PERCENTILE_CONT`).
    PercentileCont,
    /// Discrete percentile (nearest rank) (`PERCENTILE_DISC`).
    PercentileDisc,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggregateExpr {
    pub func: AggFunc,
    pub expr: Option<Box<Expr>>,
    pub distinct: bool,
    pub count_all: bool,
    /// Optional separator for `STRING_AGG(expr, sep)`.
    pub separator: Option<Box<Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseExpr {
    pub operand: Option<Box<Expr>>,
    pub when_then: Vec<CaseWhenThen>,
    pub else_expr: Option<Box<Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseWhenThen {
    pub when: Expr,
    pub then: Expr,
}

/// Type annotation expression for pattern type constraints.
/// Used in `(n :: PersonType)` and `(n :: PersonType | CompanyType)`.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    /// A single named type, e.g. `:: PersonType`
    Name(String),
    /// Union of two type expressions, e.g. `:: PersonType | CompanyType`
    Union(Box<TypeExpr>, Box<TypeExpr>),
}

/// A property definition inside a node type: `name :: TEXT NOT NULL`.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDef {
    pub name: String,
    pub value_type: ValueType,
    pub required: bool,
}

/// Node type definition (§18.2): maps a type name to a set of labels and optional property schema.
///
/// Used inside `CREATE GRAPH TYPE` with `(PersonType :Person { name :: TEXT NOT NULL })`.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTypeDef {
    pub name: String,
    pub labels: Vec<String>,
    pub properties: Vec<PropertyDef>,
}

/// Edge type definition (§18.3): maps endpoint labels to an edge label and optional property schema.
///
/// Used inside `CREATE GRAPH TYPE` with `(:Person)-[:KNOWS { since :: INT }]->(:Person)`.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTypeDef {
    pub name: String,
    pub label: String,
    pub from_labels: Vec<String>,
    pub to_labels: Vec<String>,
    pub properties: Vec<PropertyDef>,
}

/// Definition of a graph type schema — the set of allowed node and edge labels.
///
/// Used with `CREATE GRAPH TYPE name { (:Person), -[:KNOWS]-> }`.
/// Labels are sorted and deduplicated during parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphTypeDefinition {
    pub node_labels: Vec<String>,
    pub edge_labels: Vec<String>,
    pub node_types: Vec<NodeTypeDef>,
    pub edge_types: Vec<EdgeTypeDef>,
}
