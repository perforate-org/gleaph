//! Graph-scoped bidirectional name catalogs (ADR 0011 index names; ADR 0018 labels/properties).

use std::borrow::Cow;
use std::marker::PhantomData;

use ic_stable_structures::storable::Bound;
use ic_stable_structures::{Memory, StableBTreeMap, Storable};

use crate::bidirectional_catalog::{CatalogAllocationPolicy, CatalogError, CatalogId};
use crate::entry::GraphId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphScopedNameKey {
    pub graph_id: GraphId,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphScopedIdKey<Id: CatalogId> {
    pub graph_id: GraphId,
    pub id: Id,
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

impl<Id: CatalogId> Storable for GraphScopedIdKey<Id> {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        graph.copy_from_slice(&bytes[0..4]);
        let id = Id::from_bytes(Cow::Borrowed(&bytes[4..]));
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            id,
        }
    }
}

impl<Id: CatalogId> GraphScopedIdKey<Id> {
    fn encode_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 8);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.id.to_bytes());
        out
    }
}

pub struct GraphScopedNameCatalog<Id, MName, MId, Policy>
where
    Id: CatalogId,
    MName: Memory,
    MId: Memory,
    Policy: CatalogAllocationPolicy<Id>,
{
    name_to_id: StableBTreeMap<GraphScopedNameKey, Id, MName>,
    id_to_name: StableBTreeMap<GraphScopedIdKey<Id>, String, MId>,
    _policy: PhantomData<Policy>,
}

impl<Id, MName, MId, Policy> GraphScopedNameCatalog<Id, MName, MId, Policy>
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

    pub fn get_id(&self, graph_id: GraphId, name: &str) -> Option<Id> {
        self.name_to_id.get(&GraphScopedNameKey {
            graph_id,
            name: name.to_owned(),
        })
    }

    pub fn get_name(&self, graph_id: GraphId, id: Id) -> Option<String> {
        self.id_to_name.get(&GraphScopedIdKey { graph_id, id })
    }

    pub fn get_or_insert(&mut self, graph_id: GraphId, name: &str) -> Result<Id, CatalogError<Id>> {
        if let Some(id) = self.get_id(graph_id, name) {
            return Ok(id);
        }
        let next = self.next_id_for_graph(graph_id)?;
        let id = Id::from_raw_u32(next).ok_or(CatalogError::IdExhausted)?;
        self.insert_mapping(graph_id, name, id)?;
        Ok(id)
    }

    /// Read-only capacity preflight: returns the id [`Self::get_or_insert`] would yield for
    /// `name` without mutating the catalog. An already-mapped name returns its existing id (no
    /// allocation); a new name allocates and validates the next id against the reserved id, the
    /// configured max, and the id width, returning the same `CatalogError` `get_or_insert` would.
    ///
    /// Callers use this to prove a later no-`await` commit region cannot fail on id exhaustion:
    /// run `peek_next_id` for every catalog touched during preflight, then commit knowing each
    /// allocation will succeed.
    pub fn peek_next_id(&self, graph_id: GraphId, name: &str) -> Result<Id, CatalogError<Id>> {
        if let Some(id) = self.get_id(graph_id, name) {
            return Ok(id);
        }
        let next = self.next_id_for_graph(graph_id)?;
        let id = Id::from_raw_u32(next).ok_or(CatalogError::IdExhausted)?;
        Self::validate_new_id(id)?;
        Ok(id)
    }

    pub fn clear_new(&mut self) {
        self.name_to_id.clear_new();
        self.id_to_name.clear_new();
    }

    pub fn remove_graph(&mut self, graph_id: GraphId) {
        let mut names = Vec::new();
        for entry in self.name_to_id.iter() {
            if entry.key().graph_id == graph_id {
                names.push(entry.key().clone());
            }
        }
        for key in names {
            self.name_to_id.remove(&key);
        }

        let mut ids = Vec::new();
        for entry in self.id_to_name.iter() {
            if entry.key().graph_id == graph_id {
                ids.push(*entry.key());
            }
        }
        for key in ids {
            self.id_to_name.remove(&key);
        }
    }

    fn next_id_for_graph(&self, graph_id: GraphId) -> Result<u32, CatalogError<Id>> {
        let existing = self.id_to_name.iter().filter_map(|entry| {
            let key = entry.key();
            (key.graph_id == graph_id).then_some(key.id.raw_u32())
        });
        Policy::next_raw_id(existing)
    }

    /// Single source of truth for which freshly-allocated ids are admissible: not the reserved
    /// id and within the policy's configured max. Shared by [`Self::get_or_insert`] (via
    /// `insert_mapping`) and [`Self::peek_next_id`] so the preflight and commit agree exactly.
    fn validate_new_id(id: Id) -> Result<(), CatalogError<Id>> {
        if id == Policy::reserved_id() {
            return Err(CatalogError::ReservedId(id));
        }
        if let Some(max) = Policy::max_id()
            && id.raw_u32() > max.raw_u32()
        {
            return Err(CatalogError::MaxIdExceeded);
        }
        Ok(())
    }

    fn insert_mapping(
        &mut self,
        graph_id: GraphId,
        name: &str,
        id: Id,
    ) -> Result<(), CatalogError<Id>> {
        Self::validate_new_id(id)?;
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
    use crate::bidirectional_catalog::{DenseIndexNamePolicy, DenseMaxPlusOnePolicy};
    use crate::entry::IndexNameId;
    use ic_stable_structures::VectorMemory;

    type TestCatalog =
        GraphScopedNameCatalog<IndexNameId, VectorMemory, VectorMemory, DenseIndexNamePolicy>;

    fn catalog() -> TestCatalog {
        GraphScopedNameCatalog::init(VectorMemory::default(), VectorMemory::default())
    }

    /// Tiny-capacity policy (max id = 1) so id exhaustion is reachable in a unit test without
    /// inserting tens of thousands of entries.
    struct CappedPolicy;
    impl CatalogAllocationPolicy<IndexNameId> for CappedPolicy {
        fn reserved_id() -> IndexNameId {
            IndexNameId::default()
        }
        fn max_id() -> Option<IndexNameId> {
            IndexNameId::from_raw_u32(1)
        }
        fn next_raw_id(
            existing: impl Iterator<Item = u32>,
        ) -> Result<u32, CatalogError<IndexNameId>> {
            DenseMaxPlusOnePolicy::next_raw_id(existing)
        }
    }

    type CappedCatalog =
        GraphScopedNameCatalog<IndexNameId, VectorMemory, VectorMemory, CappedPolicy>;

    #[test]
    fn peek_next_id_is_readonly_and_matches_get_or_insert() {
        let mut cat: CappedCatalog =
            GraphScopedNameCatalog::init(VectorMemory::default(), VectorMemory::default());
        let g = GraphId::from_raw(1);

        // First allocation fits (id 1 == max). peek predicts it without mutating.
        assert_eq!(cat.peek_next_id(g, "a").unwrap().raw(), 1);
        let a = cat.get_or_insert(g, "a").unwrap();
        assert_eq!(a.raw(), 1);

        // Already-mapped name: peek returns the existing id, no allocation.
        assert_eq!(cat.peek_next_id(g, "a").unwrap(), a);

        // A new name would need id 2 > max 1: peek and get_or_insert reject identically...
        assert!(matches!(
            cat.peek_next_id(g, "b"),
            Err(CatalogError::MaxIdExceeded)
        ));
        assert!(matches!(
            cat.get_or_insert(g, "b"),
            Err(CatalogError::MaxIdExceeded)
        ));

        // ...and neither the rejected peek nor the rejected insert left partial state.
        assert_eq!(cat.get_id(g, "b"), None);
        assert_eq!(cat.get_name(g, a), Some("a".to_owned()));
    }

    #[test]
    fn scoped_names_are_unique_per_graph() {
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

    #[test]
    fn scoped_name_round_trips_both_directions() {
        let mut cat = catalog();
        let graph_id = GraphId::from_raw(7);
        let id = cat.get_or_insert(graph_id, "person_idx").unwrap();
        assert_eq!(cat.get_id(graph_id, "person_idx"), Some(id));
        assert_eq!(cat.get_name(graph_id, id), Some("person_idx".to_owned()));
    }
}
