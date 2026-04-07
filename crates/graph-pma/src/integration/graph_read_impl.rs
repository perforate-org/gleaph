//! [`GraphRead`] for [`GraphPmaKernelOverlayGraph`].
//!
//! # Read-only path and terminal `flush` (executor)
//!
//! This implementation serves query execution: it clones or projects records from the in-memory
//! overlay (`bridge.nodes` / `bridge.edges`), walks adjacency via the PMA graph view, and reads
//! property payloads through the store facade when needed. It does **not** implement
//! [`gleaph_graph_kernel::GraphWrite`] and does not call [`GraphWrite::flush`](gleaph_graph_kernel::GraphWrite::flush).
//!
//! None of these methods enqueue stable-memory writeback or flip the store “dirty” flags that
//! [`super::graph_write_impl`](crate::integration::graph_write_impl) clears in `flush`. Therefore a
//! purely read-only physical plan (no DML operators; see [`gleaph_gql_planner::PhysicalPlan::has_dml`])
//! against this graph type does not **require** a post-plan flush for correctness of subsequent reads
//! on the same overlay instance. The GQL executor (`gleaph-gql-executor`) may still flush when the
//! plan contains DML, when the outer plan owns the terminal flush after nested execution, or when
//! `ExecutionContext::force_terminal_graph_flush` is set (e.g. a procedure mutates graph state without
//! DML in the plan).

use std::collections::BTreeSet;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::value_cmp::compare_values;
use gleaph_graph_kernel::{
    EdgeId, EdgeLabelFilter, EdgeRecord, Expansion, ExpansionHop, GraphError, GraphRead,
    GraphResult, LabelId, NodeId, NodeRecord, PropertyMap,
};

use super::GraphPmaKernelOverlayGraph;
use super::overlay_types::ExpansionWithShard;

impl<'a, S: super::GraphPmaStore> GraphRead for GraphPmaKernelOverlayGraph<'a, S> {
    fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>> {
        match label {
            Some(label_name) => {
                let Some(label_id) = self.bridge.lookup_label_id(label_name) else {
                    return Ok(Vec::new());
                };
                let ordinals = self.bridge.vertex_label_index.ordinals_for(label_id);
                let mut out = Vec::with_capacity(ordinals.len());
                for ordinal in ordinals {
                    if let Some(Some(node_id)) =
                        self.bridge.semantic_node_id_by_forward_ordinal.get(ordinal)
                    {
                        if let Some(node) = self.bridge.nodes.get(node_id) {
                            out.push(node.clone());
                        }
                    }
                }
                Ok(out)
            }
            None => Ok(self.bridge.nodes.values().cloned().collect()),
        }
    }

    fn scan_nodes_projected(
        &self,
        label: Option<&str>,
        property_names: &[String],
    ) -> GraphResult<Vec<NodeRecord>> {
        let names: BTreeSet<String> = property_names.iter().cloned().collect();
        let ids: Vec<NodeId> = self
            .bridge
            .nodes
            .values()
            .filter(|node| label.is_none_or(|label| node.labels.iter().any(|it| it == label)))
            .map(|node| node.id)
            .collect();
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                let mut record = self.bridge.nodes.get(&id)?.clone();
                record.properties = record
                    .properties
                    .iter()
                    .filter(|(k, _)| names.contains(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(record)
            })
            .collect())
    }

    fn scan_nodes_by_property(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
    ) -> GraphResult<Vec<NodeRecord>> {
        if cmp == CmpOp::Eq {
            let ids = self.bridge.node_property_candidate_ids_eq(property, value);
            return Ok(ids
                .into_iter()
                .filter_map(|node_id| self.bridge.nodes.get(&node_id).cloned())
                .collect());
        }

        Ok(self
            .bridge
            .node_property_candidate_ids(property)
            .into_iter()
            .filter(|&node_id| {
                self.bridge
                    .nodes
                    .get(&node_id)
                    .and_then(|n| n.properties.get(property))
                    .is_some_and(|candidate| {
                        super::compare_op(compare_values(candidate, value), cmp)
                    })
            })
            .filter_map(|node_id| self.bridge.refreshed_node_record(node_id).ok())
            .collect())
    }

    fn scan_nodes_by_property_projected(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
        property_names: &[String],
    ) -> GraphResult<Vec<NodeRecord>> {
        let names: BTreeSet<String> = property_names.iter().cloned().collect();
        if cmp == CmpOp::Eq {
            let ids = self.bridge.node_property_candidate_ids_eq(property, value);
            return Ok(ids
                .into_iter()
                .filter_map(|node_id| {
                    let mut record = self.bridge.nodes.get(&node_id)?.clone();
                    record.properties = record
                        .properties
                        .iter()
                        .filter(|(k, _)| names.contains(*k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    Some(record)
                })
                .collect());
        }

        Ok(self
            .bridge
            .node_property_candidate_ids(property)
            .into_iter()
            .filter(|&node_id| {
                self.bridge
                    .nodes
                    .get(&node_id)
                    .and_then(|n| n.properties.get(property))
                    .is_some_and(|candidate| {
                        super::compare_op(compare_values(candidate, value), cmp)
                    })
            })
            .filter_map(|node_id| {
                let mut record = self.bridge.nodes.get(&node_id)?.clone();
                record.properties = record
                    .properties
                    .iter()
                    .filter(|(k, _)| names.contains(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(record)
            })
            .collect())
    }

    fn scan_edges_by_property(
        &self,
        property: &str,
        value: &Value,
    ) -> GraphResult<Vec<EdgeRecord>> {
        Ok(self
            .bridge
            .edge_property_candidate_ids_eq(property, value)
            .into_iter()
            .filter_map(|edge_id| self.bridge.refreshed_edge_record(edge_id).ok())
            .collect())
    }

    fn scan_edges_by_property_projected(
        &self,
        property: &str,
        value: &Value,
        property_names: &[String],
    ) -> GraphResult<Vec<EdgeRecord>> {
        let ids: Vec<EdgeId> = self
            .bridge
            .edge_property_candidate_ids_eq(property, value)
            .into_iter()
            .collect();
        if property_names.is_empty() {
            return Ok(ids
                .into_iter()
                .filter_map(|edge_id| {
                    let mut record = self.bridge.edges.get(&edge_id)?.clone();
                    record.properties = PropertyMap::new();
                    Some(record)
                })
                .collect());
        }
        let names: BTreeSet<String> = property_names.iter().cloned().collect();
        Ok(ids
            .into_iter()
            .filter_map(|edge_id| {
                let mut record = self.bridge.edges.get(&edge_id)?.clone();
                record.properties = record
                    .properties
                    .iter()
                    .filter(|(k, _)| names.contains(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(record)
            })
            .collect())
    }

    fn expand(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
    ) -> GraphResult<Vec<Expansion>> {
        self.expand_projected(from, direction, filter, None, None)
    }

    fn expand_projected(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
        edge_property_names: Option<&[String]>,
        dst_property_names: Option<&[String]>,
    ) -> GraphResult<Vec<Expansion>> {
        let _prof_total = crate::bench_profile::PhaseGuard::new("expand_projected_total");
        if edge_property_names.is_none() && dst_property_names.is_none() {
            let _p_match = crate::bench_profile::PhaseGuard::new("expand_projected_match_pairs");
            let built = self.expand_unprojected_rows(from, direction, filter)?;
            drop(_p_match);
            return Ok(built.into_iter().map(|(_, e)| e).collect());
        }
        let _p_match = crate::bench_profile::PhaseGuard::new("expand_projected_match_pairs");
        let matches = self.expand_match_pairs(from, direction, filter)?;
        drop(_p_match);
        if matches.is_empty() {
            return Ok(Vec::new());
        }
        let _p_build = crate::bench_profile::PhaseGuard::new("expand_projected_build_rows");
        let built = self.build_expansion_rows(matches, edge_property_names, dst_property_names)?;
        drop(_p_build);
        Ok(built.into_iter().map(|(_, e)| e).collect())
    }

    fn expand_hops_with_shard_meta(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
        edge_property_names: Option<&[String]>,
        dst_property_names: Option<&[String]>,
    ) -> GraphResult<Vec<ExpansionHop>> {
        let _prof_total =
            crate::bench_profile::PhaseGuard::new("expand_hops_with_shard_meta_total");
        Ok(self
            .expand_projected_with_shard(
                from,
                direction,
                filter,
                edge_property_names,
                dst_property_names,
            )?
            .into_iter()
            .map(|hop| ExpansionHop {
                expansion: hop.expansion,
                shard_canister_principal: hop.shard_canister_dst.map(|p| p.as_slice().to_vec()),
            })
            .collect())
    }

    fn hop_aux_bytes_for_edge(&self, edge_id: EdgeId) -> GraphResult<Option<Vec<u8>>> {
        Ok(self
            .shard_canister_principal_for_edge_id(edge_id)
            .map(|p| p.as_slice().to_vec()))
    }

    fn scan_all_edges(&self) -> GraphResult<Vec<EdgeRecord>> {
        self.bridge
            .edges
            .values()
            .map(|e| self.bridge.refreshed_edge_record(e.id))
            .collect()
    }

    fn get_node(&self, id: NodeId) -> GraphResult<Option<NodeRecord>> {
        self.bridge
            .nodes
            .contains_key(&id)
            .then(|| self.bridge.refreshed_node_record(id))
            .transpose()
    }

    fn get_node_projected(
        &self,
        id: NodeId,
        property_names: &[String],
    ) -> GraphResult<Option<NodeRecord>> {
        let Some(mut record) = self.bridge.nodes.get(&id).cloned() else {
            return Ok(None);
        };
        if property_names.is_empty() {
            record.properties = PropertyMap::new();
            return Ok(Some(record));
        }
        let names: BTreeSet<String> = property_names.iter().cloned().collect();
        record.properties = record
            .properties
            .iter()
            .filter(|(k, _)| names.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(Some(record))
    }

    fn get_edge_projected(
        &self,
        edge_id: EdgeId,
        property_names: &[String],
    ) -> GraphResult<Option<EdgeRecord>> {
        let Some(mut record) = self.bridge.edges.get(&edge_id).cloned() else {
            return Ok(None);
        };
        if property_names.is_empty() {
            record.properties = PropertyMap::new();
            return Ok(Some(record));
        }
        let names: BTreeSet<String> = property_names.iter().cloned().collect();
        record.properties = record
            .properties
            .iter()
            .filter(|(k, _)| names.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(Some(record))
    }

    fn all_property_key_names(&self) -> GraphResult<BTreeSet<String>> {
        let mut names = self.bridge.store.distinct_node_property_names();
        names.extend(self.bridge.store.distinct_edge_property_names());
        Ok(names)
    }

    fn get_node_property_value(
        &self,
        node_id: NodeId,
        property: &str,
    ) -> GraphResult<Option<Value>> {
        let Some(node) = self.bridge.nodes.get(&node_id) else {
            return Ok(None);
        };
        if let Some(value) = node.properties.get(property) {
            return Ok(Some(value.clone()));
        }
        Ok(self.bridge.store.get_node_property_value(node_id, property))
    }

    fn get_edge_property_value(
        &self,
        edge_id: EdgeId,
        property: &str,
    ) -> GraphResult<Option<Value>> {
        let Some(edge) = self.bridge.edges.get(&edge_id) else {
            return Ok(None);
        };
        if let Some(value) = edge.properties.get(property) {
            return Ok(Some(value.clone()));
        }
        Ok(self.bridge.store.get_edge_property_value(edge_id, property))
    }
}

impl<'a, S: super::GraphPmaStore> GraphPmaKernelOverlayGraph<'a, S> {
    /// Whether `edge` is traversed when expanding from `from` with the given pattern direction.
    ///
    /// Directed arrows exclude [`EdgeRecord::undirected`]; pure undirected patterns require it.
    /// [`EdgeDirection::LeftOrRight`] and [`EdgeDirection::AnyDirection`] remain fully incident-based.
    fn expand_direction_matches(from: NodeId, direction: EdgeDirection, edge: &EdgeRecord) -> bool {
        let incident = edge.src == from || edge.dst == from;
        match direction {
            EdgeDirection::PointingRight => !edge.undirected && edge.src == from,
            EdgeDirection::PointingLeft => !edge.undirected && edge.dst == from,
            EdgeDirection::Undirected => edge.undirected && incident,
            EdgeDirection::LeftOrUndirected => {
                (!edge.undirected && edge.dst == from) || (edge.undirected && incident)
            }
            EdgeDirection::UndirectedOrRight => {
                (!edge.undirected && edge.src == from) || (edge.undirected && incident)
            }
            EdgeDirection::LeftOrRight | EdgeDirection::AnyDirection => incident,
        }
    }

    fn filter_edge_ids_by_expand_direction(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        ids: &mut BTreeSet<EdgeId>,
    ) {
        ids.retain(|id| {
            self.bridge
                .edges
                .get(id)
                .is_some_and(|e| Self::expand_direction_matches(from, direction, e))
        });
    }

    /// Adds incident `edge_id`s where [`EdgeRecord::label`] matches `label_matches`.
    ///
    /// Forward cross-shard edges are skipped by the low-level label sidecar
    /// ([`EdgeMeta::is_shard_canister`](crate::low_level::edge::EdgeMeta::is_shard_canister)); this path
    /// keeps `Single` / `AnyOf` aligned with semantic labels on kernel records. Edges without a
    /// stored label string are unchanged here (same limitation as index-only matching for cross-shard
    /// forward metadata).
    /// Match + materialize expansions in one pass (no separate `edges.get` in a follow-up build).
    fn expand_unprojected_rows(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
    ) -> GraphResult<Vec<(EdgeId, Expansion)>> {
        let rows: Vec<(EdgeId, Expansion)> = match filter {
            EdgeLabelFilter::All => {
                let mut out = Vec::new();
                self.for_each_expand_all_valid_incident(
                    from,
                    direction,
                    |edge_id, edge, _t, node| {
                        out.push((
                            edge_id,
                            Expansion {
                                edge: edge.clone(),
                                node: node.clone(),
                            },
                        ));
                    },
                );
                out
            }
            EdgeLabelFilter::Single(name) => {
                let Some(ids) = self.collect_expand_edge_ids_single_label(from, direction, name)?
                else {
                    return Ok(Vec::new());
                };
                self.edge_ids_to_unprojected_rows(from, ids)
            }
            EdgeLabelFilter::AnyOf(names) => {
                let Some(ids) = self.collect_expand_edge_ids_any_of(from, direction, names)? else {
                    return Ok(Vec::new());
                };
                self.edge_ids_to_unprojected_rows(from, ids)
            }
        };
        Ok(rows)
    }

    fn supplement_expand_ids_from_edge_record_labels<F>(
        &self,
        ids: &mut BTreeSet<EdgeId>,
        from: NodeId,
        direction: EdgeDirection,
        mut label_matches: F,
    ) where
        F: FnMut(&EdgeRecord) -> bool,
    {
        let incident = self
            .bridge
            .incident_edge_ids
            .get(&from)
            .map(|v| v.as_slice())
            .unwrap_or_default();
        for &edge_id in incident {
            let Some(edge) = self.bridge.edges.get(&edge_id) else {
                continue;
            };
            if !Self::expand_direction_matches(from, direction, edge) {
                continue;
            }
            let target = if edge.src == from { edge.dst } else { edge.src };
            if !self.bridge.nodes.contains_key(&target) {
                continue;
            }
            if label_matches(edge) {
                ids.insert(edge_id);
            }
        }
    }

    fn for_each_expand_all_valid_incident(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        mut visit: impl FnMut(EdgeId, &EdgeRecord, NodeId, &NodeRecord),
    ) {
        let incident = self
            .bridge
            .incident_edge_ids
            .get(&from)
            .map(|v| v.as_slice())
            .unwrap_or_default();
        for &edge_id in incident {
            let Some(edge) = self.bridge.edges.get(&edge_id) else {
                continue;
            };
            if !Self::expand_direction_matches(from, direction, edge) {
                continue;
            }
            let target = if edge.src == from { edge.dst } else { edge.src };
            let Some(node_rec) = self.bridge.nodes.get(&target) else {
                continue;
            };
            visit(edge_id, edge, target, node_rec);
        }
    }

    /// `None` if the label surface cannot be completed (same as an empty expand result).
    fn collect_expand_edge_ids_single_label(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        name: &str,
    ) -> GraphResult<Option<BTreeSet<EdgeId>>> {
        let Some(label_id) = self.bridge.lookup_label_id(name) else {
            return Ok(None);
        };
        let Some(mapping) = self.bridge.vertex_mapping(from) else {
            return Ok(None);
        };
        let _prof_surf = crate::bench_profile::PhaseGuard::new("expand_single_label_surface");
        let graph = self.bridge.store.graph();
        let mut ids = BTreeSet::<EdgeId>::new();
        let mut append_from_surface = |use_forward: bool, ordinal: usize| -> Option<()> {
            let slots = if use_forward {
                self.bridge.forward_base_slots_by_ordinal.get(ordinal)?
            } else {
                self.bridge.reverse_base_slots_by_ordinal.get(ordinal)?
            };
            let (base, label_view, overflow) = if use_forward {
                (
                    graph.forward.base_neighborhood(ordinal)?,
                    graph.forward.label_neighborhood(ordinal, label_id),
                    graph.forward.overflow_entries_for(from.into(), ordinal)?,
                )
            } else {
                (
                    graph.reverse.base_neighborhood(ordinal)?,
                    graph.reverse.label_neighborhood(ordinal, label_id),
                    graph.reverse.overflow_entries_for(from.into(), ordinal)?,
                )
            };
            if let Some(view) = label_view {
                let base_start = usize::try_from(base.start.raw).ok()?;
                let start = usize::try_from(view.start.raw).ok()?;
                let len = usize::try_from(view.degree).ok()?;
                for logical in
                    start.saturating_sub(base_start)..start.saturating_sub(base_start) + len
                {
                    if let Some(Some(edge_id)) = slots.get(logical) {
                        ids.insert(*edge_id);
                    }
                }
            }
            for overflow_entry in overflow {
                if !overflow_entry.entry.meta.is_tombstone()
                    && overflow_entry.entry.meta.local_label_id() == Some(label_id)
                {
                    ids.insert(overflow_entry.edge_id);
                }
            }
            Some(())
        };
        let complete = match direction {
            EdgeDirection::PointingRight => {
                append_from_surface(true, mapping.forward_ordinal).is_some()
            }
            EdgeDirection::PointingLeft => {
                append_from_surface(false, mapping.reverse_ordinal).is_some()
            }
            EdgeDirection::LeftOrRight
            | EdgeDirection::Undirected
            | EdgeDirection::LeftOrUndirected
            | EdgeDirection::UndirectedOrRight
            | EdgeDirection::AnyDirection => {
                let fwd = append_from_surface(true, mapping.forward_ordinal);
                let rev = append_from_surface(false, mapping.reverse_ordinal);
                fwd.is_some() || rev.is_some()
            }
        };
        if !complete {
            return Ok(None);
        }
        self.supplement_expand_ids_from_edge_record_labels(&mut ids, from, direction, |rec| {
            rec.label.as_deref() == Some(name)
        });
        self.filter_edge_ids_by_expand_direction(from, direction, &mut ids);
        Ok(Some(ids))
    }

    /// `None` if no label ids resolve or the surface cannot be completed.
    fn collect_expand_edge_ids_any_of(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        names: &[String],
    ) -> GraphResult<Option<BTreeSet<EdgeId>>> {
        let resolved: BTreeSet<LabelId> = names
            .iter()
            .filter_map(|name| self.bridge.lookup_label_id(name))
            .collect();
        if resolved.is_empty() {
            return Ok(None);
        }
        let Some(mapping) = self.bridge.vertex_mapping(from) else {
            return Ok(None);
        };
        let _prof_surf = crate::bench_profile::PhaseGuard::new("expand_any_of_label_surface");
        let graph = self.bridge.store.graph();
        let mut ids = BTreeSet::<EdgeId>::new();
        let mut append_from_surface = |use_forward: bool, ordinal: usize| -> Option<()> {
            let slots = if use_forward {
                self.bridge.forward_base_slots_by_ordinal.get(ordinal)?
            } else {
                self.bridge.reverse_base_slots_by_ordinal.get(ordinal)?
            };
            let (base, overflow) = if use_forward {
                (
                    graph.forward.base_neighborhood(ordinal)?,
                    graph.forward.overflow_entries_for(from.into(), ordinal)?,
                )
            } else {
                (
                    graph.reverse.base_neighborhood(ordinal)?,
                    graph.reverse.overflow_entries_for(from.into(), ordinal)?,
                )
            };
            let base_start = usize::try_from(base.start.raw).ok()?;
            for &label_id in &resolved {
                let label_view = if use_forward {
                    graph.forward.label_neighborhood(ordinal, label_id)
                } else {
                    graph.reverse.label_neighborhood(ordinal, label_id)
                };
                if let Some(view) = label_view {
                    let start = usize::try_from(view.start.raw).ok()?;
                    let len = usize::try_from(view.degree).ok()?;
                    for logical in
                        start.saturating_sub(base_start)..start.saturating_sub(base_start) + len
                    {
                        if let Some(Some(edge_id)) = slots.get(logical) {
                            ids.insert(*edge_id);
                        }
                    }
                }
            }
            for overflow_entry in overflow {
                if !overflow_entry.entry.meta.is_tombstone() {
                    if let Some(lid) = overflow_entry.entry.meta.local_label_id() {
                        if resolved.contains(&lid) {
                            ids.insert(overflow_entry.edge_id);
                        }
                    }
                }
            }
            Some(())
        };
        let complete = match direction {
            EdgeDirection::PointingRight => {
                append_from_surface(true, mapping.forward_ordinal).is_some()
            }
            EdgeDirection::PointingLeft => {
                append_from_surface(false, mapping.reverse_ordinal).is_some()
            }
            EdgeDirection::LeftOrRight
            | EdgeDirection::Undirected
            | EdgeDirection::LeftOrUndirected
            | EdgeDirection::UndirectedOrRight
            | EdgeDirection::AnyDirection => {
                let fwd = append_from_surface(true, mapping.forward_ordinal);
                let rev = append_from_surface(false, mapping.reverse_ordinal);
                fwd.is_some() || rev.is_some()
            }
        };
        if !complete {
            return Ok(None);
        }
        self.supplement_expand_ids_from_edge_record_labels(&mut ids, from, direction, |rec| {
            rec.label
                .as_deref()
                .is_some_and(|l| names.iter().any(|n| n == l))
        });
        self.filter_edge_ids_by_expand_direction(from, direction, &mut ids);
        Ok(Some(ids))
    }

    fn edge_ids_to_match_pairs(
        &self,
        from: NodeId,
        ids: BTreeSet<EdgeId>,
    ) -> Vec<(EdgeId, NodeId)> {
        ids.into_iter()
            .filter_map(|edge_id| {
                let edge = self.bridge.edges.get(&edge_id)?;
                let target = if edge.src == from { edge.dst } else { edge.src };
                self.bridge
                    .nodes
                    .contains_key(&target)
                    .then_some((edge_id, target))
            })
            .collect()
    }

    fn edge_ids_to_unprojected_rows(
        &self,
        from: NodeId,
        ids: BTreeSet<EdgeId>,
    ) -> Vec<(EdgeId, Expansion)> {
        let mut out = Vec::with_capacity(ids.len());
        for edge_id in ids {
            let Some(edge) = self.bridge.edges.get(&edge_id) else {
                continue;
            };
            let target = if edge.src == from { edge.dst } else { edge.src };
            let Some(node_rec) = self.bridge.nodes.get(&target) else {
                continue;
            };
            out.push((
                edge_id,
                Expansion {
                    edge: edge.clone(),
                    node: node_rec.clone(),
                },
            ));
        }
        out
    }

    fn expand_match_pairs(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
    ) -> GraphResult<Vec<(EdgeId, NodeId)>> {
        let matches: Vec<(EdgeId, NodeId)> = match filter {
            EdgeLabelFilter::All => {
                let mut out = Vec::new();
                self.for_each_expand_all_valid_incident(
                    from,
                    direction,
                    |edge_id, _edge, target, _node| {
                        out.push((edge_id, target));
                    },
                );
                out
            }
            EdgeLabelFilter::Single(name) => {
                let Some(ids) = self.collect_expand_edge_ids_single_label(from, direction, name)?
                else {
                    return Ok(Vec::new());
                };
                self.edge_ids_to_match_pairs(from, ids)
            }
            EdgeLabelFilter::AnyOf(names) => {
                let Some(ids) = self.collect_expand_edge_ids_any_of(from, direction, names)? else {
                    return Ok(Vec::new());
                };
                self.edge_ids_to_match_pairs(from, ids)
            }
        };
        Ok(matches)
    }

    fn build_expansion_rows(
        &self,
        matches: Vec<(EdgeId, NodeId)>,
        edge_property_names: Option<&[String]>,
        dst_property_names: Option<&[String]>,
    ) -> GraphResult<Vec<(EdgeId, Expansion)>> {
        if edge_property_names.is_none() && dst_property_names.is_none() {
            let mut out = Vec::with_capacity(matches.len());
            for (edge_id, target) in matches {
                out.push((
                    edge_id,
                    Expansion {
                        edge: self.bridge.refreshed_edge_record(edge_id)?,
                        node: self.bridge.refreshed_node_record(target)?,
                    },
                ));
            }
            return Ok(out);
        }

        let edge_keys = match edge_property_names {
            None => None,
            Some([]) => None,
            Some(names) => Some(names.iter().cloned().collect::<BTreeSet<String>>()),
        };
        let dst_keys = match dst_property_names {
            None => None,
            Some([]) => None,
            Some(names) => Some(names.iter().cloned().collect::<BTreeSet<String>>()),
        };

        let mut out = Vec::with_capacity(matches.len());
        for (edge_id, target) in matches {
            let mut edge_rec = self
                .bridge
                .edges
                .get(&edge_id)
                .cloned()
                .ok_or(GraphError::EdgeNotFound(edge_id))?;
            match edge_property_names {
                None => {}
                Some([]) => edge_rec.properties = PropertyMap::new(),
                Some(_) => {
                    let bt = edge_keys
                        .as_ref()
                        .expect("non-empty names => set built above");
                    edge_rec.properties = edge_rec
                        .properties
                        .iter()
                        .filter(|(k, _)| bt.contains(*k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                }
            }

            let mut node_rec = self
                .bridge
                .nodes
                .get(&target)
                .cloned()
                .ok_or(GraphError::NodeNotFound(target))?;
            match dst_property_names {
                None => {}
                Some([]) => node_rec.properties = PropertyMap::new(),
                Some(_) => {
                    let bt = dst_keys
                        .as_ref()
                        .expect("non-empty names => set built above");
                    node_rec.properties = node_rec
                        .properties
                        .iter()
                        .filter(|(k, _)| bt.contains(*k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                }
            }

            out.push((
                edge_id,
                Expansion {
                    edge: edge_rec,
                    node: node_rec,
                },
            ));
        }
        Ok(out)
    }

    pub fn shard_canister_principal_for_edge_id(&self, edge_id: EdgeId) -> Option<Principal> {
        self.bridge.shard_canister_principal_for_edge(edge_id)
    }

    pub fn expand_projected_with_shard(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
        edge_property_names: Option<&[String]>,
        dst_property_names: Option<&[String]>,
    ) -> GraphResult<Vec<ExpansionWithShard>> {
        let _prof_total =
            crate::bench_profile::PhaseGuard::new("expand_projected_with_shard_total");
        let rows: Vec<(EdgeId, Expansion)> = if edge_property_names.is_none()
            && dst_property_names.is_none()
        {
            let _p_match =
                crate::bench_profile::PhaseGuard::new("expand_projected_with_shard_match_pairs");
            let built = self.expand_unprojected_rows(from, direction, filter)?;
            drop(_p_match);
            built
        } else {
            let _p_match =
                crate::bench_profile::PhaseGuard::new("expand_projected_with_shard_match_pairs");
            let matches = self.expand_match_pairs(from, direction, filter)?;
            drop(_p_match);
            if matches.is_empty() {
                return Ok(Vec::new());
            }
            let _p_build =
                crate::bench_profile::PhaseGuard::new("expand_projected_with_shard_build_rows");
            let built =
                self.build_expansion_rows(matches, edge_property_names, dst_property_names)?;
            drop(_p_build);
            built
        };
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let skip_shard_principal_lookup = self.bridge.shard_canister_directory().is_empty();
        let _p_shard =
            crate::bench_profile::PhaseGuard::new("expand_projected_with_shard_principal_attach");
        let out = if skip_shard_principal_lookup {
            rows.into_iter()
                .map(|(_edge_id, expansion)| ExpansionWithShard {
                    shard_canister_dst: None,
                    expansion,
                })
                .collect()
        } else {
            rows.into_iter()
                .map(|(edge_id, expansion)| ExpansionWithShard {
                    shard_canister_dst: self.bridge.shard_canister_principal_for_edge(edge_id),
                    expansion,
                })
                .collect()
        };
        drop(_p_shard);
        Ok(out)
    }

    pub fn expand_with_shard(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        filter: EdgeLabelFilter<'_, '_>,
    ) -> GraphResult<Vec<ExpansionWithShard>> {
        self.expand_projected_with_shard(from, direction, filter, None, None)
    }
}
