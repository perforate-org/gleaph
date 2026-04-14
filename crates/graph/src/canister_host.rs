//! Single [`MemoryManager`] root for graph PMA (including tail [`gleaph_graph_store::low_level::pma_stable_root`])
//! and the service aggregate stable cell.
//!
//! See `STABLE_MEMORY_LAYOUT.md` for [`MemoryId`] assignments.

use gleaph_graph_store::integration::GraphStoreKernelOverlay;
use gleaph_graph_store::{GraphStore, GraphStoreResult};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::storable::Bound;
use ic_stable_structures::{DefaultMemoryImpl, StableCell, Storable};
use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use crate::service::{GleaphService, GleaphServiceCoreSnapshot, GleaphServiceSnapshot};

/// Stable backing for candid-encoded payloads ([`GleaphServiceSnapshot`], …).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CandidBlob(pub Vec<u8>);

impl Storable for CandidBlob {
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

pub type CanisterGraphMemory = VirtualMemory<DefaultMemoryImpl>;

pub const MEMORY_ID_GRAPH: MemoryId = MemoryId::new(0);
pub const MEMORY_ID_SERVICE: MemoryId = MemoryId::new(1);
/// Reserved stable slot (previously used for a legacy region-manager cell).
#[allow(dead_code)]
pub const MEMORY_ID_LEGACY_REGION_MANAGER: MemoryId = MemoryId::new(2);
/// Graph catalog blob (`StableBTreeMap` wire) split out from the monolithic [`GleaphServiceSnapshot`].
pub const MEMORY_ID_GRAPH_CATALOG: MemoryId = MemoryId::new(3);

thread_local! {
    static CANISTER_HOST: RefCell<Option<CanisterHost>> = const { RefCell::new(None) };
}

pub struct CanisterHost {
    #[allow(dead_code)]
    memory_manager: MemoryManager<DefaultMemoryImpl>,
    graph_memory: Rc<CanisterGraphMemory>,
    graph_facade: GraphStore<CanisterGraphMemory>,
    service_cell: StableCell<CandidBlob, CanisterGraphMemory>,
    graph_catalog_cell: StableCell<CandidBlob, CanisterGraphMemory>,
    pub service: GleaphService,
}

impl CanisterHost {
    pub fn ensure_installed() {
        CANISTER_HOST.with(|slot| {
            if slot.borrow().is_none() {
                Self::install_fresh();
            }
        });
    }

    pub fn install_fresh() {
        let memory_manager = MemoryManager::init(DefaultMemoryImpl::default());
        let graph_vm = memory_manager.get(MEMORY_ID_GRAPH);
        let graph_memory = Rc::new(graph_vm);
        let graph_facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
            gleaph_graph_store::low_level::BucketSizeInPages::DEFAULT,
            Rc::clone(&graph_memory),
        )
        .expect("bootstrap graph PMA");

        let service_cell =
            StableCell::init(memory_manager.get(MEMORY_ID_SERVICE), CandidBlob::default());
        let graph_catalog_cell = StableCell::init(
            memory_manager.get(MEMORY_ID_GRAPH_CATALOG),
            CandidBlob::default(),
        );
        let service = GleaphService::new();

        let mut host = Self {
            memory_manager,
            graph_memory,
            graph_facade,
            service_cell,
            graph_catalog_cell,
            service,
        };
        host.flush_graph_metadata_stable();
        host.persist_service_stable();
        CANISTER_HOST.with(|slot| {
            *slot.borrow_mut() = Some(host);
        });
    }

    pub fn restore_after_upgrade() {
        let memory_manager = MemoryManager::init(DefaultMemoryImpl::default());
        let graph_vm = memory_manager.get(MEMORY_ID_GRAPH);
        let graph_memory = Rc::new(graph_vm);
        let graph_facade =
            GraphStore::hydrate_from_graph_stable_memory((*graph_memory).clone())
                .expect("hydrate graph PMA");

        let service_cell =
            StableCell::init(memory_manager.get(MEMORY_ID_SERVICE), CandidBlob::default());
        let graph_catalog_cell = StableCell::init(
            memory_manager.get(MEMORY_ID_GRAPH_CATALOG),
            CandidBlob::default(),
        );
        let blob = service_cell.get();
        let catalog_blob = graph_catalog_cell.get().0.clone();
        let service = if blob.0.is_empty() {
            GleaphService::new()
        } else if let Ok(core) = candid::decode_one::<GleaphServiceCoreSnapshot>(&blob.0) {
            GleaphService::from_core_and_catalog(core, catalog_blob)
                .expect("restore Gleaph service")
        } else {
            let snap: GleaphServiceSnapshot =
                candid::decode_one(&blob.0).expect("decode legacy monolithic service snapshot");
            GleaphService::from_snapshot(snap).expect("restore Gleaph service")
        };

        CANISTER_HOST.with(|slot| {
            *slot.borrow_mut() = Some(Self {
                memory_manager,
                graph_memory,
                graph_facade,
                service_cell,
                graph_catalog_cell,
                service,
            });
        });
    }

    pub fn with<R>(f: impl FnOnce(&mut Self) -> R) -> R {
        CANISTER_HOST.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let host = borrow.as_mut().expect("canister host must be installed");
            f(host)
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub fn bind_graph_overlay(&mut self) -> GraphStoreKernelOverlay<'_, CanisterGraphMemory> {
        self.graph_facade
            .bind_kernel_overlay(self.graph_memory.as_ref())
    }

    pub fn with_graph_overlay_and_service<R>(
        &mut self,
        f: impl for<'a> FnOnce(
            &mut GleaphService,
            &mut GraphStoreKernelOverlay<'a, CanisterGraphMemory>,
        ) -> R,
    ) -> R {
        let mut overlay = self
            .graph_facade
            .bind_kernel_overlay(self.graph_memory.as_ref());
        f(&mut self.service, &mut overlay)
    }

    pub fn flush_graph_to_stable(&mut self) -> GraphStoreResult<()> {
        self.graph_facade
            .try_refresh_and_write_dirty_to_stable_memory(self.graph_memory.as_ref())
            .map(|_| ())
    }

    pub fn flush_graph_metadata_stable(&mut self) {
        gleaph_graph_store::low_level::write_region_manager_footer(
            self.graph_memory.as_ref(),
            &*self.graph_facade.manager.borrow(),
        )
        .expect("write PMA stable root footer");
    }

    pub fn flush_graph_stable_full(&mut self) {
        let _ = self.flush_graph_to_stable();
        self.flush_graph_metadata_stable();
    }

    pub fn persist_service_stable(&mut self) {
        let snap = self.service.snapshot();
        let (core, catalog) = snap.split_for_stable_cells();
        let enc = candid::encode_one(&core).expect("encode service core snapshot");
        let _prev = self.service_cell.set(CandidBlob(enc));
        let _prev_cat = self.graph_catalog_cell.set(CandidBlob(catalog));
    }
}
