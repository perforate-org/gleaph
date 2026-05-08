use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

/// Stable bidirectional property-name catalog.
///
/// Property id `0` is reserved; allocated property ids start at `1`.
pub struct PropertyCatalog<MNameToId: Memory, MIdToName: Memory> {
    name_to_id: StableBTreeMap<String, PropertyId, MNameToId>,
    id_to_name: StableBTreeMap<PropertyId, String, MIdToName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyCatalogError {
    ReservedPropertyId(PropertyId),
    PropertyIdExhausted,
    NameAlreadyMapped { name: String, existing: PropertyId },
    IdAlreadyMapped { id: PropertyId, existing: String },
}

impl fmt::Display for PropertyCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedPropertyId(id) => write!(f, "property id {} is reserved", id.raw()),
            Self::PropertyIdExhausted => write!(f, "property id space exhausted"),
            Self::NameAlreadyMapped { name, existing } => {
                write!(
                    f,
                    "property name '{name}' is already mapped to {}",
                    existing.raw()
                )
            }
            Self::IdAlreadyMapped { id, existing } => {
                write!(
                    f,
                    "property id {} is already mapped to '{existing}'",
                    id.raw()
                )
            }
        }
    }
}

impl std::error::Error for PropertyCatalogError {}

impl<MNameToId: Memory, MIdToName: Memory> PropertyCatalog<MNameToId, MIdToName> {
    pub fn init(name_to_id: MNameToId, id_to_name: MIdToName) -> Self {
        Self {
            name_to_id: StableBTreeMap::init(name_to_id),
            id_to_name: StableBTreeMap::init(id_to_name),
        }
    }

    pub fn len(&self) -> u64 {
        self.id_to_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_id(&self, name: &str) -> Option<PropertyId> {
        self.name_to_id.get(&name.to_owned())
    }

    pub fn get_name(&self, id: PropertyId) -> Option<String> {
        self.id_to_name.get(&id)
    }

    pub fn get_or_insert(&mut self, name: &str) -> Result<PropertyId, PropertyCatalogError> {
        if let Some(id) = self.get_id(name) {
            return Ok(id);
        }
        let id = self.next_id()?;
        self.insert_with_id(name, id)?;
        Ok(id)
    }

    pub fn insert_with_id(
        &mut self,
        name: &str,
        id: PropertyId,
    ) -> Result<(), PropertyCatalogError> {
        if id.raw() == 0 {
            return Err(PropertyCatalogError::ReservedPropertyId(id));
        }
        if let Some(existing) = self.get_id(name) {
            return Err(PropertyCatalogError::NameAlreadyMapped {
                name: name.to_owned(),
                existing,
            });
        }
        if let Some(existing) = self.get_name(id) {
            return Err(PropertyCatalogError::IdAlreadyMapped { id, existing });
        }
        self.name_to_id.insert(name.to_owned(), id);
        self.id_to_name.insert(id, name.to_owned());
        Ok(())
    }

    pub fn into_memories(self) -> (MNameToId, MIdToName) {
        (self.name_to_id.into_memory(), self.id_to_name.into_memory())
    }

    fn next_id(&self) -> Result<PropertyId, PropertyCatalogError> {
        let mut next = 1u32;
        for entry in self.id_to_name.iter() {
            let raw = entry.key().raw();
            if raw < next {
                continue;
            }
            if raw > next {
                break;
            }
            next = next
                .checked_add(1)
                .ok_or(PropertyCatalogError::PropertyIdExhausted)?;
        }
        Ok(PropertyId::from_raw(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn catalog() -> PropertyCatalog<VectorMemory, VectorMemory> {
        PropertyCatalog::init(VectorMemory::default(), VectorMemory::default())
    }

    #[test]
    fn allocates_properties_from_one_and_round_trips_both_directions() {
        let mut catalog = catalog();

        let name = catalog.get_or_insert("name").unwrap();
        let age = catalog.get_or_insert("age").unwrap();

        assert_eq!(name.raw(), 1);
        assert_eq!(age.raw(), 2);
        assert_eq!(catalog.get_id("name"), Some(name));
        assert_eq!(catalog.get_name(age), Some("age".to_owned()));
        assert_eq!(catalog.get_or_insert("name").unwrap(), name);
    }

    #[test]
    fn persists_across_reopen() {
        let mut catalog = catalog();
        let name = catalog.get_or_insert("name").unwrap();
        let memories = catalog.into_memories();

        let reopened = PropertyCatalog::init(memories.0, memories.1);

        assert_eq!(reopened.get_id("name"), Some(name));
        assert_eq!(reopened.get_name(name), Some("name".to_owned()));
    }

    #[test]
    fn rejects_conflicting_manual_mappings() {
        let mut catalog = catalog();
        let name = PropertyId::from_raw(7);
        catalog.insert_with_id("name", name).unwrap();

        assert!(matches!(
            catalog.insert_with_id("name", PropertyId::from_raw(8)),
            Err(PropertyCatalogError::NameAlreadyMapped { .. })
        ));
        assert!(matches!(
            catalog.insert_with_id("age", name),
            Err(PropertyCatalogError::IdAlreadyMapped { .. })
        ));
    }

    #[test]
    fn skips_manual_sparse_ids_when_allocating() {
        let mut catalog = catalog();
        catalog
            .insert_with_id("reserved_later", PropertyId::from_raw(3))
            .unwrap();

        assert_eq!(catalog.get_or_insert("a").unwrap().raw(), 1);
        assert_eq!(catalog.get_or_insert("b").unwrap().raw(), 2);
        assert_eq!(catalog.get_or_insert("c").unwrap().raw(), 4);
    }

    #[test]
    fn rejects_zero_property_id() {
        let mut catalog = catalog();

        assert!(matches!(
            catalog.insert_with_id("none", PropertyId::default()),
            Err(PropertyCatalogError::ReservedPropertyId(id)) if id.raw() == 0
        ));
    }
}
