//! Source documentation extraction for prepared-operation metadata.
//!
//! The GQL parser owns comment lexing and span preservation. This module owns only the
//! prepared-operation convention: ordinary doc-comment lines describe the operation, while
//! @param <name> <text> lines describe one input parameter.

use std::collections::BTreeMap;

use gleaph_gql::ast::ValueType;
use gleaph_gql::ast::{
    BindingTypeAnnotation, Expr, ExprKind, GqlProgram, LimitClause, OffsetClause,
    ProcedureBindingInitializer, ProcedureBindingKind, ResultStatement, ReturnBody, SearchProvider,
    SelectBody, SimpleQueryStatement, Statement,
};
use gleaph_gql::token::DocComment;
use gleaph_gql::type_check::{PropertySchema, Type};
use gleaph_prepared_api::{Column, PreparedOperation, SemanticType};

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
    let structural = collect_structural_parameters(program)?;
    let mut parameters = declarations;
    for (name, ty) in inferred {
        let Some(inferred) = semantic_type_from_type(&ty) else {
            return Err(format!(
                "schema-inferred GQL parameter {name:?} has unsupported type {ty:?}"
            ));
        };
        parameters.entry(name).or_insert(inferred);
    }
    for (name, (semantic_type, not_null)) in structural {
        match parameters.get(&name) {
            Some((existing, _)) if *existing != semantic_type => {
                return Err(format!(
                    "GQL parameter {name:?} has conflicting types: structural usage requires \
                     {semantic_type:?}, program declares {existing:?}"
                ));
            }
            None => {
                parameters.insert(name, (semantic_type, not_null));
            }
            _ => {}
        }
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

/// Complete and validate result-schema metadata inferred from the GQL program output.
///
/// Output columns whose type has no [`SemanticType`] mapping (vertex, edge, union, unknown) are
/// omitted from an empty result schema; an explicit schema is validated fail-closed against the
/// inferred output, mirroring [`complete_parameter_metadata`]. A program whose final statement is
/// not a query reports environment bindings rather than result rows, so completion is skipped.
/// Current DML-tail bindings are vertex/edge-only (unmappable), so the gate also guards future
/// type-checker changes that could surface scalar bindings from a DML tail.
pub(crate) fn complete_result_schema(
    program: &GqlProgram,
    schema: &dyn PropertySchema,
    operation: &mut PreparedOperation,
) -> Result<(), String> {
    let block = program
        .transaction_activity
        .as_ref()
        .and_then(|activity| activity.body.as_ref())
        .ok_or_else(|| "missing transaction body".to_owned())?;
    let final_statement = block
        .next
        .last()
        .map_or(&block.first, |next| &next.statement);
    if !matches!(final_statement, Statement::Query(_)) {
        return Ok(());
    }
    let inferred =
        gleaph_gql::type_check::infer_statement_block_output_types_with_schema(block, schema);
    if operation.result.columns.is_empty() {
        operation.result.columns = inferred
            .into_iter()
            .filter_map(|(name, ty)| {
                let (semantic_type, not_null) = semantic_type_from_type(&ty)?;
                Some(Column {
                    name,
                    semantic_type,
                    nullable: !not_null,
                })
            })
            .collect();
        return Ok(());
    }
    for column in &operation.result.columns {
        let Some((_, ty)) = inferred.iter().find(|(name, _)| name == &column.name) else {
            return Err(format!(
                "result column {:?} is not produced by the program output",
                column.name
            ));
        };
        let (semantic_type, not_null) = semantic_type_from_type(ty).ok_or_else(|| {
            format!(
                "result column {:?} has unsupported type {ty:?}",
                column.name
            )
        })?;
        if column.semantic_type != semantic_type {
            return Err(format!(
                "result column {:?} has type {:?}, metadata declares {:?}",
                column.name, semantic_type, column.semantic_type
            ));
        }
        if not_null && column.nullable {
            return Err(format!(
                "result column {:?} is non-null but metadata permits null",
                column.name
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

/// Collect parameters that appear only in structural (non-expression) positions, where the
/// language fixes their type: `LIMIT`/`OFFSET` counts are integers and a vector `SEARCH ... FOR`
/// probe is bytes. Schema-based constraint inference only sees expression positions, so these
/// parameters would otherwise be omitted from the manifest even though plan execution requires
/// the caller to supply them.
fn collect_structural_parameters(
    program: &GqlProgram,
) -> Result<BTreeMap<String, (SemanticType, bool)>, String> {
    let body = program
        .transaction_activity
        .as_ref()
        .and_then(|activity| activity.body.as_ref())
        .ok_or_else(|| "missing transaction body".to_owned())?;
    let mut parameters = BTreeMap::new();
    for statement in body.iter_statements() {
        collect_statement_structural_parameters(statement, &mut parameters)?;
    }
    Ok(parameters)
}

fn collect_statement_structural_parameters(
    statement: &Statement,
    parameters: &mut BTreeMap<String, (SemanticType, bool)>,
) -> Result<(), String> {
    let Statement::Query(query) = statement else {
        return Ok(());
    };
    for linear in std::iter::once(&query.left).chain(query.rest.iter().map(|(_, query)| query)) {
        for part in &linear.parts {
            match part {
                SimpleQueryStatement::Limit(clause) => {
                    collect_structural_parameter(&clause.count, SemanticType::Int64, parameters)?;
                }
                SimpleQueryStatement::Offset(clause) => {
                    collect_structural_parameter(&clause.count, SemanticType::Int64, parameters)?;
                }
                SimpleQueryStatement::Search(search) => {
                    let SearchProvider::VectorIndex(spec) = &search.provider;
                    collect_structural_parameter(&spec.query, SemanticType::Bytes, parameters)?;
                    collect_structural_parameter(&spec.limit, SemanticType::Int64, parameters)?;
                }
                _ => {}
            }
        }
        // RETURN/SELECT bodies carry their own LIMIT/OFFSET clauses (the common position for
        // `... LIMIT n OFFSET $p` tails), distinct from standalone LIMIT/OFFSET parts.
        let Some(result) = &linear.result else {
            continue;
        };
        match result {
            ResultStatement::Return(statement) => {
                let ReturnBody::Items { limit, offset, .. } = &statement.body else {
                    continue;
                };
                collect_result_limit_offset(limit.as_ref(), offset.as_ref(), parameters)?;
            }
            ResultStatement::Select(statement) => match &statement.body {
                SelectBody::Star { limit, offset, .. }
                | SelectBody::Items { limit, offset, .. } => {
                    collect_result_limit_offset(limit.as_ref(), offset.as_ref(), parameters)?;
                }
            },
            ResultStatement::Finish => {}
        }
    }
    Ok(())
}

fn collect_result_limit_offset(
    limit: Option<&LimitClause>,
    offset: Option<&OffsetClause>,
    parameters: &mut BTreeMap<String, (SemanticType, bool)>,
) -> Result<(), String> {
    if let Some(clause) = limit {
        collect_structural_parameter(&clause.count, SemanticType::Int64, parameters)?;
    }
    if let Some(clause) = offset {
        collect_structural_parameter(&clause.count, SemanticType::Int64, parameters)?;
    }
    Ok(())
}

/// Record one structural parameter reference with its language-fixed type, rejecting a reference
/// that is bound to a different type by another structural position.
fn collect_structural_parameter(
    expr: &Expr,
    semantic_type: SemanticType,
    parameters: &mut BTreeMap<String, (SemanticType, bool)>,
) -> Result<(), String> {
    let ExprKind::Parameter(parameter) = &expr.kind else {
        return Ok(());
    };
    let name = parameter.trim_start_matches('$');
    if name.is_empty() {
        return Err(format!("empty GQL parameter reference {parameter:?}"));
    }
    match parameters.get(name) {
        Some((existing, _)) if *existing != semantic_type => Err(format!(
            "GQL parameter {name:?} is used with conflicting structural types {existing:?} and \
             {semantic_type:?}"
        )),
        _ => {
            parameters
                .entry(name.to_owned())
                .or_insert((semantic_type, true));
            Ok(())
        }
    }
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
    use gleaph_prepared_api::{Column, OperationKind, Parameter, ResultSchema, SemanticType};

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

    #[test]
    fn adds_offset_parameter_from_structural_position() {
        let program = gleaph_gql::parser::parse(
            "MATCH (p:Post) RETURN p.body AS body LIMIT 20 OFFSET $offset",
        )
        .expect("query should parse");
        let mut operation = operation();
        operation.parameters.clear();
        complete_parameter_metadata(&program, &NoSchema, &mut operation)
            .expect("structural completion should succeed");
        assert_eq!(operation.parameters.len(), 1);
        assert_eq!(operation.parameters[0].name, "offset");
        assert_eq!(operation.parameters[0].semantic_type, SemanticType::Int64);
        assert!(operation.parameters[0].required);
        assert!(!operation.parameters[0].nullable);
    }

    #[test]
    fn adds_search_vector_and_offset_parameters() {
        let program = gleaph_gql::parser::parse(
            "MATCH (p:Post) SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) \
             DISTANCE AS distance RETURN p.body AS body ORDER BY distance ASC LIMIT 20 OFFSET $offset",
        )
        .expect("search query should parse");
        let mut operation = operation();
        operation.parameters.clear();
        complete_parameter_metadata(&program, &NoSchema, &mut operation)
            .expect("structural completion should succeed");
        let query = operation
            .parameters
            .iter()
            .find(|parameter| parameter.name == "query")
            .expect("query parameter");
        assert_eq!(query.semantic_type, SemanticType::Bytes);
        assert!(query.required);
        let offset = operation
            .parameters
            .iter()
            .find(|parameter| parameter.name == "offset")
            .expect("offset parameter");
        assert_eq!(offset.semantic_type, SemanticType::Int64);
    }

    #[test]
    fn rejects_conflicting_structural_parameter_type() {
        let program = gleaph_gql::parser::parse(
            "VALUE offset TYPED STRING = $offset MATCH (n) RETURN n LIMIT 20 OFFSET $offset",
        )
        .expect("query should parse");
        let mut operation = operation();
        operation.parameters.clear();
        let error = complete_parameter_metadata(&program, &NoSchema, &mut operation)
            .expect_err("conflicting structural types must fail");
        assert!(error.contains("conflicting"));
    }

    #[test]
    fn completes_offset_for_realistic_fan_out_query() {
        // The social-demo home-feed shape: `OPTIONAL MATCH` chain with the LIMIT/OFFSET clauses on
        // the final RETURN body. The structural collection must see through both.
        let program = gleaph_gql::parser::parse(
            "MATCH (u:User {user_id: 'alice'})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User) \
             WHERE p.is_public = TRUE OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post)<-[:POSTED]-(parent_author:User) \
             RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, \
             parent_author.name AS parent_author_name, parent.body AS parent_body, \
             parent.created_at AS parent_created_at, author.name AS author_name, \
             p.body AS body, p.created_at AS created_at ORDER BY INSERTION(e) DESC \
             LIMIT 20 OFFSET $offset",
        )
        .expect("query should parse");
        let mut operation = operation();
        operation.parameters.clear();
        complete_parameter_metadata(&program, &NoSchema, &mut operation)
            .expect("structural completion should succeed");
        let offset = operation
            .parameters
            .iter()
            .find(|parameter| parameter.name == "offset")
            .expect("offset parameter");
        assert_eq!(offset.semantic_type, SemanticType::Int64);
        assert!(offset.required);
    }

    #[test]
    fn fills_empty_result_columns_from_scalar_output() {
        let program = gleaph_gql::parser::parse("MATCH (n) RETURN 'ok' AS tag")
            .expect("literal output should parse");
        let mut operation = operation();
        complete_result_schema(&program, &NoSchema, &mut operation)
            .expect("literal output should complete");
        assert_eq!(operation.result.columns.len(), 1);
        assert_eq!(operation.result.columns[0].name, "tag");
        assert_eq!(
            operation.result.columns[0].semantic_type,
            SemanticType::Text
        );
    }

    #[test]
    fn omits_unmappable_result_columns() {
        let program =
            gleaph_gql::parser::parse("MATCH (n) RETURN n AS name").expect("node return parses");
        let mut operation = operation();
        complete_result_schema(&program, &NoSchema, &mut operation)
            .expect("node output completes with omitted columns");
        assert!(operation.result.columns.is_empty());
    }

    #[test]
    fn fills_result_columns_from_typed_graph_properties() {
        // Regression for the social demo: the graph type must declare its property types,
        // otherwise property accesses infer as Unknown and the manifest result schema stays
        // empty (generated clients then cannot decode rows into typed values).
        let definition = gleaph_gql::parser::parse(
            "CREATE GRAPH TYPE SocialGraph { \
             NODE User { user_id STRING, name STRING }, \
             NODE Post { demo_id INT64, body STRING, created_at DATETIME, is_public BOOL } \
             }",
        )
        .expect("graph type should parse");
        let schema = graph_type_schema(&definition);
        let program = gleaph_gql::parser::parse(
            "MATCH (p:Post)<-[:POSTED]-(author:User) \
             RETURN p.demo_id AS post_id, author.name AS author_name, p.body AS body, \
             p.created_at AS created_at ORDER BY INSERTION(e) DESC LIMIT 20 OFFSET $offset",
        )
        .expect("timeline query should parse");
        let mut operation = operation();
        complete_result_schema(&program, &schema, &mut operation)
            .expect("typed schema should complete result columns");
        let by_name: std::collections::BTreeMap<_, _> = operation
            .result
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.semantic_type.clone()))
            .collect();
        assert_eq!(by_name.get("post_id"), Some(&SemanticType::Int64));
        assert_eq!(by_name.get("author_name"), Some(&SemanticType::Text));
        assert_eq!(by_name.get("body"), Some(&SemanticType::Text));
        assert_eq!(by_name.get("created_at"), Some(&SemanticType::DateTime));
    }

    #[test]
    fn fills_result_columns_across_optional_match_anchor() {
        // Regression for the social demo home feed: `p` is the OPTIONAL MATCH anchor and also
        // carries RETURNed properties (p.demo_id, p.body, p.created_at). Property access on an
        // optional anchor must still resolve through the schema (it only strips non-null).
        let definition = gleaph_gql::parser::parse(
            "CREATE GRAPH TYPE SocialGraph { \
             NODE User { user_id STRING, name STRING }, \
             NODE Post { demo_id INT64, body STRING, created_at DATETIME, is_public BOOL } \
             }",
        )
        .expect("graph type should parse");
        let schema = graph_type_schema(&definition);
        let program = gleaph_gql::parser::parse(
            "MATCH (u:User {user_id: 'alice'})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User) \
             WHERE p.is_public = TRUE \
             OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post)<-[:POSTED]-(parent_author:User) \
             RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, \
             parent_author.name AS parent_author_name, parent.body AS parent_body, \
             parent.created_at AS parent_created_at, author.name AS author_name, \
             p.body AS body, p.created_at AS created_at ORDER BY INSERTION(e) DESC \
             LIMIT 20 OFFSET $offset",
        )
        .expect("home feed query should parse");
        let mut operation = operation();
        complete_result_schema(&program, &schema, &mut operation)
            .expect("typed schema should complete result columns");
        let by_name: std::collections::BTreeMap<_, _> = operation
            .result
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.semantic_type.clone()))
            .collect();
        assert_eq!(by_name.get("post_id"), Some(&SemanticType::Int64));
        assert_eq!(by_name.get("parent_post_id"), Some(&SemanticType::Int64));
        assert_eq!(by_name.get("author_name"), Some(&SemanticType::Text));
        assert_eq!(by_name.get("parent_author_name"), Some(&SemanticType::Text));
        assert_eq!(by_name.get("body"), Some(&SemanticType::Text));
        assert_eq!(by_name.get("parent_body"), Some(&SemanticType::Text));
        assert_eq!(by_name.get("created_at"), Some(&SemanticType::DateTime));
        assert_eq!(
            by_name.get("parent_created_at"),
            Some(&SemanticType::DateTime)
        );
    }

    fn graph_type_schema(
        program: &gleaph_gql::ast::GqlProgram,
    ) -> gleaph_gql::type_check::GraphTypePropertySchema {
        let statement = program
            .transaction_activity
            .as_ref()
            .and_then(|activity| activity.body.as_ref())
            .and_then(|body| body.next.last())
            .map(|next| &next.statement)
            .unwrap_or_else(|| {
                program
                    .transaction_activity
                    .as_ref()
                    .and_then(|activity| activity.body.as_ref())
                    .map(|body| &body.first)
                    .expect("body")
            });
        let gleaph_gql::ast::Statement::CreateGraphType(create) = statement else {
            panic!("expected CREATE GRAPH TYPE");
        };
        gleaph_gql::type_check::GraphTypePropertySchema::try_from_definition(&create.definition)
            .expect("graph type schema")
    }

    #[test]
    fn validates_explicit_result_column_type_fail_closed() {
        let program = gleaph_gql::parser::parse("MATCH (n) RETURN 'ok' AS tag")
            .expect("literal output should parse");
        let mut operation = operation();
        operation.result.columns = vec![Column {
            name: "tag".into(),
            semantic_type: SemanticType::Int32,
            nullable: false,
        }];
        let error = complete_result_schema(&program, &NoSchema, &mut operation)
            .expect_err("type mismatch must fail");
        assert!(error.contains("result column"));
        assert!(error.contains("tag"));
    }

    #[test]
    fn rejects_explicit_result_column_not_in_output() {
        let program = gleaph_gql::parser::parse("MATCH (n) RETURN 'ok' AS tag")
            .expect("literal output should parse");
        let mut operation = operation();
        operation.result.columns = vec![Column {
            name: "missing".into(),
            semantic_type: SemanticType::Text,
            nullable: false,
        }];
        let error = complete_result_schema(&program, &NoSchema, &mut operation)
            .expect_err("unknown column must fail");
        assert!(error.contains("missing"));
    }

    #[test]
    fn rejects_explicit_nullable_for_non_null_output() {
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

        let program = gleaph_gql::parser::parse("MATCH (u:User) RETURN u.age AS age")
            .expect("property output should parse");
        let mut operation = operation();
        operation.result.columns = vec![Column {
            name: "age".into(),
            semantic_type: SemanticType::Int32,
            nullable: true,
        }];
        let error = complete_result_schema(&program, &UserSchema, &mut operation)
            .expect_err("nullable claim on non-null output must fail");
        assert!(error.contains("non-null"));
    }

    #[test]
    fn accepts_explicit_result_columns_matching_output() {
        let program = gleaph_gql::parser::parse("MATCH (n) RETURN 'ok' AS tag")
            .expect("literal output should parse");
        let mut operation = operation();
        operation.result.columns = vec![Column {
            name: "tag".into(),
            semantic_type: SemanticType::Text,
            nullable: true,
        }];
        complete_result_schema(&program, &NoSchema, &mut operation)
            .expect("matching explicit columns should pass");
        assert_eq!(operation.result.columns.len(), 1);
        assert_eq!(operation.result.columns[0].name, "tag");
    }

    #[test]
    fn skips_result_completion_for_dml_tail_without_consulting_schema() {
        // A labeled MATCH consults the schema during type checking. The DML-tail gate must
        // return before inference, so a schema that panics when consulted proves the gate
        // short-circuits a DML tail (a gate-less implementation would panic here).
        struct PanicSchema;
        impl PropertySchema for PanicSchema {
            fn node_property_types(&self, _labels: &[String]) -> Vec<(String, ValueType, bool)> {
                panic!("schema consulted during DML tail");
            }
            fn edge_property_types(&self, _label: &str) -> Vec<(String, ValueType, bool)> {
                panic!("schema consulted during DML tail");
            }
        }

        let program = gleaph_gql::parser::parse("MATCH (n:User) RETURN n NEXT INSERT (m)")
            .expect("DML tail parses");
        let mut operation = operation();
        complete_result_schema(&program, &PanicSchema, &mut operation)
            .expect("DML tail skips result completion");
        assert!(operation.result.columns.is_empty());
    }
}
