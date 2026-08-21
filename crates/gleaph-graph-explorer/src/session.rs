//! The central reusable abstraction: [`GraphExplorerSession`] (§5 of the
//! roadmap).
//!
//! The session owns graph-loading state, the identity map, the active query
//! preset, execution status, and the current presentation. It is a plain struct
//! (not a GPUI entity) so multiple UI components can observe and act upon the
//! same session, and so the async SDK calls are not tied to a GPUI context.

use gleaph_sdk::{GleaphClient, GqlQueryResult, ReadMode};

use crate::graph::{GraphLoad, GraphLoadSpec};
use crate::mapping::GraphIdentityMap;
use crate::presentation::{Presentation, build_presentation};
use crate::query::{QueryIdentities, QueryPreset, extract_identities};
/// The state of graph loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    /// No graph has been loaded yet.
    Idle,
    /// The graph is being loaded from the Router.
    Loading,
    /// The graph loaded successfully.
    Loaded,
    /// Graph loading failed.
    Failed,
}

/// The state of the most recent query execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryState {
    /// No query has been executed yet.
    Idle,
    /// A query is executing.
    Running,
    /// The last query succeeded.
    Succeeded,
    /// The last query failed.
    Failed,
}

/// A graph explorer session.
///
/// Owns the SDK client, the loaded graph, the identity map, the active preset,
/// and the current presentation. The session is not `Send`-safe by itself; the
/// SDK client is held behind an `Arc` so the session can be shared across UI
/// components.
pub struct GraphExplorerSession {
    client: GleaphClient,
    /// The loaded graph batch and identity map, present once loading succeeds.
    graph: Option<GraphLoad>,
    /// The identity map (a convenience handle into `graph`).
    map: GraphIdentityMap,
    /// The active query preset.
    active_preset: Option<&'static QueryPreset>,
    /// The most recent query result, if any.
    last_result: Option<GqlQueryResult>,
    /// The current presentation derived from the last result.
    presentation: Presentation,
    /// Graph loading state.
    pub load_state: LoadState,
    /// Query execution state.
    pub query_state: QueryState,
    /// The last error message, if any.
    pub error: Option<String>,
}

impl GraphExplorerSession {
    /// Create a session bound to an SDK client.
    pub fn new(client: GleaphClient) -> Self {
        Self {
            client,
            graph: None,
            map: GraphIdentityMap::new(),
            active_preset: None,
            last_result: None,
            presentation: Presentation::default(),
            load_state: LoadState::Idle,
            query_state: QueryState::Idle,
            error: None,
        }
    }

    /// The identity map correlating Gleaph identities to rendered elements.
    pub fn map(&self) -> &GraphIdentityMap {
        &self.map
    }

    /// Build the identity map from a scene that has merged this session's
    /// graph batch.
    ///
    /// Call this once after merging [`Self::graph`]'s batch into a
    /// [`gpui_graph::GraphScene`]. The map is required to correlate query
    /// results with rendered elements.
    pub fn build_map_from_scene(
        &mut self,
        scene: &gpui_graph::GraphScene<
            crate::mapping::VertexIdentity,
            crate::mapping::EdgeIdentity,
            crate::graph::ExplorerNode,
            crate::graph::ExplorerEdge,
        >,
    ) {
        if let Some(load) = &self.graph {
            self.map = crate::graph::build_map(scene, load);
        }
    }

    /// The loaded graph batch and identity map, if loading succeeded.
    pub fn graph(&self) -> Option<&GraphLoad> {
        self.graph.as_ref()
    }

    /// The active query preset.
    pub fn active_preset(&self) -> Option<&'static QueryPreset> {
        self.active_preset
    }

    /// The current presentation.
    pub fn presentation(&self) -> &Presentation {
        &self.presentation
    }

    /// The most recent query result, if any.
    pub fn last_result(&self) -> Option<&GqlQueryResult> {
        self.last_result.as_ref()
    }

    /// Load the whole graph using the given load spec.
    ///
    /// Runs the bounded queries described by `spec` and, on success, stores the
    /// resulting batch and identity map. This is an async operation; the caller
    /// drives it (e.g. from a GPUI background task).
    pub async fn load_graph(&mut self, spec: &GraphLoadSpec) -> Result<(), String> {
        self.load_state = LoadState::Loading;
        self.error = None;
        let result = self.load_graph_inner(spec).await;
        match result {
            Ok(load) => {
                self.graph = Some(load);
                self.load_state = LoadState::Loaded;
                Ok(())
            }
            Err(e) => {
                self.load_state = LoadState::Failed;
                self.error = Some(e.clone());
                Err(e)
            }
        }
    }

    async fn load_graph_inner(&self, spec: &GraphLoadSpec) -> Result<GraphLoad, String> {
        // 1. All vertex identities.
        let all_vertices = self
            .client
            .gql_query(spec.all_vertices_query.clone())
            .await
            .map_err(|e| format!("all-vertices query failed: {e}"))?;
        let vertex_ids = decode_vertex_ids(&all_vertices)?;

        // 2. Per-vertex-label queries recover label names and a display string.
        let mut vertices = Vec::new();
        for vq in &spec.vertex_label_queries {
            let result = self
                .client
                .gql_query(vq.query.clone())
                .await
                .map_err(|e| format!("vertex-label query ({}) failed: {e}", vq.label))?;
            let rows = result
                .decode_rows()
                .map_err(|e| format!("decode vertex-label rows: {e}"))?
                .ok_or_else(|| "vertex-label query returned no rows".to_string())?;
            for row in rows.rows {
                let id = row
                    .columns
                    .iter()
                    .find(|(name, _)| name == "id")
                    .and_then(|(_, v)| match v {
                        gleaph_sdk::GqlWireValue::Bytes(b) => Some(b.as_slice()),
                        _ => None,
                    })
                    .and_then(crate::mapping::VertexIdentity::from_bytes);
                let display = row
                    .columns
                    .iter()
                    .find(|(name, _)| name == "display")
                    .and_then(|(_, v)| match v {
                        gleaph_sdk::GqlWireValue::Text(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Some(identity) = id {
                    vertices.push(crate::graph::VertexRecord {
                        identity,
                        label: vq.label.clone(),
                        display,
                    });
                }
            }
        }

        // 3. Per-edge-label queries recover edge id, src, dst.
        let mut edges = Vec::new();
        for eq in &spec.edge_label_queries {
            let result = self
                .client
                .gql_query(eq.query.clone())
                .await
                .map_err(|e| format!("edge-label query ({}) failed: {e}", eq.label))?;
            let rows = result
                .decode_rows()
                .map_err(|e| format!("decode edge-label rows: {e}"))?
                .ok_or_else(|| "edge-label query returned no rows".to_string())?;
            for row in rows.rows {
                let get = |name: &str| {
                    row.columns
                        .iter()
                        .find(|(n, _)| n == name)
                        .and_then(|(_, v)| match v {
                            gleaph_sdk::GqlWireValue::Bytes(b) => Some(b.as_slice()),
                            _ => None,
                        })
                };
                let (Some(id), Some(src), Some(dst)) = (get("id"), get("src"), get("dst")) else {
                    continue;
                };
                let (Some(identity), Some(source), Some(target)) = (
                    crate::mapping::EdgeIdentity::from_bytes(id),
                    crate::mapping::VertexIdentity::from_bytes(src),
                    crate::mapping::VertexIdentity::from_bytes(dst),
                ) else {
                    continue;
                };
                edges.push(crate::graph::EdgeRecord {
                    identity,
                    source,
                    target,
                    label: eq.label.clone(),
                });
            }
        }

        // The all-vertices query is used only to size/validate; the per-label
        // queries are authoritative for the batch. Build the graph.
        let _ = vertex_ids;
        Ok(crate::graph::build_graph(vertices, edges))
    }

    /// Execute the active preset's prepared query and update the presentation.
    ///
    /// On success, the returned element identities are correlated through the
    /// identity map into a [`Presentation`]. The scene is not rebuilt.
    pub async fn run_active_query(&mut self) -> Result<QueryIdentities, String> {
        let Some(preset) = self.active_preset else {
            return Err("no active query preset".to_string());
        };
        self.query_state = QueryState::Running;
        self.error = None;
        let result = self
            .client
            .prepared_query(preset.prepared_query, Vec::new(), None, ReadMode::Eventual)
            .await
            .map_err(|e| format!("prepared query failed: {e}"))?;
        let identities = extract_identities(preset, &result)?;
        self.presentation = build_presentation(&self.map, &identities.vertices, &identities.edges);
        self.last_result = Some(result);
        self.query_state = QueryState::Succeeded;
        Ok(identities)
    }

    /// Set the active query preset.
    pub fn set_active_preset(&mut self, preset: &'static QueryPreset) {
        self.active_preset = Some(preset);
        self.query_state = QueryState::Idle;
    }
}

/// Decode all vertex identities from an all-vertices query result.
fn decode_vertex_ids(
    result: &GqlQueryResult,
) -> Result<Vec<crate::mapping::VertexIdentity>, String> {
    let rows = result
        .decode_rows()
        .map_err(|e| format!("decode all-vertices rows: {e}"))?
        .ok_or_else(|| "all-vertices query returned no rows".to_string())?;
    let mut out = Vec::with_capacity(rows.rows.len());
    for row in rows.rows {
        if let Some(id) = row
            .columns
            .iter()
            .find(|(name, _)| name == "id")
            .and_then(|(_, v)| match v {
                gleaph_sdk::GqlWireValue::Bytes(b) => Some(b.as_slice()),
                _ => None,
            })
            .and_then(crate::mapping::VertexIdentity::from_bytes)
        {
            out.push(id);
        }
    }
    Ok(out)
}
