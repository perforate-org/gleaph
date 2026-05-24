//! Federated vertex migration export/import on graph shards.

pub(crate) mod incremental;
mod index;
mod prune_source;
mod vertex;

pub use incremental::{
    migration_apply_chunk, migration_cutover, migration_cutover_with_index,
    migration_maintenance_step, migration_maintenance_step_for, migration_reconcile,
    migration_staging_begin, migration_start, migration_status, migration_visibility_filter_needed,
    set_native_pending_apply, take_native_pending_apply, vertex_migration_state,
    vertex_visible_to_query,
};
pub use index::{
    exported_vertex_for_index_sync, remove_source_index_postings_for_vertex,
    sync_migration_index_postings,
};
pub use prune_source::{
    enqueue_prune_migrated_source, prune_migrated_source_maintenance_step,
    prune_migrated_source_maintenance_step_for,
};
