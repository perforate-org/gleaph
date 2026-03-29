//! Schema interface for property-type-aware type checking.

use crate::ast::ValueType;

/// Abstract interface for schema-aware property type lookups.
///
/// Implementations provide information about node/edge property types and
/// endpoint constraints, enabling the type checker to infer property types
/// from MATCH patterns.
pub trait PropertySchema {
    /// Return property types for nodes with the given labels.
    ///
    /// Each tuple is `(property_name, value_type, required)`.
    fn node_property_types(&self, labels: &[String]) -> Vec<(String, ValueType, bool)>;

    /// Return property types for edges with the given label.
    fn edge_property_types(&self, label: &str) -> Vec<(String, ValueType, bool)>;

    /// Return endpoint constraints for edges with the given label.
    ///
    /// Each tuple is `(from_labels, to_labels)`.
    fn edge_endpoint_types(&self, _label: &str) -> Vec<(Vec<String>, Vec<String>)> {
        vec![]
    }

    /// Resolve a node type name to a set of labels.
    fn resolve_node_type_labels(&self, _type_name: &str) -> Option<Vec<String>> {
        None
    }

    /// Resolve an edge type name to `(label, from_labels, to_labels)`.
    fn resolve_edge_type(&self, _type_name: &str) -> Option<(String, Vec<String>, Vec<String>)> {
        None
    }

    /// Return the signature for a stored procedure/catalog function.
    ///
    /// `params` are `(name, type)` for input arguments.
    /// `yields` are `(name, type)` for YIELD columns.
    fn procedure_signature(&self, _name: &str) -> Option<ProcedureSignature> {
        None
    }
}

/// Signature of a stored procedure.
#[derive(Clone, Debug)]
pub struct ProcedureSignature {
    /// Input parameters: `(name, type)`.
    pub params: Vec<(String, ValueType)>,
    /// YIELD columns: `(name, type)`.
    pub yields: Vec<(String, ValueType)>,
}

/// No-op schema: all properties are unknown (open-world assumption).
pub struct NoSchema;

impl PropertySchema for NoSchema {
    fn node_property_types(&self, _labels: &[String]) -> Vec<(String, ValueType, bool)> {
        vec![]
    }

    fn edge_property_types(&self, _label: &str) -> Vec<(String, ValueType, bool)> {
        vec![]
    }
}
