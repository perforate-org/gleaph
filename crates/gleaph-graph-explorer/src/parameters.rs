//! Prepared-operation parameters: the definitions the Router manifest publishes and the values a
//! host binds to them.
//!
//! The manifest is the single source of truth for parameter names, requiredness, nullability and
//! semantic types. A preset does not declare them (that would be a second source), and this module
//! does not either: it looks operations up in the manifest and validates bound values against the
//! definitions, so a console form and an SDK caller share one rule.

use gleaph_sdk::{
    GqlParams, GqlValue, IntoGqlParam, Parameter, PreparedManifest, PreparedOperation,
};

/// Find the operation a preset executes in the Router's manifest.
pub fn find_operation<'a>(
    manifest: &'a PreparedManifest,
    name: &str,
) -> Option<&'a PreparedOperation> {
    manifest
        .operations
        .iter()
        .find(|operation| operation.name == name)
}

/// Whether a bound value may be null according to the definitions.
///
/// A null value for a required parameter is "supplied but null", so the nullability flag decides,
/// not the requiredness flag.
fn accepts(value: &GqlValue, parameter: &Parameter) -> bool {
    !matches!(value, GqlValue::Null) || parameter.nullable
}

/// Validate bound values against the manifest's definitions.
///
/// The rules are the Router's: every required parameter must be supplied, no value may be bound to
/// a name the operation does not declare, and a non-nullable parameter may not be null. Empty
/// definitions mean the manifest has not been loaded yet and skip validation, so a preset still
/// runs from a host that has not fetched its metadata.
pub fn validate(definitions: &[Parameter], parameters: &GqlParams) -> Result<(), String> {
    if definitions.is_empty() {
        return Ok(());
    }
    for (name, _) in parameters {
        if !definitions
            .iter()
            .any(|definition| &definition.name == name)
        {
            return Err(format!("unknown parameter ${name}"));
        }
    }
    for definition in definitions {
        let Some((_, value)) = parameters.iter().find(|(name, _)| name == &definition.name) else {
            if definition.required {
                return Err(format!("missing required parameter ${}", definition.name));
            }
            continue;
        };
        if !accepts(value, definition) {
            return Err(format!(
                "parameter ${} does not accept null",
                definition.name
            ));
        }
    }
    Ok(())
}

/// Bind one value, keeping the operation's declared order.
///
/// Unknown names are rejected once the manifest's definitions are known; with no definitions the
/// value is appended, because a host may bind before it fetches metadata and `validate` is the
/// authority at execution time.
pub fn bind<T: IntoGqlParam>(
    definitions: &[Parameter],
    parameters: &mut GqlParams,
    name: &str,
    value: T,
) -> Result<(), String> {
    let value = value.into_gql_param();
    if !definitions.is_empty() {
        let Some(position) = definitions
            .iter()
            .position(|definition| definition.name == name)
        else {
            return Err(format!("unknown parameter ${name}"));
        };
        parameters.retain(|(existing, _)| existing != name);
        let index = parameters
            .iter()
            .take_while(|(existing, _)| {
                definitions
                    .iter()
                    .position(|definition| &definition.name == existing)
                    .unwrap_or(usize::MAX)
                    < position
            })
            .count();
        parameters.insert(index, (name.to_owned(), value));
        return Ok(());
    }
    parameters.retain(|(existing, _)| existing != name);
    parameters.push((name.to_owned(), value));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_sdk::{
        Column, GraphIdentity, OperationKind, PreparedManifest, PreparedOperation, ResultSchema,
        SemanticType,
    };

    fn parameter(name: &str, required: bool, nullable: bool) -> Parameter {
        Parameter {
            name: name.to_owned(),
            description: None,
            required,
            nullable,
            semantic_type: SemanticType::Int64,
        }
    }

    fn manifest(parameters: Vec<Parameter>) -> PreparedManifest {
        PreparedManifest {
            manifest_version: 1,
            graph: GraphIdentity {
                id: "demo".to_owned(),
                name: Some("knowledge".to_owned()),
            },
            operations: vec![PreparedOperation {
                name: "topic-path-explanation".to_owned(),
                description: None,
                kind: OperationKind::Query,
                parameters,
                result: ResultSchema {
                    columns: Vec::<Column>::new(),
                },
                supports_consistency: false,
                supports_idempotency: false,
                allowed_sorts: Vec::new(),
            }],
        }
    }

    #[test]
    fn an_operation_is_found_by_its_preset_name() {
        let manifest = manifest(vec![parameter("depth", true, false)]);
        let found = find_operation(&manifest, "topic-path-explanation").expect("operation");
        assert_eq!(found.parameters.len(), 1);
        assert!(find_operation(&manifest, "missing").is_none());
    }

    #[test]
    fn required_parameters_must_be_supplied_and_unknown_ones_are_rejected() {
        let definitions = vec![parameter("depth", true, false)];
        let bound = vec![("depth".to_owned(), gleaph_sdk::gql_param_value(3_i64))];
        assert!(validate(&definitions, &bound).is_ok());
        assert_eq!(
            validate(&definitions, &Vec::new()).unwrap_err(),
            "missing required parameter $depth"
        );
        let extra = vec![
            ("depth".to_owned(), gleaph_sdk::gql_param_value(3_i64)),
            ("other".to_owned(), gleaph_sdk::gql_param_value(1_i64)),
        ];
        assert_eq!(
            validate(&definitions, &extra).unwrap_err(),
            "unknown parameter $other"
        );
    }

    #[test]
    fn a_non_nullable_parameter_rejects_null_but_a_nullable_one_accepts_it() {
        let strict = vec![parameter("depth", true, false)];
        let null = vec![(
            "depth".to_owned(),
            gleaph_sdk::gql_param_value(Option::<i64>::None),
        )];
        assert_eq!(
            validate(&strict, &null).unwrap_err(),
            "parameter $depth does not accept null"
        );

        let permissive = vec![parameter("depth", true, true)];
        assert!(validate(&permissive, &null).is_ok());
    }

    #[test]
    fn an_optional_parameter_may_be_absent() {
        let definitions = vec![parameter("depth", false, false)];
        assert!(validate(&definitions, &Vec::new()).is_ok());
    }

    #[test]
    fn bound_values_keep_the_manifest_order() {
        let definitions = vec![
            parameter("depth", false, false),
            parameter("limit", false, false),
        ];
        let mut parameters = GqlParams::new();
        bind(&definitions, &mut parameters, "limit", 10_i64).expect("bind limit");
        bind(&definitions, &mut parameters, "depth", 3_i64).expect("bind depth");
        let names: Vec<&str> = parameters.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            ["depth", "limit"],
            "declared order, not binding order"
        );
    }

    #[test]
    fn binding_an_undeclared_name_fails_once_definitions_are_known() {
        let definitions = vec![parameter("depth", false, false)];
        let mut parameters = GqlParams::new();
        assert_eq!(
            bind(&definitions, &mut parameters, "other", 1_i64).unwrap_err(),
            "unknown parameter $other"
        );
        assert!(bind(&[], &mut parameters, "other", 1_i64).is_ok());
    }

    #[test]
    fn without_definitions_validation_trusts_the_host() {
        let bound = vec![("anything".to_owned(), gleaph_sdk::gql_param_value(1_i64))];
        assert!(validate(&[], &bound).is_ok());
    }
}
