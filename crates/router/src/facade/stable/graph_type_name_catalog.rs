//! GQL graph type name ↔ [`GraphTypeId`] catalog (ADR 0014).

use gleaph_graph_catalog::CatalogError;
use gleaph_graph_kernel::bidirectional_catalog::CatalogError as KernelCatalogError;
use gleaph_graph_kernel::entry::GraphTypeId;

fn kernel_catalog_error(err: KernelCatalogError<GraphTypeId>) -> CatalogError {
    match err {
        KernelCatalogError::IdExhausted => {
            CatalogError::Unsupported("graph type id space exhausted".into())
        }
        other => CatalogError::Unsupported(format!("graph type name catalog: {other}")),
    }
}

pub(crate) struct RouterGraphTypeLookup<'a> {
    catalog: &'a mut super::memory::StableGraphTypeNameCatalog,
}

impl<'a> RouterGraphTypeLookup<'a> {
    pub(crate) fn new(catalog: &'a mut super::memory::StableGraphTypeNameCatalog) -> Self {
        Self { catalog }
    }
}

impl gleaph_graph_catalog::GraphTypeLookup for RouterGraphTypeLookup<'_> {
    fn lookup_graph_type_id(&self, type_name: &str) -> Option<GraphTypeId> {
        self.catalog.get_id(type_name)
    }

    fn intern_graph_type_id(&mut self, type_name: &str) -> Result<GraphTypeId, CatalogError> {
        self.catalog
            .get_or_insert(type_name)
            .map_err(kernel_catalog_error)
    }

    fn remove_graph_type_by_name(&mut self, type_name: &str) -> Option<GraphTypeId> {
        self.catalog.remove_by_name(type_name)
    }
}
