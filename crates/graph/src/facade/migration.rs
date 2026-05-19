//! Federated vertex migration export/import on graph shards.

mod index;
mod vertex;

pub use index::sync_migration_index_postings;
pub use vertex::{
    export_local_vertex_for_migration, import_migrated_vertex,
    import_migrated_vertex_with_index, tombstone_migrated_vertex,
};
