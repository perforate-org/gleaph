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
    pub graph_id: String,
    pub operations: Vec<OperationIr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationIr {
    pub wire_name: String,
    pub type_name: String,
    pub params_type_name: String,
    pub row_type_name: String,
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
