//! Stable-memory-backed graph fragments (catalogs, property stores, id allocator, init).

pub(crate) mod memory;

pub(crate) mod edge_ids;
pub(crate) mod edge_properties;
pub(crate) mod label_catalog;
pub(crate) mod property_catalog;
pub(crate) mod vertex_labels;
pub(crate) mod vertex_properties;
