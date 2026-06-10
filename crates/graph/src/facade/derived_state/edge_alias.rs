//! Edge alias index: canonical source is forward/reverse adjacency in [`GRAPH`]; derived
//! store is [`EDGE_ALIASES`], updated synchronously on edge insert/delete via
//! [`GraphStore::commit_insert_edge_alias`].

use super::super::stable::EDGE_ALIASES;
use crate::facade::store::helpers::{canonical_undirected_owner, edge_alias_slot_key};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_graph_kernel::entry::EdgeTarget;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, VertexId, labeled::OutEdgeOrder, traits::CsrEdge,
};
use std::collections::BTreeSet;

/// One alias row expected from canonical adjacency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ExpectedEdgeAlias {
    alias_vertex_id: u32,
    label_id: u16,
    alias_slot_key: u32,
    canonical_vertex_id: u32,
    canonical_slot_index: u32,
}

/// Mismatch between canonical adjacency and derived edge aliases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EdgeAliasDerivedInconsistency {
    pub missing_in_derived: Vec<ExpectedEdgeAlias>,
    pub extra_in_derived: Vec<ExpectedEdgeAlias>,
}

fn collect_expected_aliases(
    store: &GraphStore,
) -> Result<BTreeSet<ExpectedEdgeAlias>, GraphStoreError> {
    let mut expected = BTreeSet::new();
    let vertex_cap: u32 = store.vertex_count().into();
    for raw in 0..vertex_cap {
        let source = VertexId::from(raw);
        let Some(vertex) = store.vertex(source) else {
            continue;
        };
        if vertex.is_tombstone() {
            continue;
        }

        store.for_each_directed_out_edges(source, OutEdgeOrder::Ascending, |edge| {
            let Some(EdgeTarget::Local(target)) = edge.edge_target() else {
                return;
            };
            let label = LaraLabelId::from_raw(edge.label_id);
            let canonical = EdgeHandle::at_slot(source, label, edge.edge_slot_index.raw());
            if let Ok(Some(reverse)) =
                store.find_reverse_alias_for_canonical(canonical, target, source)
            {
                expected.insert(ExpectedEdgeAlias {
                    alias_vertex_id: u32::from(target),
                    label_id: label.raw(),
                    alias_slot_key: edge_alias_slot_key(reverse.slot_index, true),
                    canonical_vertex_id: u32::from(source),
                    canonical_slot_index: canonical.slot_index,
                });
            }
        })?;

        store.for_each_undirected_edges(source, OutEdgeOrder::Ascending, |edge| {
            let Some(EdgeTarget::Local(neighbor)) = edge.edge_target() else {
                return;
            };
            let owner = canonical_undirected_owner(source, neighbor);
            if source != owner {
                return;
            }
            let label = LaraLabelId::from_raw(edge.label_id);
            let canonical = EdgeHandle::at_slot(owner, label, edge.edge_slot_index.raw());
            let Ok(Some(alias)) =
                store.find_first_forward_handle_descending(neighbor, label, |candidate| {
                    candidate.neighbor_vid() == owner
                })
            else {
                return;
            };
            expected.insert(ExpectedEdgeAlias {
                alias_vertex_id: u32::from(neighbor),
                label_id: label.raw(),
                alias_slot_key: alias.slot_index,
                canonical_vertex_id: u32::from(owner),
                canonical_slot_index: canonical.slot_index,
            });
        })?;
    }
    Ok(expected)
}

fn actual_aliases() -> BTreeSet<ExpectedEdgeAlias> {
    let mut actual = BTreeSet::new();
    EDGE_ALIASES.with_borrow(|index| {
        index.for_each(|key, value| {
            actual.insert(ExpectedEdgeAlias {
                alias_vertex_id: u32::from(key.alias_vertex_id()),
                label_id: key.label_id(),
                alias_slot_key: key.alias_slot_key(),
                canonical_vertex_id: u32::from(value.canonical_vertex_id()),
                canonical_slot_index: value.canonical_slot_index(),
            });
        });
    });
    actual
}

/// Returns `Ok(())` when derived aliases match canonical adjacency.
pub(crate) fn check_edge_aliases(store: &GraphStore) -> Result<(), EdgeAliasDerivedInconsistency> {
    let expected = collect_expected_aliases(store).expect("canonical scan must succeed in check");
    let actual = actual_aliases();
    if expected == actual {
        return Ok(());
    }
    let missing_in_derived: Vec<_> = expected.difference(&actual).copied().collect();
    let extra_in_derived: Vec<_> = actual.difference(&expected).copied().collect();
    Err(EdgeAliasDerivedInconsistency {
        missing_in_derived,
        extra_in_derived,
    })
}

/// Rebuilds [`EDGE_ALIASES`] from canonical adjacency in [`GRAPH`].
pub(crate) fn rebuild_edge_aliases(store: &GraphStore) -> Result<(), GraphStoreError> {
    let expected = collect_expected_aliases(store)?;
    EDGE_ALIASES.with_borrow_mut(|index| {
        index.clear_all();
        for entry in expected {
            index.insert(
                VertexId::from(entry.alias_vertex_id),
                entry.label_id,
                entry.alias_slot_key,
                VertexId::from(entry.canonical_vertex_id),
                entry.canonical_slot_index,
            );
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_stores_are_consistent() {
        let store = GraphStore::new();
        check_edge_aliases(&store).expect("empty derived matches empty canonical");
    }

    #[test]
    fn directed_edge_insert_keeps_aliases_consistent() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("AliasDirected");
        store
            .insert_directed_edge(source, target, Some(label_id))
            .expect("edge");

        check_edge_aliases(&store).expect("sync insert path keeps aliases consistent");
    }

    #[test]
    fn undirected_edge_insert_keeps_aliases_consistent() {
        let store = GraphStore::new();
        let low = store.insert_vertex().expect("low");
        let high = store.insert_vertex().expect("high");
        let label_id = crate::test_labels::edge_label_id_for_name("AliasUndirected");
        store
            .insert_undirected_edge(low, high, Some(label_id))
            .expect("edge");

        check_edge_aliases(&store).expect("undirected alias half is derived consistently");
    }

    #[test]
    fn rebuild_repairs_extra_alias_rows() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("AliasRebuildExtra");
        store
            .insert_directed_edge(source, target, Some(label_id))
            .expect("edge");

        EDGE_ALIASES.with_borrow_mut(|index| {
            index.insert(VertexId::from(99), 0, 0, source, 0);
        });
        assert!(check_edge_aliases(&store).is_err());

        rebuild_edge_aliases(&store).expect("rebuild");
        check_edge_aliases(&store).expect("rebuild restores consistency");
    }

    #[test]
    fn rebuild_repairs_missing_alias_rows() {
        let store = GraphStore::new();
        let low = store.insert_vertex().expect("low");
        let high = store.insert_vertex().expect("high");
        let label_id = crate::test_labels::edge_label_id_for_name("AliasRebuildMissing");
        store
            .insert_undirected_edge(low, high, Some(label_id))
            .expect("edge");

        EDGE_ALIASES.with_borrow_mut(|index| index.clear_all());
        assert!(check_edge_aliases(&store).is_err());

        rebuild_edge_aliases(&store).expect("rebuild");
        check_edge_aliases(&store).expect("rebuild restores consistency");
        use crate::facade::store::helpers::{edge_storage_label, lara_label};
        let wire_label = lara_label(edge_storage_label(Some(label_id), true));
        let alias = store
            .find_first_forward_handle_descending(low, wire_label, |edge| {
                edge.neighbor_vid() == high
            })
            .expect("alias lookup")
            .expect("alias half");
        assert_eq!(store.canonical_edge_handle(alias).owner_vertex_id, high);
    }
}
