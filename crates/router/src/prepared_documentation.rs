//! Source documentation extraction for prepared-operation metadata.
//!
//! The GQL parser owns comment lexing and span preservation. This module owns only the
//! prepared-operation convention: ordinary doc-comment lines describe the operation, while
//! @param <name> <text> lines describe one input parameter.

use std::collections::BTreeMap;

use gleaph_gql::ast::ValueType;
use gleaph_gql::ast::{
    BindingTypeAnnotation, ExprKind, GqlProgram, ProcedureBindingInitializer, ProcedureBindingKind,
    Statement,
};
use gleaph_gql::token::DocComment;
use gleaph_gql::type_check::{PropertySchema, Type};
use gleaph_prepared_api::{PreparedOperation, SemanticType};

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

/// Complete and validate parameter metadata declared or inferred by the GQL program.
pub(crate) fn complete_parameter_metadata(
    program: &GqlProgram,
    schema: &dyn PropertySchema,
    operation: &mut PreparedOperation,
) -> Result<(), String> {
    let declarations = collect_typed_parameters(program)?;
    let inferred = gleaph_gql::type_check::infer_parameter_types_with_schema(program, schema);
    let mut parameters = declarations;
    for (name, ty) in inferred {
        let Some(inferred) = semantic_type_from_type(&ty) else {
            return Err(format!(
                "schema-inferred GQL parameter {name:?} has unsupported type {ty:?}"
            ));
        };
        parameters.entry(name).or_insert(inferred);
    }
    for (name, (semantic_type, not_null)) in parameters {
        let Some(parameter) = operation
            .parameters
            .iter_mut()
            .find(|parameter| parameter.name == name)
        else {
            operation.parameters.push(gleaph_prepared_api::Parameter {
                name,
                description: None,
                required: true,
                nullable: !not_null,
                semantic_type,
            });
            continue;
        };
        if parameter.semantic_type != semantic_type {
            return Err(format!(
                "GQL parameter {name:?} has type {:?}, metadata declares {:?}",
                semantic_type, parameter.semantic_type
            ));
        }
        if not_null && parameter.nullable {
            return Err(format!(
                "GQL parameter {name:?} is non-null but metadata permits null"
            ));
        }
    }
    Ok(())
}

fn collect_typed_parameters(
    program: &GqlProgram,
) -> Result<BTreeMap<String, (SemanticType, bool)>, String> {
    let body = program
        .transaction_activity
        .as_ref()
        .and_then(|activity| activity.body.as_ref())
        .ok_or_else(|| "missing transaction body".to_owned())?;
    let mut declarations = BTreeMap::new();
    for statement in body.iter_statements() {
        collect_statement_bindings(statement, &mut declarations)?;
    }
    Ok(declarations)
}

fn collect_statement_bindings(
    statement: &Statement,
    declarations: &mut BTreeMap<String, (SemanticType, bool)>,
) -> Result<(), String> {
    let Statement::Query(query) = statement else {
        return Ok(());
    };
    for linear in std::iter::once(&query.left).chain(query.rest.iter().map(|(_, query)| query)) {
        for binding in &linear.prefix_bindings {
            if binding.kind != ProcedureBindingKind::Value {
                continue;
            }
            let Some(BindingTypeAnnotation::Value(value_type)) = &binding.type_annotation else {
                continue;
            };
            let ProcedureBindingInitializer::Expr(expr) = &binding.initializer else {
                continue;
            };
            let ExprKind::Parameter(parameter) = &expr.kind else {
                continue;
            };
            let name = parameter.trim_start_matches('$');
            if name.is_empty() || name != binding.variable {
                continue;
            }
            let (semantic_type, not_null) = semantic_type_from_value_type(value_type)
                .ok_or_else(|| format!("unsupported typed GQL parameter type {name:?}"))?;
            if declarations
                .insert(name.to_owned(), (semantic_type, not_null))
                .is_some()
            {
                return Err(format!("duplicate typed GQL parameter {name:?}"));
            }
        }
    }
    Ok(())
}

fn semantic_type_from_value_type(value_type: &ValueType) -> Option<(SemanticType, bool)> {
    let (value_type, not_null) = match value_type {
        ValueType::NotNull(inner) => (&**inner, true),
        value_type => (value_type, false),
    };
    let semantic_type = match value_type {
        ValueType::Bool { .. } => SemanticType::Bool,
        ValueType::String { .. } | ValueType::Char { .. } | ValueType::Varchar { .. } => {
            SemanticType::Text
        }
        ValueType::Bytes { .. } | ValueType::Binary { .. } | ValueType::Varbinary { .. } => {
            SemanticType::Bytes
        }
        ValueType::Int8 { .. } => SemanticType::Int8,
        ValueType::Int16 { .. } => SemanticType::Int16,
        ValueType::Int32 { .. } => SemanticType::Int32,
        ValueType::Int64 { .. } => SemanticType::Int64,
        ValueType::Int128 { .. } => SemanticType::Int128,
        ValueType::Int256 { .. } => SemanticType::Int256,
        ValueType::Uint8 { .. } => SemanticType::Uint8,
        ValueType::Uint16 { .. } => SemanticType::Uint16,
        ValueType::Uint32 { .. } => SemanticType::Uint32,
        ValueType::Uint64 { .. } => SemanticType::Uint64,
        ValueType::Uint128 { .. } => SemanticType::Uint128,
        ValueType::Uint256 { .. } => SemanticType::Uint256,
        ValueType::Float16 { .. } => SemanticType::Float16,
        ValueType::Float32 { .. } => SemanticType::Float32,
        ValueType::Float64 { .. } => SemanticType::Float64,
        ValueType::Float128 => SemanticType::Float128,
        ValueType::Float256 => SemanticType::Float256,
        ValueType::Decimal { .. } => SemanticType::Decimal,
        ValueType::Date => SemanticType::Date,
        ValueType::Time => SemanticType::Time,
        ValueType::LocalTime { .. } => SemanticType::LocalTime,
        ValueType::DateTime => SemanticType::DateTime,
        ValueType::LocalDateTime { .. } => SemanticType::LocalDateTime,
        ValueType::ZonedDateTime { .. } => SemanticType::ZonedDateTime,
        ValueType::ZonedTime { .. } => SemanticType::ZonedTime,
        ValueType::Duration | ValueType::DurationYearToMonth | ValueType::DurationDayToSecond => {
            SemanticType::Duration
        }
        ValueType::List { element_type, .. } => {
            let (element, _) = semantic_type_from_value_type(element_type)?;
            SemanticType::List {
                element: Box::new(element),
            }
        }
        ValueType::Path => SemanticType::Path,
        ValueType::Record { fields, .. } => SemanticType::Record {
            fields: fields
                .iter()
                .map(|field| {
                    let (semantic_type, nullable) =
                        semantic_type_from_value_type(&field.value_type)?;
                    Some(gleaph_prepared_api::RecordField {
                        name: field.name.clone(),
                        semantic_type,
                        nullable: !nullable,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
        },
        _ => return None,
    };
    Some((semantic_type, not_null))
}

fn semantic_type_from_type(ty: &Type) -> Option<(SemanticType, bool)> {
    match ty {
        Type::NonNull(inner) => {
            let (semantic_type, _) = semantic_type_from_type(inner)?;
            Some((semantic_type, true))
        }
        Type::Scalar(value_type) => semantic_type_from_value_type(value_type),
        Type::TypedList(element) => {
            let (element, _) = semantic_type_from_type(element)?;
            Some((
                SemanticType::List {
                    element: Box::new(element),
                },
                false,
            ))
        }
        Type::Record(fields) => Some((
            SemanticType::Record {
                fields: fields
                    .iter()
                    .map(|(name, field_type)| {
                        let (semantic_type, not_null) = semantic_type_from_type(field_type)?;
                        Some(gleaph_prepared_api::RecordField {
                            name: name.clone(),
                            semantic_type,
                            nullable: !not_null,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            },
            false,
        )),
        Type::Path(_) => Some((SemanticType::Path, false)),
        Type::Union(_) | Type::Unknown | Type::Never | Type::Node(_) | Type::Edge(_) => None,
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
    use gleaph_gql::type_check::NoSchema;
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

    #[test]
    fn validates_typed_external_parameter_against_metadata() {
        let program =
            gleaph_gql::parser::parse("VALUE term TYPED STRING = $term MATCH (n) RETURN n")
                .expect("typed value binding should parse");
        let mut operation = operation();
        operation.parameters[0].semantic_type = SemanticType::Text;
        complete_parameter_metadata(&program, &NoSchema, &mut operation)
            .expect("matching type should pass");
        operation.parameters[0].semantic_type = SemanticType::Int32;
        let error = complete_parameter_metadata(&program, &NoSchema, &mut operation)
            .expect_err("type mismatch");
        assert!(error.contains("GQL parameter"));
    }

    #[test]
    fn adds_schema_inferred_parameter_to_metadata() {
        struct UserSchema;

        impl PropertySchema for UserSchema {
            fn node_property_types(&self, labels: &[String]) -> Vec<(String, ValueType, bool)> {
                if labels.iter().any(|label| label == "User") {
                    vec![(
                        "age".into(),
                        ValueType::Int32 {
                            keyword: gleaph_gql::ast::Keyword::new("INT32"),
                        },
                        true,
                    )]
                } else {
                    vec![]
                }
            }

            fn edge_property_types(&self, _label: &str) -> Vec<(String, ValueType, bool)> {
                vec![]
            }
        }

        let program = gleaph_gql::parser::parse("MATCH (u:User) WHERE u.age = $age RETURN u.age")
            .expect("query should parse");
        let mut operation = operation();
        operation.parameters.clear();
        complete_parameter_metadata(&program, &UserSchema, &mut operation)
            .expect("schema inference should complete metadata");
        assert_eq!(operation.parameters[0].name, "age");
        assert_eq!(operation.parameters[0].semantic_type, SemanticType::Int32);
        assert!(operation.parameters[0].required);
        assert!(!operation.parameters[0].nullable);
    }
}
