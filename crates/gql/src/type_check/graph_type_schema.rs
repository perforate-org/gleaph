//! [`PropertySchema`] built from an inline [`GraphTypeDefinition`] (DDL).

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{
    EdgeEndpoint, EdgeTypeDef, GraphTypeDefinition, GraphTypeElement, NodeTypeDef, ValueType,
};
use crate::types::EdgeDirection;

use super::schema::PropertySchema;

type EndpointLabelsPair = (Vec<String>, Vec<String>);
type PropertyTypeSpec = (String, ValueType, bool);

/// Runtime vertex labels, edge labels, and property names declared in a graph type definition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphTypeVocabulary {
    pub vertex_labels: BTreeSet<String>,
    pub edge_labels: BTreeSet<String>,
    pub properties: BTreeSet<String>,
}

/// Collect runtime vocabulary from a graph type definition (ADR 0018 V5).
pub fn collect_graph_type_vocabulary(def: &GraphTypeDefinition) -> GraphTypeVocabulary {
    let mut vocab = GraphTypeVocabulary::default();
    for element in &def.elements {
        match element {
            GraphTypeElement::Node(node) => {
                vocab.vertex_labels.extend(node_runtime_labels(node));
                for property in &node.properties {
                    vocab.properties.insert(property.name.clone());
                }
            }
            GraphTypeElement::Edge(edge) => {
                vocab.edge_labels.extend(edge_schema_keys(edge));
                for property in &edge.properties {
                    vocab.properties.insert(property.name.clone());
                }
            }
        }
    }
    vocab
}

fn node_runtime_labels(node: &NodeTypeDef) -> Vec<String> {
    if let Some(label_set) = &node.label_set
        && !label_set.labels.is_empty()
    {
        return label_set.labels.clone();
    }
    node.name.clone().into_iter().collect()
}

/// Property and endpoint metadata derived from `CREATE GRAPH` / `CREATE GRAPH TYPE` inline types.
#[derive(Clone, Debug, Default)]
pub struct GraphTypePropertySchema {
    edge_undirected: BTreeMap<String, bool>,
    edge_endpoints: BTreeMap<String, Vec<EndpointLabelsPair>>,
    edge_properties: BTreeMap<String, Vec<PropertyTypeSpec>>,
}

impl GraphTypePropertySchema {
    /// Build from a graph type definition. Fails if the same edge label key is both directed and undirected.
    pub fn try_from_definition(def: &GraphTypeDefinition) -> Result<Self, String> {
        let node_map = build_node_label_map(def);
        let mut s = Self::default();

        for element in &def.elements {
            let GraphTypeElement::Edge(edge) = element else {
                continue;
            };
            let keys = edge_schema_keys(edge);
            if keys.is_empty() {
                continue;
            }

            let undirected = matches!(edge.direction, EdgeDirection::Undirected);
            for k in &keys {
                if let Some(existing) = s.edge_undirected.get(k)
                    && *existing != undirected
                {
                    return Err(format!(
                        "conflicting directedness for edge label `{k}`: graph type defines both DIRECTED and UNDIRECTED edges with this label"
                    ));
                }
            }

            let from = endpoint_constraint_labels(&edge.source, &node_map);
            let to = endpoint_constraint_labels(&edge.destination, &node_map);
            let mut pairs: Vec<EndpointLabelsPair> = vec![(from.clone(), to.clone())];
            if undirected && from != to {
                pairs.push((to, from));
            }

            for k in &keys {
                s.edge_undirected.entry(k.clone()).or_insert(undirected);
                s.edge_endpoints
                    .entry(k.clone())
                    .or_default()
                    .extend(pairs.iter().cloned());
                let entry = s.edge_properties.entry(k.clone()).or_default();
                for p in &edge.properties {
                    if !entry.iter().any(|(n, _, _)| n == &p.name) {
                        entry.push((p.name.clone(), p.value_type.clone(), p.not_null));
                    }
                }
            }
        }

        Ok(s)
    }
}

fn edge_schema_keys(edge: &EdgeTypeDef) -> Vec<String> {
    let mut out: Vec<String> = edge
        .label_set
        .as_ref()
        .map(|ls| ls.labels.clone())
        .unwrap_or_default();
    if out.is_empty()
        && let Some(ref n) = edge.name
    {
        out.push(n.clone());
    }
    out
}

fn build_node_label_map(def: &GraphTypeDefinition) -> BTreeMap<String, Vec<String>> {
    let mut m = BTreeMap::new();

    for element in &def.elements {
        let GraphTypeElement::Node(node) = element else {
            continue;
        };
        let primary: Vec<String> = if let Some(ls) = &node.label_set {
            if !ls.labels.is_empty() {
                ls.labels.clone()
            } else if let Some(n) = &node.name {
                vec![n.clone()]
            } else {
                vec![]
            }
        } else if let Some(n) = &node.name {
            vec![n.clone()]
        } else {
            vec![]
        };

        if let Some(n) = &node.name {
            m.insert(n.clone(), primary.clone());
        }
        if let Some(a) = &node.alias {
            m.insert(a.clone(), primary.clone());
        }
        for lbl in &primary {
            m.insert(lbl.clone(), primary.clone());
        }
    }

    m
}

fn endpoint_constraint_labels(
    endpoint: &EdgeEndpoint,
    node_map: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    if let Some(ref l) = endpoint.label {
        return vec![l.clone()];
    }
    if let Some(ref t) = endpoint.type_name {
        return node_map.get(t).cloned().unwrap_or_else(|| vec![t.clone()]);
    }
    vec![]
}

impl PropertySchema for GraphTypePropertySchema {
    fn node_property_types(&self, _labels: &[String]) -> Vec<PropertyTypeSpec> {
        vec![]
    }

    fn edge_property_types(&self, label: &str) -> Vec<PropertyTypeSpec> {
        self.edge_properties.get(label).cloned().unwrap_or_default()
    }

    fn edge_endpoint_types(&self, label: &str) -> Vec<EndpointLabelsPair> {
        self.edge_endpoints.get(label).cloned().unwrap_or_default()
    }

    fn edge_is_undirected(&self, label: &str) -> Option<bool> {
        self.edge_undirected.get(label).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{GraphTypeDefinition, GraphTypeElement, Keyword, NodeTypeDef};
    use crate::token::Span;
    use crate::types::EdgeDirection;
    use std::collections::BTreeSet;

    fn node_named(name: &str) -> GraphTypeElement {
        GraphTypeElement::Node(NodeTypeDef {
            span: Span::DUMMY,
            keyword: Keyword::new("NODE"),
            name: Some(name.to_string()),
            alias: None,
            label_set: None,
            properties: vec![],
        })
    }

    #[test]
    fn conflicting_edge_label_directedness_fails() {
        let def = GraphTypeDefinition {
            span: Span::DUMMY,
            elements: vec![
                node_named("A"),
                node_named("B"),
                GraphTypeElement::Edge(EdgeTypeDef {
                    span: Span::DUMMY,
                    keyword: Keyword::new("EDGE"),
                    name: Some("E1".to_string()),
                    direction: EdgeDirection::PointingRight,
                    source: EdgeEndpoint {
                        span: Span::DUMMY,
                        label: None,
                        type_name: Some("A".to_string()),
                    },
                    destination: EdgeEndpoint {
                        span: Span::DUMMY,
                        label: None,
                        type_name: Some("B".to_string()),
                    },
                    label_set: Some(crate::ast::KeyLabelSet {
                        span: Span::DUMMY,
                        label_keyword_plural: false,
                        labels: vec!["R".to_string()],
                    }),
                    properties: vec![],
                }),
                GraphTypeElement::Edge(EdgeTypeDef {
                    span: Span::DUMMY,
                    keyword: Keyword::new("EDGE"),
                    name: Some("E2".to_string()),
                    direction: EdgeDirection::Undirected,
                    source: EdgeEndpoint {
                        span: Span::DUMMY,
                        label: None,
                        type_name: Some("A".to_string()),
                    },
                    destination: EdgeEndpoint {
                        span: Span::DUMMY,
                        label: None,
                        type_name: Some("B".to_string()),
                    },
                    label_set: Some(crate::ast::KeyLabelSet {
                        span: Span::DUMMY,
                        label_keyword_plural: false,
                        labels: vec!["R".to_string()],
                    }),
                    properties: vec![],
                }),
            ],
        };
        assert!(GraphTypePropertySchema::try_from_definition(&def).is_err());
    }

    #[test]
    fn collect_vocabulary_from_person_knows_shape() {
        let def = GraphTypeDefinition {
            span: Span::DUMMY,
            elements: vec![
                GraphTypeElement::Node(NodeTypeDef {
                    span: Span::DUMMY,
                    keyword: Keyword::new("NODE"),
                    name: Some("Person".to_string()),
                    alias: None,
                    label_set: Some(crate::ast::KeyLabelSet {
                        span: Span::DUMMY,
                        label_keyword_plural: false,
                        labels: vec!["Person".to_string()],
                    }),
                    properties: vec![],
                }),
                GraphTypeElement::Edge(EdgeTypeDef {
                    span: Span::DUMMY,
                    keyword: Keyword::new("EDGE"),
                    name: Some("KNOWS".to_string()),
                    direction: EdgeDirection::PointingRight,
                    source: EdgeEndpoint {
                        span: Span::DUMMY,
                        label: None,
                        type_name: Some("Person".to_string()),
                    },
                    destination: EdgeEndpoint {
                        span: Span::DUMMY,
                        label: None,
                        type_name: Some("Person".to_string()),
                    },
                    label_set: Some(crate::ast::KeyLabelSet {
                        span: Span::DUMMY,
                        label_keyword_plural: false,
                        labels: vec!["KNOWS".to_string()],
                    }),
                    properties: vec![],
                }),
            ],
        };
        let vocab = collect_graph_type_vocabulary(&def);
        assert_eq!(vocab.vertex_labels, BTreeSet::from(["Person".to_string()]));
        assert_eq!(vocab.edge_labels, BTreeSet::from(["KNOWS".to_string()]));
        assert!(vocab.properties.is_empty());
    }
}
