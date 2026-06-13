//! Graph-scoped bidirectional name catalog (ADR 0011 index names).

use std::borrow::Cow;

use gleaph_graph_kernel::bidirectional_catalog::CatalogError;
use gleaph_graph_kernel::entry::{GraphId, INDEX_NAME_CATALOG_MAX, IndexNameId};
use ic_stable_structures::storable::Bound;
use ic_stable_structures::{Memory, StableBTreeMap, Storable};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphScopedNameKey {
    pub graph_id: GraphId,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphScopedIdKey {
    pub graph_id: GraphId,
    pub id: IndexNameId,
}

impl Storable for GraphScopedNameKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(4 + self.name.len());
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(self.name.as_bytes());
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.name.len());
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(self.name.as_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut raw = [0; 4];
        raw.copy_from_slice(&bytes[..4]);
        Self {
            graph_id: GraphId::from_le_bytes(raw),
            name: String::from_utf8(bytes[4..].to_vec()).expect("graph scoped name utf8"),
        }
    }
}

impl Storable for GraphScopedIdKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut id = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        id.copy_from_slice(&bytes[4..6]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            id: IndexNameId::from_le_bytes(id),
        }
    }
}

pub struct GraphScopedNameCatalog<MName: Memory, MId: Memory> {
    name_to_id: StableBTreeMap<GraphScopedNameKey, IndexNameId, MName>,
    id_to_name: StableBTreeMap<GraphScopedIdKey, String, MId>,
}

impl<MName: Memory, MId: Memory> GraphScopedNameCatalog<MName, MId> {
    pub fn init(name_to_id: MName, id_to_name: MId) -> Self {
        Self {
            name_to_id: StableBTreeMap::init(name_to_id),
            id_to_name: StableBTreeMap::init(id_to_name),
        }
    }

    pub fn get_id(&self, graph_id: GraphId, name: &str) -> Option<IndexNameId> {
        self.name_to_id.get(&GraphScopedNameKey {
            graph_id,
            name: name.to_owned(),
        })
    }

    #[allow(
        dead_code,
        reason = "reverse lookup for index DDL and admin tooling pending"
    )]
    pub fn get_name(&self, graph_id: GraphId, id: IndexNameId) -> Option<String> {
        self.id_to_name.get(&GraphScopedIdKey { graph_id, id })
    }

    pub fn get_or_insert(
        &mut self,
        graph_id: GraphId,
        name: &str,
    ) -> Result<IndexNameId, CatalogError<IndexNameId>> {
        if let Some(id) = self.get_id(graph_id, name) {
            return Ok(id);
        }
        let next = self.next_id_for_graph(graph_id)?;
        let id = IndexNameId::from_raw(next);
        self.insert_mapping(graph_id, name, id)?;
        Ok(id)
    }

    pub fn clear_new(&mut self) {
        self.name_to_id.clear_new();
        self.id_to_name.clear_new();
    }

    fn next_id_for_graph(&self, graph_id: GraphId) -> Result<u16, CatalogError<IndexNameId>> {
        let mut max = 0u16;
        for entry in self.id_to_name.iter() {
            let key = entry.key();
            if key.graph_id == graph_id {
                max = max.max(key.id.raw());
            }
        }
        let next = max.saturating_add(1);
        if next == 0 || next > INDEX_NAME_CATALOG_MAX {
            return Err(CatalogError::IdExhausted);
        }
        Ok(next)
    }

    fn insert_mapping(
        &mut self,
        graph_id: GraphId,
        name: &str,
        id: IndexNameId,
    ) -> Result<(), CatalogError<IndexNameId>> {
        if id.is_reserved() {
            return Err(CatalogError::ReservedId(id));
        }
        let name_key = GraphScopedNameKey {
            graph_id,
            name: name.to_owned(),
        };
        let id_key = GraphScopedIdKey { graph_id, id };
        if self.name_to_id.contains_key(&name_key) {
            return Err(CatalogError::NameAlreadyMapped {
                name: name.to_owned(),
                existing: id,
            });
        }
        if self.id_to_name.contains_key(&id_key) {
            return Err(CatalogError::IdAlreadyMapped {
                id,
                existing: name.to_owned(),
            });
        }
        self.name_to_id.insert(name_key, id);
        self.id_to_name.insert(id_key, name.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    type TestCatalog = GraphScopedNameCatalog<VectorMemory, VectorMemory>;

    fn catalog() -> TestCatalog {
        GraphScopedNameCatalog::init(VectorMemory::default(), VectorMemory::default())
    }

    #[test]
    fn scoped_index_names_are_unique_per_graph() {
        let mut cat = catalog();
        let g1 = GraphId::from_raw(1);
        let g2 = GraphId::from_raw(2);
        let a1 = cat.get_or_insert(g1, "idx_a").unwrap();
        let a2 = cat.get_or_insert(g2, "idx_a").unwrap();
        assert_eq!(a1.raw(), 1);
        assert_eq!(a2.raw(), 1);
        assert_eq!(cat.get_or_insert(g1, "idx_a").unwrap(), a1);
        let b1 = cat.get_or_insert(g1, "idx_b").unwrap();
        assert_eq!(b1.raw(), 2);
    }
}
