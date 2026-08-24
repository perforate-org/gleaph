//! Graph shard property-name catalog (shared bidirectional implementation).

use gleaph_graph_kernel::bidirectional_catalog::CatalogError;
use gleaph_graph_kernel::entry::PropertyId;

pub type PropertyCatalogError = CatalogError<PropertyId>;
