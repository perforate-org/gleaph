//! Router SSOT for `(GraphId, EdgeLabelId) → EdgeInlineValueSchemaRecord` (ADR 0008, ADR 0018).
//!
//! The stable value is a versioned `EdgeInlineValueSchemaRecord`. Development stable data must be wiped
//! when this format changes because backward compatibility is not maintained.
//! The physical `EdgeInlineValueProfile` consumed by Graph is always derived from the canonical record.

use std::fmt;

use gleaph_graph_kernel::entry::{
    EdgeInlineValueProfile, EdgeInlineValueProfileError, EdgeLabelId, GraphId, PropertyId,
};
use gleaph_graph_kernel::plan_exec::{ResolvedInlineSchema, ResolvedInlineStructField};
use gleaph_graph_kernel::scoped_name_catalog::GraphScopedIdKey;
use ic_stable_structures::{Memory, StableBTreeMap};
use std::cell::RefCell;

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

    /// The physical edge-inline-value profile this scalar declaration derives.
    pub const fn edge_inline_value_profile(self) -> EdgeInlineValueProfile {
        use gleaph_graph_kernel::entry::EdgeInlineValueEncoding::*;
        match self {
            Self::U8 => EdgeInlineValueProfile {
                byte_width: 1,
                encoding: RawU8,
            },
            Self::U16 => EdgeInlineValueProfile {
                byte_width: 2,
                encoding: RawU16,
            },
            Self::U32 => EdgeInlineValueProfile {
                byte_width: 4,
                encoding: RawU32,
            },
            Self::U64 => EdgeInlineValueProfile {
                byte_width: 8,
                encoding: RawU64,
            },
            Self::I8 => EdgeInlineValueProfile {
                byte_width: 1,
                encoding: RawI8,
            },
            Self::I16 => EdgeInlineValueProfile {
                byte_width: 2,
                encoding: RawI16,
            },
            Self::I32 => EdgeInlineValueProfile {
                byte_width: 4,
                encoding: RawI32,
            },
            Self::I64 => EdgeInlineValueProfile {
                byte_width: 8,
                encoding: RawI64,
            },
            Self::U128 => EdgeInlineValueProfile {
                byte_width: 16,
                encoding: RawU128,
            },
            Self::I128 => EdgeInlineValueProfile {
                byte_width: 16,
                encoding: RawI128,
            },
            Self::F16 => EdgeInlineValueProfile {
                byte_width: 2,
                encoding: F16,
            },
            Self::F32 => EdgeInlineValueProfile {
                byte_width: 4,
                encoding: F32,
            },
            Self::F64 => EdgeInlineValueProfile {
                byte_width: 8,
                encoding: F64,
            },
            Self::Fixed32 => EdgeInlineValueProfile {
                byte_width: 32,
                encoding: RawFixed32,
            },
            Self::Fixed64 => EdgeInlineValueProfile {
                byte_width: 64,
                encoding: RawFixed64,
            },
        }
    }
}

// Slice 24: fixed-size inline STRUCT bounds and helpers.

/// Maximum number of fields in a fixed-size inline edge STRUCT.
pub const MAX_INLINE_STRUCT_FIELDS: usize = 64;

/// Conservative execution-safe total byte width for an inline STRUCT.
/// A struct wider than this cannot be transported through existing federated expand paths
/// (`MAX_FEDERATED_EXPAND_INLINE_VALUE_BYTE_WIDTH`), so the schema commit rejects it fail-closed.
pub const MAX_INLINE_STRUCT_TOTAL_BYTES: u16 =
    gleaph_graph_kernel::federation::MAX_FEDERATED_EXPAND_INLINE_VALUE_BYTE_WIDTH;

/// Maximum encoded stable-record size for an inline STRUCT schema record.
/// Must fit inside the [`EdgeInlineValueSchemaRecord`] [`Storable::BOUND`] envelope.
pub const MAX_INLINE_STRUCT_RECORD_BYTES: usize = 1024;

/// Logical specification of one fixed-size inline edge STRUCT field.
///
/// The stable record stores only the logical declaration (`name`, `scalar_type`). Byte offsets and
/// widths are deterministically derived from the declaration order, so they are not persisted.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct InlineStructFieldSpec {
    pub name: String,
    pub scalar_type: InlineScalarType,
}

/// Router-owned canonical layout for one fixed-size inline edge STRUCT.
///
/// Fields are stored in declaration order with no padding. The total width and per-field byte
/// offsets/widths are derived from the logical `field_specs` using checked arithmetic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineStructLayout {
    field_specs: Vec<InlineStructFieldSpec>,
    fields: Vec<InlineStructField>,
    total_byte_width: u16,
}

impl InlineStructLayout {
    /// Declaration-ordered logical field specs; the stable record persists only these.
    pub fn field_specs(&self) -> &[InlineStructFieldSpec] {
        &self.field_specs
    }

    /// Derived per-field byte offsets and widths in declaration order.
    #[allow(dead_code)] // used in tests and canbench builds
    pub fn fields(&self) -> &[InlineStructField] {
        &self.fields
    }

    /// Derived total byte width of the packed struct.
    #[allow(dead_code)] // used in tests and canbench builds
    pub fn total_byte_width(&self) -> u16 {
        self.total_byte_width
    }
}

/// Materialized view of one struct field, derived from `InlineStructFieldSpec`.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct InlineStructField {
    pub name: String,
    pub scalar_type: InlineScalarType,
    pub byte_offset: u16,
    pub byte_width: u16,
}

/// Validation error for a proposed inline STRUCT layout.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InlineStructLayoutError {
    #[error("inline struct must have at least one field")]
    Empty,
    #[error("inline struct field count {0} exceeds maximum {MAX_INLINE_STRUCT_FIELDS}")]
    TooManyFields(usize),
    #[error("duplicate inline struct field name `{0}`")]
    DuplicateField(String),
    #[error("inline struct total byte width {0} exceeds maximum {MAX_INLINE_STRUCT_TOTAL_BYTES}")]
    TotalWidthTooLarge(u16),
    #[error("inline struct total byte width arithmetic overflow")]
    WidthOverflow,
    #[error(
        "inline struct stable record encoded size {0} exceeds maximum {MAX_INLINE_STRUCT_RECORD_BYTES}"
    )]
    RecordTooLarge(usize),
}

impl InlineStructLayout {
    /// Validates and derives a canonical declaration-ordered packed layout.
    pub fn from_fields(
        declared: Vec<(String, InlineScalarType)>,
    ) -> Result<Self, InlineStructLayoutError> {
        if declared.is_empty() {
            return Err(InlineStructLayoutError::Empty);
        }
        if declared.len() > MAX_INLINE_STRUCT_FIELDS {
            return Err(InlineStructLayoutError::TooManyFields(declared.len()));
        }

        let mut seen = std::collections::HashSet::with_capacity(declared.len());
        let mut field_specs = Vec::with_capacity(declared.len());
        let mut fields = Vec::with_capacity(declared.len());
        let mut offset: u16 = 0;

        for (name, scalar_type) in declared {
            if !seen.insert(name.clone()) {
                return Err(InlineStructLayoutError::DuplicateField(name));
            }
            let byte_width = scalar_type.edge_inline_value_profile().byte_width;
            let byte_offset = offset;
            // Checked arithmetic: under current bounds (64 fields x 64 bytes = 4096) u16 overflow
            // is mathematically unreachable, but a future bound change must not silently saturate.
            offset = offset
                .checked_add(byte_width)
                .ok_or(InlineStructLayoutError::WidthOverflow)?;
            fields.push(InlineStructField {
                name: name.clone(),
                scalar_type,
                byte_offset,
                byte_width,
            });
            field_specs.push(InlineStructFieldSpec { name, scalar_type });
        }

        if offset > MAX_INLINE_STRUCT_TOTAL_BYTES {
            return Err(InlineStructLayoutError::TotalWidthTooLarge(offset));
        }

        Ok(Self {
            field_specs,
            fields,
            total_byte_width: offset,
        })
    }

    /// Returns `true` if the canonical stable record for this struct fits in the BOUND envelope.
    ///
    /// `PropertyId` is a fixed-width LE u32, so encoding with `PropertyId::from_raw(u32::MAX)` is
    /// an exact size representative, not merely an upper bound.
    pub fn fits_record_bound(&self, max_record_bytes: usize) -> bool {
        let record = EdgeInlineValueSchemaRecord::InlineStruct {
            property_id: PropertyId::from_raw(u32::MAX),
            field_specs: self.field_specs.clone(),
        };
        ic_stable_structures::Storable::into_bytes(record).len() <= max_record_bytes
    }

    /// Encodes the canonical stable record with a representative `PropertyId` placeholder and
    /// returns its exact byte length. `PropertyId` is a fixed-width LE u32, so this is the real
    /// encoded size for any actual property id.
    pub fn record_size_upper_bound(&self) -> usize {
        let record = EdgeInlineValueSchemaRecord::InlineStruct {
            property_id: PropertyId::from_raw(u32::MAX),
            field_specs: self.field_specs.clone(),
        };
        ic_stable_structures::Storable::into_bytes(record).len()
    }

    pub fn profile(&self) -> EdgeInlineValueProfile {
        EdgeInlineValueProfile::opaque_bytes(self.total_byte_width)
    }

    /// Same as `from_fields` but also checks the stable-record size envelope.
    pub fn from_fields_with_record_bound(
        declared: Vec<(String, InlineScalarType)>,
        max_record_bytes: usize,
    ) -> Result<Self, InlineStructLayoutError> {
        let layout = Self::from_fields(declared)?;
        if !layout.fits_record_bound(max_record_bytes) {
            return Err(InlineStructLayoutError::RecordTooLarge(
                layout.record_size_upper_bound(),
            ));
        }
        Ok(layout)
    }
}

/// Canonical Router-owned record for the edge-label payload schema.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub enum EdgeInlineValueSchemaRecord {
    /// Unnamed profile installed through the admin API. Carries no logical property identity.
    UnnamedProfile { profile: EdgeInlineValueProfile },
    /// Slice 20: one fixed-width scalar inline property per edge label.
    InlineScalar {
        property_id: PropertyId,
        scalar_type: InlineScalarType,
    },
    /// Slice 24: one fixed-size inline STRUCT property per edge label.
    /// Stable storage keeps only the canonical logical field specs; offsets, widths, and the
    /// physical profile are deterministically derived.
    InlineStruct {
        property_id: PropertyId,
        field_specs: Vec<InlineStructFieldSpec>,
    },
}

impl EdgeInlineValueSchemaRecord {
    /// Derives the physical wire profile regardless of schema kind.
    pub fn profile(&self) -> EdgeInlineValueProfile {
        match self {
            Self::UnnamedProfile { profile } => profile.clone(),
            Self::InlineScalar { scalar_type, .. } => scalar_type.edge_inline_value_profile(),
            Self::InlineStruct { field_specs, .. } => {
                let layout = InlineStructLayout::from_fields(
                    field_specs
                        .iter()
                        .map(|f| (f.name.clone(), f.scalar_type))
                        .collect(),
                )
                .expect("decoded InlineStruct field_specs must re-derive a valid layout");
                layout.profile()
            }
        }
    }

    pub fn is_inline_scalar(&self) -> bool {
        matches!(self, Self::InlineScalar { .. })
    }

    /// True for any named inline schema (scalar or struct).
    pub fn is_named_inline(&self) -> bool {
        matches!(self, Self::InlineScalar { .. } | Self::InlineStruct { .. })
    }

    pub fn inline_property_id(&self) -> Option<PropertyId> {
        match self {
            Self::InlineScalar { property_id, .. } | Self::InlineStruct { property_id, .. } => {
                Some(*property_id)
            }
            Self::UnnamedProfile { .. } => None,
        }
    }
}

const SCHEMA_RECORD_VERSION: u8 = 2;

impl ic_stable_structures::Storable for EdgeInlineValueSchemaRecord {
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
                .expect("EdgeInlineValueSchemaRecord candid encode should not fail"),
        );
        out
    }

    fn from_bytes(bytes: std::borrow::Cow<'_, [u8]>) -> Self {
        let slice = bytes.as_ref();
        assert!(
            slice.first() == Some(&SCHEMA_RECORD_VERSION),
            "EdgeInlineValueSchemaRecord version mismatch; existing stable data must be wiped"
        );
        let record: Self = candid::decode_one(&slice[1..])
            .expect("EdgeInlineValueSchemaRecord candid decode should not fail");
        // Fail-closed validation: a decoded InlineStruct must re-derive to the same canonical layout.
        if let Self::InlineStruct {
            property_id: _,
            field_specs,
        } = &record
        {
            let declared: Vec<(String, InlineScalarType)> = field_specs
                .iter()
                .map(|f| (f.name.clone(), f.scalar_type))
                .collect();
            let layout = InlineStructLayout::from_fields(declared)
                .expect("decoded InlineStruct field_specs must describe a valid layout");
            assert!(
                layout.fits_record_bound(MAX_INLINE_STRUCT_RECORD_BYTES),
                "decoded InlineStruct stable record exceeds the allowed envelope"
            );
        }
        record
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeInlineValueProfileStoreError {
    InvalidCatalogLabel(EdgeLabelId),
    InvalidProfile(EdgeInlineValueProfileError),
    InlineSchemaConflict(String),
    UnnamedProfileConflict(String),
    /// Slice 24: a proposed inline struct layout failed canonical re-derivation or record-bound
    /// validation at the stable-store write boundary.
    LayoutInvalid(String),
}

impl fmt::Display for EdgeInlineValueProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCatalogLabel(id) => {
                write!(
                    f,
                    "edge inline value profiles require catalog edge label id {}",
                    id.raw()
                )
            }
            Self::InvalidProfile(e) => write!(f, "{e}"),
            Self::InlineSchemaConflict(msg) => write!(f, "{msg}"),
            Self::UnnamedProfileConflict(msg) => write!(f, "{msg}"),
            Self::LayoutInvalid(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for EdgeInlineValueProfileStoreError {}

pub struct EdgeInlineValueProfileStore<M: Memory> {
    inner: StableBTreeMap<GraphScopedIdKey<EdgeLabelId>, EdgeInlineValueSchemaRecord, M>,
    /// Heap-only last-value cache. Stable memory remains the SSOT; this is rebuilt empty after
    /// upgrade and is invalidated or replaced by every store-owned write.
    last_record: RefCell<Option<CachedSchemaRecord>>,
}

struct CachedSchemaRecord {
    key: GraphScopedIdKey<EdgeLabelId>,
    record: Option<EdgeInlineValueSchemaRecord>,
}

impl<M: Memory> EdgeInlineValueProfileStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            inner: StableBTreeMap::init(memory),
            last_record: RefCell::new(None),
        }
    }

    pub fn get_record(
        &self,
        graph_id: GraphId,
        label: EdgeLabelId,
    ) -> Option<EdgeInlineValueSchemaRecord> {
        let key = GraphScopedIdKey {
            graph_id,
            id: label,
        };
        if let Some(cached) = self.last_record.borrow().as_ref()
            && cached.key == key
        {
            return cached.record.clone();
        }
        let record = self.inner.get(&key);
        *self.last_record.borrow_mut() = Some(CachedSchemaRecord {
            key,
            record: record.clone(),
        });
        record
    }

    /// Physical profile accessor used by tests and by callers that only need the byte width/encoding.
    #[allow(dead_code)]
    pub fn get_profile(&self, graph_id: GraphId, label: EdgeLabelId) -> EdgeInlineValueProfile {
        self.get_record(graph_id, label)
            .map(|record| record.profile())
            .unwrap_or_else(EdgeInlineValueProfile::no_inline_value)
    }

    /// Router-derived projection of the canonical record into the physical wire shape Graph needs.
    ///
    /// For `InlineStruct`, this derives per-field byte offsets and scalar profiles from the canonical
    /// `InlineStructLayout` and returns a struct schema. For `InlineScalar`, it returns the scalar
    /// schema. For `UnnamedProfile`, it returns `None`. The projection is never persisted.
    pub fn get_profile_and_inline_schema(
        &self,
        graph_id: GraphId,
        label: EdgeLabelId,
    ) -> (EdgeInlineValueProfile, Option<ResolvedInlineSchema>) {
        let record = self.get_record(graph_id, label);
        let profile = record
            .as_ref()
            .map(EdgeInlineValueSchemaRecord::profile)
            .unwrap_or_else(EdgeInlineValueProfile::no_inline_value);
        let schema = record.and_then(|record| match record {
            EdgeInlineValueSchemaRecord::UnnamedProfile { .. } => None,
            EdgeInlineValueSchemaRecord::InlineScalar { property_id, .. } => {
                Some(ResolvedInlineSchema::Scalar { property_id })
            }
            EdgeInlineValueSchemaRecord::InlineStruct {
                property_id,
                field_specs,
            } => {
                let layout = InlineStructLayout::from_fields(
                    field_specs
                        .iter()
                        .map(|f| (f.name.clone(), f.scalar_type))
                        .collect(),
                )
                .expect("decoded InlineStruct field_specs must re-derive a valid layout");
                let fields = layout
                    .fields()
                    .iter()
                    .map(|f| ResolvedInlineStructField {
                        name: f.name.clone(),
                        byte_offset: f.byte_offset,
                        profile: f.scalar_type.edge_inline_value_profile(),
                    })
                    .collect();
                Some(ResolvedInlineSchema::Struct {
                    property_id,
                    fields,
                })
            }
        });
        (profile, schema)
    }

    fn insert_record(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        record: EdgeInlineValueSchemaRecord,
    ) {
        let key = GraphScopedIdKey {
            graph_id,
            id: label,
        };
        self.inner.insert(key, record.clone());
        *self.last_record.borrow_mut() = Some(CachedSchemaRecord {
            key,
            record: Some(record),
        });
    }

    pub fn insert_unnamed_profile_profile(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        profile: EdgeInlineValueProfile,
    ) -> Result<(), EdgeInlineValueProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgeInlineValueProfileStoreError::InvalidCatalogLabel(label));
        }
        profile
            .validate()
            .map_err(EdgeInlineValueProfileStoreError::InvalidProfile)?;
        if let Some(existing) = self.get_record(graph_id, label)
            && existing.is_named_inline()
        {
            return Err(EdgeInlineValueProfileStoreError::InlineSchemaConflict(
                format!(
                    "edge label {} has an inline schema; admin profile setter cannot override it",
                    label.raw()
                ),
            ));
        }
        self.insert_record(
            graph_id,
            label,
            EdgeInlineValueSchemaRecord::UnnamedProfile { profile },
        );
        Ok(())
    }

    pub fn insert_if_absent_no_inline_value(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
    ) -> Result<(), EdgeInlineValueProfileStoreError> {
        if self.get_record(graph_id, label).is_some() {
            return Ok(());
        }
        self.insert_record(
            graph_id,
            label,
            EdgeInlineValueSchemaRecord::UnnamedProfile {
                profile: EdgeInlineValueProfile::no_inline_value(),
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
    ) -> Result<(), EdgeInlineValueProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgeInlineValueProfileStoreError::InvalidCatalogLabel(label));
        }
        let profile = scalar_type.edge_inline_value_profile();
        profile
            .validate()
            .map_err(EdgeInlineValueProfileStoreError::InvalidProfile)?;

        if let Some(existing) = self.get_record(graph_id, label) {
            match existing {
                EdgeInlineValueSchemaRecord::InlineScalar {
                    property_id: existing_pid,
                    scalar_type: existing_st,
                    ..
                } => {
                    if existing_pid == property_id && existing_st == scalar_type {
                        return Ok(());
                    }
                    return Err(EdgeInlineValueProfileStoreError::InlineSchemaConflict(
                        format!(
                            "edge label {} already has a different inline scalar schema",
                            label.raw()
                        ),
                    ));
                }
                EdgeInlineValueSchemaRecord::InlineStruct { .. } => {
                    return Err(EdgeInlineValueProfileStoreError::InlineSchemaConflict(
                        format!(
                            "edge label {} already has an inline struct schema",
                            label.raw()
                        ),
                    ));
                }
                EdgeInlineValueSchemaRecord::UnnamedProfile { profile } => {
                    if profile != EdgeInlineValueProfile::no_inline_value() {
                        return Err(EdgeInlineValueProfileStoreError::UnnamedProfileConflict(
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
            EdgeInlineValueSchemaRecord::InlineScalar {
                property_id,
                scalar_type,
            },
        );
        Ok(())
    }

    /// Slice 24: install the canonical fixed-size inline STRUCT schema for a label.
    ///
    /// The caller supplies a validated `InlineStructLayout`, but the setter re-derives the canonical
    /// layout from the logical field specs and re-checks the stable-record envelope before writing.
    /// This keeps the stable record as the single source of truth and prevents a publicly
    /// constructible layout from smuggling inconsistent derived state or an oversized record into
    /// storage.
    pub fn set_inline_struct_schema(
        &mut self,
        graph_id: GraphId,
        label: EdgeLabelId,
        property_id: PropertyId,
        layout: InlineStructLayout,
    ) -> Result<(), EdgeInlineValueProfileStoreError> {
        if !label.is_catalog_allocatable() {
            return Err(EdgeInlineValueProfileStoreError::InvalidCatalogLabel(label));
        }

        // Re-derive and record-bound-check from the canonical logical specs at the owning stable
        // boundary. The layout argument may have been constructed without the record bound, so this
        // is the fail-closed gate before any insert.
        let validated = InlineStructLayout::from_fields_with_record_bound(
            layout
                .field_specs()
                .iter()
                .map(|f| (f.name.clone(), f.scalar_type))
                .collect(),
            MAX_INLINE_STRUCT_RECORD_BYTES,
        )
        .map_err(|e| EdgeInlineValueProfileStoreError::LayoutInvalid(e.to_string()))?;

        let profile = validated.profile();
        profile
            .validate()
            .map_err(EdgeInlineValueProfileStoreError::InvalidProfile)?;

        if let Some(existing) = self.get_record(graph_id, label) {
            match existing {
                EdgeInlineValueSchemaRecord::InlineStruct {
                    property_id: existing_pid,
                    field_specs: ref existing_specs,
                } => {
                    if existing_pid == property_id && existing_specs == validated.field_specs() {
                        return Ok(());
                    }
                    return Err(EdgeInlineValueProfileStoreError::InlineSchemaConflict(
                        format!(
                            "edge label {} already has a different inline struct schema",
                            label.raw()
                        ),
                    ));
                }
                EdgeInlineValueSchemaRecord::InlineScalar { .. } => {
                    return Err(EdgeInlineValueProfileStoreError::InlineSchemaConflict(
                        format!(
                            "edge label {} already has an inline scalar schema",
                            label.raw()
                        ),
                    ));
                }
                EdgeInlineValueSchemaRecord::UnnamedProfile { profile } => {
                    if profile != EdgeInlineValueProfile::no_inline_value() {
                        return Err(EdgeInlineValueProfileStoreError::UnnamedProfileConflict(
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
            EdgeInlineValueSchemaRecord::InlineStruct {
                property_id,
                field_specs: validated.field_specs().to_vec(),
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
        if self
            .last_record
            .borrow()
            .as_ref()
            .is_some_and(|cached| cached.key.graph_id == graph_id)
        {
            self.last_record.borrow_mut().take();
        }
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
    use gleaph_graph_kernel::entry::{EdgeInlineValueEncoding, EdgeInlineValueProfile};
    use ic_stable_structures::{Storable, VectorMemory};
    use std::{cell::RefCell, rc::Rc};

    fn mem() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn insert_legacy_profile_rejects_invalid_profile_encoding() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let label = EdgeLabelId::from_raw(1);
        let profile = EdgeInlineValueProfile {
            byte_width: 4,
            encoding: EdgeInlineValueEncoding::WeightRawU16,
        };
        assert!(matches!(
            store.insert_unnamed_profile_profile(GraphId::from_raw(1), label, profile),
            Err(EdgeInlineValueProfileStoreError::InvalidProfile(
                EdgeInlineValueProfileError::WidthEncodingMismatch
            ))
        ));
    }

    #[test]
    fn legacy_profile_round_trips() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(2);
        let profile = EdgeInlineValueProfile {
            byte_width: 2,
            encoding: EdgeInlineValueEncoding::WeightRawU16,
        };
        store
            .insert_unnamed_profile_profile(graph, label, profile.clone())
            .expect("insert");
        assert_eq!(store.get_profile(graph, label), profile);
        assert!(!store.get_record(graph, label).unwrap().is_inline_scalar());
    }

    #[test]
    fn inline_scalar_schema_round_trips() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(3);
        let property_id = PropertyId::from_raw(7);
        store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::F32)
            .expect("set inline");
        let record = store.get_record(graph, label).expect("record");
        assert_eq!(
            record,
            EdgeInlineValueSchemaRecord::InlineScalar {
                property_id,
                scalar_type: InlineScalarType::F32,
            }
        );
        assert_eq!(
            store.get_profile(graph, label),
            EdgeInlineValueProfile {
                byte_width: 4,
                encoding: EdgeInlineValueEncoding::F32,
            }
        );
    }

    #[test]
    fn last_record_cache_is_cleared_when_graph_is_removed() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(11);
        let label = EdgeLabelId::from_raw(12);
        store
            .set_inline_scalar_schema(
                graph,
                label,
                PropertyId::from_raw(13),
                InlineScalarType::F32,
            )
            .expect("set inline");
        assert!(store.get_record(graph, label).is_some());

        store.remove_graph(graph);

        assert!(store.get_record(graph, label).is_none());
    }

    #[test]
    fn inline_scalar_is_idempotent() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
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
        let mut store = EdgeInlineValueProfileStore::init(mem());
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
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_scalar_conflicts_on_different_property() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
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
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_scalar_rejects_unnamed_profile_override() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(7);
        store
            .insert_unnamed_profile_profile(
                graph,
                label,
                EdgeInlineValueProfile {
                    byte_width: 2,
                    encoding: EdgeInlineValueEncoding::WeightRawU16,
                },
            )
            .expect("legacy");
        let err = store
            .set_inline_scalar_schema(graph, label, PropertyId::from_raw(9), InlineScalarType::U16)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::UnnamedProfileConflict(_)
        ));
    }

    #[test]
    fn unnamed_profile_cannot_override_inline_scalar() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(8);
        store
            .set_inline_scalar_schema(graph, label, PropertyId::from_raw(9), InlineScalarType::F32)
            .expect("inline");
        let err = store
            .insert_unnamed_profile_profile(
                graph,
                label,
                EdgeInlineValueProfile {
                    byte_width: 2,
                    encoding: EdgeInlineValueEncoding::WeightRawU16,
                },
            )
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn default_no_inline_value_record_is_unnamed_profile() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(9);
        store
            .insert_if_absent_no_inline_value(graph, label)
            .expect("default");
        assert_eq!(
            store.get_record(graph, label),
            Some(EdgeInlineValueSchemaRecord::UnnamedProfile {
                profile: EdgeInlineValueProfile::no_inline_value(),
            })
        );
    }

    #[test]
    fn versioned_record_bytes_round_trip() {
        let record = EdgeInlineValueSchemaRecord::InlineScalar {
            property_id: PropertyId::from_raw(42),
            scalar_type: InlineScalarType::F32,
        };
        let bytes = record.clone().into_bytes();
        assert_eq!(bytes[0], SCHEMA_RECORD_VERSION);
        let decoded = EdgeInlineValueSchemaRecord::from_bytes(std::borrow::Cow::Owned(bytes));
        assert_eq!(decoded, record);
    }
    #[test]
    fn inline_property_id_accessor_returns_property_id() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(3);
        let property_id = PropertyId::from_raw(7);
        store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::F32)
            .expect("set inline");
        let record = store.get_record(graph, label).expect("record");
        assert_eq!(record.inline_property_id(), Some(property_id));
        let (profile, inline_schema) = store.get_profile_and_inline_schema(graph, label);
        assert_eq!(profile.byte_width, 4);
        assert!(matches!(
            inline_schema,
            Some(ResolvedInlineSchema::Scalar { property_id: pid }) if pid == property_id
        ));
    }

    #[test]
    fn unnamed_profile_inline_property_id_is_none() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(2);
        store
            .insert_unnamed_profile_profile(
                graph,
                label,
                EdgeInlineValueProfile {
                    byte_width: 2,
                    encoding: EdgeInlineValueEncoding::WeightRawU16,
                },
            )
            .expect("legacy");
        assert_eq!(
            store.get_record(graph, label).unwrap().inline_property_id(),
            None
        );
        let (profile, inline_schema) = store.get_profile_and_inline_schema(graph, label);
        assert_eq!(profile.byte_width, 2);
        assert_eq!(inline_schema, None);
    }

    #[test]
    fn inline_struct_layout_derives_offsets_and_total_width() {
        let layout = InlineStructLayout::from_fields(vec![
            ("score".into(), InlineScalarType::F32),
            ("confidence".into(), InlineScalarType::F32),
            ("updated_at".into(), InlineScalarType::U64),
        ])
        .expect("layout");
        assert_eq!(layout.total_byte_width(), 16);
        assert_eq!(layout.fields().len(), 3);
        assert_eq!(layout.fields()[0].byte_offset, 0);
        assert_eq!(layout.fields()[0].byte_width, 4);
        assert_eq!(layout.fields()[1].byte_offset, 4);
        assert_eq!(layout.fields()[1].byte_width, 4);
        assert_eq!(layout.fields()[2].byte_offset, 8);
        assert_eq!(layout.fields()[2].byte_width, 8);
        assert_eq!(layout.profile(), EdgeInlineValueProfile::opaque_bytes(16));
    }

    #[test]
    fn inline_struct_layout_rejects_empty() {
        assert!(matches!(
            InlineStructLayout::from_fields(vec![]),
            Err(InlineStructLayoutError::Empty)
        ));
    }

    #[test]
    fn inline_struct_layout_rejects_duplicate_field() {
        let err = InlineStructLayout::from_fields(vec![
            ("x".into(), InlineScalarType::U8),
            ("x".into(), InlineScalarType::U16),
        ])
        .unwrap_err();
        assert!(matches!(err, InlineStructLayoutError::DuplicateField(_)));
    }

    #[test]
    fn inline_struct_layout_accepts_max_total_width() {
        // 64 FIXED64 fields exactly reach the 4096-byte execution-safe bound and are accepted.
        let mut fields = Vec::new();
        for i in 0..64 {
            fields.push((format!("f{i}"), InlineScalarType::Fixed64));
        }
        let layout = InlineStructLayout::from_fields(fields).expect("64*64=4096 fits in u16");
        assert_eq!(layout.total_byte_width(), 4096);
        assert_eq!(layout.total_byte_width(), MAX_INLINE_STRUCT_TOTAL_BYTES);
    }

    #[test]
    fn inline_struct_layout_rejects_max_field_count() {
        // With MAX_INLINE_STRUCT_FIELDS=64, the 65th field is rejected before any width/overflow
        // arithmetic. Under current bounds 64*64=4096 fits in u16, so `WidthOverflow` and
        // `TotalWidthTooLarge` are mathematically unreachable; they remain as defensive guards for
        // future bound changes.
        let mut fields = Vec::new();
        for i in 0..65 {
            fields.push((format!("f{i}"), InlineScalarType::Fixed64));
        }
        let err = InlineStructLayout::from_fields(fields).unwrap_err();
        assert_eq!(
            err,
            InlineStructLayoutError::TooManyFields(65),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn inline_struct_record_size_is_exact_for_any_property_id() {
        // PropertyId is a fixed-width LE u32, so the encoded record size is the same for every
        // possible property id. The preflight is therefore exact, not merely an upper bound.
        let mut declared = Vec::new();
        for i in 0..30 {
            declared.push((format!("field_{:03}", i), InlineScalarType::U64));
        }
        let layout = InlineStructLayout::from_fields(declared.clone()).expect("layout ok");
        let small_id_size = {
            let record = EdgeInlineValueSchemaRecord::InlineStruct {
                property_id: PropertyId::from_raw(0),
                field_specs: layout.field_specs().to_vec(),
            };
            ic_stable_structures::Storable::into_bytes(record).len()
        };
        let max_id_size = layout.record_size_upper_bound();
        assert_eq!(
            small_id_size, max_id_size,
            "PropertyId fixed-width encoding makes the record-size preflight exact"
        );
        // Sanity: a struct with long field names still fits in the 1024-byte envelope.
        assert!(layout.fits_record_bound(MAX_INLINE_STRUCT_RECORD_BYTES));
    }

    #[test]
    fn inline_struct_record_size_rejected_before_catalog_mutation() {
        // Build a struct whose encoded record exceeds the 1024-byte envelope.
        let mut declared = Vec::new();
        for i in 0..64 {
            // Each name ~60 bytes x 64 fields plus scalar overhead exceeds 1024.
            declared.push((
                format!("very_long_field_name_to_inflate_record_size_{:03}", i),
                InlineScalarType::U8,
            ));
        }
        let err = InlineStructLayout::from_fields_with_record_bound(
            declared,
            MAX_INLINE_STRUCT_RECORD_BYTES,
        )
        .unwrap_err();
        assert!(
            matches!(err, InlineStructLayoutError::RecordTooLarge(_)),
            "expected RecordTooLarge, got {err:?}"
        );
    }

    #[test]
    fn inline_struct_schema_round_trips() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(10);
        let property_id = PropertyId::from_raw(20);
        let layout = InlineStructLayout::from_fields(vec![
            ("score".into(), InlineScalarType::F32),
            ("confidence".into(), InlineScalarType::F32),
            ("updated_at".into(), InlineScalarType::U64),
        ])
        .expect("layout");
        store
            .set_inline_struct_schema(graph, label, property_id, layout.clone())
            .expect("set inline struct");
        let record = store.get_record(graph, label).expect("record");
        assert_eq!(
            record,
            EdgeInlineValueSchemaRecord::InlineStruct {
                property_id,
                field_specs: layout.field_specs().to_vec(),
            }
        );
        assert_eq!(
            store.get_profile(graph, label),
            EdgeInlineValueProfile::opaque_bytes(16)
        );
        assert_eq!(record.inline_property_id(), Some(property_id));
    }

    #[test]
    fn inline_struct_schema_is_idempotent() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(11);
        let property_id = PropertyId::from_raw(21);
        let layout = InlineStructLayout::from_fields(vec![
            ("a".into(), InlineScalarType::U8),
            ("b".into(), InlineScalarType::I16),
        ])
        .expect("layout");
        store
            .set_inline_struct_schema(graph, label, property_id, layout.clone())
            .expect("first");
        store
            .set_inline_struct_schema(graph, label, property_id, layout)
            .expect("idempotent");
    }

    #[test]
    fn inline_struct_conflicts_on_different_field_order() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(12);
        let property_id = PropertyId::from_raw(22);
        let first = InlineStructLayout::from_fields(vec![
            ("a".into(), InlineScalarType::U8),
            ("b".into(), InlineScalarType::U16),
        ])
        .expect("first layout");
        let reordered = InlineStructLayout::from_fields(vec![
            ("b".into(), InlineScalarType::U16),
            ("a".into(), InlineScalarType::U8),
        ])
        .expect("reordered layout");
        store
            .set_inline_struct_schema(graph, label, property_id, first)
            .expect("first");
        let err = store
            .set_inline_struct_schema(graph, label, property_id, reordered)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_struct_conflicts_on_different_field_type() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(13);
        let property_id = PropertyId::from_raw(23);
        let first = InlineStructLayout::from_fields(vec![("a".into(), InlineScalarType::U8)])
            .expect("layout");
        let second = InlineStructLayout::from_fields(vec![("a".into(), InlineScalarType::I8)])
            .expect("layout");
        store
            .set_inline_struct_schema(graph, label, property_id, first)
            .expect("first");
        let err = store
            .set_inline_struct_schema(graph, label, property_id, second)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_scalar_rejects_existing_inline_struct() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(14);
        let property_id = PropertyId::from_raw(24);
        let layout = InlineStructLayout::from_fields(vec![("a".into(), InlineScalarType::U8)])
            .expect("layout");
        store
            .set_inline_struct_schema(graph, label, property_id, layout)
            .expect("struct");
        let err = store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::U8)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_struct_rejects_existing_inline_scalar() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(15);
        let property_id = PropertyId::from_raw(25);
        store
            .set_inline_scalar_schema(graph, label, property_id, InlineScalarType::U8)
            .expect("scalar");
        let layout = InlineStructLayout::from_fields(vec![("a".into(), InlineScalarType::U8)])
            .expect("layout");
        let err = store
            .set_inline_struct_schema(graph, label, property_id, layout)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn inline_struct_rejects_legacy_unnamed_profile() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(16);
        store
            .insert_unnamed_profile_profile(
                graph,
                label,
                EdgeInlineValueProfile {
                    byte_width: 2,
                    encoding: EdgeInlineValueEncoding::WeightRawU16,
                },
            )
            .expect("legacy");
        let layout = InlineStructLayout::from_fields(vec![("a".into(), InlineScalarType::U8)])
            .expect("layout");
        let err = store
            .set_inline_struct_schema(graph, label, PropertyId::from_raw(26), layout)
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::UnnamedProfileConflict(_)
        ));
    }

    #[test]
    fn unnamed_profile_cannot_override_inline_struct() {
        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(17);
        let layout = InlineStructLayout::from_fields(vec![("a".into(), InlineScalarType::U8)])
            .expect("layout");
        store
            .set_inline_struct_schema(graph, label, PropertyId::from_raw(27), layout)
            .expect("struct");
        let err = store
            .insert_unnamed_profile_profile(
                graph,
                label,
                EdgeInlineValueProfile {
                    byte_width: 2,
                    encoding: EdgeInlineValueEncoding::WeightRawU16,
                },
            )
            .expect_err("conflict");
        assert!(matches!(
            err,
            EdgeInlineValueProfileStoreError::InlineSchemaConflict(_)
        ));
    }

    #[test]
    fn versioned_inline_struct_record_bytes_round_trip() {
        let layout = InlineStructLayout::from_fields(vec![
            ("score".into(), InlineScalarType::F32),
            ("confidence".into(), InlineScalarType::F32),
        ])
        .expect("layout");
        let record = EdgeInlineValueSchemaRecord::InlineStruct {
            property_id: PropertyId::from_raw(42),
            field_specs: layout.field_specs().to_vec(),
        };
        let bytes = record.clone().into_bytes();
        assert_eq!(bytes[0], SCHEMA_RECORD_VERSION);
        let decoded = EdgeInlineValueSchemaRecord::from_bytes(std::borrow::Cow::Owned(bytes));
        assert_eq!(decoded, record);
    }

    #[test]
    fn inline_struct_setter_rejects_oversized_record_at_stable_boundary() {
        // A layout built without the record-bound check can describe logical field specs whose
        // encoded stable record exceeds the 1024-byte envelope. The stable setter must re-derive
        // and bound-check before inserting, so the record cannot be smuggled into storage.
        let mut declared = Vec::new();
        for i in 0..64 {
            declared.push((
                format!("very_long_field_name_to_inflate_record_size_{:03}", i),
                InlineScalarType::U8,
            ));
        }
        let oversized = InlineStructLayout::from_fields(declared).expect("layout object built");
        assert!(
            !oversized.fits_record_bound(MAX_INLINE_STRUCT_RECORD_BYTES),
            "test fixture should exceed the record envelope"
        );

        let mut store = EdgeInlineValueProfileStore::init(mem());
        let graph = GraphId::from_raw(1);
        let label = EdgeLabelId::from_raw(30);
        let property_id = PropertyId::from_raw(40);
        let err = store
            .set_inline_struct_schema(graph, label, property_id, oversized)
            .expect_err("oversized struct must be rejected at stable boundary");
        assert!(
            matches!(err, EdgeInlineValueProfileStoreError::LayoutInvalid(_)),
            "expected LayoutInvalid, got {err:?}"
        );
        assert!(
            store.get_record(graph, label).is_none(),
            "no record should be inserted for rejected oversized layout"
        );
    }
}
