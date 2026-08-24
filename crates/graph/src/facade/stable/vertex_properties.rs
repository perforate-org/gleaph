use gleaph_gql::{Value, ValueBinaryError};
use gleaph_gql_ic::IcExtensionBinaryDecode;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::{borrow::Cow, fmt, ops::Bound as RangeBound};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VertexPropertyKey {
    vertex_id: u32,
    property_id: PropertyId,
}

impl VertexPropertyKey {
    pub fn new(vertex_id: VertexId, property_id: PropertyId) -> Self {
        Self {
            vertex_id: u32::from_le_bytes(vertex_id.to_le_bytes()),
            property_id,
        }
    }

    pub fn vertex_id(self) -> VertexId {
        VertexId::from(self.vertex_id)
    }

    pub fn property_id(self) -> PropertyId {
        self.property_id
    }
}

impl Storable for VertexPropertyKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&self.vertex_id.to_be_bytes());
        out.extend_from_slice(&self.property_id.raw().to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 8, "VertexPropertyKey expects exactly 8 bytes");
        let mut vertex = [0; 4];
        let mut property = [0; 4];
        vertex.copy_from_slice(&bytes[0..4]);
        property.copy_from_slice(&bytes[4..8]);
        Self {
            vertex_id: u32::from_be_bytes(vertex),
            property_id: PropertyId::from_raw(u32::from_be_bytes(property)),
        }
    }
}

/// Wire-layout version of the [`StoredPropertyValue`] envelope (regions 33/34).
/// Writers prepend this byte; readers reject every other first byte so future
/// schema revisions stay additive instead of silently misparsing.
const LAYOUT_VERSION_V1: u8 = 1;

/// Versioned stable value for vertex/edge property stores.
#[derive(Clone, Debug, PartialEq)]
pub enum StoredPropertyValue {
    V1(Value),
}

impl StoredPropertyValue {
    pub fn into_value(self) -> Value {
        match self {
            StoredPropertyValue::V1(value) => value,
        }
    }
}

impl Storable for StoredPropertyValue {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let StoredPropertyValue::V1(value) = self;
        let mut encoded = value
            .to_binary_bytes()
            .expect("Value must encode to binary bytes");
        let mut out = Vec::with_capacity(encoded.len() + 1);
        out.push(LAYOUT_VERSION_V1);
        out.append(&mut encoded);
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert!(
            !bytes.is_empty(),
            "stored property value truncated: missing layout version byte"
        );
        let (version, payload) = bytes.split_first().expect("non-empty checked above");
        assert_eq!(
            *version, LAYOUT_VERSION_V1,
            "unknown stored property value layout version {version}"
        );
        Self::V1(
            Value::from_binary_bytes_with_extensions(payload, &IcExtensionBinaryDecode::INSTANCE)
                .expect("Value bytes must decode"),
        )
    }
}

pub struct VertexPropertyStore<M: Memory> {
    properties: StableBTreeMap<VertexPropertyKey, StoredPropertyValue, M>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexPropertyStoreError {
    ReservedPropertyId(PropertyId),
    InvalidValue(ValueBinaryError),
}

impl fmt::Display for VertexPropertyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedPropertyId(id) => write!(f, "property id {} is reserved", id.raw()),
            Self::InvalidValue(err) => write!(f, "invalid property value: {err}"),
        }
    }
}

impl std::error::Error for VertexPropertyStoreError {}

impl From<ValueBinaryError> for VertexPropertyStoreError {
    fn from(value: ValueBinaryError) -> Self {
        Self::InvalidValue(value)
    }
}

impl<M: Memory> VertexPropertyStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            properties: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, vertex_id: VertexId, property_id: PropertyId) -> Option<Value> {
        if property_id.raw() == 0 {
            return None;
        }
        self.properties
            .get(&VertexPropertyKey::new(vertex_id, property_id))
            .map(StoredPropertyValue::into_value)
    }

    pub fn set(
        &mut self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        crate::property::ensure_property_id(property_id)
            .map_err(VertexPropertyStoreError::ReservedPropertyId)?;
        crate::property::ensure_persistable(&value)
            .map_err(VertexPropertyStoreError::InvalidValue)?;
        Ok(self
            .properties
            .insert(
                VertexPropertyKey::new(vertex_id, property_id),
                StoredPropertyValue::V1(value),
            )
            .map(StoredPropertyValue::into_value))
    }

    pub fn remove(&mut self, vertex_id: VertexId, property_id: PropertyId) -> Option<Value> {
        if property_id.raw() == 0 {
            return None;
        }
        self.properties
            .remove(&VertexPropertyKey::new(vertex_id, property_id))
            .map(StoredPropertyValue::into_value)
    }

    pub fn properties_for(&self, vertex_id: VertexId) -> Vec<(PropertyId, Value)> {
        let mut out = Vec::new();
        self.for_each_property_for(vertex_id, |pid, v| out.push((pid, v)));
        out
    }

    /// Visits `(property_id, value)` pairs for `vertex_id` in key order without building an
    /// intermediate [`Vec`].
    pub(crate) fn for_each_property_for<F>(&self, vertex_id: VertexId, mut f: F)
    where
        F: FnMut(PropertyId, Value),
    {
        let vertex_id_raw = u32::from_le_bytes(vertex_id.to_le_bytes());
        let start = VertexPropertyKey {
            vertex_id: vertex_id_raw,
            property_id: PropertyId::from_raw(0),
        };
        let upper = vertex_id_raw.checked_add(1).map(|next_vertex_id| {
            RangeBound::Excluded(VertexPropertyKey {
                vertex_id: next_vertex_id,
                property_id: PropertyId::from_raw(0),
            })
        });
        let range = match upper {
            Some(upper) => (RangeBound::Included(start), upper),
            None => (RangeBound::Included(start), RangeBound::Unbounded),
        };
        let vid = VertexId::from(vertex_id_raw);
        for entry in self
            .properties
            .range(range)
            .take_while(|entry| entry.key().vertex_id() == vid)
        {
            let (key, value) = entry.into_pair();
            f(key.property_id(), value.into_value());
        }
    }

    /// Returns up to `max_entries` properties after `after` in stable key order.
    pub(crate) fn scan_properties_batch(
        &self,
        after: Option<VertexPropertyKey>,
        max_entries: u32,
    ) -> Vec<(VertexPropertyKey, Value)> {
        let max = usize::try_from(max_entries).unwrap_or(usize::MAX);
        let range = after
            .map(|key| (RangeBound::Excluded(key), RangeBound::Unbounded))
            .unwrap_or((RangeBound::Unbounded, RangeBound::Unbounded));
        self.properties
            .range(range)
            .take(max)
            .map(|entry| (*entry.key(), entry.value().into_value()))
            .collect()
    }

    pub fn into_memory(self) -> M {
        self.properties.into_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::value::types::Decimal;
    use gleaph_gql_ic::{Principal, PrincipalValue};
    use ic_stable_structures::VectorMemory;

    fn store() -> VertexPropertyStore<VectorMemory> {
        VertexPropertyStore::init(VectorMemory::default())
    }

    fn roundtrip_stored_value(value: Value) {
        let stored = StoredPropertyValue::V1(value);
        let bytes = stored.clone().into_bytes();
        assert_eq!(
            bytes.first(),
            Some(&LAYOUT_VERSION_V1),
            "stored property values must carry the v1 layout-version prefix"
        );
        assert_eq!(
            StoredPropertyValue::from_bytes(Cow::Owned(bytes)),
            stored,
            "v1 wire bytes must round-trip"
        );
    }

    #[test]
    fn roundtrips_representative_values_with_version_prefix() {
        let ic_extension: Value =
            PrincipalValue(Principal::from_text("aaaaa-aa").expect("management id")).into();
        let values = vec![
            Value::Null,
            Value::Int64(-42),
            Value::Int128(i128::MIN),
            Value::Float64(2.5),
            Value::Text("Alice".into()),
            Value::Bytes(vec![0x00, 0xFF]),
            Value::List(vec![Value::Null, Value::Int64(7)]),
            Value::Record(vec![("age".to_string(), Value::Int64(42))]),
            Value::Decimal(Decimal::from_i64(1234)),
            ic_extension,
        ];
        for value in values {
            roundtrip_stored_value(value);
        }
    }

    #[test]
    #[should_panic(expected = "missing layout version byte")]
    fn empty_stored_value_payload_is_rejected() {
        StoredPropertyValue::from_bytes(Cow::Borrowed(&[]));
    }

    #[test]
    #[should_panic(expected = "unknown stored property value layout version")]
    fn stored_value_rejects_unknown_layout_version_2() {
        let mut bytes = StoredPropertyValue::V1(Value::Null).into_bytes();
        bytes[0] = 0x02;
        StoredPropertyValue::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "unknown stored property value layout version")]
    fn stored_value_rejects_unknown_layout_version_255() {
        let mut bytes = StoredPropertyValue::V1(Value::Null).into_bytes();
        bytes[0] = 0xFF;
        StoredPropertyValue::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    #[should_panic(expected = "Value bytes must decode")]
    fn truncated_stored_value_v1_payload_is_rejected() {
        let bytes = StoredPropertyValue::V1(Value::Text("Alice".into())).into_bytes();
        StoredPropertyValue::from_bytes(Cow::Owned(bytes[..1].to_vec()));
    }

    #[test]
    fn set_get_and_replace_vertex_property() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = PropertyId::from_raw(1);

        assert_eq!(store.get(vid, name), None);
        assert_eq!(
            store.set(vid, name, Value::Text("Alice".into())).unwrap(),
            None
        );
        assert_eq!(store.get(vid, name), Some(Value::Text("Alice".into())));
        assert_eq!(
            store.set(vid, name, Value::Text("Bob".into())).unwrap(),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(store.get(vid, name), Some(Value::Text("Bob".into())));
    }

    #[test]
    fn remove_vertex_property() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = PropertyId::from_raw(1);

        store.set(vid, name, Value::Text("Alice".into())).unwrap();

        assert_eq!(store.remove(vid, name), Some(Value::Text("Alice".into())));
        assert_eq!(store.remove(vid, name), None);
        assert_eq!(store.get(vid, name), None);
    }

    #[test]
    fn properties_for_returns_only_one_vertex() {
        let mut store = store();
        let alice = VertexId::from(7);
        let bob = VertexId::from(8);
        let name = PropertyId::from_raw(1);
        let age = PropertyId::from_raw(2);

        store
            .set(alice, age, Value::Int64(42))
            .expect("set alice age");
        store
            .set(alice, name, Value::Text("Alice".into()))
            .expect("set alice name");
        store
            .set(bob, name, Value::Text("Bob".into()))
            .expect("set bob name");

        assert_eq!(
            store.properties_for(alice),
            vec![(name, Value::Text("Alice".into())), (age, Value::Int64(42)),]
        );
    }

    #[test]
    fn properties_for_handles_max_vertex_id() {
        let mut store = store();
        let max = VertexId::from(u32::MAX);
        let name = PropertyId::from_raw(1);

        store
            .set(max, name, Value::Text("Last".into()))
            .expect("set max vertex property");

        assert_eq!(
            store.properties_for(max),
            vec![(name, Value::Text("Last".into()))]
        );
    }

    #[test]
    fn persists_across_reopen() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = PropertyId::from_raw(1);

        store.set(vid, name, Value::Text("Alice".into())).unwrap();
        let memory = store.into_memory();

        let reopened = VertexPropertyStore::init(memory);

        assert_eq!(reopened.get(vid, name), Some(Value::Text("Alice".into())));
    }

    #[test]
    fn persists_principal_value_across_reopen() {
        let mut store = store();
        let vid = VertexId::from(7);
        let prop = PropertyId::from_raw(1);
        let p = Principal::from_text("aaaaa-aa").expect("management id");
        let value: Value = PrincipalValue(p).into();

        store.set(vid, prop, value.clone()).unwrap();
        let memory = store.into_memory();
        let reopened = VertexPropertyStore::init(memory);

        assert_eq!(reopened.get(vid, prop), Some(value));
    }

    #[test]
    fn rejects_reserved_property_id() {
        let mut store = store();

        assert!(matches!(
            store.set(VertexId::from(7), PropertyId::default(), Value::Null),
            Err(VertexPropertyStoreError::ReservedPropertyId(id)) if id.raw() == 0
        ));
        assert_eq!(store.get(VertexId::from(7), PropertyId::default()), None);
    }
}
