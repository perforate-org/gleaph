//! Federated vertex migration export/import on graph shards.

mod index;
mod vertex;

pub use index::{remove_source_index_postings_for_vertex, sync_migration_index_postings};
pub use vertex::{
    export_local_vertex_for_migration, import_migrated_vertex, import_migrated_vertex_with_index,
    tombstone_migrated_vertex, tombstone_migrated_vertex_with_index,
};
