//! Graph shard property-name catalog (shared bidirectional implementation).

use gleaph_graph_kernel::bidirectional_catalog::CatalogError;
pub use gleaph_graph_kernel::bidirectional_catalog::{BidirectionalCatalog, SparseFromOnePolicy};

pub type PropertyCatalogError = CatalogError<PropertyId>;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_structures::Memory;

pub type PropertyCatalog<MNameToId: Memory, MIdToName: Memory> =
    BidirectionalCatalog<PropertyId, MNameToId, MIdToName, SparseFromOnePolicy>;
