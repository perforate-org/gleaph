//! Federated vertex migration export/import on graph shards.

mod vertex;

pub use vertex::{
    export_local_vertex_for_migration, import_migrated_vertex, tombstone_migrated_vertex,
};
