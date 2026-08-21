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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityColumn {
    /// The column holds an 8-byte vertex `element_id`.
    Vertex(&'static str),
    /// The column holds a 12-byte edge `element_id`.
    Edge(&'static str),
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
            let gleaph_sdk::GqlWireValue::Bytes(bytes) = value else {
                continue;
            };
            match column {
                IdentityColumn::Vertex(_) => {
                    if let Some(id) = VertexIdentity::from_bytes(bytes) {
                        out.vertices.push(id);
                    }
                }
                IdentityColumn::Edge(_) => {
                    if let Some(id) = EdgeIdentity::from_bytes(bytes) {
                        out.edges.push(id);
                    }
                }
            }
        }
    }
    Ok(out)
}

impl IdentityColumn {
    /// The column name on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            IdentityColumn::Vertex(name) | IdentityColumn::Edge(name) => name,
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
