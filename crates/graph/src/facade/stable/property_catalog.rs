//! Graph shard property-name catalog (shared bidirectional implementation).

pub use gleaph_graph_kernel::bidirectional_catalog::{BidirectionalCatalog, SparseFromOnePolicy};
use gleaph_graph_kernel::bidirectional_catalog::CatalogError;

pub type PropertyCatalogError = CatalogError<PropertyId>;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_structures::Memory;

pub type PropertyCatalog<MNameToId: Memory, MIdToName: Memory> =
    BidirectionalCatalog<PropertyId, MNameToId, MIdToName, SparseFromOnePolicy>;
