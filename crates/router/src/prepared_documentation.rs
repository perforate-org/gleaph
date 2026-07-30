//! Source documentation extraction for prepared-operation metadata.
//!
//! The GQL parser owns comment lexing and span preservation. This module owns only the
//! prepared-operation convention: ordinary doc-comment lines describe the operation, while
//! @param <name> <text> lines describe one input parameter.

use std::collections::BTreeMap;

use gleaph_gql::token::DocComment;
use gleaph_prepared_api::PreparedOperation;

/// Apply GQL source documentation to metadata fields that were not explicitly supplied.
pub(crate) fn apply_to_operation(comments: &[DocComment], operation: &mut PreparedOperation) {
    let documentation = parse_comments(comments);
    if operation.description.is_none() {
        operation.description = documentation.operation;
    }
    for parameter in &mut operation.parameters {
        if parameter.description.is_none() {
            parameter.description = documentation.parameters.get(&parameter.name).cloned();
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Documentation {
    operation: Option<String>,
    parameters: BTreeMap<String, String>,
}

fn parse_comments(comments: &[DocComment]) -> Documentation {
    let mut operation_lines = Vec::new();
    let mut parameters = BTreeMap::new();

    for comment in comments {
        let text = comment.text.trim();
        if let Some(parameter) = text.strip_prefix("@param") {
            let parameter = parameter.trim_start();
            let Some((name, description)) = parameter.split_once(char::is_whitespace) else {
                continue;
            };
            let name = name.trim();
            let description = description.trim();
            if !name.is_empty() && !description.is_empty() {
                parameters.insert(name.to_owned(), description.to_owned());
            }
        } else if !text.is_empty() {
            operation_lines.push(text.to_owned());
        }
    }

    Documentation {
        operation: (!operation_lines.is_empty()).then(|| operation_lines.join("\n")),
        parameters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_prepared_api::{OperationKind, Parameter, ResultSchema, SemanticType};

    fn operation() -> PreparedOperation {
        PreparedOperation {
            name: "find-users".into(),
            description: None,
            kind: OperationKind::Query,
            parameters: vec![
                Parameter {
                    name: "term".into(),
                    description: None,
                    required: true,
                    nullable: false,
                    semantic_type: SemanticType::Text,
                },
                Parameter {
                    name: "limit".into(),
                    description: Some("explicit".into()),
                    required: true,
                    nullable: false,
                    semantic_type: SemanticType::Uint32,
                },
            ],
            result: ResultSchema { columns: vec![] },
            supports_consistency: false,
            supports_idempotency: false,
            allowed_sorts: vec![],
        }
    }

    fn comment(text: &str, start: usize) -> DocComment {
        DocComment {
            span: gleaph_gql::token::Span {
                start,
                end: start + text.len(),
            },
            text: text.into(),
        }
    }

    #[test]
    fn applies_operation_and_parameter_docs() {
        let comments = vec![
            comment(" Find users.", 0),
            comment(" @param term Search keyword.", 20),
        ];
        let mut operation = operation();
        apply_to_operation(&comments, &mut operation);
        assert_eq!(operation.description.as_deref(), Some("Find users."));
        assert_eq!(
            operation.parameters[0].description.as_deref(),
            Some("Search keyword.")
        );
    }

    #[test]
    fn explicit_metadata_wins_over_source_docs() {
        let comments = vec![
            comment(" Source operation.", 0),
            comment(" @param limit Source limit.", 20),
        ];
        let mut operation = operation();
        operation.description = Some("explicit operation".into());
        apply_to_operation(&comments, &mut operation);
        assert_eq!(operation.description.as_deref(), Some("explicit operation"));
        assert_eq!(
            operation.parameters[1].description.as_deref(),
            Some("explicit")
        );
    }

    #[test]
    fn ignores_unknown_parameters_and_malformed_tags() {
        let comments = vec![
            comment(" @param unknown Hidden.", 0),
            comment(" @param", 20),
            comment(" ordinary text", 30),
        ];
        let mut operation = operation();
        apply_to_operation(&comments, &mut operation);
        assert_eq!(operation.description.as_deref(), Some("ordinary text"));
        assert!(operation.parameters[0].description.is_none());
        assert_eq!(
            operation.parameters[1].description.as_deref(),
            Some("explicit")
        );
    }
}
