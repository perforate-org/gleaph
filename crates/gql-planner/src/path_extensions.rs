//! Generic path-pattern extension planning hooks.

use gleaph_gql::ast::{PathPatternExtension, PathPatternPrefix};
use gleaph_gql::types::{EdgeDirection, LabelExpr};

use crate::plan::{ShortestMode, ShortestPathCost, VarLenSpec};
use crate::planner::PlannerError;

/// Metadata for a single-edge path shape, when the planner can recognize one.
#[derive(Clone, Debug, PartialEq)]
pub struct SingleEdgePathInfo {
    pub edge_var: Option<String>,
    pub direction: EdgeDirection,
    pub label: Option<String>,
    pub label_expr: Option<LabelExpr>,
    pub var_len: Option<VarLenSpec>,
}

/// Context passed to extension handlers during shortest-path planning.
pub struct PathPatternExtensionContext<'a> {
    pub prefix: Option<&'a PathPatternPrefix>,
    pub extensions: &'a [PathPatternExtension],
    pub shortest_mode: Option<ShortestMode>,
    pub single_edge: Option<SingleEdgePathInfo>,
}

/// Maps generic path-pattern extension clauses to planner semantics.
pub trait PathPatternExtensionHandler {
    fn plan_shortest_path_cost(
        &self,
        ctx: &PathPatternExtensionContext<'_>,
    ) -> Result<ShortestPathCost, PlannerError>;
}

/// Default handler: rejects any path-pattern extension clause.
pub struct RejectingPathExtensionHandler;

impl PathPatternExtensionHandler for RejectingPathExtensionHandler {
    fn plan_shortest_path_cost(
        &self,
        ctx: &PathPatternExtensionContext<'_>,
    ) -> Result<ShortestPathCost, PlannerError> {
        let Some(ext) = ctx.extensions.first() else {
            return Ok(ShortestPathCost::HopCount);
        };
        let name = ext.name.parts.join(".");
        Err(PlannerError::UnsupportedExtension(format!(
            "unsupported path pattern extension '{name}'"
        )))
    }
}

pub(crate) static REJECTING_PATH_EXTENSION_HANDLER: RejectingPathExtensionHandler =
    RejectingPathExtensionHandler;

/// Build options for physical plan construction.
#[derive(Clone, Copy)]
pub struct PlanBuildOptions<'a> {
    pub stats: Option<&'a dyn crate::stats::GraphStats>,
    pub path_extensions: &'a dyn PathPatternExtensionHandler,
}

impl Default for PlanBuildOptions<'_> {
    fn default() -> Self {
        Self {
            stats: None,
            path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
        }
    }
}
