//! Resolve `EdgeInlinePropertyProfile` from execution wire (ADR 0008).

use std::cell::RefCell;

use gleaph_graph_kernel::entry::{EdgeInlinePropertyProfile, EdgeLabelId};
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

pub(crate) fn lookup_edge_inline_property_profile_with(
    labels: Option<&ResolvedLabelTable>,
    label: EdgeLabelId,
) -> EdgeInlinePropertyProfile {
    if let Some(profile) = labels.and_then(|table| table.edge_inline_property_profile(label)) {
        return profile.clone();
    }
    if let Some(profile) = ACTIVE_RESOLVED_LABELS.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|table| table.edge_inline_property_profile(label))
            .cloned()
    }) {
        return profile;
    }
    #[cfg(any(test, feature = "canbench"))]
    if let Some(profile) = crate::test_labels::edge_inline_property_profile_for_id(label) {
        return profile;
    }
    EdgeInlinePropertyProfile::no_inline_property()
}

pub(crate) fn lookup_edge_inline_property_profile(label: EdgeLabelId) -> EdgeInlinePropertyProfile {
    lookup_edge_inline_property_profile_with(None, label)
}

/// Returns the Router-resolved edge label entry, if one was projected for this execution.
pub(crate) fn resolved_edge_label_with(
    labels: Option<&ResolvedLabelTable>,
    label: EdgeLabelId,
) -> Option<ResolvedEdgeLabel> {
    if let Some(entry) = labels.and_then(|table| table.resolved_edge_label(label).cloned()) {
        return Some(entry);
    }
    if let Some(entry) = ACTIVE_RESOLVED_LABELS.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|table| table.resolved_edge_label(label).cloned())
    }) {
        return Some(entry);
    }
    #[cfg(any(test, feature = "canbench"))]
    {
        if let Some(schema) = crate::test_labels::edge_inline_struct_schema_for_id(label) {
            let profile = crate::test_labels::edge_inline_property_profile_for_id(label)?;
            let name = crate::test_labels::edge_label_name_for_id(label).unwrap_or_default();
            return Some(ResolvedEdgeLabel::with_inline_schema(
                name,
                label,
                profile,
                Some(schema),
            ));
        }
        let profile = crate::test_labels::edge_inline_property_profile_for_id(label)?;
        if profile.required_byte_width() == 0 {
            return None;
        }
        let name = crate::test_labels::edge_label_name_for_id(label).unwrap_or_default();
        let property_id = crate::test_labels::edge_inline_property_for_id(label);
        Some(ResolvedEdgeLabel::with_inline_property(
            name,
            label,
            profile,
            property_id,
        ))
    }
    #[cfg(not(any(test, feature = "canbench")))]
    {
        None
    }
}

pub(crate) fn edge_label_ids_for_predicate_fusion(
    labels: Option<&ResolvedLabelTable>,
) -> Vec<EdgeLabelId> {
    if let Some(table) = labels {
        return table.edge_label_ids_with_nonzero_inline_property_bytes();
    }
    if let Some(table) = ACTIVE_RESOLVED_LABELS.with(|cell| cell.borrow().clone()) {
        return table.edge_label_ids_with_nonzero_inline_property_bytes();
    }
    #[cfg(any(test, feature = "canbench"))]
    {
        crate::test_labels::edge_label_ids_with_inline_property_profiles()
    }
    #[cfg(not(any(test, feature = "canbench")))]
    {
        Vec::new()
    }
}
