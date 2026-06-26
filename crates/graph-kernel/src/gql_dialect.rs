//! Canonical Gleaph GQL dialect extension manifest.
//!
//! This module is a registry and recognizer layer, not an execution dispatcher. It records the
//! canonical names, syntax classes, implementation status, owning boundary, and design-document
//! anchor for each Gleaph-specific GQL extension. Recognition helpers are kept exact or
//! case-insensitive explicitly so callers do not accidentally change existing behavior.
//!
//! Owners remain responsible for their semantics:
//!
//! - `gleaph-gql-ic` owns `IC.PRINCIPAL` value encoding/decoding.
//! - Graph execution owns `MSG_CALLER()` and runtime-function context.
//! - Graph planner integration owns `GLEAPH.COST` and `GLEAPH.VECTOR.*` fusion helpers.
//! - Graph execution owns `GLEAPH.WEIGHT` edge-payload decode and `GLEAPH.SEQUENCE` edge ordering.
//! - Graph mutation executor owns operational `GLEAPH.FINALIZE_*` / `GLEAPH.DRAIN_*` procedures.
//! - Router owns planned `SEARCH`, `INLINE`, and `CREATE VECTOR INDEX` syntax.
//!
//! See `design/gql/extension-syntax.md` and `design/adr/0034-gleaph-gql-extension-syntax.md`.

/// A canonical dotted name such as `GLEAPH.VECTOR.L2_SQUARED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QualifiedName {
    parts: &'static [&'static str],
}

impl QualifiedName {
    pub const fn new(parts: &'static [&'static str]) -> Self {
        Self { parts }
    }

    pub fn parts(&self) -> &'static [&'static str] {
        self.parts
    }

    /// True when `parts` has the same length and each part matches exactly.
    pub fn matches_exact(&self, parts: &[impl AsRef<str>]) -> bool {
        parts.len() == self.parts.len()
            && parts
                .iter()
                .zip(self.parts)
                .all(|(part, expected)| part.as_ref() == *expected)
    }

    /// True when `parts` has the same length and each part matches ignoring ASCII case.
    pub fn matches_ascii_case_insensitive(&self, parts: &[impl AsRef<str>]) -> bool {
        parts.len() == self.parts.len()
            && parts
                .iter()
                .zip(self.parts)
                .all(|(part, expected)| part.as_ref().eq_ignore_ascii_case(expected))
    }
}

/// Syntax class of a Gleaph GQL extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GqlDialectExtensionKind {
    /// Extension value type such as `IC.PRINCIPAL`.
    ValueType,
    /// Runtime function such as `MSG_CALLER()`.
    RuntimeFunction,
    /// Path-pattern extension such as `GLEAPH.COST`.
    PathExtension,
    /// Function that reads a fixed-width edge-payload value.
    EdgePayloadFunction,
    /// Function that reads Graph-owned edge insertion-order metadata.
    EdgeOrderingFunction,
    /// Search subclause such as `SEARCH ... IN (VECTOR INDEX ...)`.
    SearchClause,
    /// Schema modifier such as `INLINE`.
    SchemaModifier,
    /// DDL statement such as `CREATE VECTOR INDEX`.
    DdlStatement,
    /// Imperative maintenance/finalize procedure under `GLEAPH.*`.
    OperationalProcedure,
    /// Function that is only valid as an argument expression for an operational procedure,
    /// not as a `CALL` target itself (e.g. `GLEAPH.VERTEX_LIST(...)` inside finalize args).
    OperationalProcedureArgumentFunction,
}

/// Implementation status of a dialect extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GqlDialectExtensionStatus {
    /// Fully implemented and available to users.
    Implemented,
    /// Implemented compatibility surface that may later be replaced by more ordinary GQL syntax.
    Compatibility,
    /// Planned but not yet implemented.
    Planned,
}

/// Owning boundary responsible for the semantics of an extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GqlDialectExtensionOwner {
    /// Generic IC GQL value/function bridge (`gleaph-gql-ic`).
    GqlIc,
    /// Graph shard-local query execution.
    GraphExecution,
    /// Graph mutation executor (finalize/drain procedures).
    GraphMutationExecutor,
    /// Graph/planner integration layer that attaches Gleaph meaning to generic plan shapes.
    GraphPlannerIntegration,
    /// Router query orchestration and catalog/index resolution.
    Router,
    /// Vector-index canister search and maintenance.
    VectorIndex,
}

/// Metadata for one Gleaph GQL dialect extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GqlDialectExtensionSpec {
    pub canonical_name: QualifiedName,
    pub kind: GqlDialectExtensionKind,
    pub status: GqlDialectExtensionStatus,
    pub owner: GqlDialectExtensionOwner,
    pub doc_anchor: &'static str,
}

// ---------------------------------------------------------------------------
// Canonical names
// ---------------------------------------------------------------------------

/// `IC.PRINCIPAL`
pub const IC_PRINCIPAL: QualifiedName = QualifiedName::new(&["IC", "PRINCIPAL"]);

/// `MSG_CALLER`
pub const MSG_CALLER: QualifiedName = QualifiedName::new(&["MSG_CALLER"]);

/// `GLEAPH.COST`
pub const GLEAPH_COST: QualifiedName = QualifiedName::new(&["GLEAPH", "COST"]);

/// `GLEAPH.WEIGHT`
pub const GLEAPH_WEIGHT: QualifiedName = QualifiedName::new(&["GLEAPH", "WEIGHT"]);

/// `GLEAPH.SEQUENCE`
pub const GLEAPH_SEQUENCE: QualifiedName = QualifiedName::new(&["GLEAPH", "SEQUENCE"]);

/// `GLEAPH.VECTOR.L2_SQUARED`
pub const GLEAPH_VECTOR_L2_SQUARED: QualifiedName =
    QualifiedName::new(&["GLEAPH", "VECTOR", "L2_SQUARED"]);

/// `GLEAPH.VECTOR.COSINE_DISTANCE`
pub const GLEAPH_VECTOR_COSINE_DISTANCE: QualifiedName =
    QualifiedName::new(&["GLEAPH", "VECTOR", "COSINE_DISTANCE"]);

/// `GLEAPH.VECTOR.DOT`
pub const GLEAPH_VECTOR_DOT: QualifiedName = QualifiedName::new(&["GLEAPH", "VECTOR", "DOT"]);

/// `GLEAPH.FINALIZE_BULK_INGEST`
pub const GLEAPH_FINALIZE_BULK_INGEST: QualifiedName =
    QualifiedName::new(&["GLEAPH", "FINALIZE_BULK_INGEST"]);

/// `GLEAPH.FINALIZE_FORWARD_EDGE_SPAN`
pub const GLEAPH_FINALIZE_FORWARD_EDGE_SPAN: QualifiedName =
    QualifiedName::new(&["GLEAPH", "FINALIZE_FORWARD_EDGE_SPAN"]);

/// `GLEAPH.DRAIN_DEFERRED_MAINTENANCE`
pub const GLEAPH_DRAIN_DEFERRED_MAINTENANCE: QualifiedName =
    QualifiedName::new(&["GLEAPH", "DRAIN_DEFERRED_MAINTENANCE"]);

/// `GLEAPH.VERTEX_LIST`
pub const GLEAPH_VERTEX_LIST: QualifiedName = QualifiedName::new(&["GLEAPH", "VERTEX_LIST"]);

/// `SEARCH`
pub const SEARCH: QualifiedName = QualifiedName::new(&["SEARCH"]);

/// `INLINE`
pub const INLINE: QualifiedName = QualifiedName::new(&["INLINE"]);

/// `CREATE VECTOR INDEX`
pub const CREATE_VECTOR_INDEX: QualifiedName = QualifiedName::new(&["CREATE", "VECTOR", "INDEX"]);

// ---------------------------------------------------------------------------
// Full manifest
// ---------------------------------------------------------------------------

/// Canonical registry of Gleaph GQL dialect extensions.
pub const GLEAPH_DIALECT_EXTENSIONS: &[GqlDialectExtensionSpec] = &[
    GqlDialectExtensionSpec {
        canonical_name: IC_PRINCIPAL,
        kind: GqlDialectExtensionKind::ValueType,
        status: GqlDialectExtensionStatus::Implemented,
        owner: GqlDialectExtensionOwner::GqlIc,
        doc_anchor: "design/gql/extension-syntax.md#icprincipal",
    },
    GqlDialectExtensionSpec {
        canonical_name: MSG_CALLER,
        kind: GqlDialectExtensionKind::RuntimeFunction,
        status: GqlDialectExtensionStatus::Implemented,
        owner: GqlDialectExtensionOwner::GraphExecution,
        doc_anchor: "design/gql/extension-syntax.md#msg_caller",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_COST,
        kind: GqlDialectExtensionKind::PathExtension,
        status: GqlDialectExtensionStatus::Compatibility,
        owner: GqlDialectExtensionOwner::GraphPlannerIntegration,
        doc_anchor: "design/gql/extension-syntax.md#edge-inline-properties",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_WEIGHT,
        kind: GqlDialectExtensionKind::EdgePayloadFunction,
        status: GqlDialectExtensionStatus::Compatibility,
        owner: GqlDialectExtensionOwner::GraphExecution,
        doc_anchor: "design/gql/extension-syntax.md#edge-inline-properties",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_SEQUENCE,
        kind: GqlDialectExtensionKind::EdgeOrderingFunction,
        status: GqlDialectExtensionStatus::Compatibility,
        owner: GqlDialectExtensionOwner::GraphExecution,
        doc_anchor: "design/gql/extension-syntax.md#edge-insertion-order-sequence",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_VECTOR_L2_SQUARED,
        kind: GqlDialectExtensionKind::EdgePayloadFunction,
        status: GqlDialectExtensionStatus::Compatibility,
        owner: GqlDialectExtensionOwner::GraphPlannerIntegration,
        doc_anchor: "design/gql/extension-syntax.md#edge-payload-vector-predicates",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_VECTOR_COSINE_DISTANCE,
        kind: GqlDialectExtensionKind::EdgePayloadFunction,
        status: GqlDialectExtensionStatus::Compatibility,
        owner: GqlDialectExtensionOwner::GraphPlannerIntegration,
        doc_anchor: "design/gql/extension-syntax.md#edge-payload-vector-predicates",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_VECTOR_DOT,
        kind: GqlDialectExtensionKind::EdgePayloadFunction,
        status: GqlDialectExtensionStatus::Compatibility,
        owner: GqlDialectExtensionOwner::GraphPlannerIntegration,
        doc_anchor: "design/gql/extension-syntax.md#edge-payload-vector-predicates",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_FINALIZE_BULK_INGEST,
        kind: GqlDialectExtensionKind::OperationalProcedure,
        status: GqlDialectExtensionStatus::Implemented,
        owner: GqlDialectExtensionOwner::GraphMutationExecutor,
        doc_anchor: "design/gql/extension-syntax.md#namespace-policy",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_FINALIZE_FORWARD_EDGE_SPAN,
        kind: GqlDialectExtensionKind::OperationalProcedure,
        status: GqlDialectExtensionStatus::Implemented,
        owner: GqlDialectExtensionOwner::GraphMutationExecutor,
        doc_anchor: "design/gql/extension-syntax.md#namespace-policy",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_DRAIN_DEFERRED_MAINTENANCE,
        kind: GqlDialectExtensionKind::OperationalProcedure,
        status: GqlDialectExtensionStatus::Implemented,
        owner: GqlDialectExtensionOwner::GraphMutationExecutor,
        doc_anchor: "design/gql/extension-syntax.md#namespace-policy",
    },
    GqlDialectExtensionSpec {
        canonical_name: GLEAPH_VERTEX_LIST,
        kind: GqlDialectExtensionKind::OperationalProcedureArgumentFunction,
        status: GqlDialectExtensionStatus::Implemented,
        owner: GqlDialectExtensionOwner::GraphMutationExecutor,
        doc_anchor: "design/gql/extension-syntax.md#namespace-policy",
    },
    GqlDialectExtensionSpec {
        canonical_name: SEARCH,
        kind: GqlDialectExtensionKind::SearchClause,
        status: GqlDialectExtensionStatus::Planned,
        owner: GqlDialectExtensionOwner::Router,
        doc_anchor: "design/gql/extension-syntax.md#search-subclause",
    },
    GqlDialectExtensionSpec {
        canonical_name: INLINE,
        kind: GqlDialectExtensionKind::SchemaModifier,
        status: GqlDialectExtensionStatus::Planned,
        owner: GqlDialectExtensionOwner::Router,
        doc_anchor: "design/gql/extension-syntax.md#edge-inline-properties",
    },
    GqlDialectExtensionSpec {
        canonical_name: CREATE_VECTOR_INDEX,
        kind: GqlDialectExtensionKind::DdlStatement,
        status: GqlDialectExtensionStatus::Planned,
        owner: GqlDialectExtensionOwner::Router,
        doc_anchor: "design/gql/extension-syntax.md#vector-index-ddl",
    },
];

/// Operational procedures declared in the manifest.
pub fn operational_procedures() -> impl Iterator<Item = &'static GqlDialectExtensionSpec> {
    GLEAPH_DIALECT_EXTENSIONS
        .iter()
        .filter(|spec| spec.kind == GqlDialectExtensionKind::OperationalProcedure)
}

/// Edge-payload functions declared in the manifest.
pub fn edge_payload_functions() -> impl Iterator<Item = &'static GqlDialectExtensionSpec> {
    GLEAPH_DIALECT_EXTENSIONS
        .iter()
        .filter(|spec| spec.kind == GqlDialectExtensionKind::EdgePayloadFunction)
}

/// Edge-ordering functions declared in the manifest.
pub fn edge_ordering_functions() -> impl Iterator<Item = &'static GqlDialectExtensionSpec> {
    GLEAPH_DIALECT_EXTENSIONS
        .iter()
        .filter(|spec| spec.kind == GqlDialectExtensionKind::EdgeOrderingFunction)
}

/// Planned extensions declared in the manifest.
pub fn planned_extensions() -> impl Iterator<Item = &'static GqlDialectExtensionSpec> {
    GLEAPH_DIALECT_EXTENSIONS
        .iter()
        .filter(|spec| spec.status == GqlDialectExtensionStatus::Planned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_has_no_duplicate_canonical_names() {
        let mut names: Vec<&'static [&'static str]> = GLEAPH_DIALECT_EXTENSIONS
            .iter()
            .map(|spec| spec.canonical_name.parts())
            .collect();
        let original_len = names.len();
        names.sort_by(|a, b| a.iter().cmp(b.iter()));
        names.dedup_by(|a, b| a.iter().eq(b.iter()));
        assert_eq!(
            names.len(),
            original_len,
            "duplicate canonical names in GLEAPH_DIALECT_EXTENSIONS"
        );
    }

    #[test]
    fn every_spec_has_non_empty_name_and_anchor() {
        for spec in GLEAPH_DIALECT_EXTENSIONS {
            assert!(
                !spec.canonical_name.parts().is_empty(),
                "empty canonical name in {spec:?}"
            );
            assert!(!spec.doc_anchor.is_empty(), "empty doc anchor in {spec:?}");
        }
    }

    #[test]
    fn known_tricky_doc_anchors_are_pinned() {
        assert_eq!(
            GLEAPH_DIALECT_EXTENSIONS
                .iter()
                .find(|spec| spec.canonical_name == IC_PRINCIPAL)
                .map(|spec| spec.doc_anchor),
            Some("design/gql/extension-syntax.md#icprincipal")
        );
        assert_eq!(
            GLEAPH_DIALECT_EXTENSIONS
                .iter()
                .find(|spec| spec.canonical_name == MSG_CALLER)
                .map(|spec| spec.doc_anchor),
            Some("design/gql/extension-syntax.md#msg_caller")
        );
    }

    #[test]
    fn sequence_is_edge_ordering_not_payload() {
        let spec = GLEAPH_DIALECT_EXTENSIONS
            .iter()
            .find(|spec| spec.canonical_name == GLEAPH_SEQUENCE)
            .expect("GLEAPH.SEQUENCE in manifest");
        assert_eq!(spec.kind, GqlDialectExtensionKind::EdgeOrderingFunction);
        assert_ne!(spec.kind, GqlDialectExtensionKind::EdgePayloadFunction);
    }

    #[test]
    fn vector_entries_are_edge_payload_functions() {
        for name in [
            GLEAPH_VECTOR_L2_SQUARED,
            GLEAPH_VECTOR_COSINE_DISTANCE,
            GLEAPH_VECTOR_DOT,
        ] {
            let spec = GLEAPH_DIALECT_EXTENSIONS
                .iter()
                .find(|spec| spec.canonical_name == name)
                .unwrap_or_else(|| panic!("{name:?} in manifest"));
            assert_eq!(spec.kind, GqlDialectExtensionKind::EdgePayloadFunction);
        }
    }

    #[test]
    fn finalize_and_drain_procedures_are_operational() {
        for name in [
            GLEAPH_FINALIZE_BULK_INGEST,
            GLEAPH_FINALIZE_FORWARD_EDGE_SPAN,
            GLEAPH_DRAIN_DEFERRED_MAINTENANCE,
        ] {
            let spec = GLEAPH_DIALECT_EXTENSIONS
                .iter()
                .find(|spec| spec.canonical_name == name)
                .unwrap_or_else(|| panic!("{name:?} in manifest"));
            assert_eq!(spec.kind, GqlDialectExtensionKind::OperationalProcedure);
        }
    }

    #[test]
    fn vertex_list_is_argument_helper_not_callable_procedure() {
        let spec = GLEAPH_DIALECT_EXTENSIONS
            .iter()
            .find(|spec| spec.canonical_name == GLEAPH_VERTEX_LIST)
            .expect("GLEAPH.VERTEX_LIST in manifest");
        assert_eq!(
            spec.kind,
            GqlDialectExtensionKind::OperationalProcedureArgumentFunction
        );
        assert_ne!(spec.kind, GqlDialectExtensionKind::OperationalProcedure);
    }

    #[test]
    fn planned_extensions_are_present_and_marked_planned() {
        let planned: Vec<_> = planned_extensions().collect();
        assert!(
            planned.iter().any(|spec| spec.canonical_name == SEARCH),
            "SEARCH must be planned"
        );
        assert!(
            planned.iter().any(|spec| spec.canonical_name == INLINE),
            "INLINE must be planned"
        );
        assert!(
            planned
                .iter()
                .any(|spec| spec.canonical_name == CREATE_VECTOR_INDEX),
            "CREATE VECTOR INDEX must be planned"
        );
        for spec in &planned {
            assert_eq!(spec.status, GqlDialectExtensionStatus::Planned);
        }
    }

    #[test]
    fn exact_and_case_insensitive_matching_differ() {
        assert!(MSG_CALLER.matches_exact(&["MSG_CALLER"]));
        assert!(!MSG_CALLER.matches_exact(&["msg_caller"]));
        assert!(MSG_CALLER.matches_ascii_case_insensitive(&["msg_caller"]));
        assert!(!MSG_CALLER.matches_ascii_case_insensitive(&["msg_caller", "extra"]));
    }

    #[test]
    fn qualified_name_rejects_wrong_length() {
        assert!(!GLEAPH_WEIGHT.matches_exact(&["GLEAPH"]));
        assert!(!GLEAPH_WEIGHT.matches_exact(&["GLEAPH", "WEIGHT", "EXTRA"]));
        assert!(!GLEAPH_WEIGHT.matches_ascii_case_insensitive(&["gleaph"]));
    }

    #[test]
    fn helper_groups_are_consistent_with_manifest() {
        assert_eq!(operational_procedures().count(), 3);
        assert_eq!(edge_payload_functions().count(), 4);
        assert_eq!(edge_ordering_functions().count(), 1);
        assert_eq!(planned_extensions().count(), 3);
    }
}
