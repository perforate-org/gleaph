#[path = "storage_region_io.rs"]
pub(crate) mod region_io;
#[path = "storage_scan.rs"]
mod scan;

pub use region_io::{
    ensure_pidx_v3_btree_subregion_for_hydrate, read_pidx_v3_header_from_stable_memory,
    read_property_index_region_bytes, read_property_index_region_header_from_stable_memory,
    read_property_index_region_magic, read_property_index_snapshot_from_stable_memory,
    read_property_index_snapshot_section_from_stable_memory,
    read_property_index_storage_image_from_stable_memory,
    sync_property_index_pidx_v3_header_to_stable_memory,
    write_property_index_snapshot_to_stable_memory,
    write_property_index_stable_equality_to_stable_memory,
    write_property_index_storage_image_to_stable_memory,
};
pub use scan::{
    scan_edge_property_index_property_prefix_from_stable_memory,
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory,
};
