use gleaph_graph_kernel::entry::{INLINE_EDGE_LABEL_MAX, LabelId, VERTEX_LABEL_MIN};
use ic_stable_structures::{Memory, StableBTreeMap};
use std::fmt;

/// Stable bidirectional label catalog.
///
/// Label id `0` is reserved. Edge-capable names allocate in `0x0001..=0x3FFF`; vertex-only names
/// allocate in `0x4000..=0xFFFF`.
pub struct LabelCatalog<MNameToId: Memory, MIdToName: Memory> {
    name_to_id: StableBTreeMap<String, LabelId, MNameToId>,
    id_to_name: StableBTreeMap<LabelId, String, MIdToName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelCatalogError {
    ReservedLabelId(LabelId),
    LabelIdExhausted,
    NameAlreadyMapped {
        name: String,
        existing: LabelId,
    },
    IdAlreadyMapped {
        id: LabelId,
        existing: String,
    },
    /// Id is outside the edge inline band `0x0001..=0x3FFF`.
    EdgeLabelIdOutOfRange(LabelId),
    /// Id is outside the vertex catalog band `0x4000..=0xFFFF`.
    VertexLabelIdOutOfRange(LabelId),
}

impl fmt::Display for LabelCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedLabelId(id) => write!(f, "label id {} is reserved", id.raw()),
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
            Self::EdgeLabelIdOutOfRange(id) => write!(
                f,
                "edge label id {} is outside the inline edge range 0x0001..=0x3FFF",
                id.raw()
            ),
            Self::VertexLabelIdOutOfRange(id) => write!(
                f,
                "vertex label id {} is outside the vertex range 0x4000..=0xFFFF",
                id.raw()
            ),
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

    pub fn get_or_insert_edge_label(&mut self, name: &str) -> Result<LabelId, LabelCatalogError> {
        if let Some(id) = self.get_id(name) {
            if !id.is_edge_inline_capable() {
                return Err(LabelCatalogError::VertexLabelIdOutOfRange(id));
            }
            return Ok(id);
        }
        let id = self.next_edge_label_id()?;
        self.insert_edge_label_with_id(name, id)?;
        Ok(id)
    }

    pub fn get_or_insert_vertex_label(&mut self, name: &str) -> Result<LabelId, LabelCatalogError> {
        if let Some(id) = self.get_id(name) {
            if !id.is_vertex_catalog_range() {
                return Err(LabelCatalogError::EdgeLabelIdOutOfRange(id));
            }
            return Ok(id);
        }
        let id = self.next_vertex_label_id()?;
        self.insert_vertex_label_with_id(name, id)?;
        Ok(id)
    }

    pub fn insert_edge_label_with_id(
        &mut self,
        name: &str,
        id: LabelId,
    ) -> Result<(), LabelCatalogError> {
        if id.raw() == 0 {
            return Err(LabelCatalogError::ReservedLabelId(id));
        }
        if !id.is_edge_inline_capable() {
            return Err(LabelCatalogError::EdgeLabelIdOutOfRange(id));
        }
        self.insert_any_with_id(name, id)
    }

    pub fn insert_vertex_label_with_id(
        &mut self,
        name: &str,
        id: LabelId,
    ) -> Result<(), LabelCatalogError> {
        if id.raw() == 0 {
            return Err(LabelCatalogError::ReservedLabelId(id));
        }
        if !id.is_vertex_catalog_range() {
            return Err(LabelCatalogError::VertexLabelIdOutOfRange(id));
        }
        self.insert_any_with_id(name, id)
    }

    fn insert_any_with_id(&mut self, name: &str, id: LabelId) -> Result<(), LabelCatalogError> {
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

    fn next_edge_label_id(&self) -> Result<LabelId, LabelCatalogError> {
        let mut next = 1u16;
        for entry in self.id_to_name.iter() {
            let raw = entry.key().raw();
            if !LabelId::from_raw(raw).is_edge_inline_capable() {
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
                .filter(|&n| n <= INLINE_EDGE_LABEL_MAX)
                .ok_or(LabelCatalogError::LabelIdExhausted)?;
        }
        if next > INLINE_EDGE_LABEL_MAX {
            return Err(LabelCatalogError::LabelIdExhausted);
        }
        Ok(LabelId::from_raw(next))
    }

    fn next_vertex_label_id(&self) -> Result<LabelId, LabelCatalogError> {
        let mut next = VERTEX_LABEL_MIN;
        for entry in self.id_to_name.iter() {
            let raw = entry.key().raw();
            if raw < VERTEX_LABEL_MIN {
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
                .ok_or(LabelCatalogError::LabelIdExhausted)?;
        }
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
    fn edge_labels_allocate_from_one() {
        let mut catalog = catalog();
        let knows = catalog.get_or_insert_edge_label("KNOWS").unwrap();
        let rel = catalog.get_or_insert_edge_label("REL").unwrap();
        assert_eq!(knows.raw(), 1);
        assert_eq!(rel.raw(), 2);
        assert_eq!(catalog.get_or_insert_edge_label("KNOWS").unwrap(), knows);
    }

    #[test]
    fn vertex_labels_allocate_from_vertex_band() {
        let mut catalog = catalog();
        let person = catalog.get_or_insert_vertex_label("Person").unwrap();
        assert_eq!(person.raw(), VERTEX_LABEL_MIN);
        let post = catalog.get_or_insert_vertex_label("Post").unwrap();
        assert_eq!(post.raw(), VERTEX_LABEL_MIN + 1);
    }

    #[test]
    fn rejects_edge_label_outside_inline_range() {
        let mut catalog = catalog();
        assert!(matches!(
            catalog.insert_edge_label_with_id("X", LabelId::from_raw(0)),
            Err(LabelCatalogError::ReservedLabelId(_))
        ));
        assert!(matches!(
            catalog.insert_edge_label_with_id("X", LabelId::from_raw(VERTEX_LABEL_MIN)),
            Err(LabelCatalogError::EdgeLabelIdOutOfRange(_))
        ));
    }

    #[test]
    fn rejects_vertex_label_in_edge_range() {
        let mut catalog = catalog();
        assert!(matches!(
            catalog.insert_vertex_label_with_id("Bad", LabelId::from_raw(10)),
            Err(LabelCatalogError::VertexLabelIdOutOfRange(_))
        ));
    }

    #[test]
    fn persists_across_reopen() {
        let mut catalog = catalog();
        let person = catalog.get_or_insert_vertex_label("Person").unwrap();
        let memories = catalog.into_memories();
        let reopened = LabelCatalog::init(memories.0, memories.1);
        assert_eq!(reopened.get_id("Person"), Some(person));
    }

    #[test]
    fn skips_sparse_ids_when_allocating_edges() {
        let mut catalog = catalog();
        catalog
            .insert_edge_label_with_id("Gap", LabelId::from_raw(3))
            .unwrap();
        assert_eq!(catalog.get_or_insert_edge_label("A").unwrap().raw(), 1);
        assert_eq!(catalog.get_or_insert_edge_label("B").unwrap().raw(), 2);
        assert_eq!(catalog.get_or_insert_edge_label("C").unwrap().raw(), 4);
    }
}
