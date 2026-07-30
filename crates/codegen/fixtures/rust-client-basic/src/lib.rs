use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
pub const GLEAPH_GRAPH_ID: &str = "fixture-graph";
/// Sort specification accepted by a generated prepared query.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedSortSpec {
    /// Stable sort-key identifier.
    pub key: String,
    /// Sort direction.
    pub direction: String,
}
/// Response envelope decoded by a runtime executor.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedResponse<Row> {
    /// Query execution metadata.
    pub explain: serde_json::Value,
    /// Planner summary.
    pub plan_summary: serde_json::Value,
    /// Execution result containing typed rows.
    pub execution: PreparedExecution<Row>,
}
/// Typed execution result inside a prepared response.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedExecution<Row> {
    /// Rows returned by the operation.
    pub rows: Vec<Row>,
    /// Non-fatal execution warnings.
    pub warnings: Vec<String>,
    /// Runtime-specific execution summary.
    pub summary: serde_json::Value,
}
/// Runtime boundary implemented by the selected Rust SDK or canister adapter.
pub trait PreparedExecutor {
    /// Error returned by the runtime boundary.
    type Error;
    /// Execute a read-only prepared operation.
    fn execute_query<'a, Row>(
        &'a self,
        name: &'static str,
        params: BTreeMap<String, serde_json::Value>,
        sort: Option<Vec<PreparedSortSpec>>,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedResponse<Row>, Self::Error>> + 'a>>
    where
        Row: DeserializeOwned + 'a;
    /// Execute a mutating prepared operation.
    fn execute_update<'a, Row>(
        &'a self,
        name: &'static str,
        params: BTreeMap<String, serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedResponse<Row>, Self::Error>> + 'a>>
    where
        Row: DeserializeOwned + 'a;
}
/// Date-time representation used by generated Rust declarations.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedDateTime {
    /// Unix seconds.
    pub seconds: i64,
    /// Nanoseconds within the second.
    pub nanos: u32,
}
/// Zoned date-time representation used by generated Rust declarations.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedZonedDateTime {
    /// Unix seconds.
    pub seconds: i64,
    /// Nanoseconds within the second.
    pub nanos: u32,
    /// UTC offset in seconds.
    pub offset_seconds: i32,
}
/// Zoned time representation used by generated Rust declarations.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedZonedTime {
    /// Nanoseconds since midnight.
    pub nanos: u64,
    /// UTC offset in seconds.
    pub offset_seconds: i32,
}
/// Duration representation used by generated Rust declarations.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedDuration {
    /// Calendar months.
    pub months: i32,
    /// Nanoseconds.
    pub nanos: i64,
}
/// Path element representation used by generated Rust declarations.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PreparedPathElement {
    /// Vertex identifier.
    Vertex(Vec<u8>),
    /// Edge identifier.
    Edge(Vec<u8>),
}
/// Find users by their search term.
/// Parameters for the `find-users` prepared operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FindUsersParams {
    /// Text to search for.
    /// Wire parameter `term`.
    #[serde(rename = "term")]
    pub term: String,
}
/// One result row from the `find-users` prepared operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FindUsersRow {
    /// Wire column `user_name`.
    #[serde(rename = "user_name")]
    pub user_name: String,
}
/// Typed facade over a [`PreparedExecutor`].
pub struct PreparedQueries<'a, E: PreparedExecutor> {
    executor: &'a E,
}
impl<'a, E: PreparedExecutor> PreparedQueries<'a, E> {
    /// Bind generated operations to a runtime executor.
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }
    /// Find users by their search term.
    /// Execute the `find-users` prepared operation.
    pub async fn find_users(
        &self,
        params: FindUsersParams,
    ) -> Result<PreparedResponse<FindUsersRow>, E::Error> {
        let params = serde_json::to_value(params)
            .expect("generated parameter struct must serialize")
            .as_object()
            .expect("generated parameter struct must serialize to an object")
            .clone()
            .into_iter()
            .collect();
        self.executor.execute_query::<FindUsersRow>("find-users", params, None).await
    }
}
