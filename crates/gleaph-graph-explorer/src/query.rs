//! Prepared-query presets and execution (§7 of the roadmap).
//!
//! A [`QueryPreset`] describes a user-facing prepared query: a title, a
//! one-sentence explanation, the prepared-query name, and the presentation
//! columns that carry Gleaph element identities. The query itself remains a
//! Gleaph concern; the preset is an explorer concern.
//!
//! The vertical slice uses `topic-path-explanation`, which returns
//! `ELEMENT_ID(...)` edge identities — the prototype for result-to-entity
//! correlation described in the plan.

use gleaph_sdk::GqlQueryResult;

use crate::mapping::{EdgeIdentity, VertexIdentity};

/// A column in a query result that carries a Gleaph element identity.
///
/// Scalar columns carry one identity per row. List columns carry a list of
/// identities for a single row — e.g. a quantified or variable-length trail
/// whose hop-edge ids arrive as `List<Bytes>` (GAP-2026-08-26-001 hop-trail
/// semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityColumn {
    /// The column holds one 8-byte vertex `element_id`.
    Vertex(&'static str),
    /// The column holds one 12-byte edge `element_id`.
    Edge(&'static str),
    /// The column holds 8-byte vertex `element_id`s as a list.
    VertexList(&'static str),
    /// The column holds 12-byte edge `element_id`s as a list.
    EdgeList(&'static str),
}

/// A curated prepared-query preset.
#[derive(Debug, Clone)]
pub struct QueryPreset {
    /// Stable preset identifier.
    pub id: &'static str,
    /// User-facing title.
    pub title: &'static str,
    /// One-sentence explanation.
    pub description: &'static str,
    /// The prepared-query name registered on the Router.
    pub prepared_query: &'static str,
    /// The columns that carry Gleaph element identities, in result order.
    pub identity_columns: &'static [IdentityColumn],
}

/// The element identities extracted from one query result.
#[derive(Debug, Default)]
pub struct QueryIdentities {
    /// Vertex identities returned by the query.
    pub vertices: Vec<VertexIdentity>,
    /// Edge identities returned by the query.
    pub edges: Vec<EdgeIdentity>,
}

impl QueryIdentities {
    /// Whether the result carried no element identities.
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty() && self.edges.is_empty()
    }
}

/// Extract the element identities a preset cares about from a decoded result.
///
/// Rows are decoded from the SDK's `GqlQueryResult`; each identity column is
/// read as a `Bytes` wire value and parsed into a [`VertexIdentity`] or
/// [`EdgeIdentity`]. Malformed or missing values are skipped.
pub fn extract_identities(
    preset: &QueryPreset,
    result: &GqlQueryResult,
) -> Result<QueryIdentities, String> {
    let rows = result
        .decode_rows()
        .map_err(|e| format!("failed to decode rows: {e}"))?
        .ok_or_else(|| "query returned no rows blob".to_string())?;

    let mut out = QueryIdentities::default();
    for row in rows.rows {
        for column in preset.identity_columns {
            let value = row
                .columns
                .iter()
                .find(|(name, _)| name == column.name())
                .map(|(_, v)| v);
            let Some(value) = value else { continue };
            let is_list = matches!(
                column,
                IdentityColumn::VertexList(_) | IdentityColumn::EdgeList(_)
            );
            match (is_list, value) {
                // Scalar identity column: one `Bytes` value.
                (false, gleaph_sdk::GqlWireValue::Bytes(bytes)) => match column {
                    IdentityColumn::Vertex(_) | IdentityColumn::VertexList(_) => {
                        if let Some(id) = VertexIdentity::from_bytes(bytes) {
                            out.vertices.push(id);
                        }
                    }
                    IdentityColumn::Edge(_) | IdentityColumn::EdgeList(_) => {
                        if let Some(id) = EdgeIdentity::from_bytes(bytes) {
                            out.edges.push(id);
                        }
                    }
                },
                // List identity column: many `Bytes` values (hop-trail ids).
                (true, gleaph_sdk::GqlWireValue::List(items)) => {
                    for item in items {
                        let gleaph_sdk::GqlWireValue::Bytes(bytes) = item else {
                            continue;
                        };
                        match column {
                            IdentityColumn::VertexList(_) => {
                                if let Some(id) = VertexIdentity::from_bytes(bytes) {
                                    out.vertices.push(id);
                                }
                            }
                            IdentityColumn::EdgeList(_) => {
                                if let Some(id) = EdgeIdentity::from_bytes(bytes) {
                                    out.edges.push(id);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

impl IdentityColumn {
    /// The column name on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            IdentityColumn::Vertex(name)
            | IdentityColumn::Edge(name)
            | IdentityColumn::VertexList(name)
            | IdentityColumn::EdgeList(name) => name,
        }
    }
}

/// The `topic-path-explanation` preset: a four-edge relationship trail with
/// edge identities.
pub const TOPIC_PATH_PRESET: QueryPreset = QueryPreset {
    id: "topic-path-explanation",
    title: "Topic path explanation",
    description: "A four-edge trail from Alice to a Graph-databases topic, with edge identities.",
    prepared_query: "topic-path-explanation",
    identity_columns: &[
        IdentityColumn::Edge("follows_edge_id"),
        IdentityColumn::Edge("second_follows_edge_id"),
        IdentityColumn::Edge("posted_edge_id"),
        IdentityColumn::Edge("topic_edge_id"),
    ],
};

/// The four curated presets of the `demo/knowledge` graph.
///
/// `citation-reach` returns the reachable documents as a scalar `document_id`
/// plus the hop trail as a `cite_edge_id` list — the prototype for
/// list-column (`EdgeList`) overlay extraction.
///
/// `shortest-path` and `variable-length-reach` return concept vertices only
/// (`concept_id`); the matched edges are not surfaced on the wire, so the
/// explorer emphasizes the returned vertices and never fabricates edge
/// provenance (§9).
pub const KNOWLEDGE_PRESETS: &[&QueryPreset] = &[
    &CITATION_REACH_PRESET,
    &VARIABLE_LENGTH_REACH_PRESET,
    &SHORTEST_PATH_PRESET,
    &TEAM_READABLE_DOCUMENTS_PRESET,
];

/// Citation reach: Documents reachable from the GraphRAG paper through `CITES`
/// up to depth 3; the trail edges are returned as a hop list.
pub const CITATION_REACH_PRESET: QueryPreset = QueryPreset {
    id: "citation-reach",
    title: "Citation reach",
    description: "Documents citing GraphRAG retrieval, up to 3 hops, with the CITES trail.",
    prepared_query: "citation-reach",
    identity_columns: &[
        IdentityColumn::Vertex("document_id"),
        IdentityColumn::EdgeList("cite_edge_id"),
    ],
};

/// Variable-length reach: Concepts reachable from `graph-databases` through
/// `RELATED_TO` in 1..3 hops.
pub const VARIABLE_LENGTH_REACH_PRESET: QueryPreset = QueryPreset {
    id: "variable-length-reach",
    title: "Variable-length reach",
    description: "Concepts reachable from Graph databases through RELATED_TO in 1..3 hops.",
    prepared_query: "variable-length-reach",
    identity_columns: &[IdentityColumn::Vertex("concept_id")],
};

/// Shortest path: the shortest `RELATED_TO` chain between two Concepts.
pub const SHORTEST_PATH_PRESET: QueryPreset = QueryPreset {
    id: "shortest-path",
    title: "Shortest path",
    description: "The shortest RELATED_TO chain between Vector search and Graph databases.",
    prepared_query: "shortest-path",
    identity_columns: &[IdentityColumn::Vertex("concept_id")],
};

/// Access-constrained semantic search: Documents about `$query` that are public
/// or owned by a Team Platform member.
///
/// This preset runs the prepared query with no parameters, so the similarity
/// branch uses the browser-provided default vector; the emphasized vertices are
/// the returned Documents.
pub const TEAM_READABLE_DOCUMENTS_PRESET: QueryPreset = QueryPreset {
    id: "team-readable-documents",
    title: "Team-readable documents",
    description: "Public or Platform-owned Documents matching the semantic query.",
    prepared_query: "team-readable-documents",
    identity_columns: &[IdentityColumn::Vertex("document_id")],
};

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_ic_wire::{GqlWireRow, GqlWireRows, encode_rows_blob};
    use gleaph_sdk::types::GqlQueryResult;

    fn result(rows: Vec<GqlWireRow>) -> GqlQueryResult {
        GqlQueryResult {
            row_count: rows.len() as u64,
            rows_blob: Some(encode_rows_blob(&GqlWireRows { rows }).expect("encode")),
            phase: None,
            token: None,
            truncated: None,
        }
    }

    #[test]
    fn list_column_extracts_hop_trail_edges() {
        // citation-reach: document_id (scalar vertex) + cite_edge_id (list of edges).
        let row = GqlWireRow {
            columns: vec![
                (
                    "document_id".into(),
                    gleaph_sdk::types::GqlWireValue::Bytes(vec![1; 8]),
                ),
                (
                    "cite_edge_id".into(),
                    gleaph_sdk::types::GqlWireValue::List(vec![
                        gleaph_sdk::types::GqlWireValue::Bytes(vec![2; 12]),
                        gleaph_sdk::types::GqlWireValue::Bytes(vec![3; 12]),
                    ]),
                ),
            ],
        };
        let ids = extract_identities(&CITATION_REACH_PRESET, &result(vec![row])).expect("extract");
        assert_eq!(ids.vertices, vec![VertexIdentity([1; 8])]);
        assert_eq!(ids.edges.len(), 2);
        assert!(ids.edges.contains(&EdgeIdentity([2; 12])));
        assert!(ids.edges.contains(&EdgeIdentity([3; 12])));
    }

    #[test]
    fn scalar_vertex_column_extracts_single_vertex() {
        let row = GqlWireRow {
            columns: vec![(
                "concept_id".into(),
                gleaph_sdk::types::GqlWireValue::Bytes(vec![7; 8]),
            )],
        };
        let ids =
            extract_identities(&VARIABLE_LENGTH_REACH_PRESET, &result(vec![row])).expect("extract");
        assert_eq!(ids.vertices, vec![VertexIdentity([7; 8])]);
        assert!(ids.edges.is_empty());
    }

    #[test]
    fn malformed_or_missing_identities_skipped() {
        // Non-Bytes trailing column and an out-of-length edge bytes are skipped.
        let row = GqlWireRow {
            columns: vec![
                (
                    "document_id".into(),
                    gleaph_sdk::types::GqlWireValue::Bytes(vec![9; 8]),
                ),
                (
                    "cite_edge_id".into(),
                    gleaph_sdk::types::GqlWireValue::Text("nope".into()),
                ),
            ],
        };
        let ids = extract_identities(&CITATION_REACH_PRESET, &result(vec![row])).expect("extract");
        assert_eq!(ids.vertices.len(), 1);
        assert!(ids.edges.is_empty());
    }
}
