use std::collections::BTreeSet;

use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::value_cmp::compare_values;
use gleaph_graph_kernel::{
    EdgeId, EdgeLabelFilter, EdgeRecord, Expansion, GraphError, GraphRead, GraphResult, LabelId,
    NodeId, NodeRecord, PropertyMap,
};

use super::RewriteKernelOverlayGraph;

impl<'a, S: super::RewriteGraphStore> GraphRead for RewriteKernelOverlayGraph<'a, S> {
    fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>> {
        let ids: Vec<NodeId> = if let Some(label_name) = label {
            if let Some(label_id) = self.bridge.lookup_label_id(label_name) {
                let ordinals = self.bridge.vertex_label_index.ordinals_for(label_id);
                let mut ids = Vec::with_capacity(ordinals.len());
                for ordinal in ordinals {
                    if let Some(Some(node_id)) =
                        self.bridge.semantic_node_id_by_forward_ordinal.get(ordinal)
                    {
                        ids.push(*node_id);
                    }
                }
                ids
            } else {
                Vec::new()
            }
        } else {
            self.bridge.nodes.values().map(|node| node.id).collect()
        };
        Ok(ids
            .into_iter()
            .filter_map(|id| self.bridge.nodes.get(&id).cloned())
            .collect())
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
        let matches: Vec<(EdgeId, NodeId)> = match filter {
            EdgeLabelFilter::All => {
                let mut out = Vec::new();
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
                    let matched = match direction {
                        EdgeDirection::PointingRight => edge.src == from,
                        EdgeDirection::PointingLeft => edge.dst == from,
                        EdgeDirection::LeftOrRight
                        | EdgeDirection::Undirected
                        | EdgeDirection::LeftOrUndirected
                        | EdgeDirection::UndirectedOrRight
                        | EdgeDirection::AnyDirection => edge.src == from || edge.dst == from,
                    };
                    if !matched {
                        continue;
                    }
                    let target = if edge.src == from { edge.dst } else { edge.src };
                    if self.bridge.nodes.contains_key(&target) {
                        out.push((edge_id, target));
                    }
                }
                out
            }
            EdgeLabelFilter::Single(name) => {
                let Some(label_id) = self.bridge.lookup_label_id(name) else {
                    return Ok(Vec::new());
                };
                let Some(mapping) = self.bridge.vertex_mapping(from) else {
                    return Ok(Vec::new());
                };
                let _prof_surf =
                    crate::bench_profile::PhaseGuard::new("expand_single_label_surface");
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
                            && overflow_entry.entry.meta.label_id() == label_id
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
                    return Ok(Vec::new());
                }
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
            EdgeLabelFilter::AnyOf(names) => {
                let resolved: BTreeSet<LabelId> = names
                    .iter()
                    .filter_map(|name| self.bridge.lookup_label_id(name))
                    .collect();
                if resolved.is_empty() {
                    return Ok(Vec::new());
                }
                let Some(mapping) = self.bridge.vertex_mapping(from) else {
                    return Ok(Vec::new());
                };
                let _prof_surf =
                    crate::bench_profile::PhaseGuard::new("expand_any_of_label_surface");
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
                            for logical in start.saturating_sub(base_start)
                                ..start.saturating_sub(base_start) + len
                            {
                                if let Some(Some(edge_id)) = slots.get(logical) {
                                    ids.insert(*edge_id);
                                }
                            }
                        }
                    }
                    for overflow_entry in overflow {
                        if !overflow_entry.entry.meta.is_tombstone()
                            && resolved.contains(&overflow_entry.entry.meta.label_id())
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
                    return Ok(Vec::new());
                }
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
        };

        if matches.is_empty() {
            return Ok(Vec::new());
        }

        if edge_property_names.is_none() && dst_property_names.is_none() {
            let mut out = Vec::with_capacity(matches.len());
            for (edge_id, target) in matches {
                out.push(Expansion {
                    edge: self.bridge.refreshed_edge_record(edge_id)?,
                    node: self.bridge.refreshed_node_record(target)?,
                });
            }
            return Ok(out);
        }

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
                Some(names) if names.is_empty() => edge_rec.properties = PropertyMap::new(),
                Some(names) => {
                    let bt: BTreeSet<String> = names.iter().cloned().collect();
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
                Some(names) if names.is_empty() => node_rec.properties = PropertyMap::new(),
                Some(names) => {
                    let bt: BTreeSet<String> = names.iter().cloned().collect();
                    node_rec.properties = node_rec
                        .properties
                        .iter()
                        .filter(|(k, _)| bt.contains(*k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                }
            }

            out.push(Expansion {
                edge: edge_rec,
                node: node_rec,
            });
        }
        Ok(out)
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
