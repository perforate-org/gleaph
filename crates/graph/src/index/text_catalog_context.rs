//! Ephemeral, router-sourced indexed-text catalog for the current operation (plan 0297).
//!
//! Mirrors [`crate::index::vector_catalog_context`] for text documents (ADR 0077 engine): the
//! Router owns the set of indexed text definitions and supplies a snapshot per operation. The
//! shard never persists text index definitions, so this gate can never go stale across the
//! canister upgrade boundary.
//!
//! DML dispatch ([`crate::index::text_dispatch`]) consults this gate on every canonical property
//! change and stays inert while no snapshot is installed. Two entry points still await their
//! production callers and carry narrow `dead_code` allowances: [`enter`] (the per-operation
//! install arrives with the Router TEXT catalog slice) and [`has_specs`] (its caller is the
//! vertex-delete hook awaiting wiring in `crate::facade::store::vertex_delete`).

use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
use std::cell::RefCell;

/// One router-issued text-index definition relevant to DML sync: the indexed property plus its
/// creation-fixed label scope. Ops carry no index identity because the doc key space is the
/// vertex identity itself (`u64`) and Router placement targets one text canister per graph shard,
/// mirroring vector target selection. Edge-property text indexes are a documented v1 non-goal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexedTextSpec {
    pub(crate) property_id: PropertyId,
    /// Creation-fixed label set from the index definition; a vertex qualifies when its current
    /// label set intersects this one.
    pub(crate) labels: Vec<VertexLabelId>,
}

impl IndexedTextSpec {
    /// `true` when the spec's label set includes any of `labels`.
    pub(crate) fn matches_labels(&self, labels: &[VertexLabelId]) -> bool {
        self.labels.iter().any(|scoped| labels.contains(scoped))
    }

    /// The (label, property) identity of this definition, used to route pending ops to the
    /// definition's own text canister. Catalog entries carry exactly one label today.
    pub(crate) fn target(&self) -> (u16, u32) {
        (
            self.labels
                .first()
                .map(|label| label.raw())
                .unwrap_or_default(),
            self.property_id.raw(),
        )
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Vec<IndexedTextSpec>>> = const { RefCell::new(None) };
}

/// Fetch the Router's indexed-TEXT catalog for this shard's graph and install it for the current
/// operation (the per-operation install site). Returns `None` when the shard has no federation
/// routing or the Router call fails: DML text dispatch then stays inert (fail-closed) instead of
/// deriving against an unknown index set.
pub(crate) async fn enter_from_router(
    store: &crate::facade::GraphStore,
) -> Option<TextCatalogGuard> {
    let routing = store.federation_routing()?;
    let graph_name = store.logical_graph_name().unwrap_or_default();
    let entries = crate::index::federation_routing::fetch_indexed_text_catalog(
        routing.router_canister,
        &graph_name,
    )
    .await
    .ok()?;
    Some(enter(
        entries
            .into_iter()
            .map(|entry| IndexedTextSpec {
                property_id: PropertyId::from_raw(entry.property_id),
                labels: vec![VertexLabelId::from_raw(entry.label_id)],
            })
            .collect(),
    ))
}

/// RAII guard that keeps a router-sourced text catalog active for the current operation and
/// restores the previous value (if any) on drop.
#[must_use = "the catalog is only active while the guard is alive"]
pub(crate) struct TextCatalogGuard {
    previous: Option<Vec<IndexedTextSpec>>,
}

impl Drop for TextCatalogGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.previous.take());
    }
}

/// Install `specs` as the current operation's indexed-text catalog.
pub(crate) fn enter(specs: Vec<IndexedTextSpec>) -> TextCatalogGuard {
    let previous = CURRENT.with(|c| c.borrow_mut().replace(specs));
    TextCatalogGuard { previous }
}

/// Every installed spec (whole-vertex removal fans a delete out to each definition's target).
pub(crate) fn all_specs() -> Vec<IndexedTextSpec> {
    CURRENT.with(|c| c.borrow().clone().unwrap_or_default())
}

/// The installed specs that index `property_id`, or an empty slice semantics when no catalog is
/// installed. Label filtering is the caller's job because only it has resolved the vertex's
/// current labels.
pub(crate) fn specs_for_property(property_id: PropertyId) -> Vec<IndexedTextSpec> {
    CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .map(|specs| {
                specs
                    .iter()
                    .filter(|spec| spec.property_id == property_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Cheap gate for whole-vertex removals: `true` when any text spec is installed at all. A single
/// delete op covers every indexed property of the vertex (the doc key is the vertex identity).
// Its only production caller is the vertex-delete hook awaiting wiring in
// `crate::facade::store::vertex_delete`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn has_specs() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(|specs| !specs.is_empty()))
}

#[cfg(test)]
pub(crate) fn enter_indexed(specs: &[IndexedTextSpec]) -> TextCatalogGuard {
    enter(specs.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::PropertyId;

    fn spec(property_id: u32, labels: &[u16]) -> IndexedTextSpec {
        IndexedTextSpec {
            property_id: PropertyId::from_raw(property_id),
            labels: labels.iter().map(|l| VertexLabelId::from_raw(*l)).collect(),
        }
    }

    #[test]
    fn specs_for_property_filters_installed_catalog() {
        let _guard = enter_indexed(&[spec(1, &[10]), spec(2, &[20])]);
        assert_eq!(specs_for_property(PropertyId::from_raw(2)).len(), 1);
        assert!(specs_for_property(PropertyId::from_raw(3)).is_empty());
    }

    #[test]
    fn without_catalog_every_lookup_is_inert() {
        assert!(specs_for_property(PropertyId::from_raw(1)).is_empty());
        assert!(!has_specs());
    }

    #[test]
    fn guard_restores_previous_catalog_on_drop() {
        let outer = enter_indexed(&[spec(1, &[])]);
        {
            let _inner = enter_indexed(&[spec(2, &[])]);
            assert!(has_specs());
        }
        assert!(has_specs(), "outer catalog must be restored");
        drop(outer);
        assert!(!has_specs());
    }

    #[test]
    fn matches_labels_intersects_spec_scope() {
        let scoped = spec(1, &[1, 2]);
        assert!(scoped.matches_labels(&[VertexLabelId::from_raw(2)]));
        assert!(!scoped.matches_labels(&[VertexLabelId::from_raw(3)]));
        // A vertex with no labels intersects no labeled scope (fail-closed; v0 text definitions
        // are always label-scoped).
        assert!(!scoped.matches_labels(&[]));
    }
}
