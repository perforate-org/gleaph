//! SDK → `gpui-graph` conversion (§5 of the roadmap).
//!
//! This module owns the boundary between Gleaph graph data and the
//! `gpui-graph` representation. It defines Gleaph-agnostic node/edge payloads
//! and a pure [`build_graph`] that turns decoded topology records into a
//! [`gpui_graph::GraphBatch`] plus a [`mapping::GraphIdentityMap`]. No
//! Gleaph-specific type leaks into `gpui-graph`, and no UI styling happens
//! during conversion.
//!
//! Whole-graph loading is measured (2026-08-21) to require multiple bounded
//! SDK queries under the 2 MiB safe payload limit: one all-vertex
//! `element_id` query, one query per vertex label (to recover label names and
//! a display property), and one query per edge label (edge `id`, `src`,
//! `dst`). The [`GraphLoadSpec`] describes those queries; the async execution
//! lives in [`crate::session`].

use gpui_graph::{EdgeDirection, GraphBatch};

use crate::mapping::{EdgeIdentity, GraphIdentityMap, VertexIdentity};

/// A rendered node payload. Gleaph-agnostic: carries only the vertex label and
/// a display string, never a Gleaph identity or SDK type.
#[derive(Debug, Clone)]
pub struct ExplorerNode {
    /// The vertex label (e.g. `User`, `Post`).
    pub label: String,
    /// A human-readable display string for the node.
    pub display: String,
}

/// A rendered edge payload. Gleaph-agnostic: carries only the edge label.
#[derive(Debug, Clone)]
pub struct ExplorerEdge {
    /// The edge label (e.g. `FOLLOWS`, `POSTED`).
    pub label: String,
}

/// One decoded vertex topology record.
#[derive(Debug, Clone)]
pub struct VertexRecord {
    /// The Gleaph vertex identity.
    pub identity: VertexIdentity,
    /// The vertex label.
    pub label: String,
    /// A display string (may be empty when the loader did not fetch one).
    pub display: String,
}

/// One decoded edge topology record.
#[derive(Debug, Clone)]
pub struct EdgeRecord {
    /// The Gleaph edge identity.
    pub identity: EdgeIdentity,
    /// The source vertex identity.
    pub source: VertexIdentity,
    /// The target vertex identity.
    pub target: VertexIdentity,
    /// The edge label.
    pub label: String,
}

/// The result of converting decoded topology records into a scene batch.
#[derive(Debug)]
pub struct GraphLoad {
    /// The batch to merge into a [`gpui_graph::GraphScene`].
    pub batch: GraphBatch<VertexIdentity, EdgeIdentity, ExplorerNode, ExplorerEdge>,
    /// The Gleaph vertex identities, in the same order they were added to the
    /// batch. Used to build the identity map after the batch is merged.
    pub vertex_identities: Vec<VertexIdentity>,
    /// The Gleaph edge identities, in the same order they were added to the
    /// batch.
    pub edge_identities: Vec<EdgeIdentity>,
}

/// Convert decoded vertex and edge topology records into a scene batch.
///
/// Edges whose endpoints are not present in `vertices` are skipped (the
/// `gpui-graph` scene cannot create an edge without both endpoints).
pub fn build_graph(vertices: Vec<VertexRecord>, edges: Vec<EdgeRecord>) -> GraphLoad {
    let mut batch = GraphBatch::new();
    let mut vertex_identities = Vec::with_capacity(vertices.len());
    let mut edge_identities = Vec::new();

    for vertex in vertices {
        batch = batch.node(
            vertex.identity,
            ExplorerNode {
                label: vertex.label,
                display: vertex.display,
            },
        );
        vertex_identities.push(vertex.identity);
    }

    for edge in edges {
        // Skip edges whose endpoints are not in the vertex set.
        if !vertex_identities.contains(&edge.source) || !vertex_identities.contains(&edge.target) {
            continue;
        }
        batch = batch.edge(
            edge.identity,
            edge.source,
            edge.target,
            EdgeDirection::Directed,
            ExplorerEdge { label: edge.label },
        );
        edge_identities.push(edge.identity);
    }

    GraphLoad {
        batch,
        vertex_identities,
        edge_identities,
    }
}

/// Build the identity map from a merged scene and the ordered identity lists.
///
/// The batch must already be merged into `scene` (via
/// [`gpui_graph::GraphScene::merge`]); the scene resolves each Gleaph identity
/// to its rendered [`gpui_graph::NodeId`]/[`gpui_graph::EdgeId`].
pub fn build_map(
    scene: &gpui_graph::GraphScene<VertexIdentity, EdgeIdentity, ExplorerNode, ExplorerEdge>,
    load: &GraphLoad,
) -> GraphIdentityMap {
    let mut map = GraphIdentityMap::new();
    for identity in &load.vertex_identities {
        if let Some(node) = scene.node_id(identity) {
            map.insert_vertex(*identity, node);
        }
    }
    for identity in &load.edge_identities {
        if let Some(edge) = scene.edge_id(identity) {
            map.insert_edge(*identity, edge);
        }
    }
    map
}

/// Describes the bounded queries used to load the whole graph.
///
/// The loader runs one all-vertex query, one query per vertex label, and one
/// query per edge label. Each query must stay under the 2 MiB safe payload
/// limit; the caller is responsible for choosing label scopes that do.
#[derive(Debug, Clone)]
pub struct GraphLoadSpec {
    /// The GQL query returning every vertex `element_id` as column `id`.
    pub all_vertices_query: String,
    /// Per-vertex-label queries returning `element_id(n)` as `id` and a display
    /// property as `display`, scoped `MATCH (n:Label)`.
    pub vertex_label_queries: Vec<VertexLabelQuery>,
    /// Per-edge-label queries returning `element_id(e)` as `id`,
    /// `element_id(a)` as `src`, `element_id(b)` as `dst`, scoped
    /// `MATCH (a)-[e:Label]->(b)`.
    pub edge_label_queries: Vec<EdgeLabelQuery>,
}

/// One per-vertex-label load query.
#[derive(Debug, Clone)]
pub struct VertexLabelQuery {
    /// The vertex label this query scopes to.
    pub label: String,
    /// The GQL query text.
    pub query: String,
}

/// One per-edge-label load query.
#[derive(Debug, Clone)]
pub struct EdgeLabelQuery {
    /// The edge label this query scopes to.
    pub label: String,
    /// The GQL query text.
    pub query: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::{EdgeIdentity, VertexIdentity};

    fn vid(n: u8) -> VertexIdentity {
        VertexIdentity([n; 8])
    }

    fn eid(n: u8) -> EdgeIdentity {
        EdgeIdentity([n; 12])
    }

    #[test]
    fn build_graph_creates_batch_and_identity_lists() {
        let load = build_graph(
            vec![
                VertexRecord {
                    identity: vid(1),
                    label: "User".into(),
                    display: "alice".into(),
                },
                VertexRecord {
                    identity: vid(2),
                    label: "User".into(),
                    display: "bob".into(),
                },
            ],
            vec![EdgeRecord {
                identity: eid(1),
                source: vid(1),
                target: vid(2),
                label: "FOLLOWS".into(),
            }],
        );
        assert_eq!(load.vertex_identities, vec![vid(1), vid(2)]);
        assert_eq!(load.edge_identities, vec![eid(1)]);
        assert_eq!(load.batch.nodes.len(), 2);
        assert_eq!(load.batch.edges.len(), 1);
    }

    #[test]
    fn build_graph_skips_edges_with_missing_endpoints() {
        let load = build_graph(
            vec![VertexRecord {
                identity: vid(1),
                label: "User".into(),
                display: "alice".into(),
            }],
            vec![EdgeRecord {
                identity: eid(1),
                source: vid(1),
                target: vid(99), // not present
                label: "FOLLOWS".into(),
            }],
        );
        assert_eq!(load.edge_identities.len(), 0);
        assert_eq!(load.batch.edges.len(), 0);
    }

    #[test]
    fn build_map_resolves_identities_through_merged_scene() {
        let load = build_graph(
            vec![
                VertexRecord {
                    identity: vid(1),
                    label: "User".into(),
                    display: "alice".into(),
                },
                VertexRecord {
                    identity: vid(2),
                    label: "User".into(),
                    display: "bob".into(),
                },
            ],
            vec![EdgeRecord {
                identity: eid(1),
                source: vid(1),
                target: vid(2),
                label: "FOLLOWS".into(),
            }],
        );
        let mut scene = gpui_graph::GraphScene::new();
        scene.merge(load.batch.clone());
        let map = build_map(&scene, &load);
        assert_eq!(map.vertex_count(), 2);
        assert_eq!(map.edge_count(), 1);
        assert!(map.node_for_vertex(&vid(1)).is_some());
        assert!(map.edge_for_edge(&eid(1)).is_some());
    }
}
