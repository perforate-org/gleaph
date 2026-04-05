use crate::auth::{AccessLevel, AclEntry, AuthContext, Operation, PermissionChecker, Principal};
use crate::prepared::{
    PreparedQueryInfo, PreparedQueryKind, PreparedQueryRegistry, PreparedSortSpec,
    plan_for_prepared_execute,
};
use crate::{
    ApiDropPreparedEndpointRequest, ApiDropPreparedRequest, ApiDropPreparedResponse,
    ApiExecuteEndpointRequest, ApiExecutePreparedEndpointRequest, ApiExecutePreparedRequest,
    ApiListPreparedEndpointRequest, ApiListPreparedResponse, ApiPlanEndpointRequest,
    ApiPlanResponse, ApiPrepareEndpointRequest, ApiPrepareRequest, ApiPrepareResponse,
    ApiPreparedQueryInfo, ApiQueryRequest, ApiQueryResponse, GleaphError, QueryRequest,
    QueryResponse, execute_plan_with_normalized_params, parse_block, plan_request,
};
use candid::CandidType;
use gleaph_gql::Value;
use gleaph_gql_executor::{
    ExecutionContext, GraphRegistryResolver, ProcedureRegistry, UseGraphRouter,
};
use gleaph_gql_planner::{GraphStats, PlanOp, build_block_plan_output};
use gleaph_graph_kernel::{GraphRead, GraphWrite};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

fn caller_value_from_auth(auth: &AuthContext) -> Option<Value> {
    let p = auth.query_subject.as_ref().or(auth.caller.as_ref())?;
    Some(Value::Extension(Box::new(gleaph_gql_ic::PrincipalValue(*p))))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct PreparedQuerySnapshot {
    pub name: String,
    pub query: String,
    pub options: Option<crate::PreparedOptions>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct GleaphServiceSnapshot {
    pub acl_entries: Vec<AclEntry>,
    pub prepared_queries: Vec<PreparedQuerySnapshot>,
    pub supported_extension_types: Vec<String>,
}

#[derive(Clone)]
pub struct GleaphService {
    permissions: PermissionChecker,
    prepared: PreparedQueryRegistry,
    supported_extension_types: BTreeSet<String>,
    procedure_registry: Arc<dyn ProcedureRegistry>,
    graph_registry_resolver: Option<Arc<dyn GraphRegistryResolver>>,
    ic_graph_registry_resolver: Option<crate::IcGraphRegistryResolver>,
    use_graph_router: Option<Arc<dyn UseGraphRouter>>,
    /// Peers allowed to supply `query_subject` on routed query endpoints (`msg_caller` must match).
    federation_trusted_callers: BTreeSet<Principal>,
}

impl Default for GleaphService {
    fn default() -> Self {
        Self {
            permissions: PermissionChecker::default(),
            prepared: PreparedQueryRegistry::default(),
            supported_extension_types: BTreeSet::default(),
            procedure_registry: crate::standard_procedure_registry(),
            graph_registry_resolver: None,
            ic_graph_registry_resolver: None,
            use_graph_router: None,
            federation_trusted_callers: BTreeSet::new(),
        }
    }
}

impl std::fmt::Debug for GleaphService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GleaphService")
            .field("permissions", &self.permissions)
            .field("prepared", &self.prepared)
            .field("supported_extension_types", &self.supported_extension_types)
            .field("procedure_registry", &"<configured>")
            .field(
                "graph_registry_resolver",
                &self
                    .graph_registry_resolver
                    .as_ref()
                    .map(|_| "<configured>"),
            )
            .field(
                "ic_graph_registry_resolver",
                &self
                    .ic_graph_registry_resolver
                    .as_ref()
                    .map(|_| "<configured>"),
            )
            .field(
                "use_graph_router",
                &self.use_graph_router.as_ref().map(|_| "<configured>"),
            )
            .field(
                "federation_trusted_callers",
                &self.federation_trusted_callers.len(),
            )
            .finish()
    }
}

impl GleaphService {
    pub fn new() -> Self {
        let mut service = Self::default();
        gleaph_gql_ic::IcExtensionBinaryDecode::for_each_extension_type(|name| {
            service.register_extension_type(name);
        });
        service
    }

    pub fn permissions(&self) -> &PermissionChecker {
        &self.permissions
    }

    pub fn permissions_mut(&mut self) -> &mut PermissionChecker {
        &mut self.permissions
    }

    /// Replaces the procedure registry used by all service execution paths.
    pub fn set_procedure_registry(&mut self, registry: Arc<dyn ProcedureRegistry>) {
        self.procedure_registry = registry;
    }

    /// Returns the currently configured procedure registry.
    pub fn procedure_registry(&self) -> Arc<dyn ProcedureRegistry> {
        Arc::clone(&self.procedure_registry)
    }

    /// Replaces the graph registry resolver used by `USE GRAPH`.
    pub fn set_graph_registry_resolver(&mut self, resolver: Arc<dyn GraphRegistryResolver>) {
        self.graph_registry_resolver = Some(resolver);
    }

    /// Convenience helper for an IC registry-backed resolver.
    pub fn set_ic_graph_registry_resolver(&mut self, resolver: crate::IcGraphRegistryResolver) {
        self.graph_registry_resolver = Some(Arc::new(resolver.clone()));
        self.ic_graph_registry_resolver = Some(resolver);
    }

    /// Clears the configured graph registry resolver.
    pub fn clear_graph_registry_resolver(&mut self) {
        self.graph_registry_resolver = None;
        self.ic_graph_registry_resolver = None;
    }

    /// Refreshes registry cache entries for all `USE GRAPH` names found in the query plan.
    pub async fn warm_use_graph_cache_for_query(
        &self,
        auth: &AuthContext,
        request: &QueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<Vec<String>, GleaphError> {
        let Some(resolver) = self.ic_graph_registry_resolver.as_ref() else {
            return Ok(Vec::new());
        };
        self.ensure_allowed(auth, Operation::PlanQuery)?;
        let block = crate::parse_block(&request.query)?;
        self.ensure_supported_extension_types_in_block(&block)?;
        let planned = build_block_plan_output(&block, stats)?;
        crate::ensure_plan_supported_by_executor(&planned.plan)?;
        let mut names = Vec::new();
        collect_use_graph_names(&planned.plan.ops, &mut names);
        names.sort();
        names.dedup();
        for name in &names {
            resolver.refresh_graph(name).await?;
        }
        Ok(names)
    }

    /// Clears IC registry resolver cache entries (`None` = entire cache).
    pub fn invalidate_graph_registry_cache(
        &self,
        auth: &AuthContext,
        graph_name: Option<&str>,
    ) -> Result<(), GleaphError> {
        let Some(resolver) = self.ic_graph_registry_resolver.as_ref() else {
            return Ok(());
        };
        self.ensure_allowed(auth, Operation::PlanQuery)?;
        match graph_name {
            Some(name) if !name.is_empty() => resolver.invalidate_cached_graph(name),
            _ => resolver.clear_cache(),
        }
        Ok(())
    }

    /// Replaces the router used for remote `USE GRAPH` delegation.
    pub fn set_use_graph_router(&mut self, router: Arc<dyn UseGraphRouter>) {
        self.use_graph_router = Some(router);
    }

    /// Convenience helper for an IC call-backed `USE GRAPH` router.
    pub fn set_ic_use_graph_router(&mut self, router: crate::IcUseGraphRouter) {
        self.use_graph_router = Some(Arc::new(router));
    }

    /// Clears the configured remote `USE GRAPH` router.
    pub fn clear_use_graph_router(&mut self) {
        self.use_graph_router = None;
    }

    /// Allow this peer principal to pass `query_subject` on federated routed-query endpoints.
    pub fn add_federation_trusted_caller(&mut self, caller: Principal) {
        self.federation_trusted_callers.insert(caller);
    }

    pub fn remove_federation_trusted_caller(&mut self, caller: &Principal) -> bool {
        self.federation_trusted_callers.remove(caller)
    }

    pub fn clear_federation_trusted_callers(&mut self) {
        self.federation_trusted_callers.clear();
    }

    pub fn federation_trusted_callers(&self) -> &BTreeSet<Principal> {
        &self.federation_trusted_callers
    }

    /// Builds [`AuthContext`] for `execute_routed_query*` after validating optional delegation.
    pub fn auth_for_routed_query(
        &self,
        msg_caller: Principal,
        is_controller: bool,
        query_subject: Option<Principal>,
    ) -> Result<AuthContext, GleaphError> {
        let mut auth = AuthContext {
            caller: Some(msg_caller),
            is_controller,
            query_subject: None,
        };
        if let Some(subject) = query_subject {
            if is_controller || self.federation_trusted_callers.contains(&msg_caller) {
                auth.query_subject = Some(subject);
            } else {
                return Err(GleaphError::FederationRoutedQuery(
                    "query_subject requires caller to be a controller or in federation_trusted_callers"
                        .to_owned(),
                ));
            }
        }
        Ok(auth)
    }

    /// Registers one host-side supported extension type name (case-insensitive).
    pub fn register_extension_type(&mut self, type_name: impl Into<String>) {
        self.supported_extension_types
            .insert(type_name.into().to_ascii_uppercase());
    }

    /// Removes one host-side supported extension type name.
    pub fn unregister_extension_type(&mut self, type_name: &str) -> bool {
        self.supported_extension_types
            .remove(&type_name.to_ascii_uppercase())
    }

    pub fn snapshot(&self) -> GleaphServiceSnapshot {
        let prepared_queries = self
            .prepared
            .list()
            .into_iter()
            .map(|prepared| PreparedQuerySnapshot {
                name: prepared.name,
                query: prepared.source,
                options: Some(crate::PreparedOptions {
                    description: prepared.description,
                    allowed_sorts: prepared.allowed_sorts,
                    default_sort: prepared.default_sort,
                }),
            })
            .collect();
        GleaphServiceSnapshot {
            acl_entries: self.permissions.list_acl_entries(),
            prepared_queries,
            supported_extension_types: self.supported_extension_types.iter().cloned().collect(),
        }
    }

    pub fn from_snapshot(snapshot: GleaphServiceSnapshot) -> Result<Self, GleaphError> {
        let mut service = Self::new();
        service.supported_extension_types.clear();
        for type_name in snapshot.supported_extension_types {
            service.register_extension_type(type_name);
        }
        for acl in snapshot.acl_entries {
            service.permissions.set_acl_entry(acl.principal, acl.level);
        }
        for prepared in snapshot.prepared_queries {
            service.prepared.prepare(
                prepared.name,
                prepared.query,
                prepared.options.as_ref(),
                None,
            )?;
        }
        Ok(service)
    }

    pub fn set_acl_entry(
        &mut self,
        auth: &AuthContext,
        principal: impl Into<String>,
        level: AccessLevel,
    ) -> Result<(), GleaphError> {
        self.ensure_allowed(auth, Operation::SetAcl)?;
        self.permissions.set_acl_entry(principal, level);
        Ok(())
    }

    pub fn remove_acl_entry(
        &mut self,
        auth: &AuthContext,
        principal: &str,
    ) -> Result<bool, GleaphError> {
        self.ensure_allowed(auth, Operation::RemoveAcl)?;
        Ok(self.permissions.remove_acl_entry(principal))
    }

    pub fn list_acl_entries(&self, auth: &AuthContext) -> Result<Vec<AclEntry>, GleaphError> {
        self.ensure_allowed(auth, Operation::ListPrepared)?;
        Ok(self.permissions.list_acl_entries())
    }

    pub fn plan_request(
        &self,
        auth: &AuthContext,
        request: &QueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<crate::PlanResponse, GleaphError> {
        self.ensure_allowed(auth, Operation::PlanQuery)?;
        let block = parse_block(&request.query)?;
        self.ensure_supported_extension_types_in_block(&block)?;
        plan_request(request, stats)
    }

    pub fn plan_api_request(
        &self,
        auth: &AuthContext,
        request: &ApiQueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiPlanResponse, GleaphError> {
        let request = QueryRequest {
            query: request.query.clone(),
            params: request
                .params
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v)))
                .collect(),
        };
        let response = self.plan_request(auth, &request, stats)?;
        Ok(ApiPlanResponse {
            explain: response.explain,
            summary: crate::ApiPlanSummary::from(&response.summary),
            use_graph_pushdown: response.use_graph_pushdown,
        })
    }

    pub fn plan_api_endpoint(
        &self,
        endpoint: &ApiPlanEndpointRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiPlanResponse, GleaphError> {
        let auth = AuthContext::from(&endpoint.auth);
        self.plan_api_request(&auth, &endpoint.request, stats)
    }

    pub fn execute_request<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        request: &QueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<QueryResponse, GleaphError> {
        self.ensure_allowed(auth, Operation::ExecuteQuery)?;
        self.execute_block_request(graph, auth, request, stats)
    }

    pub fn execute_update_request<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        request: &QueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<QueryResponse, GleaphError> {
        self.ensure_allowed(auth, Operation::Update)?;
        self.execute_block_request(graph, auth, request, stats)
    }

    fn execute_block_request<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        request: &QueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<QueryResponse, GleaphError> {
        let block = parse_block(&request.query)?;
        self.ensure_supported_extension_types_in_block(&block)?;
        let ctx = ExecutionContext {
            params: crate::normalize_params(&request.params),
            caller: caller_value_from_auth(auth),
            procedure_registry: Some(Arc::clone(&self.procedure_registry)),
            graph_registry_resolver: self.graph_registry_resolver.as_ref().map(Arc::clone),
            use_graph_router: self.use_graph_router.as_ref().map(Arc::clone),
            ..ExecutionContext::default()
        };
        let output = crate::execute_query_str(graph, &request.query, stats, &ctx)?;
        Ok(QueryResponse {
            explain: output.plan.explain,
            plan_summary: output.plan.summary,
            use_graph_pushdown: output
                .plan
                .plan
                .use_graph_pushdown()
                .iter()
                .map(crate::ApiUseGraphPushdownInfo::from)
                .collect(),
            execution: output.execution,
        })
    }

    pub fn execute_api_request<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        request: &ApiQueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiQueryResponse, GleaphError> {
        let request = QueryRequest {
            query: request.query.clone(),
            params: request
                .params
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v)))
                .collect(),
        };
        let response = self.execute_request(graph, auth, &request, stats)?;
        Ok(ApiQueryResponse {
            explain: response.explain,
            plan_summary: crate::ApiPlanSummary::from(&response.plan_summary),
            use_graph_pushdown: response.use_graph_pushdown,
            execution: crate::ApiExecutionResult::from(&response.execution),
        })
    }

    pub fn execute_update_api_request<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        request: &ApiQueryRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiQueryResponse, GleaphError> {
        let request = QueryRequest {
            query: request.query.clone(),
            params: request
                .params
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v)))
                .collect(),
        };
        let response = self.execute_update_request(graph, auth, &request, stats)?;
        Ok(ApiQueryResponse {
            explain: response.explain,
            plan_summary: crate::ApiPlanSummary::from(&response.plan_summary),
            use_graph_pushdown: response.use_graph_pushdown,
            execution: crate::ApiExecutionResult::from(&response.execution),
        })
    }

    pub fn execute_api_endpoint<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        endpoint: &ApiExecuteEndpointRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiQueryResponse, GleaphError> {
        let auth = AuthContext::from(&endpoint.auth);
        self.execute_api_request(graph, &auth, &endpoint.request, stats)
    }

    pub fn prepare(
        &mut self,
        auth: &AuthContext,
        name: impl Into<String>,
        source: impl Into<String>,
        options: Option<&crate::PreparedOptions>,
        stats: Option<&dyn GraphStats>,
    ) -> Result<PreparedQueryInfo, GleaphError> {
        self.ensure_allowed(auth, Operation::Prepare)?;
        let name = name.into();
        let source = source.into();
        let block = parse_block(&source)?;
        self.ensure_supported_extension_types_in_block(&block)?;
        let prepared = self.prepared.prepare(name, source, options, stats)?;
        self.ensure_supported_extension_types(&prepared.extension_types)?;
        Ok(prepared)
    }

    pub fn prepare_api(
        &mut self,
        auth: &AuthContext,
        request: &ApiPrepareRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiPrepareResponse, GleaphError> {
        let prepared = self.prepare(
            auth,
            &request.name,
            &request.query,
            request.options.as_ref(),
            stats,
        )?;
        Ok(ApiPrepareResponse {
            prepared: ApiPreparedQueryInfo::from(&prepared),
        })
    }

    pub fn prepare_api_endpoint(
        &mut self,
        endpoint: &ApiPrepareEndpointRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiPrepareResponse, GleaphError> {
        let auth = AuthContext::from(&endpoint.auth);
        self.prepare_api(&auth, &endpoint.request, stats)
    }

    pub fn list_prepared(&self, auth: &AuthContext) -> Result<Vec<PreparedQueryInfo>, GleaphError> {
        self.ensure_allowed(auth, Operation::ListPrepared)?;
        Ok(self.prepared.list())
    }

    pub fn list_prepared_api(
        &self,
        auth: &AuthContext,
    ) -> Result<ApiListPreparedResponse, GleaphError> {
        let statements = self
            .list_prepared(auth)?
            .iter()
            .map(ApiPreparedQueryInfo::from)
            .collect();
        Ok(ApiListPreparedResponse { statements })
    }

    pub fn list_prepared_api_endpoint(
        &self,
        endpoint: &ApiListPreparedEndpointRequest,
    ) -> Result<ApiListPreparedResponse, GleaphError> {
        let auth = AuthContext::from(&endpoint.auth);
        self.list_prepared_api(&auth)
    }

    pub fn drop_prepared(&mut self, auth: &AuthContext, name: &str) -> Result<bool, GleaphError> {
        self.ensure_allowed(auth, Operation::DropPrepared)?;
        Ok(self.prepared.drop(name))
    }

    pub fn drop_prepared_api(
        &mut self,
        auth: &AuthContext,
        request: &ApiDropPreparedRequest,
    ) -> Result<ApiDropPreparedResponse, GleaphError> {
        Ok(ApiDropPreparedResponse {
            dropped: self.drop_prepared(auth, &request.name)?,
        })
    }

    pub fn drop_prepared_api_endpoint(
        &mut self,
        endpoint: &ApiDropPreparedEndpointRequest,
    ) -> Result<ApiDropPreparedResponse, GleaphError> {
        let auth = AuthContext::from(&endpoint.auth);
        self.drop_prepared_api(&auth, &endpoint.request)
    }

    pub fn execute_prepared<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        name: &str,
        params: &BTreeMap<String, Value>,
        sort: Option<&Vec<PreparedSortSpec>>,
        stats: Option<&dyn GraphStats>,
    ) -> Result<QueryResponse, GleaphError> {
        let entry = self
            .prepared
            .get(name)
            .ok_or_else(|| GleaphError::PreparedNotFound(name.to_owned()))?;
        self.ensure_supported_extension_types(&entry.info.extension_types)?;
        let op = match entry.info.kind {
            PreparedQueryKind::Query => Operation::ExecutePreparedQuery,
            PreparedQueryKind::Update => Operation::ExecutePreparedUpdate,
        };
        self.ensure_allowed(auth, op)?;
        let ctx = ExecutionContext {
            params: crate::normalize_params(params),
            caller: caller_value_from_auth(auth),
            procedure_registry: Some(Arc::clone(&self.procedure_registry)),
            graph_registry_resolver: self.graph_registry_resolver.as_ref().map(Arc::clone),
            use_graph_router: self.use_graph_router.as_ref().map(Arc::clone),
            ..ExecutionContext::default()
        };
        let planned = plan_for_prepared_execute(entry, sort, stats)?;
        let execution = execute_plan_with_normalized_params(graph, &planned.plan, &ctx)?;
        Ok(QueryResponse {
            explain: planned.explain,
            plan_summary: planned.summary,
            use_graph_pushdown: planned
                .plan
                .use_graph_pushdown()
                .iter()
                .map(crate::ApiUseGraphPushdownInfo::from)
                .collect(),
            execution,
        })
    }

    pub fn execute_prepared_api<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        name: &str,
        params: &BTreeMap<String, crate::ApiValue>,
        sort: Option<&Vec<PreparedSortSpec>>,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiQueryResponse, GleaphError> {
        let params = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v)))
            .collect();
        let response = self.execute_prepared(graph, auth, name, &params, sort, stats)?;
        Ok(ApiQueryResponse {
            explain: response.explain,
            plan_summary: crate::ApiPlanSummary::from(&response.plan_summary),
            use_graph_pushdown: response.use_graph_pushdown,
            execution: crate::ApiExecutionResult::from(&response.execution),
        })
    }

    pub fn execute_prepared_api_request<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        request: &ApiExecutePreparedRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiQueryResponse, GleaphError> {
        self.execute_prepared_api(
            graph,
            auth,
            &request.name,
            &request.params,
            request.sort.as_ref(),
            stats,
        )
    }

    pub fn execute_prepared_api_endpoint<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        endpoint: &ApiExecutePreparedEndpointRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiQueryResponse, GleaphError> {
        let auth = AuthContext::from(&endpoint.auth);
        self.execute_prepared_api_request(graph, &auth, &endpoint.request, stats)
    }

    fn ensure_allowed(&self, auth: &AuthContext, op: Operation) -> Result<(), GleaphError> {
        if self.permissions.is_allowed(auth, op) {
            return Ok(());
        }
        Err(GleaphError::PermissionDenied {
            operation: op.as_str().to_owned(),
            caller: auth.caller,
            level: self.permissions.resolve_access_level(auth),
        })
    }

    fn ensure_supported_extension_types(
        &self,
        extension_types: &[String],
    ) -> Result<(), GleaphError> {
        for ty in extension_types {
            if !self
                .supported_extension_types
                .contains(&ty.to_ascii_uppercase())
            {
                return Err(GleaphError::UnsupportedExtensionType(ty.clone()));
            }
        }
        Ok(())
    }

    fn ensure_supported_extension_types_in_block(
        &self,
        block: &gleaph_gql::ast::StatementBlock,
    ) -> Result<(), GleaphError> {
        let extension_types =
            crate::prepared::collect_extension_types_from_statement_block_ast(block);
        self.ensure_supported_extension_types(&extension_types)
    }
}

fn collect_use_graph_names(ops: &[PlanOp], out: &mut Vec<String>) {
    for op in ops {
        match op {
            PlanOp::UseGraph {
                graph_name,
                sub_plan,
            } => {
                let name = graph_name
                    .iter()
                    .map(|s| s.as_ref())
                    .collect::<Vec<_>>()
                    .join(".");
                out.push(name);
                if let Some(sub_plan) = sub_plan {
                    collect_use_graph_names(sub_plan, out);
                }
            }
            PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right, .. } => {
                collect_use_graph_names(left, out);
                collect_use_graph_names(right, out);
            }
            PlanOp::SetOperation { right, .. } => collect_use_graph_names(&right.ops, out),
            PlanOp::OptionalMatch { sub_plan, .. } => collect_use_graph_names(sub_plan, out),
            _ => {}
        }
    }
}
