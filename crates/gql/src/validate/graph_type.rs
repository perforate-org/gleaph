use std::collections::BTreeMap;

use crate::ast::*;
use crate::name_limits::{
    validate_graph_type_identifier, validate_label_name, validate_property_name,
};
use rapidhash::RapidHashSet;

use super::{VResult, verr};

pub(super) fn validate_graph_type_definition(def: &GraphTypeDefinition) -> VResult {
    let mut node_names = RapidHashSet::default();
    let mut node_aliases = RapidHashSet::default();
    let mut edge_names = RapidHashSet::default();
    let mut node_refs = RapidHashSet::default();
    let mut node_ref_counts: BTreeMap<String, usize> = BTreeMap::new();

    for element in &def.elements {
        match element {
            GraphTypeElement::Node(node) => {
                validate_graph_type_properties(&node.properties)?;
                if let Some(name) = &node.name {
                    validate_graph_type_identifier(name).map_err(|e| verr(&e.to_string()))?;
                    if !node_names.insert(name.clone()) {
                        return Err(verr(&format!("duplicate graph type node name '{}'", name)));
                    }
                }
                if let Some(alias) = &node.alias {
                    validate_graph_type_identifier(alias).map_err(|e| verr(&e.to_string()))?;
                    if !node_aliases.insert(alias.clone()) {
                        return Err(verr(&format!(
                            "duplicate graph type node alias '{}'",
                            alias
                        )));
                    }
                }
                if let Some(name) = &node.name {
                    node_refs.insert(name.clone());
                    *node_ref_counts.entry(name.clone()).or_insert(0) += 1;
                }
                if let Some(alias) = &node.alias {
                    node_refs.insert(alias.clone());
                    *node_ref_counts.entry(alias.clone()).or_insert(0) += 1;
                }
                if let Some(ref ls) = node.label_set {
                    for label in &ls.labels {
                        validate_label_name(label).map_err(|e| verr(&e.to_string()))?;
                        node_refs.insert(label.clone());
                        *node_ref_counts.entry(label.clone()).or_insert(0) += 1;
                    }
                }
            }
            GraphTypeElement::Edge(edge) => {
                validate_graph_type_properties(&edge.properties)?;
                if let Some(name) = &edge.name {
                    validate_graph_type_identifier(name).map_err(|e| verr(&e.to_string()))?;
                    if !edge_names.insert(name.clone()) {
                        return Err(verr(&format!("duplicate graph type edge name '{}'", name)));
                    }
                }
                if let Some(ref ls) = edge.label_set {
                    for label in &ls.labels {
                        validate_label_name(label).map_err(|e| verr(&e.to_string()))?;
                    }
                }
            }
        }
    }

    if !node_refs.is_empty() {
        for element in &def.elements {
            let GraphTypeElement::Edge(edge) = element else {
                continue;
            };
            validate_graph_type_endpoint(&edge.source, &node_refs, &node_ref_counts, "source")?;
            validate_graph_type_endpoint(
                &edge.destination,
                &node_refs,
                &node_ref_counts,
                "destination",
            )?;
        }
    }

    crate::type_check::GraphTypePropertySchema::try_from_definition(def)
        .map_err(|msg| verr(&msg))?;

    Ok(())
}

fn validate_graph_type_properties(properties: &[PropertyDef]) -> VResult {
    let mut names = RapidHashSet::default();
    for property in properties {
        validate_property_name(&property.name).map_err(|e| verr(&e.to_string()))?;
        if !names.insert(property.name.clone()) {
            return Err(verr(&format!(
                "duplicate graph type property '{}'",
                property.name
            )));
        }
    }
    Ok(())
}

fn validate_graph_type_endpoint(
    endpoint: &EdgeEndpoint,
    node_refs: &RapidHashSet<String>,
    node_ref_counts: &BTreeMap<String, usize>,
    role: &str,
) -> VResult {
    if let Some(l) = &endpoint.label {
        validate_graph_type_identifier(l).map_err(|e| verr(&e.to_string()))?;
    }
    if let Some(t) = &endpoint.type_name {
        validate_graph_type_identifier(t).map_err(|e| verr(&e.to_string()))?;
    }
    let reference = endpoint
        .type_name
        .as_ref()
        .or(endpoint.label.as_ref())
        .ok_or_else(|| {
            verr(&format!(
                "graph type {role} endpoint is missing a node reference"
            ))
        })?;

    if !node_refs.contains(reference) {
        return Err(verr(&format!(
            "graph type {role} endpoint '{}' does not match any node name, alias, or label in the same definition",
            reference
        )));
    }
    if node_ref_counts.get(reference).copied().unwrap_or(0) > 1 {
        return Err(verr(&format!(
            "graph type {role} endpoint '{}' is ambiguous across multiple node names, aliases, or labels",
            reference
        )));
    }

    Ok(())
}
