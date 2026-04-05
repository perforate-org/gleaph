use std::collections::BTreeSet;

use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;
use gleaph_gql::types::EdgeDirection;

use crate::{EdgeId, EdgeRecord, Expansion, ExpansionHop, GraphResult, NodeId, NodeRecord, PropertyMap};

/// Incident-edge label constraint for [`GraphRead::expand`].
///
/// This is a **physical** filter for one hop: only disjunctions of plain names can use
/// `AnyOf` with label-indexed storage; general label-expression logic stays in the executor.
///
/// `AnyOf` uses a slice of owned names: the executor fills a scratch [`Vec<String>`] per hop filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeLabelFilter<'a, 'b> {
    /// All incident edges (both endpoints for undirected-style directions).
    All,
    /// Edges whose stored label equals this name.
    Single(&'a str),
    /// Edges whose label equals one of these names (OR). Implementations merge label-index ranges and dedupe by `edge_id`.
    AnyOf(&'b [String]),
}

pub trait GraphRead {
    fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>>;

    /// Vertex scan with only the listed property keys materialized on each [`NodeRecord`].
    ///
    /// Names are de-duplicated; an empty slice yields empty [`PropertyMap`]s.
    fn scan_nodes_projected(
        &self,
        label: Option<&str>,
        property_names: &[String],
    ) -> GraphResult<Vec<NodeRecord>>;

    fn scan_nodes_by_property(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
    ) -> GraphResult<Vec<NodeRecord>>;

    /// Like [`Self::scan_nodes_by_property`], but only retains `property_names` on each row.
    fn scan_nodes_by_property_projected(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
        property_names: &[String],
    ) -> GraphResult<Vec<NodeRecord>>;

    fn scan_edges_by_property(&self, property: &str, value: &Value)
    -> GraphResult<Vec<EdgeRecord>>;

    /// Edge scan with only the listed property keys on each [`EdgeRecord`].
    fn scan_edges_by_property_projected(
        &self,
        property: &str,
        value: &Value,
        property_names: &[String],
    ) -> GraphResult<Vec<EdgeRecord>>;

    fn expand(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
    ) -> GraphResult<Vec<Expansion>>;

    /// Like [`Self::expand`], but optionally loads only the given property maps on each
    /// [`Expansion::edge`] / [`Expansion::node`]. `None` means full maps for that side.
    fn expand_projected(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
        edge_property_names: Option<&[String]>,
        dst_property_names: Option<&[String]>,
    ) -> GraphResult<Vec<Expansion>>;

    /// Like [`Self::expand_projected`], but each hop may carry remote shard principal bytes ([`ExpansionHop::shard_canister_principal`]).
    ///
    /// Default: delegates to [`Self::expand_projected`] with [`None`] shard principal bytes on every hop.
    /// Persistent stores (e.g. graph-pma kernel overlay) may override to surface cross-canister targets.
    fn expand_hops_with_shard_meta(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
        edge_property_names: Option<&[String]>,
        dst_property_names: Option<&[String]>,
    ) -> GraphResult<Vec<ExpansionHop>> {
        Ok(self
            .expand_projected(
                from,
                direction,
                filter,
                edge_property_names,
                dst_property_names,
            )?
            .into_iter()
            .map(|expansion| ExpansionHop {
                expansion,
                shard_canister_principal: None,
            })
            .collect())
    }

    /// Optional hop auxiliary bytes for one edge (e.g. remote shard canister principal for cross-shard stubs).
    ///
    /// Default: [`None`]. Stores that materialize shard metadata may override.
    fn hop_aux_bytes_for_edge(&self, _edge_id: EdgeId) -> GraphResult<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Returns every edge visible to this snapshot in one pass (no per-node `expand`).
    fn scan_all_edges(&self) -> GraphResult<Vec<EdgeRecord>>;

    fn get_node(&self, id: NodeId) -> GraphResult<Option<NodeRecord>>;

    /// Loads a node with only the listed properties (labels/id from the graph).
    fn get_node_projected(
        &self,
        id: NodeId,
        property_names: &[String],
    ) -> GraphResult<Option<NodeRecord>>;

    fn get_edge_projected(
        &self,
        edge_id: EdgeId,
        property_names: &[String],
    ) -> GraphResult<Option<EdgeRecord>>;

    /// All distinct property names present on any node or edge (for `CALL db.propertyKeys`).
    fn all_property_key_names(&self) -> GraphResult<BTreeSet<String>>;

    /// Latest value for one node property without building a full [`PropertyMap`].
    fn get_node_property_value(
        &self,
        node_id: NodeId,
        property: &str,
    ) -> GraphResult<Option<Value>>;

    /// Latest value for one edge property without building a full [`PropertyMap`].
    fn get_edge_property_value(
        &self,
        edge_id: EdgeId,
        property: &str,
    ) -> GraphResult<Option<Value>>;
}

pub trait GraphWrite {
    fn insert_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord>;

    fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord>;

    fn set_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<NodeRecord>;

    fn remove_node_property(&mut self, node_id: NodeId, property: &str) -> GraphResult<NodeRecord>;

    fn add_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord>;

    fn remove_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord>;

    fn set_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<EdgeRecord>;

    fn remove_edge_property(&mut self, edge_id: EdgeId, property: &str) -> GraphResult<EdgeRecord>;

    fn set_edge_label(&mut self, edge_id: EdgeId, label: Option<&str>) -> GraphResult<EdgeRecord>;

    fn delete_edge(&mut self, edge_id: EdgeId) -> GraphResult<()>;

    fn delete_node(&mut self, node_id: NodeId, detach: bool) -> GraphResult<()>;

    /// Persists any deferred graph mutations (e.g. stable-memory writeback).
    ///
    /// Default is a no-op for purely in-memory graphs. Persistent backends should
    /// override this and callers should invoke it after a logical write batch such as
    /// a full plan execution.
    fn flush(&mut self) -> GraphResult<()> {
        Ok(())
    }
}
