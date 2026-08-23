//! Durable Graph-owned scope records for ADR 0059 canonical exports.

use gleaph_graph_kernel::canonical_export::CanonicalExportRecord;
use gleaph_graph_kernel::index::PhysicalIndexId;
use ic_stable_structures::{Memory, StableBTreeMap};

/// One lifecycle record per physical posting namespace. Cursor positions are deliberately not
/// stored here: the migration owner persists the opaque token returned by each export page.
pub(crate) struct CanonicalExportScopeStore<M: Memory> {
    scopes: StableBTreeMap<PhysicalIndexId, CanonicalExportRecord, M>,
}

impl<M: Memory> CanonicalExportScopeStore<M> {
    pub(crate) fn init(memory: M) -> Self {
        Self {
            scopes: StableBTreeMap::init(memory),
        }
    }

    pub(crate) fn get(&self, physical_index_id: PhysicalIndexId) -> Option<CanonicalExportRecord> {
        self.scopes.get(&physical_index_id)
    }

    pub(crate) fn insert(
        &mut self,
        physical_index_id: PhysicalIndexId,
        record: CanonicalExportRecord,
    ) -> Option<CanonicalExportRecord> {
        self.scopes.insert(physical_index_id, record)
    }

    pub(crate) fn remove(
        &mut self,
        physical_index_id: PhysicalIndexId,
    ) -> Option<CanonicalExportRecord> {
        self.scopes.remove(&physical_index_id)
    }

    pub(crate) fn into_memory(self) -> M {
        self.scopes.into_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::canonical_export::{
        CanonicalExportPhase, CanonicalExportRecord, CanonicalExportScope, CanonicalExportTarget,
    };
    use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
    use ic_stable_structures::VectorMemory;

    #[test]
    fn scope_store_reopens_from_the_same_stable_memory() {
        let physical = PhysicalIndexId::new(7).expect("non-zero");
        let scope = CanonicalExportScope {
            graph_id: GraphId::from_raw(1),
            index_name_id: IndexNameId::from_raw(2),
            catalog_epoch: 3,
            target: CanonicalExportTarget::Vertex {
                label_id: 3,
                property_id: PropertyId::from_raw(4),
                record_source: None,
            },
            inline: None,
        };
        let record = CanonicalExportRecord {
            scope: scope.clone(),
            phase: CanonicalExportPhase::Sealing,
            epoch: 4,
            admitted_through: 5,
            drained_through: 3,
        };
        let mut first = CanonicalExportScopeStore::init(VectorMemory::default());
        first.insert(physical, record.clone());
        let memory = first.into_memory();
        let reopened = CanonicalExportScopeStore::init(memory);
        // The exact lifecycle record — including the captured seal watermark (`admitted_through`)
        // and the contiguous drain watermark — must survive a stable reopen byte-for-byte.
        assert_eq!(reopened.get(physical), Some(record));
        assert_eq!(reopened.get(physical).map(|record| record.epoch), Some(4));
        assert_eq!(
            reopened.get(physical).map(|record| record.admitted_through),
            Some(5)
        );
        assert_eq!(
            reopened.get(physical).map(|record| record.drained_through),
            Some(3)
        );
    }
}
