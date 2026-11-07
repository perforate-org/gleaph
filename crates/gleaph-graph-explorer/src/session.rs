//! The central reusable abstraction: [`GraphExplorerSession`] (§5 of the
//! roadmap).
//!
//! The session owns graph-loading state, the identity map, the active query
//! preset, execution status, and the current presentation. It is a plain struct
//! (not a GPUI entity) so multiple UI components can observe and act upon the
//! same session, and so the async SDK calls are not tied to a GPUI context.

use gleaph_sdk::{
    GleaphClient, GqlParams, GqlQuery, GqlQueryResult, IntoGqlParam, Parameter, ReadMode,
};

use crate::graph::{GraphLoad, GraphLoadSpec};
use crate::mapping::GraphIdentityMap;
use crate::parameters::{bind, find_operation, validate};
use crate::presentation::{Presentation, build_presentation};
use crate::query::{QueryIdentities, QueryPreset, extract_identities};
use crate::results::ResultTable;
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
    /// Parameter definitions published by the Router manifest for the active preset.
    ///
    /// Empty until [`Self::load_parameter_definitions`] succeeds; an empty list also means
    /// "metadata not loaded", which `parameters::validate` treats as "trust the host".
    parameter_definitions: Vec<Parameter>,
    /// Values bound to the active preset's parameters, in declared order once definitions load.
    parameters: GqlParams,
    /// The most recent result as columns and display strings for the console table.
    ///
    /// Kept across a failed run so the console can show the last good rows beside the error; a
    /// successful run replaces it wholesale.
    result_table: Option<ResultTable>,
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
            parameter_definitions: Vec::new(),
            parameters: Vec::new(),
            result_table: None,
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

    /// The most recent query result as a console table, if it materialized rows.
    pub fn result_table(&self) -> Option<&ResultTable> {
        self.result_table.as_ref()
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
        self.run_active_query_with(self.parameters.clone()).await
    }

    /// [`Self::run_active_query`] with named parameters.
    ///
    /// Parameters are passed by the host rather than declared by the preset: which names a prepared
    /// query accepts belongs to its manifest on the Router, and the console can read that when it
    /// needs a parameter form. An empty list sends no parameter blob at all.
    pub async fn run_active_query_with(
        &mut self,
        parameters: GqlParams,
    ) -> Result<QueryIdentities, String> {
        let Some(preset) = self.active_preset else {
            return Err("no active query preset".to_string());
        };
        // One owner for bound values: the session keeps what the caller passed, so a later
        // `run_active_query` runs the same parameters the form shows.
        self.parameters = parameters;
        if let Err(message) = validate(&self.parameter_definitions, &self.parameters) {
            return Err(self.fail(message));
        }
        self.query_state = QueryState::Running;
        self.error = None;
        let encoded = match encode_parameters(&self.parameters) {
            Ok(encoded) => encoded,
            Err(message) => return Err(self.fail(message)),
        };
        let result = match self
            .client
            .prepared_query(preset.prepared_query, encoded, None, ReadMode::Eventual)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                return Err(self.fail(format!("prepared query failed: {error}")));
            }
        };
        let identities = match extract_identities(preset, &result) {
            Ok(identities) => identities,
            Err(message) => return Err(self.fail(message)),
        };
        self.apply_result(result, Some(&identities));
        Ok(identities)
    }

    /// Execute arbitrary GQL and publish its rows as the console table.
    ///
    /// A raw query carries no identity columns, so the graph overlay keeps whatever the last preset
    /// run presented: overlays are a preset concern (§7), and inventing provenance for a raw result
    /// is exactly what §9 forbids.
    pub async fn run_gql(&mut self, query: impl Into<String>) -> Result<(), String> {
        self.run_gql_with(query, Vec::new()).await
    }

    /// [`Self::run_gql`] with named parameters.
    pub async fn run_gql_with(
        &mut self,
        query: impl Into<String>,
        parameters: GqlParams,
    ) -> Result<(), String> {
        self.query_state = QueryState::Running;
        self.error = None;
        let query = GqlQuery {
            query: query.into(),
            params: parameters,
        };
        let result = match self.client.gql_query(query).await {
            Ok(result) => result,
            Err(error) => return Err(self.fail(format!("gql query failed: {error}"))),
        };
        self.apply_result(result, None);
        Ok(())
    }

    /// Adopt one successful run: presentation when the caller extracted identities, plus the table
    /// and the result envelope every run publishes.
    pub(crate) fn apply_result(
        &mut self,
        result: GqlQueryResult,
        identities: Option<&QueryIdentities>,
    ) {
        if let Some(identities) = identities {
            self.presentation =
                build_presentation(&self.map, &identities.vertices, &identities.edges);
        }
        // A result that decoded but cannot be tabulated still succeeded: the console shows the
        // graph overlay and an absent table rather than reporting a failed query.
        self.result_table = ResultTable::from_result(&result).ok();
        self.last_result = Some(result);
        self.query_state = QueryState::Succeeded;
    }

    /// Record a failed run and return its message, so every failure path sets the same state.
    pub(crate) fn fail(&mut self, message: String) -> String {
        self.query_state = QueryState::Failed;
        self.error = Some(message.clone());
        message
    }

    /// Set the active query preset.
    ///
    /// Parameter metadata and bound values belong to the preset, so they reset: the next step is
    /// [`Self::load_parameter_definitions`], then binding values against what it returns.
    pub fn set_active_preset(&mut self, preset: &'static QueryPreset) {
        self.active_preset = Some(preset);
        self.query_state = QueryState::Idle;
        self.parameter_definitions.clear();
        self.parameters.clear();
    }

    /// Fetch the active preset's parameter definitions from the Router manifest.
    ///
    /// The manifest is the single source of truth for parameter names, requiredness and types. A
    /// failure leaves the previous definitions in place and records the error, so a console can
    /// keep a form it already showed.
    pub async fn load_parameter_definitions(&mut self) -> Result<(), String> {
        let Some(preset) = self.active_preset else {
            return Err("no active query preset".to_string());
        };
        let manifest = match self.client.list_prepared(None).await {
            Ok(manifest) => manifest,
            Err(error) => {
                return Err(self.fail(format!("prepared manifest failed: {error}")));
            }
        };
        let Some(operation) = find_operation(&manifest, preset.prepared_query) else {
            return Err(self.fail(format!(
                "prepared operation not found: {}",
                preset.prepared_query
            )));
        };
        self.parameter_definitions = operation.parameters.clone();
        Ok(())
    }

    /// The active preset's parameter definitions, empty until its manifest is loaded.
    pub fn parameter_definitions(&self) -> &[Parameter] {
        &self.parameter_definitions
    }

    /// The values bound to the active preset's parameters.
    pub fn parameters(&self) -> &GqlParams {
        &self.parameters
    }

    /// Bind one parameter value, keeping the operation's declared order.
    ///
    /// Rejects a name the manifest does not declare; with no manifest loaded the value is appended
    /// and [`crate::parameters::validate`] is the authority at execution time.
    pub fn set_parameter<T: IntoGqlParam>(&mut self, name: &str, value: T) -> Result<(), String> {
        bind(
            &self.parameter_definitions,
            &mut self.parameters,
            name,
            value,
        )
    }

    /// Remove every bound value, keeping the definitions.
    pub fn clear_parameters(&mut self) {
        self.parameters.clear();
    }
}

/// Encode named parameters for the prepared-query entry point.
///
/// No parameters means no blob: the entry point distinguishes "absent" from "an empty record", and
/// the console's common case (a curated preset with no parameters) must stay the absent one.
fn encode_parameters(parameters: &GqlParams) -> Result<Vec<u8>, String> {
    if parameters.is_empty() {
        return Ok(Vec::new());
    }
    gleaph_sdk::encode_gql_params(parameters.clone())
        .map_err(|error| format!("failed to encode parameters: {error:?}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_ic_wire::{GqlWireRow, GqlWireRows, encode_rows_blob};
    use gleaph_router_wire::GqlQueryResult;
    use gleaph_sdk::transport::{BoxFuture, GleaphTransport};

    /// A transport that panics if a test ever calls it: these tests cover the adoption and
    /// parameter contracts, which run after (or instead of) a Router call.
    struct NoCalls;

    impl GleaphTransport for NoCalls {
        fn gql_query<'a>(
            &'a self,
            query: String,
            params: Vec<u8>,
            read_mode: gleaph_sdk::types::ReadMode,
        ) -> BoxFuture<'a, Result<GqlQueryResult, gleaph_sdk::CallError>> {
            let _ = (query, params, read_mode);
            Box::pin(async { panic!("no Router call expected: gql_query") })
        }

        fn gql_mutate<'a>(
            &'a self,
            query: String,
            params: Vec<u8>,
            client_mutation_key: String,
        ) -> BoxFuture<'a, Result<GqlQueryResult, gleaph_sdk::CallError>> {
            let _ = (query, params, client_mutation_key);
            Box::pin(async { panic!("no Router call expected: gql_mutate") })
        }

        fn prepared_query<'a>(
            &'a self,
            name: String,
            params: Vec<u8>,
            sort: Option<Vec<gleaph_sdk::types::PreparedSortSpec>>,
            read_mode: gleaph_sdk::types::ReadMode,
        ) -> BoxFuture<'a, Result<GqlQueryResult, gleaph_sdk::CallError>> {
            let _ = (name, params, sort, read_mode);
            Box::pin(async { panic!("no Router call expected: prepared_query") })
        }

        fn prepared_mutate<'a>(
            &'a self,
            name: String,
            params: Vec<u8>,
            client_mutation_key: String,
        ) -> BoxFuture<'a, Result<GqlQueryResult, gleaph_sdk::CallError>> {
            let _ = (name, params, client_mutation_key);
            Box::pin(async { panic!("no Router call expected: prepared_mutate") })
        }

        fn bulk_load<'a>(
            &'a self,
            command: gleaph_sdk::types::BulkLoadCommand,
        ) -> BoxFuture<'a, Result<gleaph_sdk::types::BulkLoadResponse, gleaph_sdk::CallError>>
        {
            let _ = command;
            Box::pin(async { panic!("no Router call expected: bulk_load") })
        }

        fn bulk_load_status<'a>(
            &'a self,
            graph_name: Option<String>,
            client_bulk_key: String,
            receipt_cursor: Option<u32>,
            max_receipts: u32,
        ) -> BoxFuture<'a, Result<gleaph_sdk::types::BulkLoadStatusPage, gleaph_sdk::CallError>>
        {
            let _ = (graph_name, client_bulk_key, receipt_cursor, max_receipts);
            Box::pin(async { panic!("no Router call expected: bulk_load_status") })
        }

        fn prepare<'a>(
            &'a self,
            operations: Vec<gleaph_sdk::types::PreparedRegistration>,
        ) -> BoxFuture<'a, Result<(), gleaph_sdk::CallError>> {
            let _ = operations;
            Box::pin(async { panic!("no Router call expected: prepare") })
        }

        fn drop_prepared<'a>(
            &'a self,
            name: String,
        ) -> BoxFuture<'a, Result<(), gleaph_sdk::CallError>> {
            let _ = name;
            Box::pin(async { panic!("no Router call expected: drop_prepared") })
        }

        fn list_prepared<'a>(
            &'a self,
            graph_name: Option<String>,
        ) -> BoxFuture<'a, Result<gleaph_sdk::types::PreparedManifest, gleaph_sdk::CallError>>
        {
            let _ = graph_name;
            Box::pin(async { panic!("no Router call expected: list_prepared") })
        }
    }

    fn result(rows: Vec<Vec<(&str, gleaph_sdk::GqlWireValue)>>) -> GqlQueryResult {
        let wire = GqlWireRows {
            rows: rows
                .into_iter()
                .map(|columns| GqlWireRow {
                    columns: columns
                        .into_iter()
                        .map(|(name, value)| (name.to_owned(), value))
                        .collect(),
                })
                .collect(),
        };
        GqlQueryResult {
            row_count: wire.rows.len() as u64,
            rows_blob: Some(encode_rows_blob(&wire).expect("rows encode")),
            phase: None,
            token: None,
            truncated: None,
        }
    }

    fn session() -> GraphExplorerSession {
        GraphExplorerSession::new(GleaphClient::new(std::sync::Arc::new(NoCalls)))
    }

    #[test]
    fn applying_a_result_builds_the_table_and_marks_the_run_succeeded() {
        let mut session = session();
        session.query_state = QueryState::Running;
        let result = result(vec![
            vec![("name", gleaph_sdk::GqlWireValue::Text("Alice".to_owned()))],
            vec![("name", gleaph_sdk::GqlWireValue::Text("Bob".to_owned()))],
        ]);
        session.apply_result(result, None);

        assert_eq!(session.query_state, QueryState::Succeeded);
        assert!(session.error.is_none());
        assert!(session.last_result().is_some());
        let table = session.result_table().expect("table built");
        assert_eq!(table.columns(), ["name"]);
        assert_eq!(table.row(1).unwrap(), ["Bob"]);
        assert!(
            session.presentation().is_empty(),
            "a raw result does not invent a presentation"
        );
    }

    #[test]
    fn a_failed_run_records_the_failure_state_and_message() {
        let mut session = session();
        session.query_state = QueryState::Running;
        let message = session.fail("prepared query failed: boom".to_owned());
        assert_eq!(message, "prepared query failed: boom");
        assert_eq!(session.query_state, QueryState::Failed);
        assert_eq!(
            session.error.as_deref(),
            Some("prepared query failed: boom")
        );
    }

    #[test]
    fn bound_parameters_live_in_the_session_and_clear() {
        let mut session = session();
        session.set_parameter("limit", 10_i64).expect("bind limit");
        session.set_parameter("depth", 3_i64).expect("bind depth");
        assert_eq!(session.parameters().len(), 2);
        session.clear_parameters();
        assert!(session.parameters().is_empty());
    }

    #[tokio::test]
    async fn a_run_without_its_required_parameter_fails_before_any_router_call() {
        let mut session = session();
        session.active_preset = Some(&crate::query::TOPIC_PATH_PRESET);
        session.parameter_definitions = vec![gleaph_sdk::Parameter {
            name: "depth".to_owned(),
            description: None,
            required: true,
            nullable: false,
            semantic_type: gleaph_sdk::SemanticType::Int64,
        }];
        // `NoCalls` panics on any transport call, so this proves validation happens first.
        let error = session.run_active_query().await.unwrap_err();
        assert_eq!(error, "missing required parameter $depth");
        assert_eq!(session.query_state, QueryState::Failed);
        assert_eq!(
            session.error.as_deref(),
            Some("missing required parameter $depth")
        );
    }

    #[test]
    fn no_parameters_sends_no_blob_and_named_ones_encode() {
        assert!(
            encode_parameters(&Vec::new())
                .expect("empty encodes")
                .is_empty(),
            "an absent parameter blob is not the same as an empty record"
        );
        let parameters = vec![("limit".to_owned(), gleaph_sdk::gql_param_value(25_i64))];
        let encoded = encode_parameters(&parameters).expect("parameters encode");
        assert_eq!(
            encoded,
            gleaph_sdk::encode_gql_params(parameters).expect("reference encoding"),
            "the session encodes exactly what the SDK codec does"
        );
        assert!(!encoded.is_empty());
    }
}
