use gleaph_graph_kernel::entry::VertexLabelId;
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

/// Stable vertex label name catalog (`VertexLabelId` uses full `u16` space; `0` reserved).
pub struct VertexLabelCatalog<MNameToId: Memory, MIdToName: Memory> {
    name_to_id: StableBTreeMap<String, VertexLabelId, MNameToId>,
    id_to_name: StableBTreeMap<VertexLabelId, String, MIdToName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexLabelCatalogError {
    ReservedLabelId(VertexLabelId),
    LabelIdExhausted,
    NameAlreadyMapped {
        name: String,
        existing: VertexLabelId,
    },
    IdAlreadyMapped {
        id: VertexLabelId,
        existing: String,
    },
}

impl fmt::Display for VertexLabelCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedLabelId(id) => write!(f, "vertex label id {} is reserved", id.raw()),
            Self::LabelIdExhausted => write!(f, "vertex label id space exhausted"),
            Self::NameAlreadyMapped { name, existing } => {
                write!(
                    f,
                    "vertex label name '{name}' is already mapped to {}",
                    existing.raw()
                )
            }
            Self::IdAlreadyMapped { id, existing } => {
                write!(
                    f,
                    "vertex label id {} is already mapped to '{existing}'",
                    id.raw()
                )
            }
        }
    }
}

impl std::error::Error for VertexLabelCatalogError {}

impl<MNameToId: Memory, MIdToName: Memory> VertexLabelCatalog<MNameToId, MIdToName> {
    pub fn init(name_to_id: MNameToId, id_to_name: MIdToName) -> Self {
        Self {
            name_to_id: StableBTreeMap::init(name_to_id),
            id_to_name: StableBTreeMap::init(id_to_name),
        }
    }

    pub fn get_id(&self, name: &str) -> Option<VertexLabelId> {
        self.name_to_id.get(&name.to_owned())
    }

    pub fn get_name(&self, id: VertexLabelId) -> Option<String> {
        self.id_to_name.get(&id)
    }

    pub fn get_or_insert(&mut self, name: &str) -> Result<VertexLabelId, VertexLabelCatalogError> {
        if let Some(id) = self.get_id(name) {
            return Ok(id);
        }
        let id = self.next_label_id()?;
        self.insert_with_id(name, id)?;
        Ok(id)
    }

    pub fn insert_with_id(
        &mut self,
        name: &str,
        id: VertexLabelId,
    ) -> Result<(), VertexLabelCatalogError> {
        if id.is_reserved() {
            return Err(VertexLabelCatalogError::ReservedLabelId(id));
        }
        if let Some(existing) = self.get_id(name) {
            return Err(VertexLabelCatalogError::NameAlreadyMapped {
                name: name.to_owned(),
                existing,
            });
        }
        if let Some(existing) = self.get_name(id) {
            return Err(VertexLabelCatalogError::IdAlreadyMapped { id, existing });
        }
        self.name_to_id.insert(name.to_owned(), id);
        self.id_to_name.insert(id, name.to_owned());
        Ok(())
    }

    pub fn into_memories(self) -> (MNameToId, MIdToName) {
        (self.name_to_id.into_memory(), self.id_to_name.into_memory())
    }

    fn next_label_id(&self) -> Result<VertexLabelId, VertexLabelCatalogError> {
        let mut next = 1u16;
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
                .ok_or(VertexLabelCatalogError::LabelIdExhausted)?;
        }
        Ok(VertexLabelId::from_raw(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    #[test]
    fn allocates_from_one() {
        let mut catalog =
            VertexLabelCatalog::init(VectorMemory::default(), VectorMemory::default());
        let person = catalog.get_or_insert("Person").unwrap();
        let post = catalog.get_or_insert("Post").unwrap();
        assert_eq!(person.raw(), 1);
        assert_eq!(post.raw(), 2);
    }
}
