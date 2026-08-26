//! Graph-owned canonical property-index export contracts (ADR 0059).
//!
//! These types deliberately carry only the compact numeric identity needed to address one
//! immutable export scope.  Graph owns the cursor encoding and the storage walk; callers must treat
//! cursor bytes as opaque and persist them with the migration's durable progress.

use crate::entry::{EdgeInlinePropertyProfile, EdgeLabelId, GraphId, IndexNameId, PropertyId};
use crate::index::{EdgeIndexDirection, PhysicalIndexId};
use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_structures::{Storable, storable::Bound};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Version byte for the Graph-owned cursor encoding. It guards against decoding foreign or
/// corrupt bytes: a reader that sees any other leading byte rejects the cursor outright instead
/// of guessing a different layout.
pub const CANONICAL_EXPORT_CURSOR_VERSION: u8 = 1;

/// Maximum number of canonical facts examined or returned by one bounded export step.
pub const MAX_CANONICAL_EXPORT_PAGE_ITEMS: u32 = 10_000;

/// Maximum cumulative encoded-value bytes emitted by one canonical export page.
///
/// The page item limit and this fixed byte budget are independent bounds.  The wire envelope,
/// cursor, and per-fact metadata remain outside this budget; the message-sizing target leaves
/// the documented headroom for those fields and future envelope growth.
pub const MAX_CANONICAL_EXPORT_PAGE_BYTES: usize =
    gleaph_message_sizing::INTER_CANISTER_TARGET_PAYLOAD_BYTES;

/// Compact typed failure returned by Graph's canonical export control and data-plane methods.
///
/// The public wire intentionally carries stable classifications instead of implementation detail
/// strings. Graph Index uses these variants to distinguish exact-replay/lifecycle failures from
/// malformed requests without parsing text; Graph keeps storage diagnostics in local logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum CanonicalExportError {
    InvalidScope,
    InvalidRequest,
    ScopeNotFound,
    ScopeConflict,
    ScopeMismatch,
    /// The namespace is sealed and new build DML must be retried after the lifecycle advances.
    RetryableSealing,
    /// The requested lifecycle operation is not valid for the current phase.
    InvalidPhase,
    /// The sequence acknowledgement was not the next contiguous sequence.
    SequenceGap,
    /// The sequence was already acknowledged; callers must replay the original envelope only
    /// through the graph-index exact-replay path, not by advancing this watermark again.
    SequenceReplay,
    /// The sequence is outside the admitted/captured range.
    SequenceOutOfRange,
    /// The graph-index convergence proof does not match this Graph's captured seal watermark.
    NotConverged,
    /// Removal would discard admitted work that has not reached the contiguous drain watermark.
    UnsafeRemoval,
    /// The caller is not the frozen scope's authorized puller. Graph owns export-scope
    /// admission: every registered namespace names exactly one puller principal and page
    /// reads fail closed for anyone else.
    UnauthorizedPuller,
    CursorMalformed,
    UnsupportedInlineProfile,
    FactTooLarge {
        encoded_value_bytes: u64,
    },
    Storage,
}

impl fmt::Display for CanonicalExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScope => write!(f, "invalid canonical export scope"),
            Self::InvalidRequest => write!(f, "invalid canonical export request"),
            Self::ScopeNotFound => write!(f, "canonical export scope not found"),
            Self::ScopeConflict => write!(f, "canonical export scope conflicts with registration"),
            Self::ScopeMismatch => write!(f, "canonical export scope mismatch"),
            Self::RetryableSealing => {
                write!(f, "canonical export scope is sealing; retry after the seal")
            }
            Self::InvalidPhase => write!(f, "canonical export lifecycle phase is invalid"),
            Self::SequenceGap => write!(
                f,
                "canonical export sequence acknowledgement is not contiguous"
            ),
            Self::SequenceReplay => write!(
                f,
                "canonical export sequence acknowledgement was already applied"
            ),
            Self::SequenceOutOfRange => {
                write!(f, "canonical export sequence is outside the admitted range")
            }
            Self::NotConverged => write!(
                f,
                "canonical export graph-index convergence proof is incomplete"
            ),
            Self::UnsafeRemoval => write!(f, "canonical export scope has pending admitted work"),
            Self::UnauthorizedPuller => write!(f, "caller is not the scope's authorized puller"),
            Self::CursorMalformed => write!(f, "malformed canonical export cursor"),
            Self::UnsupportedInlineProfile => {
                write!(f, "canonical export inline profile is unsupported")
            }
            Self::FactTooLarge {
                encoded_value_bytes,
            } => write!(
                f,
                "canonical export fact encoded value is too large: {encoded_value_bytes} bytes"
            ),
            Self::Storage => write!(f, "canonical export storage error"),
        }
    }
}

impl std::error::Error for CanonicalExportError {}

/// One explicit logical property-index target.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum CanonicalExportTarget {
    /// A vertex property identified by its Router-issued property id.
    ///
    /// `property_id` is always the posting identity: the written property for a flat index,
    /// the interned dotted leaf for a nested record index. A nested index additionally
    /// carries `record_source`, which locates the leaf inside stored records so the export
    /// can walk the ancestor's sidecar values.
    Vertex {
        label_id: u16,
        property_id: PropertyId,
        record_source: Option<CanonicalRecordSource>,
    },
    /// An edge property identified by label, property, and the stable direction used by the
    /// Router/index catalog (see ADR 0012).
    Edge {
        label_id: EdgeLabelId,
        property_id: PropertyId,
        direction: EdgeIndexDirection,
    },
    /// Raw-text vertex projection for `CREATE TEXT INDEX` backfill (ADR 0059 §Text build
    /// kind). Pages carry raw UTF-8 values instead of sortable index keys so the text
    /// canister can analyze them.
    Text {
        label_id: u16,
        property_id: PropertyId,
    },
}

/// Locates one nested record leaf inside stored vertex records (ADR 0073 §3).
///
/// The ancestor id is the interned top-level property that stores the root record; Graph
/// resolves walks by id because it owns no property-name catalog.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct CanonicalRecordSource {
    pub ancestor_property_id: PropertyId,
    /// Dotted path inside the record, excluding the ancestor head (for example `score`).
    pub field_tail: String,
}

/// Minimal Graph-owned projection needed to decode one fixed-width inline property value.
///
/// `source_property_id` identifies the top-level inline slot in the edge schema.  For a scalar
/// inline property it equals the target property id and `byte_offset` is zero.  For a struct leaf,
/// it names the enclosing slot while `value_profile` describes the selected leaf byte range.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct CanonicalInlineProjection {
    pub source_property_id: PropertyId,
    pub byte_offset: u16,
    pub source_profile: EdgeInlinePropertyProfile,
    pub value_profile: EdgeInlinePropertyProfile,
}

/// Durable scope frozen for one physical posting namespace.
///
/// The stable Graph map is keyed by [`PhysicalIndexId`]; this value intentionally excludes that
/// key and stores no cursor position.  A scope is immutable after first registration except for
/// idempotent exact replay.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct CanonicalExportScope {
    pub graph_id: GraphId,
    pub index_name_id: IndexNameId,
    pub catalog_epoch: u64,
    pub target: CanonicalExportTarget,
    /// `Some` selects canonical edge INLINE bytes; `None` selects canonical edge sidecars.  This
    /// field is ignored for vertex targets and must be absent there.
    pub inline: Option<CanonicalInlineProjection>,
}

/// Durable lifecycle phase for one Graph-owned physical export namespace.
///
/// `Building` admits and sequences exact DML envelopes. `Sealing` captures the last admitted
/// sequence under the old epoch and rejects new admissions retryably. `Active` is published only
/// after the graph-index convergence proof matches the captured watermark. `Aborting` is never
/// planner-visible and may be removed only after all admitted work is drained.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum CanonicalExportPhase {
    Building,
    Sealing,
    Active,
    Aborting,
}

/// Versioned Candid wire envelope for one durable MemoryId 51 value.
///
/// Persisted rows always carry this outer tag so a future schema revision is wire-additive
/// (a new variant) instead of bricking decode of existing rows.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum CanonicalExportStableRecord {
    V1(CanonicalExportRecord),
}

/// Durable value stored under one `PhysicalIndexId` in Graph MemoryId 51.
///
/// Rows persist through [`CanonicalExportStableRecord::V1`]: schema revisions add a new
/// envelope variant instead of replacing bytes in place (Plan 0301; registry compat is
/// `ProductionCompat::VersionedSurvivor`).
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct CanonicalExportRecord {
    /// Immutable logical scope identity. `catalog_epoch` is the registration (old-epoch) value;
    /// [`epoch`](Self::epoch) advances at the seal fence while graph/index/inline identity stays
    /// frozen.
    pub scope: CanonicalExportScope,
    pub phase: CanonicalExportPhase,
    /// Current lifecycle/catalog epoch. During `Sealing`, this is the fresh epoch that fences new
    /// admissions while `scope.catalog_epoch` remains the old epoch used by admitted envelopes.
    pub epoch: u64,
    pub admitted_through: u64,
    pub drained_through: u64,
    /// Graph-owned export admission: the ONE principal allowed to pull canonical pages for
    /// this namespace. Posting builds bind the shard's configured graph-index canister; the
    /// TEXT backfill lane binds the provisioned text canister when its scope registers.
    /// Reads fail closed for every other caller.
    pub authorized_puller: Principal,
}

/// Graph-local status projection for Router and maintenance callers.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct CanonicalExportStatus {
    pub physical_index_id: PhysicalIndexId,
    pub scope: CanonicalExportScope,
    pub phase: CanonicalExportPhase,
    pub epoch: u64,
    pub admitted_through: u64,
    pub drained_through: u64,
}

/// Result of one successful Graph-local admission. The caller puts `sequence` into the exact
/// namespace-scoped DML envelope and retains that envelope until graph-index acknowledges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct CanonicalExportAdmission {
    pub sequence: u64,
    pub admitted_through: u64,
    pub epoch: u64,
}

/// Bounded Graph-side drain of one physical namespace's build-DML outbox entries.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct IndexBuildOutboxDrainRequest {
    pub physical_index_id: PhysicalIndexId,
    pub max_entries: u32,
}

/// Progress of one bounded build-DML drain step.
///
/// `converged` is true only when no build-DML entries remain for the namespace AND the scope
/// record's `drained_through` has reached its `admitted_through` watermark.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct IndexBuildOutboxDrainProgress {
    pub drained: u32,
    pub remaining: u64,
    pub converged: bool,
}

/// Request for one bounded canonical export page.
///
/// The cursor is an opaque Graph-owned token. It embeds this entire compact scope, and Graph
/// validates those bindings so a token cannot be resumed under another graph, logical index,
/// generation, epoch, or target.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct CanonicalExportRequest {
    pub graph_id: GraphId,
    pub index_name_id: IndexNameId,
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    pub target: CanonicalExportTarget,
    pub cursor: Option<Vec<u8>>,
    pub limit: u32,
}

/// One canonical indexable fact projected by Graph.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum CanonicalIndexableFact {
    Vertex {
        vertex_id: u32,
        property_id: PropertyId,
        encoded_value: Vec<u8>,
    },
    Edge {
        owner_vertex_id: u32,
        label_id: u16,
        slot_index: u32,
        property_id: PropertyId,
        encoded_value: Vec<u8>,
    },
    /// Raw-text vertex fact for `CREATE TEXT INDEX` backfill (ADR 0059 §Text build kind).
    ///
    /// Carries the raw UTF-8 property value instead of a sortable index key so the text
    /// canister can analyze it. The distinction is type-level: no page can be decoded
    /// under the wrong projection because consumers match this variant explicitly.
    VertexText {
        vertex_id: u32,
        property_id: PropertyId,
        raw_value: String,
    },
}

/// Result of one bounded canonical export page.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct CanonicalExportPage {
    pub facts: Vec<CanonicalIndexableFact>,
    /// Opaque continuation.  `None` means this page reached the end of the selected source.
    pub next: Option<Vec<u8>>,
    pub done: bool,
}

/// Candid encoding is the stable value format for the frozen scope map.  The map is Graph-owned;
/// this implementation remains here with the wire type so a future Graph canister endpoint can
/// reuse the exact contract without introducing another schema owner.
impl Storable for CanonicalExportScope {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("canonical export scope must encode"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("canonical export scope must encode")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("canonical export scope must decode")
    }
}

impl Storable for CanonicalExportStableRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("canonical export record must encode"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("canonical export record must encode")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), Self).expect("canonical export record must decode") {
            // Exhaustive over the envelope: a future variant forces an explicit decision here,
            // and foreign Candid payloads fail closed through the expect above.
            v1 @ Self::V1(_) => v1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
    use candid::Encode;

    fn scope() -> CanonicalExportScope {
        CanonicalExportScope {
            graph_id: GraphId::from_raw(7),
            index_name_id: IndexNameId::from_raw(9),
            catalog_epoch: 11,
            target: CanonicalExportTarget::Edge {
                label_id: EdgeLabelId::from_raw(3),
                property_id: PropertyId::from_raw(5),
                direction: EdgeIndexDirection::Any,
            },
            inline: Some(CanonicalInlineProjection {
                source_property_id: PropertyId::from_raw(5),
                byte_offset: 0,
                source_profile: EdgeInlinePropertyProfile {
                    byte_width: 4,
                    encoding: EdgeInlinePropertyEncoding::F32,
                },
                value_profile: EdgeInlinePropertyProfile {
                    byte_width: 4,
                    encoding: EdgeInlinePropertyEncoding::F32,
                },
            }),
        }
    }

    #[test]
    fn scope_round_trips_through_stable_encoding() {
        let original = scope();
        let bytes = Storable::into_bytes(original.clone());
        assert_eq!(
            CanonicalExportScope::from_bytes(Cow::Owned(bytes)),
            original
        );
    }

    /// Builds one enveloped lifecycle record over an arbitrary scope projection.
    fn envelope_record(
        target: CanonicalExportTarget,
        inline: Option<CanonicalInlineProjection>,
        phase: CanonicalExportPhase,
    ) -> CanonicalExportRecord {
        CanonicalExportRecord {
            scope: CanonicalExportScope {
                graph_id: GraphId::from_raw(7),
                index_name_id: IndexNameId::from_raw(9),
                catalog_epoch: 11,
                target,
                inline,
            },
            phase,
            epoch: 12,
            admitted_through: 9,
            drained_through: 7,
            authorized_puller: Principal::from_slice(&[0x5E, 0x11]),
        }
    }

    #[test]
    fn v1_envelope_round_trips_every_target_across_building_and_active() {
        let cases = [
            (
                "flat vertex",
                CanonicalExportTarget::Vertex {
                    label_id: 4,
                    property_id: PropertyId::from_raw(6),
                    record_source: None,
                },
                None,
            ),
            (
                "nested vertex",
                CanonicalExportTarget::Vertex {
                    label_id: 4,
                    property_id: PropertyId::from_raw(6),
                    record_source: Some(CanonicalRecordSource {
                        ancestor_property_id: PropertyId::from_raw(5),
                        field_tail: "meta.deep".to_owned(),
                    }),
                },
                None,
            ),
            (
                "text vertex",
                CanonicalExportTarget::Text {
                    label_id: 4,
                    property_id: PropertyId::from_raw(6),
                },
                None,
            ),
            (
                "sidecar edge",
                CanonicalExportTarget::Edge {
                    label_id: EdgeLabelId::from_raw(3),
                    property_id: PropertyId::from_raw(5),
                    direction: EdgeIndexDirection::Any,
                },
                None,
            ),
            (
                "inline edge",
                CanonicalExportTarget::Edge {
                    label_id: EdgeLabelId::from_raw(3),
                    property_id: PropertyId::from_raw(5),
                    direction: EdgeIndexDirection::Any,
                },
                Some(CanonicalInlineProjection {
                    source_property_id: PropertyId::from_raw(5),
                    byte_offset: 0,
                    source_profile: EdgeInlinePropertyProfile {
                        byte_width: 4,
                        encoding: EdgeInlinePropertyEncoding::F32,
                    },
                    value_profile: EdgeInlinePropertyProfile {
                        byte_width: 4,
                        encoding: EdgeInlinePropertyEncoding::F32,
                    },
                }),
            ),
        ];
        for phase in [CanonicalExportPhase::Building, CanonicalExportPhase::Active] {
            for (label, target, inline) in &cases {
                let record = envelope_record(target.clone(), inline.clone(), phase);
                let expected = CanonicalExportStableRecord::V1(record.clone());
                let bytes = Storable::into_bytes(expected.clone());

                // The outer tag comes first: durable rows carry the V1 envelope, never a bare
                // pre-envelope record payload.
                assert_eq!(
                    Decode!(bytes.as_ref(), CanonicalExportStableRecord)
                        .expect("canonical export record must decode"),
                    expected,
                    "{label} rows must persist behind the V1 envelope tag",
                );
                assert_eq!(
                    CanonicalExportStableRecord::from_bytes(Cow::Owned(bytes)),
                    expected,
                    "{label} record must survive the stable round trip in {phase:?}",
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "canonical export record must decode")]
    fn foreign_scope_bytes_rejected_by_record_decode() {
        // Encoded CanonicalExportScope bytes are foreign to the enveloped record layout and must
        // fail closed instead of decoding into any envelope variant.
        let foreign = Storable::into_bytes(scope());
        drop(CanonicalExportStableRecord::from_bytes(Cow::Owned(foreign)));
    }

    #[test]
    fn page_byte_budget_leaves_candid_message_headroom() {
        let item_count = usize::try_from(MAX_CANONICAL_EXPORT_PAGE_ITEMS).unwrap();
        let base = MAX_CANONICAL_EXPORT_PAGE_BYTES / item_count;
        let remainder = MAX_CANONICAL_EXPORT_PAGE_BYTES % item_count;
        let facts: Vec<_> = (0..item_count)
            .map(|index| CanonicalIndexableFact::Edge {
                owner_vertex_id: u32::try_from(index).unwrap(),
                label_id: 1,
                slot_index: u32::try_from(index).unwrap(),
                property_id: PropertyId::from_raw(1),
                encoded_value: vec![0; base + usize::from(index < remainder)],
            })
            .collect();
        let encoded_values = facts
            .iter()
            .map(|fact| match fact {
                CanonicalIndexableFact::Edge { encoded_value, .. }
                | CanonicalIndexableFact::Vertex { encoded_value, .. } => encoded_value.len(),
                CanonicalIndexableFact::VertexText { raw_value, .. } => raw_value.len(),
            })
            .sum::<usize>();
        assert_eq!(encoded_values, MAX_CANONICAL_EXPORT_PAGE_BYTES);

        let bytes = Encode!(&CanonicalExportPage {
            facts,
            next: Some(vec![0; 64]),
            done: false,
        })
        .expect("canonical page must encode");
        assert!(
            bytes.len() <= gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES,
            "encoded page exceeds inter-canister ceiling: {} bytes",
            bytes.len()
        );
    }
}
