use gleaph_graph_kernel::entry::{
    EDGE_LABEL_CATALOG_MAX, EdgeDirectedness, EdgeLabelId, TaggedEdgeLabelId,
};
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

/// Stable edge label name catalog (allocates MSB-clear ids in `0x0001..=0x7FFF`).
pub struct EdgeLabelCatalog<MNameToId: Memory, MIdToName: Memory> {
    name_to_id: StableBTreeMap<String, EdgeLabelId, MNameToId>,
    id_to_name: StableBTreeMap<EdgeLabelId, String, MIdToName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeLabelCatalogError {
    ReservedLabelId(EdgeLabelId),
    LabelIdExhausted,
    NameAlreadyMapped { name: String, existing: EdgeLabelId },
    IdAlreadyMapped { id: EdgeLabelId, existing: String },
    EdgeLabelIdOutOfRange(EdgeLabelId),
}

impl fmt::Display for EdgeLabelCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedLabelId(id) => write!(f, "edge label id {} is reserved", id.raw()),
            Self::LabelIdExhausted => write!(f, "edge label id space exhausted"),
            Self::NameAlreadyMapped { name, existing } => {
                write!(
                    f,
                    "edge label name '{name}' is already mapped to {}",
                    existing.raw()
                )
            }
            Self::IdAlreadyMapped { id, existing } => {
                write!(
                    f,
                    "edge label id {} is already mapped to '{existing}'",
                    id.raw()
                )
            }
            Self::EdgeLabelIdOutOfRange(id) => write!(
                f,
                "edge label id {} is outside catalog range 0x0001..=0x7FFF",
                id.raw()
            ),
        }
    }
}

impl std::error::Error for EdgeLabelCatalogError {}

impl<MNameToId: Memory, MIdToName: Memory> EdgeLabelCatalog<MNameToId, MIdToName> {
    pub fn init(name_to_id: MNameToId, id_to_name: MIdToName) -> Self {
        Self {
            name_to_id: StableBTreeMap::init(name_to_id),
            id_to_name: StableBTreeMap::init(id_to_name),
        }
    }

    pub fn get_id(&self, name: &str) -> Option<EdgeLabelId> {
        self.name_to_id.get(&name.to_owned())
    }

    pub fn get_tagged_directed(&self, name: &str) -> Option<TaggedEdgeLabelId> {
        self.get_id(name)
            .map(|id| id.pack(EdgeDirectedness::Directed))
    }

    pub fn get_tagged_undirected(&self, name: &str) -> Option<TaggedEdgeLabelId> {
        self.get_id(name)
            .map(|id| id.pack(EdgeDirectedness::Undirected))
    }

    pub fn get_name(&self, id: EdgeLabelId) -> Option<String> {
        self.id_to_name.get(&id)
    }

    pub fn get_or_insert(&mut self, name: &str) -> Result<EdgeLabelId, EdgeLabelCatalogError> {
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
        id: EdgeLabelId,
    ) -> Result<(), EdgeLabelCatalogError> {
        if !id.is_catalog_allocatable() {
            if id.raw() == 0 {
                return Err(EdgeLabelCatalogError::ReservedLabelId(id));
            }
            return Err(EdgeLabelCatalogError::EdgeLabelIdOutOfRange(id));
        }
        if let Some(existing) = self.get_id(name) {
            return Err(EdgeLabelCatalogError::NameAlreadyMapped {
                name: name.to_owned(),
                existing,
            });
        }
        if let Some(existing) = self.get_name(id) {
            return Err(EdgeLabelCatalogError::IdAlreadyMapped { id, existing });
        }
        self.name_to_id.insert(name.to_owned(), id);
        self.id_to_name.insert(id, name.to_owned());
        Ok(())
    }

    pub fn into_memories(self) -> (MNameToId, MIdToName) {
        (self.name_to_id.into_memory(), self.id_to_name.into_memory())
    }

    fn next_label_id(&self) -> Result<EdgeLabelId, EdgeLabelCatalogError> {
        let mut next = 1u16;
        for entry in self.id_to_name.iter() {
            let raw = entry.key().raw();
            if raw > EDGE_LABEL_CATALOG_MAX {
                continue;
            }
            if raw < next {
                continue;
            }
            if raw > next {
                break;
            }
            next = next
                .checked_add(1)
                .ok_or(EdgeLabelCatalogError::LabelIdExhausted)?;
        }
        if next > EDGE_LABEL_CATALOG_MAX {
            return Err(EdgeLabelCatalogError::LabelIdExhausted);
        }
        Ok(EdgeLabelId::from_raw(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    #[test]
    fn allocates_msb_clear_ids() {
        let mut catalog = EdgeLabelCatalog::init(VectorMemory::default(), VectorMemory::default());
        let knows = catalog.get_or_insert("KNOWS").unwrap();
        let rel = catalog.get_or_insert("REL").unwrap();
        assert_eq!(knows.raw(), 1);
        assert_eq!(rel.raw(), 2);
        assert_eq!(
            catalog.get_tagged_directed("KNOWS"),
            Some(knows.pack(EdgeDirectedness::Directed))
        );
        assert_eq!(
            catalog.get_tagged_undirected("KNOWS"),
            Some(knows.pack(EdgeDirectedness::Undirected))
        );
    }
}
