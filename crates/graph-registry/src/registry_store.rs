//! `StableBTreeMap` persistence for graph registry entries (wasm32 canister).

#[cfg(any(target_arch = "wasm32", test))]
mod imp {
    use std::borrow::Cow;
    #[cfg(target_arch = "wasm32")]
    use std::cell::RefCell;

    #[cfg(target_arch = "wasm32")]
    use ic_stable_structures::DefaultMemoryImpl;
    #[cfg(target_arch = "wasm32")]
    use ic_stable_structures::memory_manager::VirtualMemory;
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};
    use ic_stable_structures::storable::Bound;
    use ic_stable_structures::{StableBTreeMap, Storable};

    use crate::GraphEntry;

    pub const MEMORY_ID_REGISTRY_MAP: MemoryId = MemoryId::new(0);

    #[cfg(target_arch = "wasm32")]
    type RegistryBackingMem = VirtualMemory<DefaultMemoryImpl>;
    #[cfg(target_arch = "wasm32")]
    type RegistryMap = StableBTreeMap<GraphNameKey, EntryBlob, RegistryBackingMem>;

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct GraphNameKey(Vec<u8>);

    impl GraphNameKey {
        fn from_name(name: &str) -> Self {
            Self(name.as_bytes().to_vec())
        }

        #[cfg(target_arch = "wasm32")]
        fn to_name(&self) -> String {
            // Graph names are ASCII-only per validate_graph_name.
            String::from_utf8_lossy(&self.0).into_owned()
        }
    }

    impl Storable for GraphNameKey {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(&self.0)
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            Self(bytes.into_owned())
        }

        const BOUND: Bound = Bound::Unbounded;
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct EntryBlob(Vec<u8>);

    impl Storable for EntryBlob {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(&self.0)
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            Self(bytes.into_owned())
        }

        const BOUND: Bound = Bound::Unbounded;
    }

    #[cfg(target_arch = "wasm32")]
    thread_local! {
        static MEMORY_MANAGER: RefCell<Option<MemoryManager<DefaultMemoryImpl>>> = const { RefCell::new(None) };
    }

    #[cfg(target_arch = "wasm32")]
    pub fn ensure_installed() {
        MEMORY_MANAGER.with(|slot| {
            if slot.borrow().is_none() {
                *slot.borrow_mut() = Some(MemoryManager::init(DefaultMemoryImpl::default()));
            }
        });
    }

    #[cfg(target_arch = "wasm32")]
    fn open_map() -> RegistryMap {
        ensure_installed();
        MEMORY_MANAGER.with(|slot| {
            let borrow = slot.borrow();
            let mm = borrow.as_ref().expect("memory manager installed");
            StableBTreeMap::init(mm.get(MEMORY_ID_REGISTRY_MAP))
        })
    }

    /// Writes the full in-memory registry snapshot to stable (simple full refresh).
    #[cfg(target_arch = "wasm32")]
    pub fn persist_full(snapshot: &crate::RegistryStableState) {
        let mut map = open_map();
        let keys: Vec<GraphNameKey> = map.iter().map(|e| e.key().clone()).collect();
        for k in keys {
            map.remove(&k);
        }
        for (name, entry) in snapshot {
            let enc = candid::encode_one(entry).expect("encode GraphEntry for stable");
            map.insert(GraphNameKey::from_name(name), EntryBlob(enc));
        }
    }

    /// Loads all entries from stable for [`crate::restore_registry_state`] (wasm init / post_upgrade).
    #[cfg(target_arch = "wasm32")]
    pub fn load_all() -> crate::RegistryStableState {
        let map = open_map();
        let mut out = crate::RegistryStableState::new();
        for e in map.iter() {
            let name = e.key().to_name();
            let entry: GraphEntry =
                candid::decode_one(&e.value().0).expect("decode GraphEntry from stable");
            out.push((name, entry));
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use candid::Principal;
        use gleaph_gql_ic::graph_registry::{GraphStatus, ProvisioningState};
        use ic_stable_structures::VectorMemory;

        #[test]
        fn roundtrip_stable_btree_on_vector_memory() {
            let mem_mgr = MemoryManager::init(VectorMemory::default());
            let mut map: StableBTreeMap<GraphNameKey, EntryBlob, _> =
                StableBTreeMap::init(mem_mgr.get(MEMORY_ID_REGISTRY_MAP));

            let owner = Principal::from_text("2vxsx-fae").unwrap();
            let canister = Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").unwrap();
            let entry = GraphEntry {
                graph_name: "tenant.main".to_owned(),
                canister_id: canister,
                owner,
                admins: vec![owner],
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 42,
                provisioning_state: ProvisioningState::None,
            };
            let enc = candid::encode_one(&entry).unwrap();
            map.insert(GraphNameKey::from_name("tenant.main"), EntryBlob(enc));

            let map2: StableBTreeMap<GraphNameKey, EntryBlob, _> =
                StableBTreeMap::init(mem_mgr.get(MEMORY_ID_REGISTRY_MAP));
            let v = map2
                .get(&GraphNameKey::from_name("tenant.main"))
                .expect("key");
            let decoded: GraphEntry = candid::decode_one(&v.0).unwrap();
            assert_eq!(decoded, entry);
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use imp::{ensure_installed, load_all, persist_full};

#[cfg(not(target_arch = "wasm32"))]
pub fn persist_full(_snapshot: &crate::RegistryStableState) {}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)] // host stub; wasm calls `imp::load_all` via re-export
pub fn load_all() -> crate::RegistryStableState {
    Vec::new()
}
