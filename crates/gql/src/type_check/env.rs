//! Type checking environment and warning types.

use rapidhash::{RapidHashMap, RapidHashSet};

use crate::token::Span;

use super::schema::PropertySchema;
use super::types::{Type, types_broadly_same_category};

/// Source location of a type-check warning for diagnostic tooling.
#[derive(Clone, Debug, PartialEq)]
pub enum WarningProvenance {
    /// From constraint solving (constraint index).
    Constraint(usize),
    /// From binding validation (variable name).
    Binding(String),
    /// From endpoint contradiction checking.
    EndpointCheck { edge_label: String },
    /// From aggregation boundary validation.
    AggregationBoundary,
}

/// A type-check warning emitted during static analysis.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeWarning {
    pub code: Option<&'static str>,
    pub message: String,
    pub kind: WarningKind,
    pub span: Option<Span>,
    pub provenance: Option<WarningProvenance>,
}

/// Category of type warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// Incompatible operands for a binary operator (e.g. `42 + 'hello'`).
    BinaryOpMismatch,
    /// WHERE/FILTER condition inferred as non-boolean.
    NonBooleanCondition,
    /// Function argument type mismatch (e.g. `id('hello')`).
    FunctionArgMismatch,
    /// Both sides of a comparison are known and incompatible.
    ComparisonMismatch,
    /// IS NULL applied to a NOT NULL property (always false).
    NullCheckOnNonNull,
    /// Pattern endpoints contradict active graph type edge constraints.
    ImpossiblePattern,
    /// Expression appears in aggregate projection but is neither grouped nor aggregated.
    GroupingViolation,
    /// Value assigned to a property is incompatible with the schema-declared type.
    PropertyTypeMismatch,
    /// CASE/COALESCE branches return incompatible types.
    CaseBranchTypeMismatch,
    /// Variable bound multiple times in the same scope with different types.
    VariableRedefinition,
    /// LIMIT/OFFSET expression is non-numeric.
    NonNumericLimitOffset,
    /// A required property is missing from an INSERT statement.
    MissingRequiredProperty,
    /// DML target is not a node or edge, or could not be typed statically.
    DmlTargetMismatch,
    /// DML form is recognized but not supported by the current executor/planner stack.
    UnsupportedDml,
    /// UNION/EXCEPT/INTERSECT columns have incompatible types.
    SetOpColumnMismatch,
    /// Edge pattern direction disagrees with graph schema (`DIRECTED` vs `UNDIRECTED` edge type).
    SchemaEdgeDirectionMismatch,
}

/// Typing environment: maps variable names to inferred types.
pub(crate) struct TypeEnv<'a> {
    pub bindings: RapidHashMap<String, Type>,
    pub warnings: Vec<TypeWarning>,
    pub schema: &'a dyn PropertySchema,
    /// Variables introduced by OPTIONAL MATCH — property access strips NonNull.
    pub optional_vars: RapidHashSet<String>,
    /// `(var, property)` pairs narrowed to non-null by flow-sensitive WHERE analysis.
    pub narrowed_nonnull: RapidHashSet<(String, String)>,
    /// Variables whose label sets were narrowed by WHERE predicates.
    pub narrowed_labels: RapidHashMap<String, Vec<String>>,
    /// Edge variables narrowed to a specific label by `type(e) = 'X'` in WHERE (Cypher extension).
    #[cfg(feature = "cypher")]
    pub narrowed_edge_labels: RapidHashMap<String, String>,
}

impl<'a> TypeEnv<'a> {
    pub fn new(schema: &'a dyn PropertySchema) -> Self {
        Self {
            bindings: RapidHashMap::default(),
            warnings: Vec::new(),
            schema,
            optional_vars: RapidHashSet::default(),
            narrowed_nonnull: RapidHashSet::default(),
            narrowed_labels: RapidHashMap::default(),
            #[cfg(feature = "cypher")]
            narrowed_edge_labels: RapidHashMap::default(),
        }
    }

    /// Create an independent copy for subquery inference.
    /// Inherits bindings and schema but has its own warning list.
    pub fn fork(&self) -> Self {
        Self {
            bindings: self.bindings.clone(),
            warnings: Vec::new(),
            schema: self.schema,
            optional_vars: self.optional_vars.clone(),
            narrowed_nonnull: self.narrowed_nonnull.clone(),
            narrowed_labels: self.narrowed_labels.clone(),
            #[cfg(feature = "cypher")]
            narrowed_edge_labels: self.narrowed_edge_labels.clone(),
        }
    }

    pub fn bind(&mut self, name: String, ty: Type) {
        if let Some(existing) = self.bindings.get(&name) {
            // Only warn if the types are from different categories.
            // Same-category rebinding is normal in subqueries and NEXT YIELD.
            if !types_broadly_same_category(existing, &ty)
                && !matches!(existing, Type::Unknown)
                && !matches!(&ty, Type::Unknown)
            {
                self.warnings.push(TypeWarning {
                    code: None,
                    message: format!("variable `{name}` already bound with a different type"),
                    kind: WarningKind::VariableRedefinition,
                    span: None,
                    provenance: Some(WarningProvenance::Binding(name.clone())),
                });
            }
        }
        self.bindings.insert(name, ty);
    }

    pub fn get(&self, name: &str) -> Type {
        self.bindings.get(name).cloned().unwrap_or(Type::Unknown)
    }

    pub fn warn(&mut self, kind: WarningKind, message: String) {
        self.warnings.push(TypeWarning {
            code: None,
            message,
            kind,
            span: None,
            provenance: None,
        });
    }

    pub fn warn_at(&mut self, kind: WarningKind, message: String, span: Span) {
        self.warnings.push(TypeWarning {
            code: None,
            message,
            kind,
            span: Some(span),
            provenance: None,
        });
    }

    #[allow(dead_code)]
    /// Create a snapshot of current bindings for scoped inference (e.g., LET-IN).
    /// Returns the previous bindings so they can be restored.
    pub fn snapshot_bindings(&self) -> RapidHashMap<String, Type> {
        self.bindings.clone()
    }

    /// Restore bindings from a snapshot.
    pub fn restore_bindings(&mut self, snapshot: RapidHashMap<String, Type>) {
        self.bindings = snapshot;
    }

    #[allow(dead_code)]
    pub fn warn_with_provenance(
        &mut self,
        kind: WarningKind,
        message: String,
        provenance: WarningProvenance,
    ) {
        self.warnings.push(TypeWarning {
            code: None,
            message,
            kind,
            span: None,
            provenance: Some(provenance),
        });
    }

    pub fn warn_at_with_provenance(
        &mut self,
        kind: WarningKind,
        message: String,
        span: Span,
        provenance: WarningProvenance,
    ) {
        self.warnings.push(TypeWarning {
            code: None,
            message,
            kind,
            span: Some(span),
            provenance: Some(provenance),
        });
    }

    pub fn warn_at_with_code(
        &mut self,
        kind: WarningKind,
        code: &'static str,
        message: String,
        span: Span,
    ) {
        self.warnings.push(TypeWarning {
            code: Some(code),
            message,
            kind,
            span: Some(span),
            provenance: None,
        });
    }
}
