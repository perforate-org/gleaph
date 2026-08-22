//! Durable retirement records for dropped physical posting namespaces (ADR 0023 D6).
//!
//! `DROP INDEX` removes the catalog row before the postings it owned are purged from
//! graph-index. The purge obligation therefore cannot live in the catalog row or in an
//! active call: this collection is the sole durable identity of "this PhysicalIndexId's
//! postings must be purged until every frozen drain target confirms completion".
//!
//! Ownership boundaries:
//! - Router owns this record set (catalog SSOT); graph-index owns posting state and
//!   validates every resume cursor against the purge request identity.
//! - One record per PhysicalIndexId. Allocation is monotonic and never reused, so a
//!   deleted record can never be resurrected by a later CREATE INDEX.
//! - Drain targets are frozen at retirement time (`graph_index_lookup_targets` resolved
//!   BEFORE the destructive catalog mutation), so shard attach/detach between attempts
//!   can neither add nor drop purge work.
//! - A record is deleted exactly when its last pending target confirms `done`. There are
//!   no terminal tombstones; PhysicalIndexId non-reuse makes them unnecessary.

use candid::{CandidType, Principal, decode_one, encode_one};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;

use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{IndexPostingPurgeCursor, IndexPurgeKind};
use gleaph_graph_kernel::index::PhysicalIndexId;

use super::memory;

/// `PhysicalIndexId → pending posting-purge obligation` for one dropped index definition.
#[derive(CandidType, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetiredIndexRecord {
    pub graph_id: GraphId,
    pub kind: IndexPurgeKind,
    pub property_id: u32,
    /// Catalog edge label scope; ignored by vertex purges on the index side.
    pub label_id: u16,
    /// Targets whose drain has not yet been confirmed `done`, each carrying its own
    /// durable resume cursor (`None` = fresh purge on the next attempt).
    pub pending: Vec<RetirementTargetDrain>,
    /// IC timestamp when retirement was enqueued (observability only).
    pub enqueued_at_ns: u64,
}

/// One frozen drain target and its resumable bounded-purge position.
#[derive(CandidType, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetirementTargetDrain {
    pub canister: Principal,
    pub resume: Option<IndexPostingPurgeCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RetiredPhysicalIndexKey(pub(crate) PhysicalIndexId);

const RECORD_KEY_BYTES: usize = 8;

impl Storable for RetiredPhysicalIndexKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: RECORD_KEY_BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.raw().to_be_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        assert!(
            bytes.len() == RECORD_KEY_BYTES,
            "retired-index key bytes must be {RECORD_KEY_BYTES}, got {}",
            bytes.len()
        );
        let mut raw = [0u8; RECORD_KEY_BYTES];
        raw.copy_from_slice(bytes.as_ref());
        Self(
            PhysicalIndexId::new(u64::from_be_bytes(raw))
                .expect("a stored retired-index key is a valid nonzero PhysicalIndexId"),
        )
    }
}

impl Storable for RetiredIndexRecord {
    // Pending-target count is bounded by the graph's live index-canister fan-out, which is
    // far below the transport ceiling; unbounded keeps B-tree pages independent of any
    // assumed fan-out maximum.
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(encode_one(self).expect("encode RetiredIndexRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        encode_one(&self).expect("encode RetiredIndexRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        decode_one(bytes.as_ref()).expect("decode RetiredIndexRecord")
    }
}

thread_local! {
    pub(crate) static ROUTER_INDEX_RETIRED: RefCell<memory::StableIndexRetiredMap> =
        RefCell::new(memory::init_index_retired());
}

/// Durably records that `physical_index_id`'s postings must be purged from every frozen
/// target before the namespace may be considered gone.
pub(crate) fn enqueue_retirement(physical_index_id: PhysicalIndexId, record: RetiredIndexRecord) {
    ROUTER_INDEX_RETIRED.with_borrow_mut(|map| {
        let previous = map.insert(RetiredPhysicalIndexKey(physical_index_id), record);
        debug_assert!(
            previous.is_none(),
            "retirement records are never overwritten"
        );
    });
}

/// Returns the record for `physical_index_id`, or `None` once retired (or never enqueued).
pub(crate) fn lookup_retirement(physical_index_id: PhysicalIndexId) -> Option<RetiredIndexRecord> {
    ROUTER_INDEX_RETIRED.with_borrow(|map| map.get(&RetiredPhysicalIndexKey(physical_index_id)))
}

/// Scans up to `budget` records with key strictly greater than `after`, ascending.
/// Returns `(rows, last_examined_key)` where rows carry their raw PhysicalIndexId keys.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the retirement-drain lane and tests")
)]
pub(crate) fn scan_retirements_after(
    after: Option<u64>,
    budget: usize,
) -> (Vec<(u64, RetiredIndexRecord)>, Option<u64>) {
    ROUTER_INDEX_RETIRED.with_borrow(|map| {
        let start_bound = match after {
            Some(raw) => std::ops::Bound::Excluded(RetiredPhysicalIndexKey(
                PhysicalIndexId::new(raw).expect("scan cursor raw id"),
            )),
            None => std::ops::Bound::Unbounded,
        };
        let mut rows = Vec::new();
        let mut last_examined = None;
        for entry in map.range((start_bound, std::ops::Bound::Unbounded)) {
            if rows.len() == budget {
                break;
            }
            let raw = entry.key().0.raw();
            rows.push((raw, entry.value()));
            last_examined = Some(raw);
        }
        (rows, last_examined)
    })
}

/// Replaces one record's pending drain list. An empty list retires the namespace (the
/// record is deleted); a non-empty list requires an existing record and preserves its
/// immutable identity fields.
pub(crate) fn persist_pending(
    physical_index_id: PhysicalIndexId,
    pending: Vec<RetirementTargetDrain>,
) {
    let key = RetiredPhysicalIndexKey(physical_index_id);
    ROUTER_INDEX_RETIRED.with_borrow_mut(|map| {
        if pending.is_empty() {
            map.remove(&key);
            return;
        }
        let previous = map
            .get(&key)
            .unwrap_or_else(|| panic!("persist_pending requires an existing retirement record"));
        map.insert(
            key,
            RetiredIndexRecord {
                pending,
                ..previous
            },
        );
    });
}

#[cfg(test)]
pub(crate) fn clear_for_test() {
    ROUTER_INDEX_RETIRED.with_borrow_mut(|map| map.clear_new());
}
