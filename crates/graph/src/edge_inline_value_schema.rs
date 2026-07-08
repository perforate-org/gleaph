//! Resolve `EdgeInlineValueProfile` from execution wire (ADR 0008).

use std::cell::RefCell;

use gleaph_graph_kernel::entry::{EdgeInlineValueProfile, EdgeLabelId};
use gleaph_graph_kernel::plan_exec::{ResolvedEdgeLabel, ResolvedLabelTable};

thread_local! {
    static ACTIVE_RESOLVED_LABELS: RefCell<Option<ResolvedLabelTable>> =
        const { RefCell::new(None) };
}

/// Binds router-resolved label schema for the current graph invocation (plan/DML).
pub(crate) fn set_execution_resolved_labels(labels: Option<ResolvedLabelTable>) {
    ACTIVE_RESOLVED_LABELS.with(|cell| *cell.borrow_mut() = labels);
}

pub(crate) fn clear_execution_resolved_labels() {
    ACTIVE_RESOLVED_LABELS.with(|cell| *cell.borrow_mut() = None);
}

pub(crate) fn lookup_edge_inline_value_profile_with(
    labels: Option<&ResolvedLabelTable>,
    label: EdgeLabelId,
) -> EdgeInlineValueProfile {
    if let Some(profile) = labels.and_then(|table| table.edge_inline_value_profile(label)) {
        return profile.clone();
    }
    if let Some(profile) = ACTIVE_RESOLVED_LABELS.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|table| table.edge_inline_value_profile(label))
            .cloned()
    }) {
        return profile;
    }
    #[cfg(any(test, feature = "canbench"))]
    if let Some(profile) = crate::test_labels::edge_inline_value_profile_for_id(label) {
        return profile;
    }
    EdgeInlineValueProfile::no_inline_value()
}

pub(crate) fn lookup_edge_inline_value_profile(label: EdgeLabelId) -> EdgeInlineValueProfile {
    lookup_edge_inline_value_profile_with(None, label)
}

/// Returns the Router-resolved edge label entry, if one was projected for this execution.
pub(crate) fn resolved_edge_label_with(
    labels: Option<&ResolvedLabelTable>,
    label: EdgeLabelId,
) -> Option<ResolvedEdgeLabel> {
    if let Some(entry) = labels.and_then(|table| table.resolved_edge_label(label).cloned()) {
        return Some(entry);
    }
    ACTIVE_RESOLVED_LABELS.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|table| table.resolved_edge_label(label).cloned())
    })
}

pub(crate) fn edge_label_ids_for_predicate_fusion(
    labels: Option<&ResolvedLabelTable>,
) -> Vec<EdgeLabelId> {
    if let Some(table) = labels {
        return table.edge_label_ids_with_nonzero_payload();
    }
    if let Some(table) = ACTIVE_RESOLVED_LABELS.with(|cell| cell.borrow().clone()) {
        return table.edge_label_ids_with_nonzero_payload();
    }
    #[cfg(any(test, feature = "canbench"))]
    {
        crate::test_labels::edge_label_ids_with_inline_value_profiles()
    }
    #[cfg(not(any(test, feature = "canbench")))]
    {
        Vec::new()
    }
}
