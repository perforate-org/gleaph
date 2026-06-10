//! Stable bidirectional name/id catalogs with pluggable allocation policy.

use ic_stable_structures::{Memory, StableBTreeMap, Storable};
use std::fmt;
use std::marker::PhantomData;

/// Catalog identifier stored in both map directions.
pub trait CatalogId: Copy + Ord + Storable + Default {
    fn raw_u32(self) -> u32;
    fn from_raw_u32(raw: u32) -> Option<Self>;
    fn saturating_add_one(self) -> Self;
}

/// Chooses the next catalog id for [`BidirectionalCatalog::get_or_insert`].
pub trait CatalogAllocationPolicy<Id: CatalogId> {
    fn reserved_id() -> Id;
    fn max_id() -> Option<Id> {
        None
    }
    fn next_raw_id(existing: impl Iterator<Item = u32>) -> Result<u32, CatalogError<Id>>;
}

/// Lowest free id starting at `1`, skipping manual sparse holes.
pub struct SparseFromOnePolicy;

/// `max(existing)+1` dense allocation starting at `1`.
pub struct DenseMaxPlusOnePolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError<Id> {
    ReservedId(Id),
    IdExhausted,
    MaxIdExceeded,
    NameAlreadyMapped { name: String, existing: Id },
    IdAlreadyMapped { id: Id, existing: String },
}

impl<Id: fmt::Display> fmt::Display for CatalogError<Id> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedId(id) => write!(f, "catalog id {id} is reserved"),
            Self::IdExhausted => write!(f, "catalog id space exhausted"),
            Self::MaxIdExceeded => write!(f, "catalog id exceeds configured maximum"),
            Self::NameAlreadyMapped { name, existing } => {
                write!(f, "catalog name '{name}' is already mapped to {existing}")
            }
            Self::IdAlreadyMapped { id, existing } => {
                write!(f, "catalog id {id} is already mapped to '{existing}'")
            }
        }
    }
}

impl<Id: fmt::Debug + fmt::Display> std::error::Error for CatalogError<Id> {}

pub struct BidirectionalCatalog<Id: CatalogId, MName: Memory, MId: Memory, Policy> {
    name_to_id: StableBTreeMap<String, Id, MName>,
    id_to_name: StableBTreeMap<Id, String, MId>,
    _policy: PhantomData<Policy>,
}

impl<Id, MName, MId, Policy> BidirectionalCatalog<Id, MName, MId, Policy>
where
    Id: CatalogId,
    MName: Memory,
    MId: Memory,
    Policy: CatalogAllocationPolicy<Id>,
{
    pub fn init(name_to_id: MName, id_to_name: MId) -> Self {
        Self {
            name_to_id: StableBTreeMap::init(name_to_id),
            id_to_name: StableBTreeMap::init(id_to_name),
            _policy: PhantomData,
        }
    }

    pub fn len(&self) -> u64 {
        self.id_to_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_id(&self, name: &str) -> Option<Id> {
        self.name_to_id.get(&name.to_owned())
    }

    pub fn get_name(&self, id: Id) -> Option<String> {
        self.id_to_name.get(&id)
    }

    pub fn get_or_insert(&mut self, name: &str) -> Result<Id, CatalogError<Id>> {
        if let Some(id) = self.get_id(name) {
            return Ok(id);
        }
        let id = self.next_id()?;
        self.insert_with_id(name, id)?;
        Ok(id)
    }

    pub fn insert_with_id(&mut self, name: &str, id: Id) -> Result<(), CatalogError<Id>> {
        if id == Policy::reserved_id() {
            return Err(CatalogError::ReservedId(id));
        }
        if let Some(max) = Policy::max_id()
            && id.raw_u32() > max.raw_u32()
        {
            return Err(CatalogError::MaxIdExceeded);
        }
        if let Some(existing) = self.get_id(name) {
            return Err(CatalogError::NameAlreadyMapped {
                name: name.to_owned(),
                existing,
            });
        }
        if let Some(existing) = self.get_name(id) {
            return Err(CatalogError::IdAlreadyMapped { id, existing });
        }
        self.name_to_id.insert(name.to_owned(), id);
        self.id_to_name.insert(id, name.to_owned());
        Ok(())
    }

    pub fn clear_new(&mut self) {
        self.name_to_id.clear_new();
        self.id_to_name.clear_new();
    }

    pub fn into_memories(self) -> (MName, MId) {
        (self.name_to_id.into_memory(), self.id_to_name.into_memory())
    }

    fn next_id(&self) -> Result<Id, CatalogError<Id>> {
        let raw = Policy::next_raw_id(self.id_to_name.iter().map(|entry| entry.key().raw_u32()))?;
        let id = Id::from_raw_u32(raw).ok_or(CatalogError::IdExhausted)?;
        if id == Policy::reserved_id() {
            return Err(CatalogError::IdExhausted);
        }
        if let Some(max) = Policy::max_id()
            && id.raw_u32() > max.raw_u32()
        {
            return Err(CatalogError::MaxIdExceeded);
        }
        Ok(id)
    }
}

impl<Id: CatalogId> CatalogAllocationPolicy<Id> for SparseFromOnePolicy {
    fn reserved_id() -> Id {
        Id::default()
    }

    fn next_raw_id(existing: impl Iterator<Item = u32>) -> Result<u32, CatalogError<Id>> {
        let mut next = 1u32;
        for raw in existing {
            if raw < next {
                continue;
            }
            if raw > next {
                break;
            }
            next = next.saturating_add(1);
            if next == 0 {
                return Err(CatalogError::IdExhausted);
            }
        }
        Ok(next)
    }
}

impl<Id: CatalogId> CatalogAllocationPolicy<Id> for DenseMaxPlusOnePolicy {
    fn reserved_id() -> Id {
        Id::default()
    }

    fn next_raw_id(existing: impl Iterator<Item = u32>) -> Result<u32, CatalogError<Id>> {
        let next = existing.max().unwrap_or(0).saturating_add(1);
        if next == 0 {
            return Err(CatalogError::IdExhausted);
        }
        Ok(next)
    }
}

macro_rules! impl_catalog_id {
    ($ty:ty, $raw:ty) => {
        impl CatalogId for $ty {
            fn raw_u32(self) -> u32 {
                self.raw() as u32
            }

            fn from_raw_u32(raw: u32) -> Option<Self> {
                (raw <= <$raw>::MAX as u32).then(|| Self::from_raw(raw as $raw))
            }

            fn saturating_add_one(self) -> Self {
                Self::from_raw(self.raw().saturating_add(1))
            }
        }
    };
}

impl_catalog_id!(crate::entry::PropertyId, u32);
impl_catalog_id!(crate::entry::VertexLabelId, u16);
impl_catalog_id!(crate::entry::EdgeLabelId, u16);

/// Dense edge-label allocation capped at [`crate::entry::EDGE_LABEL_CATALOG_MAX`].
pub struct DenseEdgeLabelPolicy;

impl<Id: CatalogId> CatalogAllocationPolicy<Id> for DenseEdgeLabelPolicy {
    fn reserved_id() -> Id {
        Id::default()
    }

    fn max_id() -> Option<Id> {
        Id::from_raw_u32(crate::entry::EDGE_LABEL_CATALOG_MAX as u32)
    }

    fn next_raw_id(existing: impl Iterator<Item = u32>) -> Result<u32, CatalogError<Id>> {
        DenseMaxPlusOnePolicy::next_raw_id(existing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::PropertyId;
    use ic_stable_structures::VectorMemory;

    type TestCatalog<Policy> =
        BidirectionalCatalog<PropertyId, VectorMemory, VectorMemory, Policy>;

    fn catalog<Policy: CatalogAllocationPolicy<PropertyId>>() -> TestCatalog<Policy> {
        BidirectionalCatalog::init(VectorMemory::default(), VectorMemory::default())
    }

    #[test]
    fn sparse_allocates_from_one_and_round_trips_both_directions() {
        let mut catalog = catalog::<SparseFromOnePolicy>();
        let name = catalog.get_or_insert("name").unwrap();
        let age = catalog.get_or_insert("age").unwrap();
        assert_eq!(name.raw(), 1);
        assert_eq!(age.raw(), 2);
        assert_eq!(catalog.get_id("name"), Some(name));
        assert_eq!(catalog.get_name(age), Some("age".to_owned()));
        assert_eq!(catalog.get_or_insert("name").unwrap(), name);
    }

    #[test]
    fn sparse_skips_manual_sparse_ids_when_allocating() {
        let mut catalog = catalog::<SparseFromOnePolicy>();
        catalog
            .insert_with_id("reserved_later", PropertyId::from_raw(3))
            .unwrap();
        assert_eq!(catalog.get_or_insert("a").unwrap().raw(), 1);
        assert_eq!(catalog.get_or_insert("b").unwrap().raw(), 2);
        assert_eq!(catalog.get_or_insert("c").unwrap().raw(), 4);
    }

    #[test]
    fn dense_allocates_max_plus_one() {
        let mut catalog = catalog::<DenseMaxPlusOnePolicy>();
        catalog
            .insert_with_id("first", PropertyId::from_raw(5))
            .unwrap();
        assert_eq!(catalog.get_or_insert("second").unwrap().raw(), 6);
    }

    #[test]
    fn rejects_conflicting_manual_mappings() {
        let mut catalog = catalog::<SparseFromOnePolicy>();
        let name = PropertyId::from_raw(7);
        catalog.insert_with_id("name", name).unwrap();
        assert!(matches!(
            catalog.insert_with_id("name", PropertyId::from_raw(8)),
            Err(CatalogError::NameAlreadyMapped { .. })
        ));
        assert!(matches!(
            catalog.insert_with_id("age", name),
            Err(CatalogError::IdAlreadyMapped { .. })
        ));
    }

    #[test]
    fn rejects_reserved_zero_id() {
        let mut catalog = catalog::<SparseFromOnePolicy>();
        assert!(matches!(
            catalog.insert_with_id("none", PropertyId::default()),
            Err(CatalogError::ReservedId(id)) if id.raw() == 0
        ));
    }
}
