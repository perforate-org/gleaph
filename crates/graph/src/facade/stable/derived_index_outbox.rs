//! Durable FIFO outbox for derived property, label, and vector-index work.
//!
//! The outbox is the persistence boundary for successful Graph mutations.  The mutation path may
//! collect derived-index deltas in heap memory while it runs, but once the mutation commits those
//! deltas are appended here before the message returns.  Maintenance removes only acknowledged
//! prefixes, so an upgrade or retry cannot discard unacknowledged work.

use super::repair_journal::RepairPostingOp;
use candid::{Decode, Encode};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

/// One durable derived-index operation awaiting its first delivery attempt.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct DerivedIndexOutboxEntry {
    pub mutation_id: u64,
    pub op: RepairPostingOp,
}

impl Storable for DerivedIndexOutboxEntry {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DerivedIndexOutboxEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DerivedIndexOutboxEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode DerivedIndexOutboxEntry")
    }
}

/// Stable FIFO keyed by a monotonic sequence.  The sequence is the durable cursor identity;
/// callers acknowledge only entries they know the target canister accepted.
pub struct DerivedIndexOutbox<M: Memory> {
    map: StableBTreeMap<u64, DerivedIndexOutboxEntry, M>,
}

impl<M: Memory> DerivedIndexOutbox<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    fn next_seq(&self) -> u64 {
        self.map.last_key_value().map_or(0, |(seq, _)| {
            seq.checked_add(1)
                .expect("derived-index outbox sequence overflow")
        })
    }

    /// Appends one DML's derived-index operations in their existing order.
    pub fn append_all(&mut self, mutation_id: u64, ops: impl IntoIterator<Item = RepairPostingOp>) {
        let mut next = self.next_seq();
        for op in ops {
            self.map
                .insert(next, DerivedIndexOutboxEntry { mutation_id, op });
            next = next
                .checked_add(1)
                .expect("derived-index outbox sequence overflow");
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.map.len()
    }

    /// Returns the oldest durable prefix without advancing the cursor.
    pub fn peek(&self, limit: usize) -> Vec<(u64, DerivedIndexOutboxEntry)> {
        self.map
            .iter()
            .take(limit)
            .map(|entry| (*entry.key(), entry.value()))
            .collect()
    }

    /// Acknowledges one entry.  The maintenance owner must call this only for an accepted entry.
    pub fn remove(&mut self, seq: u64) {
        self.map.remove(&seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn entry(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::VertexProperty {
            remove: false,
            property_id: 7,
            payload_bytes: vec![1, 2, 3],
            vertex_id,
        }
    }

    #[test]
    fn appends_in_order_and_acknowledges_only_removed_entries() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(11, [entry(3), entry(4)]);

        assert_eq!(outbox.len(), 2);
        let first = outbox.peek(1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, 0);
        assert_eq!(first[0].1.mutation_id, 11);
        assert_eq!(first[0].1.op, entry(3));

        outbox.remove(first[0].0);
        let remaining = outbox.peek(10);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, 1);
        assert_eq!(remaining[0].1.op, entry(4));
    }

    #[test]
    fn sequence_continues_after_a_prefix_is_removed() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(1, [entry(1), entry(2)]);
        outbox.remove(0);
        outbox.append_all(2, [entry(3)]);

        let entries = outbox.peek(10);
        assert_eq!(
            entries.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(entries[1].1.mutation_id, 2);
    }
}
