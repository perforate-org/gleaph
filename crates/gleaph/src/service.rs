use crate::auth::{AccessLevel, AclEntry, AuthContext, Operation, PermissionChecker};
use crate::prepared::{PreparedRegistry, PreparedStatementInfo, PreparedStatementKind};
use crate::{
    execute_plan_with_normalized_params, plan_request, ApiDropPreparedEndpointRequest,
    ApiDropPreparedRequest, ApiDropPreparedResponse, ApiExecuteEndpointRequest,
    ApiExecutePreparedEndpointRequest, ApiExecutePreparedRequest, ApiListPreparedEndpointRequest,
    ApiListPreparedResponse, ApiPlanEndpointRequest, ApiPlanResponse, ApiPrepareEndpointRequest,
    ApiPrepareRequest, ApiPrepareResponse, ApiPreparedStatementInfo, ApiQueryRequest,
    ApiQueryResponse, GleaphError, QueryRequest, QueryResponse,
};
use gleaph_gql::Value;
use gleaph_gql_executor::ExecutionContext;
use gleaph_gql_planner::GraphStats;
use gleaph_graph_kernel::{GraphRead, GraphWrite};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct GleaphService {
    permissions: PermissionChecker,
    prepared: PreparedRegistry,
}

impl GleaphService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn permissions(&self) -> &PermissionChecker {
        &self.permissions
    }

    pub fn permissions_mut(&mut self) -> &mut PermissionChecker {
        &mut self.permissions
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
        crate::execute_request(graph, request, stats)
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
        stats: Option<&dyn GraphStats>,
    ) -> Result<PreparedStatementInfo, GleaphError> {
        self.ensure_allowed(auth, Operation::Prepare)?;
        self.prepared.prepare(name, source, stats)
    }

    pub fn prepare_api(
        &mut self,
        auth: &AuthContext,
        request: &ApiPrepareRequest,
        stats: Option<&dyn GraphStats>,
    ) -> Result<ApiPrepareResponse, GleaphError> {
        let prepared = self.prepare(auth, &request.name, &request.query, stats)?;
        Ok(ApiPrepareResponse {
            prepared: ApiPreparedStatementInfo::from(&prepared),
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

    pub fn list_prepared(
        &self,
        auth: &AuthContext,
    ) -> Result<Vec<PreparedStatementInfo>, GleaphError> {
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
            .map(ApiPreparedStatementInfo::from)
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
    ) -> Result<QueryResponse, GleaphError> {
        let entry = self
            .prepared
            .get(name)
            .ok_or_else(|| GleaphError::PreparedNotFound(name.to_owned()))?;
        let op = match entry.info.kind {
            PreparedStatementKind::Query => Operation::ExecutePreparedQuery,
            PreparedStatementKind::Mutation => Operation::ExecutePreparedMutation,
        };
        self.ensure_allowed(auth, op)?;
        let ctx = ExecutionContext {
            params: crate::normalize_params(params),
        };
        let execution = execute_plan_with_normalized_params(graph, &entry.plan.plan, &ctx)?;
        Ok(QueryResponse {
            explain: entry.info.explain.clone(),
            plan_summary: entry.plan.summary.clone(),
            execution,
        })
    }

    pub fn execute_prepared_api<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        name: &str,
        params: &BTreeMap<String, crate::ApiValue>,
    ) -> Result<ApiQueryResponse, GleaphError> {
        let params = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v)))
            .collect();
        let response = self.execute_prepared(graph, auth, name, &params)?;
        Ok(ApiQueryResponse {
            explain: response.explain,
            plan_summary: crate::ApiPlanSummary::from(&response.plan_summary),
            execution: crate::ApiExecutionResult::from(&response.execution),
        })
    }

    pub fn execute_prepared_api_request<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        auth: &AuthContext,
        request: &ApiExecutePreparedRequest,
    ) -> Result<ApiQueryResponse, GleaphError> {
        self.execute_prepared_api(graph, auth, &request.name, &request.params)
    }

    pub fn execute_prepared_api_endpoint<G: GraphRead + GraphWrite>(
        &self,
        graph: &mut G,
        endpoint: &ApiExecutePreparedEndpointRequest,
    ) -> Result<ApiQueryResponse, GleaphError> {
        let auth = AuthContext::from(&endpoint.auth);
        self.execute_prepared_api_request(graph, &auth, &endpoint.request)
    }

    fn ensure_allowed(&self, auth: &AuthContext, op: Operation) -> Result<(), GleaphError> {
        if self.permissions.is_allowed(auth, op) {
            return Ok(());
        }
        Err(GleaphError::PermissionDenied {
            operation: op.as_str().to_owned(),
            caller: auth.caller.clone(),
            level: self.permissions.resolve_access_level(auth),
        })
    }
}
