//! GraphStore access to the graph-shard-local unique value table (ADR 0030 slice 10).
//!
//! These back the `ShardLocalGlobal` fast path: the acquire path preflights every claim and inserts
//! all of them inside the canonical write segment; the release path frees a value by owner match;
//! the DROP drain purges a constraint's entries and gates `Removed` on the table being empty.

use super::super::stable::GRAPH_LOCAL_UNIQUE_VALUES;
use super::super::stable::local_unique::LocalPurgeProgress;
use super::GraphStore;
use gleaph_graph_kernel::entry::ConstraintNameId;

impl GraphStore {
    /// Whether `(constraint_id, encoded_value)` is currently claimed locally.
    pub(crate) fn local_unique_contains(
        &self,
        constraint_id: ConstraintNameId,
        encoded_value: &[u8],
    ) -> bool {
        GRAPH_LOCAL_UNIQUE_VALUES.with_borrow(|table| table.contains(constraint_id, encoded_value))
    }

    /// Claims `(constraint_id, encoded_value)` for `owner_element_id`. Must run inside the canonical
    /// write segment, after the acquire preflight has proven the value absent.
    pub(crate) fn local_unique_insert(
        &self,
        constraint_id: ConstraintNameId,
        encoded_value: Vec<u8>,
        owner_element_id: Vec<u8>,
    ) {
        GRAPH_LOCAL_UNIQUE_VALUES
            .with_borrow_mut(|table| table.insert(constraint_id, encoded_value, owner_element_id));
    }

    /// Frees `(constraint_id, encoded_value)` iff currently owned by `owner_element_id` (owner match
    /// guards against a stale release freeing another element's value). Returns whether it removed.
    pub(crate) fn local_unique_remove_if_owner(
        &self,
        constraint_id: ConstraintNameId,
        encoded_value: &[u8],
        owner_element_id: &[u8],
    ) -> bool {
        GRAPH_LOCAL_UNIQUE_VALUES.with_borrow_mut(|table| {
            table.remove_if_owner(constraint_id, encoded_value, owner_element_id)
        })
    }

    /// Deletes up to `budget` of the constraint's local entries (DROP purge); returns the count
    /// removed and whether the constraint's local range is now empty.
    pub(crate) fn local_unique_purge(
        &self,
        constraint_id: ConstraintNameId,
        budget: usize,
    ) -> LocalPurgeProgress {
        GRAPH_LOCAL_UNIQUE_VALUES.with_borrow_mut(|table| table.purge(constraint_id, budget))
    }

    /// Whether the constraint has no remaining local entries (DROP completion gate).
    pub(crate) fn local_unique_is_empty(&self, constraint_id: ConstraintNameId) -> bool {
        GRAPH_LOCAL_UNIQUE_VALUES.with_borrow(|table| table.is_empty(constraint_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(raw: u16) -> ConstraintNameId {
        ConstraintNameId::from_raw(raw)
    }

    #[test]
    fn insert_contains_and_owner_matched_remove() {
        let store = GraphStore::new();
        let c = cid(7);
        assert!(!store.local_unique_contains(c, b"alice@example.com"));

        store.local_unique_insert(c, b"alice@example.com".to_vec(), vec![1u8; 8]);
        assert!(store.local_unique_contains(c, b"alice@example.com"));

        // A release with the wrong owner must not free the value.
        assert!(!store.local_unique_remove_if_owner(c, b"alice@example.com", &[9u8; 8]));
        assert!(store.local_unique_contains(c, b"alice@example.com"));

        // The owning element frees it.
        assert!(store.local_unique_remove_if_owner(c, b"alice@example.com", &[1u8; 8]));
        assert!(!store.local_unique_contains(c, b"alice@example.com"));
    }

    #[test]
    fn purge_is_bounded_scoped_and_empties_only_its_constraint() {
        let store = GraphStore::new();
        let target = cid(100);
        let other = cid(101);
        for n in 0u8..5 {
            store.local_unique_insert(target, vec![n], vec![n; 8]);
        }
        store.local_unique_insert(other, vec![42], vec![42; 8]);

        assert!(!store.local_unique_is_empty(target));

        // First bounded page removes at most `budget`, not yet done.
        let page = store.local_unique_purge(target, 2);
        assert_eq!(page.removed, 2);
        assert!(!page.done);

        // Drain the rest; the final page reports done.
        let page = store.local_unique_purge(target, 100);
        assert_eq!(page.removed, 3);
        assert!(page.done);
        assert!(store.local_unique_is_empty(target));

        // A neighbouring constraint's entry is untouched by the scoped purge.
        assert!(store.local_unique_contains(other, &[42]));
        assert!(!store.local_unique_is_empty(other));
    }
}
