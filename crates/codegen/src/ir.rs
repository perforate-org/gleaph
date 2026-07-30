//! Normalized intermediate representation shared by all code-generation profiles.
//!
//! The IR resolves graph and operation identity once while preserving the manifest's wire names
//! and semantic metadata. Language renderers must consume this representation instead of
//! interpreting the input manifest independently.

use crate::common::pascal_case;
use crate::{ManifestError, PreparedManifest, PreparedOperation};

/// Normalized, renderer-independent representation of a prepared manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodegenIr {
    /// Stable identifier of the logical graph targeted by the generated client.
    pub graph_id: String,
    /// Normalized operations available to renderers.
    pub operations: Vec<OperationIr>,
}

/// Normalized metadata and generated identifiers for one operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationIr {
    /// Original operation name used on the wire.
    pub wire_name: String,
    /// Generated type name for the operation's client API.
    pub type_name: String,
    /// Generated type name for the operation's parameters.
    pub params_type_name: String,
    /// Generated type name for one result row.
    pub row_type_name: String,
    /// Validated manifest metadata for the operation.
    pub operation: PreparedOperation,
}

impl PreparedManifest {
    /// Validate and normalize a manifest before handing it to a language renderer.
    pub fn normalize(&self) -> Result<CodegenIr, ManifestError> {
        self.validate()?;
        Ok(CodegenIr {
            graph_id: self.graph.id.clone(),
            operations: self
                .operations
                .iter()
                .map(|operation| {
                    let type_name = pascal_case(&operation.name);
                    OperationIr {
                        wire_name: operation.name.clone(),
                        params_type_name: format!("{type_name}Params"),
                        row_type_name: format!("{type_name}Row"),
                        type_name,
                        operation: operation.clone(),
                    }
                })
                .collect(),
        })
    }
}
