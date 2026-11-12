//! Build the caller-visible general routing context (ADR 0114 slice 3).
//!
//! The Router is the source of truth for the graph-type summary and the prepared-operation
//! catalog. This module derives one `GeneralRoutingContext` per request: it never persists a
//! duplicate catalog, and it filters out operations the caller may not execute.

use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use gleaph_gql::ast::{GraphTypeDefinition, GraphTypeElement};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_prepared_api::{OperationKind, Parameter, PreparedOperation, SemanticType};
use gleaph_routing_wire::{
    GeneralDomainValue, GeneralEdge, GeneralGraphSchema, GeneralLabel, GeneralOperation,
    GeneralParameter, GeneralProperty, GeneralRoutingContext, MAX_GENERAL_OPERATIONS,
    general_context_fingerprint,
};

use crate::facade::stable::ROUTER_PREPARED_PLANS;
use crate::facade::stable::graph_type_catalog::parsed_graph_type_definition_for_graph_id;
use crate::rbac::authorize_prepared_execute;
use crate::state::RouterError;

/// Validation for the per-graph profile. One owner for the range and policy-version rules; the admin
/// API and future graph-owner authoring both call it.
pub(crate) fn validate_profile(
    abstain_threshold_bps: u32,
    policy_version: u32,
) -> Result<(), RouterError> {
    if abstain_threshold_bps > 10_000 {
        return Err(RouterError::InvalidArgument(format!(
            "abstain threshold must be 0..=10000 basis points, got {abstain_threshold_bps}"
        )));
    }
    if policy_version == 0 {
        return Err(RouterError::InvalidArgument(
            "policy version must be non-zero".to_owned(),
        ));
    }
    Ok(())
}

fn semantic_type_name(semantic_type: &SemanticType) -> &'static str {
    match semantic_type {
        SemanticType::Bool => "bool",
        SemanticType::Int8
        | SemanticType::Int16
        | SemanticType::Int32
        | SemanticType::Int64
        | SemanticType::Int128
        | SemanticType::Int256
        | SemanticType::Uint8
        | SemanticType::Uint16
        | SemanticType::Uint32
        | SemanticType::Uint64
        | SemanticType::Uint128
        | SemanticType::Uint256 => "int",
        SemanticType::Float16
        | SemanticType::Float32
        | SemanticType::Float64
        | SemanticType::Float128
        | SemanticType::Float256
        | SemanticType::Decimal => "float",
        SemanticType::Bytes => "bytes",
        SemanticType::Text => "string",
        _ => "any",
    }
}

fn endpoint_label(endpoint: &gleaph_gql::ast::EdgeEndpoint) -> Option<&str> {
    endpoint.label.as_deref().or(endpoint.type_name.as_deref())
}

/// Convert a GQL graph type into the bounded schema summary the router sees. Unknown endpoint
/// labels are skipped rather than inventing a schema; an empty schema is rejected by the caller.
pub(crate) fn schema_from_definition(
    definition: &GraphTypeDefinition,
) -> Result<GeneralGraphSchema, RouterError> {
    let mut node_labels: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut label_properties: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for element in &definition.elements {
        if let GraphTypeElement::Node(node) = element {
            let labels: Vec<String> = node
                .label_set
                .as_ref()
                .map(|set| set.labels.clone())
                .unwrap_or_else(|| node.name.clone().into_iter().collect());
            if labels.is_empty() {
                continue;
            }
            if let Some(name) = &node.name {
                node_labels.insert(name.clone(), labels.iter().cloned().collect());
            }
            for label in labels {
                let properties = label_properties.entry(label.clone()).or_default();
                for property in &node.properties {
                    properties.insert(property.name.clone());
                }
            }
        }
    }

    let labels: Vec<GeneralLabel> = label_properties
        .into_iter()
        .map(|(name, properties)| GeneralLabel {
            name,
            properties: properties
                .into_iter()
                .map(|property| GeneralProperty {
                    name: property,
                    property_type: "string".to_owned(),
                })
                .collect(),
        })
        .collect();
    if labels.is_empty() {
        return Err(RouterError::NotImplemented(
            "graph has no node labels in its graph type".to_owned(),
        ));
    }

    let known: BTreeSet<&str> = labels.iter().map(|label| label.name.as_str()).collect();
    let mut edges: BTreeSet<(String, String, String)> = BTreeSet::new();
    for element in &definition.elements {
        let GraphTypeElement::Edge(edge) = element else {
            continue;
        };
        let edge_labels: Vec<&str> = edge
            .label_set
            .as_ref()
            .map(|set| set.labels.iter().map(String::as_str).collect())
            .unwrap_or_else(|| edge.name.as_deref().into_iter().collect());
        let from = endpoint_label(&edge.source);
        let to = endpoint_label(&edge.destination);
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        for edge_label in edge_labels {
            if known.contains(from) && known.contains(to) {
                edges.insert((edge_label.to_owned(), from.to_owned(), to.to_owned()));
            }
        }
    }
    // Endpoint type names are resolved to their first runtime label, which is what the demo
    // graph types use. If a node type has several labels, the first is the summary anchor.
    let graph = GeneralGraphSchema {
        labels,
        edges: edges
            .into_iter()
            .map(|(name, from_label, to_label)| GeneralEdge {
                name,
                from_label,
                to_label,
            })
            .collect(),
    };
    Ok(graph)
}

fn operation_from_metadata(
    metadata: &PreparedOperation,
    domains: &std::collections::BTreeMap<String, Vec<GeneralDomainValue>>,
) -> Option<GeneralOperation> {
    if metadata.kind != OperationKind::Query {
        return None;
    }
    let parameters: Vec<GeneralParameter> = metadata
        .parameters
        .iter()
        .map(|parameter: &Parameter| GeneralParameter {
            name: parameter.name.clone(),
            parameter_type: semantic_type_name(&parameter.semantic_type).to_owned(),
            required: parameter.required,
            domain: domains.get(&parameter.name).cloned().unwrap_or_default(),
        })
        .collect();
    Some(GeneralOperation {
        id: metadata.name.clone(),
        description: metadata
            .description
            .clone()
            .unwrap_or_else(|| metadata.name.clone()),
        parameters,
        result: metadata
            .result
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect(),
    })
}

/// Validate a graph's declared parameter domains against the ADR 0117 bounds.
///
/// Domains are bounded at write time, not at route time: a truncated domain would silently make
/// extraction worse, so an over-committed domain is rejected instead.
pub(crate) fn validate_domains(
    domains: &std::collections::BTreeMap<String, Vec<GeneralDomainValue>>,
) -> Result<(), RouterError> {
    use gleaph_routing_wire::{
        MAX_DOMAIN_ALIAS_BYTES, MAX_DOMAIN_ALIASES, MAX_DOMAIN_PARAMETERS, MAX_DOMAIN_TOTAL_BYTES,
        MAX_DOMAIN_VALUE_BYTES, MAX_DOMAIN_VALUES,
    };
    if domains.len() > MAX_DOMAIN_PARAMETERS {
        return Err(RouterError::InvalidArgument(format!(
            "graph declares {} parameter domains; max {MAX_DOMAIN_PARAMETERS}",
            domains.len()
        )));
    }
    let mut total = 0usize;
    for (parameter, values) in domains {
        if parameter.trim().is_empty() || parameter.len() > 64 {
            return Err(RouterError::InvalidArgument(format!(
                "invalid parameter name for a domain: {parameter:?}"
            )));
        }
        if values.is_empty() {
            return Err(RouterError::InvalidArgument(format!(
                "domain for {parameter:?} has no values"
            )));
        }
        if values.len() > MAX_DOMAIN_VALUES {
            return Err(RouterError::InvalidArgument(format!(
                "domain for {parameter:?} has {} values; max {MAX_DOMAIN_VALUES}",
                values.len()
            )));
        }
        let mut seen_values = std::collections::BTreeSet::new();
        for entry in values {
            let value = entry.value.as_str();
            if value.trim().is_empty()
                || value.trim() != value
                || value.len() > MAX_DOMAIN_VALUE_BYTES
            {
                return Err(RouterError::InvalidArgument(format!(
                    "domain value for {parameter:?} is empty, not trimmed, or longer than {MAX_DOMAIN_VALUE_BYTES} bytes"
                )));
            }
            if !seen_values.insert(value) {
                return Err(RouterError::InvalidArgument(format!(
                    "domain for {parameter:?} repeats value {value:?}"
                )));
            }
            total = total.saturating_add(value.len());
            if entry.aliases.len() > MAX_DOMAIN_ALIASES {
                return Err(RouterError::InvalidArgument(format!(
                    "domain value {value:?} for {parameter:?} has {} aliases; max {MAX_DOMAIN_ALIASES}",
                    entry.aliases.len()
                )));
            }
            let mut seen_aliases = std::collections::BTreeSet::new();
            for alias in &entry.aliases {
                if alias.trim().is_empty()
                    || alias.trim() != alias
                    || alias.len() > MAX_DOMAIN_ALIAS_BYTES
                {
                    return Err(RouterError::InvalidArgument(format!(
                        "domain alias for {parameter:?} is empty, not trimmed, or longer than {MAX_DOMAIN_ALIAS_BYTES} bytes"
                    )));
                }
                if alias == value {
                    return Err(RouterError::InvalidArgument(format!(
                        "domain alias for {parameter:?} repeats its canonical value {value:?}"
                    )));
                }
                if !seen_aliases.insert(alias) {
                    return Err(RouterError::InvalidArgument(format!(
                        "domain value {value:?} for {parameter:?} repeats alias {alias:?}"
                    )));
                }
                total = total.saturating_add(alias.len());
            }
        }
    }
    if total > MAX_DOMAIN_TOTAL_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "graph domains are {total} bytes; max {MAX_DOMAIN_TOTAL_BYTES}"
        )));
    }
    Ok(())
}

/// Fingerprint of the exact context sent to the inference canister. One owner for the rule; the
/// canister recomputes the same function from the wire value.
pub(crate) fn context_fingerprint(context: &GeneralRoutingContext) -> Vec<u8> {
    general_context_fingerprint(context)
}

/// Build the caller-visible context for one graph. Operations without metadata, non-query
/// operations and operations the caller may not execute are omitted; the returned context is
/// therefore safe to send to the inference canister.
pub(crate) fn context_for_graph(
    graph_id: GraphId,
    caller: Principal,
) -> Result<GeneralRoutingContext, RouterError> {
    let definition = parsed_graph_type_definition_for_graph_id(graph_id)?.ok_or_else(|| {
        RouterError::NotImplemented("general routing requires a typed graph".to_owned())
    })?;
    let graph = schema_from_definition(&definition)?;
    let candidates: Vec<(String, PreparedOperation)> = ROUTER_PREPARED_PLANS.with_borrow(|plans| {
        plans
            .iter()
            .filter_map(|entry| {
                let value = entry.value();
                let record = value.as_v1();
                if record.graph_id != graph_id {
                    return None;
                }
                let metadata = record.metadata.as_ref()?;
                Some((entry.key().name.clone(), metadata.clone()))
            })
            .filter(|(_, metadata)| metadata.kind == OperationKind::Query)
            .collect()
    });
    let domains = crate::facade::stable::graph_catalog::routing_domains(graph_id)?;
    let mut operations = Vec::new();
    for (name, metadata) in candidates {
        if authorize_prepared_execute(&caller, &name, graph_id).is_err() {
            continue;
        }
        if let Some(operation) = operation_from_metadata(&metadata, &domains) {
            operations.push(operation);
        }
    }
    operations.sort_by(|left, right| left.id.cmp(&right.id));
    operations.dedup_by(|left, right| left.id == right.id);
    if operations.is_empty() {
        return Err(RouterError::NotImplemented(
            "graph has no caller-visible routable prepared operations".to_owned(),
        ));
    }
    if operations.len() > MAX_GENERAL_OPERATIONS {
        return Err(RouterError::InvalidArgument(format!(
            "graph has {} routable operations; max {MAX_GENERAL_OPERATIONS}",
            operations.len()
        )));
    }
    let profile = crate::facade::stable::graph_catalog::general_routing_profile(graph_id)?;
    let mut context = GeneralRoutingContext {
        schema_version: 1,
        graph,
        operations,
        abstain_option: "none-of-these".to_owned(),
        abstain_threshold: profile.abstain_threshold_bps as f32 / 10_000.0,
        policy_version: profile.policy_version,
        context_fingerprint: Vec::new(),
    };
    context.context_fingerprint = general_context_fingerprint(&context);
    Ok(context)
}

#[cfg(test)]
mod tests {
    use gleaph_gql::ast::{Statement, StatementBlock};
    use gleaph_gql::parser;

    use super::*;

    fn definition(ddl: &str) -> GraphTypeDefinition {
        let program = parser::parse(ddl).expect("parse DDL");
        let block: StatementBlock = program
            .transaction_activity
            .expect("transaction")
            .body
            .expect("body");
        block
            .iter_statements()
            .find_map(|statement| match statement {
                Statement::CreateGraphType(create) => create.nested_definition().cloned(),
                _ => None,
            })
            .expect("CREATE GRAPH TYPE")
    }

    #[test]
    fn schema_summary_uses_runtime_labels_and_endpoints() {
        let definition = definition(
            "CREATE GRAPH TYPE MovieGraph {
                NODE Film { title STRING, year INT32 },
                NODE Person { name STRING },
                DIRECTED EDGE ActedIn LABEL ACTED_IN CONNECTING (Person -> Film)
            }",
        );
        let schema = schema_from_definition(&definition).expect("schema");
        assert_eq!(
            schema
                .labels
                .iter()
                .map(|label| label.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Film", "Person"]
        );
        assert_eq!(schema.edges.len(), 1);
        assert_eq!(schema.edges[0].name, "ACTED_IN");
        assert_eq!(schema.edges[0].from_label, "Person");
        assert_eq!(schema.edges[0].to_label, "Film");
    }

    #[test]
    fn metadata_conversion_skips_mutations_and_maps_types() {
        let mut metadata = PreparedOperation {
            name: "find-films".to_owned(),
            description: Some("Find films by title.".to_owned()),
            kind: OperationKind::Query,
            parameters: vec![Parameter {
                name: "title".to_owned(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Text,
            }],
            result: gleaph_prepared_api::ResultSchema { columns: vec![] },
            supports_consistency: false,
            supports_idempotency: false,
            allowed_sorts: vec![],
            text_fusion: None,
        };
        let operation = operation_from_metadata(&metadata, &std::collections::BTreeMap::new())
            .expect("query operation");
        assert_eq!(operation.id, "find-films");
        assert_eq!(operation.parameters[0].parameter_type, "string");
        metadata.kind = OperationKind::Update;
        assert!(operation_from_metadata(&metadata, &std::collections::BTreeMap::new()).is_none());
    }

    #[test]
    fn context_contains_only_caller_visible_query_operations() {
        use std::collections::BTreeSet;

        use gleaph_prepared_api::{
            OperationKind, Parameter, PreparedOperation, ResultSchema, SemanticType,
        };

        use crate::facade::auth;
        use crate::facade::stable::graph_catalog::lookup_graph_id;
        use crate::facade::stable::graph_type_catalog::apply_catalog_statement_block;
        use crate::facade::stable::prepared_catalog::{
            PreparedPlanKey, PreparedPlanRecord, PreparedPlanRecordV1, insert_prepared_plan,
        };
        use crate::facade::store::RouterStore;
        use crate::init::RouterInitArgs;
        use crate::types::{GraphRegistryEntry, GraphStatus, ProvisioningState};

        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let admin = Principal::from_slice(&[1; 29]);
        auth::grant_admins(&[admin]);
        let graph_id = GraphId::from_raw(7);
        store
            .admin_register_graph(
                admin,
                GraphRegistryEntry {
                    graph_id,
                    canister_id: Principal::management_canister(),
                    owner: admin,
                    admins: BTreeSet::new(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
                "gtest",
            )
            .expect("register graph");
        let ddl = "CREATE GRAPH gtest { NODE Film { title STRING }, NODE Person, DIRECTED EDGE ActedIn LABEL ACTED_IN CONNECTING (Person -> Film) }";
        let block: StatementBlock = parser::parse(ddl)
            .expect("parse")
            .transaction_activity
            .expect("transaction")
            .body
            .expect("body");
        apply_catalog_statement_block(&block, ddl).expect("graph type");
        let graph_id = lookup_graph_id("gtest").expect("graph id");
        let metadata = PreparedOperation {
            name: "find-films".to_owned(),
            description: Some("Find films by title.".to_owned()),
            kind: OperationKind::Query,
            parameters: vec![Parameter {
                name: "title".to_owned(),
                description: None,
                required: true,
                nullable: false,
                semantic_type: SemanticType::Text,
            }],
            result: ResultSchema { columns: vec![] },
            supports_consistency: false,
            supports_idempotency: false,
            allowed_sorts: vec![],
            text_fusion: None,
        };
        insert_prepared_plan(
            PreparedPlanKey::new("find-films"),
            PreparedPlanRecord::V1(PreparedPlanRecordV1 {
                graph_id,
                query: "MATCH (f:Film) WHERE f.title = $title RETURN f.title AS title".to_owned(),
                metadata: Some(metadata),
                required_privileges: Default::default(),
            }),
        );
        let context = context_for_graph(graph_id, admin).expect("context");
        assert_eq!(context.abstain_threshold, 0.2);
        assert_eq!(context.policy_version, 1);
        crate::facade::stable::graph_catalog::set_general_routing_profile(
            graph_id,
            crate::facade::stable::memory::GeneralRoutingProfile {
                abstain_threshold_bps: 7_500,
                policy_version: 3,
            },
        )
        .expect("profile");
        let tuned = context_for_graph(graph_id, admin).expect("tuned context");
        assert_eq!(tuned.abstain_threshold, 0.75);
        assert_eq!(tuned.policy_version, 3);
        assert_eq!(context.operations.len(), 1);
        assert_eq!(context.operations[0].id, "find-films");
        assert_eq!(context.operations[0].parameters[0].parameter_type, "string");
        assert_eq!(context.graph.labels.len(), 2);
        assert_eq!(context.graph.edges[0].name, "ACTED_IN");
        assert_eq!(context.context_fingerprint.len(), 32);

        // ADR 0117: a declared domain reaches the operation card and changes the fingerprint, so a
        // route can never be answered against a domain the caller was not shown.
        assert!(tuned.operations[0].parameters[0].domain.is_empty());
        let entries = vec![
            GeneralDomainValue {
                value: "Film A".to_owned(),
                aliases: vec!["A".to_owned(), "The A".to_owned()],
            },
            GeneralDomainValue {
                value: "Film B".to_owned(),
                aliases: vec![],
            },
        ];
        crate::facade::stable::graph_catalog::set_routing_domain(
            graph_id,
            "title".to_owned(),
            entries.clone(),
        )
        .expect("domain");
        let with_domain = context_for_graph(graph_id, admin).expect("domain context");
        assert_eq!(with_domain.operations[0].parameters[0].domain, entries);
        assert_ne!(tuned.context_fingerprint, with_domain.context_fingerprint);
    }

    #[test]
    fn domain_validation_rejects_out_of_bounds_values() {
        use gleaph_routing_wire::{
            MAX_DOMAIN_PARAMETERS, MAX_DOMAIN_TOTAL_BYTES, MAX_DOMAIN_VALUE_BYTES,
            MAX_DOMAIN_VALUES,
        };
        use std::collections::BTreeMap;

        let mut domains: BTreeMap<String, Vec<GeneralDomainValue>> = BTreeMap::new();
        assert!(validate_domains(&domains).is_ok());
        domains.insert(
            "genre".to_owned(),
            vec![
                GeneralDomainValue {
                    value: "SF映画".to_owned(),
                    aliases: vec!["SF".to_owned()],
                },
                GeneralDomainValue {
                    value: "ホラー映画".to_owned(),
                    aliases: vec!["horror".to_owned()],
                },
            ],
        );
        assert!(validate_domains(&domains).is_ok());

        let mut empty = domains.clone();
        empty.insert("genre".to_owned(), Vec::new());
        assert!(validate_domains(&empty).is_err());

        let mut too_many_values = domains.clone();
        too_many_values.insert(
            "genre".to_owned(),
            (0..=MAX_DOMAIN_VALUES)
                .map(|i| GeneralDomainValue {
                    value: format!("v{i}"),
                    aliases: Vec::new(),
                })
                .collect(),
        );
        assert!(validate_domains(&too_many_values).is_err());

        let mut too_long_value = domains.clone();
        too_long_value.insert(
            "genre".to_owned(),
            vec![GeneralDomainValue {
                value: "x".repeat(MAX_DOMAIN_VALUE_BYTES + 1),
                aliases: Vec::new(),
            }],
        );
        assert!(validate_domains(&too_long_value).is_err());

        let mut too_many_parameters = BTreeMap::new();
        for index in 0..=MAX_DOMAIN_PARAMETERS {
            too_many_parameters.insert(
                format!("p{index}"),
                vec![GeneralDomainValue {
                    value: "v".to_owned(),
                    aliases: Vec::new(),
                }],
            );
        }
        assert!(validate_domains(&too_many_parameters).is_err());

        let mut duplicate_alias = domains.clone();
        duplicate_alias.insert(
            "genre".to_owned(),
            vec![GeneralDomainValue {
                value: "SF映画".to_owned(),
                aliases: vec!["SF".to_owned(), "SF".to_owned()],
            }],
        );
        assert!(validate_domains(&duplicate_alias).is_err());

        let mut too_many_aliases = domains.clone();
        too_many_aliases.insert(
            "genre".to_owned(),
            vec![GeneralDomainValue {
                value: "SF映画".to_owned(),
                aliases: (0..=gleaph_routing_wire::MAX_DOMAIN_ALIASES)
                    .map(|i| format!("a{i}"))
                    .collect(),
            }],
        );
        assert!(validate_domains(&too_many_aliases).is_err());

        let mut alias_equals_value = domains.clone();
        alias_equals_value.insert(
            "genre".to_owned(),
            vec![GeneralDomainValue {
                value: "SF".to_owned(),
                aliases: vec!["SF".to_owned()],
            }],
        );
        assert!(validate_domains(&alias_equals_value).is_err());

        let mut too_large = BTreeMap::new();
        too_large.insert(
            "genre".to_owned(),
            vec![GeneralDomainValue {
                value: "x".repeat(MAX_DOMAIN_TOTAL_BYTES / 2 + 1),
                aliases: Vec::new(),
            }],
        );
        assert!(validate_domains(&too_large).is_err());
    }

    #[test]
    fn profile_validation_rejects_out_of_range_values() {
        assert!(validate_profile(2_000, 1).is_ok());
        assert!(validate_profile(10_000, 7).is_ok());
        assert!(validate_profile(10_001, 1).is_err());
        assert!(validate_profile(2_000, 0).is_err());
    }

    #[test]
    fn movie_and_knowledge_catalogs_have_expected_schema_summaries() {
        let movie = definition(
            "CREATE GRAPH TYPE MovieGraph {
                NODE Film { title STRING, year INT32, synopsis STRING },
                NODE Person { name STRING },
                NODE Studio { name STRING },
                NODE Genre { name STRING },
                DIRECTED EDGE Directed LABEL DIRECTED CONNECTING (Person -> Film),
                DIRECTED EDGE ActedIn LABEL ACTED_IN CONNECTING (Person -> Film),
                DIRECTED EDGE ProducedBy LABEL PRODUCED_BY CONNECTING (Film -> Studio),
                DIRECTED EDGE HasGenre LABEL HAS_GENRE CONNECTING (Film -> Genre),
                DIRECTED EDGE SequelOf LABEL SEQUEL_OF CONNECTING (Film -> Film)
            }",
        );
        let movie_schema = schema_from_definition(&movie).expect("movie schema");
        assert_eq!(movie_schema.labels.len(), 4);
        assert_eq!(movie_schema.edges.len(), 5);
        assert!(
            movie_schema
                .edges
                .iter()
                .any(|edge| edge.name == "HAS_GENRE")
        );

        let knowledge = definition(
            "CREATE GRAPH TYPE KnowledgeGraph {
                NODE Document { title STRING, body STRING, is_public BOOL },
                NODE Concept { name STRING, definition STRING },
                NODE Person { name STRING, role STRING },
                NODE Team { name STRING },
                DIRECTED EDGE Cites LABEL CITES CONNECTING (Document -> Document),
                DIRECTED EDGE About LABEL ABOUT CONNECTING (Document -> Concept),
                DIRECTED EDGE Relates LABEL RELATED_TO CONNECTING (Concept -> Concept),
                DIRECTED EDGE Authored LABEL AUTHORED_BY CONNECTING (Person -> Document),
                DIRECTED EDGE Owns LABEL OWNS CONNECTING (Person -> Document),
                DIRECTED EDGE BelongsTo LABEL BELONGS_TO CONNECTING (Person -> Team),
                DIRECTED EDGE RoutedVia LABEL ROUTED_VIA CONNECTING (Person -> Document)
            }",
        );
        let knowledge_schema = schema_from_definition(&knowledge).expect("knowledge schema");
        assert_eq!(knowledge_schema.labels.len(), 4);
        assert_eq!(knowledge_schema.edges.len(), 7);
        assert!(
            knowledge_schema
                .edges
                .iter()
                .any(|edge| edge.name == "RELATED_TO")
        );
    }
}
