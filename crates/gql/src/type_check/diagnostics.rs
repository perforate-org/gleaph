use crate::token::Span;

use super::{TypeWarning, WarningKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Node,
    Edge,
    Path,
    Value,
    Unknown,
}

pub const DML001_UNSUPPORTED_SET_REPLACE: &str = "DML001";
pub const DML002_TARGET_VALUE: &str = "DML002";
pub const DML003_TARGET_PATH: &str = "DML003";
pub const DML004_TARGET_UNKNOWN: &str = "DML004";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmlDiagnosticSeverity {
    Fatal,
    Warning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeDiagnostic {
    pub code: Option<&'static str>,
    pub message: String,
    pub span: Span,
    pub severity: DiagnosticSeverity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmlDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub severity: DmlDiagnosticSeverity,
}

pub fn dml_diagnostic_severity(code: &str) -> Option<DmlDiagnosticSeverity> {
    match code {
        DML001_UNSUPPORTED_SET_REPLACE | DML002_TARGET_VALUE | DML003_TARGET_PATH => {
            Some(DmlDiagnosticSeverity::Fatal)
        }
        DML004_TARGET_UNKNOWN => Some(DmlDiagnosticSeverity::Warning),
        _ => None,
    }
}

pub fn dml_diagnostic_from_warning(warning: &TypeWarning) -> Option<DmlDiagnostic> {
    let code = warning.code?;
    if warning.kind != WarningKind::DmlTargetMismatch && warning.kind != WarningKind::UnsupportedDml
    {
        return None;
    }
    Some(DmlDiagnostic {
        code,
        message: warning.message.clone(),
        span: warning.span.unwrap_or(Span::DUMMY),
        severity: dml_diagnostic_severity(code)?,
    })
}

pub fn type_diagnostic_from_warning(warning: &TypeWarning) -> TypeDiagnostic {
    TypeDiagnostic {
        code: warning.code,
        message: warning.message.clone(),
        span: warning.span.unwrap_or(Span::DUMMY),
        severity: DiagnosticSeverity::Warning,
    }
}

pub fn dml_target_value_message(op_name: &str, variable: Option<&str>) -> String {
    format!(
        "{op_name} target{} is inferred as a value, not a node/edge",
        format_dml_target_ref(variable)
    )
}

pub fn dml_target_path_message(op_name: &str, variable: Option<&str>) -> String {
    format!(
        "{op_name} target{} is inferred as a path, not a node/edge",
        format_dml_target_ref(variable)
    )
}

pub fn dml_target_unknown_message(op_name: &str, variable: Option<&str>) -> String {
    format!(
        "{op_name} target{} could not be typed statically",
        format_dml_target_ref(variable)
    )
}

pub fn dml_unsupported_set_replace_message(variable: &str) -> String {
    format!("SET {variable} = ... is not yet supported by the executor")
}

fn format_dml_target_ref(variable: Option<&str>) -> String {
    variable
        .map(|name| format!(" `{name}`"))
        .unwrap_or_default()
}
