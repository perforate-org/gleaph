//! Router SSOT for `(GraphId, EdgeLabelId) → EdgePayloadSchemaRecord` (ADR 0008, ADR 0018).
//!
//! The stable value is a versioned `EdgePayloadSchemaRecord`. Development stable data must be wiped
//! when this format changes because backward compatibility is not maintained.
//! The physical `EdgePayloadProfile` consumed by Graph is always derived from the canonical record.

use std::fmt;

use gleaph_graph_kernel::entry::{
    EdgeLabelId, EdgePayloadProfile, EdgePayloadProfileError, GraphId, PropertyId,
};
use gleaph_graph_kernel::scoped_name_catalog::GraphScopedIdKey;
use ic_stable_structures::{Memory, StableBTreeMap};

/// Fixed-width scalar types accepted in a standalone `CREATE EDGE LABEL ... INLINE` declaration.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Serialize, serde::Deserialize,
)]
pub enum InlineScalarType {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    U128,
    I128,
    F16,
    F32,
    F64,
    Fixed32,
    Fixed64,
}

impl InlineScalarType {
    /// Parses the scalar type token case-insensitively. Returns `None` for unrecognised names.
    pub fn from_ddl_name(name: &str) -> Option<Self> {
        let upper = name.to_ascii_uppercase();
        match upper.as_str() {
            "UINT8" | "U8" => Some(Self::U8),
            "UINT16" | "U16" => Some(Self::U16),
            "UINT32" | "U32" => Some(Self::U32),
            "UINT64" | "U64" => Some(Self::U64),
            "INT8" | "I8" => Some(Self::I8),
            "INT16" | "I16" => Some(Self::I16),
            "INT32" | "I32" => Some(Self::I32),
            "INT64" | "I64" => Some(Self::I64),
            "UINT128" | "U128" => Some(Self::U128),
            "INT128" | "I128" => Some(Self::I128),
            "FLOAT16" | "F16" => Some(Self::F16),
            "FLOAT32" | "F32" => Some(Self::F32),
            "FLOAT64" | "F64" => Some(Self::F64),
            "FIXED32" => Some(Self::Fixed32),
            "FIXED64" => Some(Self::Fixed64),
            _ => None,
        }
    }

    /// The physical edge-payload profile this scalar declaration derives.
    pub const fn edge_payload_profile(self) -> EdgePayloadProfile {
        use gleaph_graph_kernel::entry::EdgePayloadEncoding::*;
        match self {
            Self::U8 => EdgePayloadProfile {
                byte_width: 1,
                encoding: RawU8,
            },
            Self::U16 => EdgePayloadProfile {
                byte_width: 2,
                encoding: RawU16,
            },
            Self::U32 => EdgePayloadProfile {
                byte_width: 4,
                encoding: RawU32,
            },
            Self::U64 => EdgePayloadProfile {
                byte_width: 8,
                encoding: RawU64,
            },
            Self::I8 => EdgePayloadProfile {
                byte_width: 1,
                encoding: RawI8,
            },
            Self::I16 => EdgePayloadProfile {
                byte_width: 2,
                encoding: RawI16,
            },
            Self::I32 => EdgePayloadProfile {
                byte_width: 4,
                encoding: RawI32,
            },
            Self::I64 => EdgePayloadProfile {
                byte_width: 8,
                encoding: RawI64,
            },
            Self::U128 => EdgePayloadProfile {
                byte_width: 16,
                encoding: RawU128,
            },
            Self::I128 => EdgePayloadProfile {
                byte_width: 16,
                encoding: RawI128,
            },
            Self::F16 => EdgePayloadProfile {
                byte_width: 2,
                encoding: F16,
            },
            Self::F32 => EdgePayloadProfile {
                byte_width: 4,
                encoding: F32,
            },
            Self::F64 => EdgePayloadProfile {
                byte_width: 8,
                encoding: F64,
            },
            Self::Fixed32 => EdgePayloadProfile {
                byte_width: 32,
                encoding: RawFixed32,
            },
            Self::Fixed64 => EdgePayloadProfile {
                byte_width: 64,
                encoding: RawFixed64,
            },
        }
    }
}

/// Canonical Router-owned record for the edge-label payload schema.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub enum EdgePayloadSchemaRecord {
    /// Unnamed profile installed through the admin API. Carries no logical property identity.
    UnnamedProfile { profile: EdgePayloadProfile },
    /// Slice 20: one fixed-width scalar inline property per edge label.
    InlineScalar {
        property_id: PropertyId,
        scalar_type: InlineScalarType,
    },
}

impl EdgePayloadSchemaRecord {
    /// Derives the physical wire profile regardless of schema kind.
    pub fn profile(&self) -> EdgePayloadProfile {
        match self {
            Self::UnnamedProfile { profile } => profile.clone(),
            Self::InlineScalar { scalar_type, .. } => scalar_type.edge_payload_profile(),
        }
    }

    pub fn is_inline_scalar(&self) -> bool {
        matches!(self, Self::InlineScalar { .. })
    }
}

const SCHEMA_RECORD_VERSION: u8 = 1;

impl ic_stable_structures::Storable for EdgePayloadSchemaRecord {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Bounded {
            max_size: 1024,
            is_fixed_size: false,
        };

    fn to_bytes(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        out.push(SCHEMA_RECORD_VERSION);
        out.extend_from_slice(
            &candid::encode_one(&self)
                .expect("EdgePayloadSchemaRecord candid encode should not fail"),
        );
        out
    }

    fn from_bytes(bytes: std::borrow::Cow<'_, [u8]>) -> Self {
        let slice = bytes.as_ref();
        assert!(
            slice.first() == Some(&SCHEMA_RECORD_VERSION),
            "EdgePayloadSchemaRecord version mismatch; existing stable data must be wiped"
        );
        candid::decode_one(&slice[1..])
            .expect("EdgePayloadSchemaRecord candid decode should not fail")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgePayloadProfileStoreError {
    InvalidCatalogLabel(EdgeLabelId),
    InvalidProfile(EdgePayloadProfileError),
    InlineSchemaConflict(String),
    UnnamedProfileConflict(String),
}

impl fmt::Display for EdgePayloadProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCatalogLabel(id) => {
                write!(
                    f,
                    "edge payload profiles require catalog edge label id {}",
                    id.raw()
                )
            }
            Self::InvalidProfile(e) => write!(f, "{e}"),
            Self::InlineSchemaConflict(msg) => write!(f, "{msg}"),
            Self::UnnamedProfileConflict(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for EdgePayloadProfileStoreError {}

pub struct EdgePayloadProfileStore<M: Memory> {
    inner: StableBTreeMap<GraphScopedIdKey<EdgeLabelId>, EdgePayloadSchemaRecord, M>,
}

impl<M: Memory> EdgePayloadProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
        }
    }

    pub fn get_record(
        &self,
        graph_id: GraphId,
        label: EdgeLabelId,
    ) -> Option<EdgePayloadSchemaRecord> {
        self.inner.get(&GraphScopedIdKey {
            graph_id,
            id: label,
        })
    }

    pub fn get_profile(&self, graph_id: GraphId, label: EdgeLabelId) -> EdgePayloadProfile {
        self.get_record(graph_id, label)
            .map(|record| record.profile())
            .unwrap_or_else(EdgePayloadProfile::no_payload)
    }

    fn insert_record(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        record: EdgePayloadSchemaRecord,
    ) {
        self.inner.insert(
            GraphScopedIdKey {
                graph_id,
                id: label,
            },
            record,
        );
    }

    pub fn insert_unnamed_profile_profile(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgePayloadProfileStoreError::InvalidCatalogLabel(label));
        }
        profile
            .validate()
            .map_err(EdgePayloadProfileStoreError::InvalidProfile)?;
        if let Some(existing) = self.get_record(graph_id, label)
            && existing.is_inline_scalar()
        {
            return Err(EdgePayloadProfileStoreError::InlineSchemaConflict(format!(
                "edge label {} has an inline scalar schema; admin profile setter cannot override it",
                label.raw()
            )));
        }
        self.insert_record(
            graph_id,
            label,
            EdgePayloadSchemaRecord::UnnamedProfile { profile },
        );
        Ok(())
    }

    pub fn insert_if_absent_no_payload(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        if self.get_record(graph_id, label).is_some() {
            return Ok(());
        }
        self.insert_record(
            graph_id,
            label,
            EdgePayloadSchemaRecord::UnnamedProfile {
                profile: EdgePayloadProfile::no_payload(),
            },
        );
        Ok(())
    }

    pub fn set_inline_scalar_schema(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        property_id: PropertyId,
        scalar_type: InlineScalarType,
    ) -> Result<(), EdgePayloadProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgePayloadProfileStoreError::InvalidCatalogLabel(label));
        }
        let profile = scalar_type.edge_payload_profile();
        profile
            .validate()
            .map_err(EdgePayloadProfileStoreError::InvalidProfile)?;

        if let Some(existing) = self.get_record(graph_id, label) {
            match existing {
                EdgePayloadSchemaRecord::InlineScalar {
                    property_id: existing_pid,
                    scalar_type: existing_st,
                    ..
                } => {
                    if existing_pid == property_id && existing_st == scalar_type {
                        return Ok(());
                    }
                    return Err(EdgePayloadProfileStoreError::InlineSchemaConflict(format!(
                        "edge label {} already has a different inline scalar schema",
                        label.raw()
                    )));
                }
                EdgePayloadSchemaRecord::UnnamedProfile { profile } => {
                    if profile != EdgePayloadProfile::no_payload() {
                        return Err(EdgePayloadProfileStoreError::UnnamedProfileConflict(
                            format!(
                                "edge label {} has a legacy unnamed payload profile; install inline schema before the legacy profile",
                                label.raw()
                            ),
                        ));
                    }
                }
            }
        }

        self.insert_record(
            graph_id,
            label,
            EdgePayloadSchemaRecord::InlineScalar {
                property_id,
                scalar_type,
            },
        );
        Ok(())
    }

    pub fn label_ids_with_nonzero_payload(&self, graph_id: GraphId) -> Vec<EdgeLabelId> {
        self.inner
            .iter()
            .filter_map(|entry| {
                let key = entry.key();
                (key.graph_id == graph_id && entry.value().profile().required_byte_width() > 0)
                    .then_some(key.id)
            })
            .collect()
    }

    pub fn remove_graph(&mut self, graph_id: GraphId) {
        let mut keys = Vec::new();
        for entry in self.inner.iter() {
            if entry.key().graph_id == graph_id {
                keys.push(*entry.key());
            }
        }
        for key in keys {
            self.inner.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    use ic_stable_structures::{Storable, VectorMemory};
    use std::{cell::RefCell, rc::Rc};

    fn mem() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn insert_legacy_profile_rejects_invalid_profile_encoding() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(1);
        let profile = EdgePayloadProfile {
            byte_width: 4,
            encoding: EdgePayloadEncoding::WeightRawU16,
        };
        assert!(matches!(
            store.insert_unnamed_profile_profile(GraphId::from_raw(1), label, profile),
            Err(EdgePayloadProfileStoreError::InvalidProfile(
                EdgePayloadProfileError::WidthEncodingMismatch
            ))
        ));
    }

    #[test]
    fn legacy_profile_round_trips() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(2);
        let profile = EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightRawU16,
        };
        store
            .insert_unnamed_profile_profile(graph, label, profile.clone())
            .expect("insert");
        assert_eq!(store.get_profile(graph, label), profile);
        assert!(!store.get_record(graph, label).unwrap().is_inline_scalar());
    }

    #[test]
    fn inline_scalar_schema_round_trips() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(3);
        let property_id = PropertyId::from_raw(7);
        store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::F32)
            .expect("set inline");
        let record = store.get_record(graph, label).expect("record");
        assert_eq!(
            record,
            EdgePayloadSchemaRecord::InlineScalar {
                property_id,
                scalar_type: InlineScalarType::F32,
            }
        );
        assert_eq!(
            store.get_profile(graph, label),
            EdgePayloadProfile {
                byte_width: 4,
                encoding: EdgePayloadEncoding::F32,
            }
        );
    }

    #[test]
    fn inline_scalar_is_idempotent() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(4);
        let property_id = PropertyId::from_raw(7);
        store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::U16)
            .expect("first");
        store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::U16)
            .expect("second idempotent");
        assert_eq!(
            store.get_record(graph, label).unwrap().profile().byte_width,
            2
        );
    }

    #[test]
    fn inline_scalar_conflicts_on_different_scalar() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(5);
        let property_id = PropertyId::from_raw(7);
        store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::U16)
            .expect("first");
        let err = store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::I16)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgePayloadProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_scalar_conflicts_on_different_property() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(6);
        store
            .set_inline_scalar_schema(graph, label, PropertyId::from_raw(7), InlineScalarType::U16)
            .expect("first");
        let err = store
            .set_inline_scalar_schema(graph, label, PropertyId::from_raw(8), InlineScalarType::U16)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgePayloadProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_scalar_rejects_unnamed_profile_override() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(7);
        store
            .insert_unnamed_profile_profile(
                graph,
                label,
                EdgePayloadProfile {
                    byte_width: 2,
                    encoding: EdgePayloadEncoding::WeightRawU16,
                },
            )
            .expect("legacy");
        let err = store
            .set_inline_scalar_schema(graph, label, PropertyId::from_raw(9), InlineScalarType::U16)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgePayloadProfileStoreError::UnnamedProfileConflict(_)
        ));
    }

    #[test]
    fn unnamed_profile_cannot_override_inline_scalar() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(8);
        store
            .set_inline_scalar_schema(graph, label, PropertyId::from_raw(9), InlineScalarType::F32)
            .expect("inline");
        let err = store
            .insert_unnamed_profile_profile(
                graph,
                label,
                EdgePayloadProfile {
                    byte_width: 2,
                    encoding: EdgePayloadEncoding::WeightRawU16,
                },
            )
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgePayloadProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn default_no_payload_record_is_unnamed_profile() {
        let mut store = EdgePayloadProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(9);
        store
            .insert_if_absent_no_payload(graph, label)
            .expect("default");
        assert_eq!(
            store.get_record(graph, label),
            Some(EdgePayloadSchemaRecord::UnnamedProfile {
                profile: EdgePayloadProfile::no_payload(),
            })
        );
    }

    #[test]
    fn versioned_record_bytes_round_trip() {
        let record = EdgePayloadSchemaRecord::InlineScalar {
            property_id: PropertyId::from_raw(42),
            scalar_type: InlineScalarType::F32,
        };
        let bytes = record.clone().into_bytes();
        assert_eq!(bytes[0], SCHEMA_RECORD_VERSION);
        let decoded = EdgePayloadSchemaRecord::from_bytes(std::borrow::Cow::Owned(bytes));
        assert_eq!(decoded, record);
    }
}
