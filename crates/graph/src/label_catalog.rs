use gleaph_graph_kernel::entry::LabelId;
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

/// Stable bidirectional label catalog.
///
/// Label id `0` is reserved for "no label"; allocated labels start at `1`.
pub struct LabelCatalog<MNameToId: Memory, MIdToName: Memory> {
    name_to_id: StableBTreeMap<String, LabelId, MNameToId>,
    id_to_name: StableBTreeMap<LabelId, String, MIdToName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelCatalogError {
    LabelIdExhausted,
    NameAlreadyMapped { name: String, existing: LabelId },
    IdAlreadyMapped { id: LabelId, existing: String },
}

impl fmt::Display for LabelCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LabelIdExhausted => write!(f, "label id space exhausted"),
            Self::NameAlreadyMapped { name, existing } => {
                write!(
                    f,
                    "label name '{name}' is already mapped to {}",
                    existing.raw()
                )
            }
            Self::IdAlreadyMapped { id, existing } => {
                write!(f, "label id {} is already mapped to '{existing}'", id.raw())
            }
        }
    }
}

impl std::error::Error for LabelCatalogError {}

impl<MNameToId: Memory, MIdToName: Memory> LabelCatalog<MNameToId, MIdToName> {
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

    pub fn get_id(&self, name: &str) -> Option<LabelId> {
        self.name_to_id.get(&name.to_owned())
    }

    pub fn get_name(&self, id: LabelId) -> Option<String> {
        self.id_to_name.get(&id)
    }

    pub fn get_or_insert(&mut self, name: &str) -> Result<LabelId, LabelCatalogError> {
        if let Some(id) = self.get_id(name) {
            return Ok(id);
        }
        let id = self.next_id()?;
        self.insert_with_id(name, id)?;
        Ok(id)
    }

    pub fn insert_with_id(&mut self, name: &str, id: LabelId) -> Result<(), LabelCatalogError> {
        if let Some(existing) = self.get_id(name) {
            return Err(LabelCatalogError::NameAlreadyMapped {
                name: name.to_owned(),
                existing,
            });
        }
        if let Some(existing) = self.get_name(id) {
            return Err(LabelCatalogError::IdAlreadyMapped { id, existing });
        }
        self.name_to_id.insert(name.to_owned(), id);
        self.id_to_name.insert(id, name.to_owned());
        Ok(())
    }

    pub fn into_memories(self) -> (MNameToId, MIdToName) {
        (self.name_to_id.into_memory(), self.id_to_name.into_memory())
    }

    fn next_id(&self) -> Result<LabelId, LabelCatalogError> {
        let next = self
            .len()
            .checked_add(1)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or(LabelCatalogError::LabelIdExhausted)?;
        Ok(LabelId::from_raw(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn catalog() -> LabelCatalog<VectorMemory, VectorMemory> {
        LabelCatalog::init(VectorMemory::default(), VectorMemory::default())
    }

    #[test]
    fn allocates_labels_from_one_and_round_trips_both_directions() {
        let mut catalog = catalog();

        let person = catalog.get_or_insert("Person").unwrap();
        let post = catalog.get_or_insert("Post").unwrap();

        assert_eq!(person.raw(), 1);
        assert_eq!(post.raw(), 2);
        assert_eq!(catalog.get_id("Person"), Some(person));
        assert_eq!(catalog.get_name(post), Some("Post".to_owned()));
        assert_eq!(catalog.get_or_insert("Person").unwrap(), person);
    }

    #[test]
    fn persists_across_reopen() {
        let mut catalog = catalog();
        let person = catalog.get_or_insert("Person").unwrap();
        let memories = catalog.into_memories();

        let reopened = LabelCatalog::init(memories.0, memories.1);

        assert_eq!(reopened.get_id("Person"), Some(person));
        assert_eq!(reopened.get_name(person), Some("Person".to_owned()));
    }

    #[test]
    fn rejects_conflicting_manual_mappings() {
        let mut catalog = catalog();
        let person = LabelId::from_raw(7);
        catalog.insert_with_id("Person", person).unwrap();

        assert!(matches!(
            catalog.insert_with_id("Person", LabelId::from_raw(8)),
            Err(LabelCatalogError::NameAlreadyMapped { .. })
        ));
        assert!(matches!(
            catalog.insert_with_id("Post", person),
            Err(LabelCatalogError::IdAlreadyMapped { .. })
        ));
    }
}
