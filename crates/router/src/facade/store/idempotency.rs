//! Mutation idempotency and client mutation journal.

use super::super::stable::label_stats::{
    ClientMutationKey, RouterMutationRecord, RouterMutationShard,
};
use super::super::stable::{ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_MUTATION_COUNTER};
use super::{
    CLIENT_MUTATION_KEY_TTL_NS, ClientMutationReservation, RouterStore, ic_time_ns,
    validate_client_mutation_key,
};
use crate::facade::auth;
use crate::state::RouterError;
use crate::types::{AdminSweepMutationKeysStepResult, ShardId};
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::{MutationId, ResolvedLabelTable, ResolvedPropertyTable};
use std::cell::RefCell;
use std::ops::Bound;

thread_local! {
    /// Ephemeral round-robin cursor for amortized GC (ADR 0025, mechanism B). It is
    /// heap-only on purpose: resetting to the start on upgrade just restarts the lap,
    /// and the journal itself (the source of truth) is fully stable.
    static MUTATION_GC_CURSOR: RefCell<Option<ClientMutationKey>> = const { RefCell::new(None) };
}

/// Entries examined per amortized GC step on the mutation-reservation path. Each new
/// reservation evicts up to this many expired records, so eviction keeps pace with the
/// only source of growth (new client keys) and the journal converges to its TTL window.
const MUTATION_GC_BUDGET: u32 = 2;

#[cfg(test)]
pub(crate) fn reset_mutation_gc_cursor_for_test() {
    MUTATION_GC_CURSOR.with_borrow_mut(|cursor| *cursor = None);
}

/// Scan up to `budget` records starting strictly after `start_after`, removing those
/// past [`CLIENT_MUTATION_KEY_TTL_NS`] that are not actively routing. Returns
/// `(scanned, removed, last_examined_key)`. `created_at_ns` on the record stays the sole
/// source of truth for age.
fn evict_expired_client_mutation_keys(
    start_after: Option<&ClientMutationKey>,
    budget: usize,
    now: u64,
) -> (u32, u32, Option<ClientMutationKey>) {
    let mut scanned: u32 = 0;
    let mut last_key: Option<ClientMutationKey> = None;
    let mut expired: Vec<ClientMutationKey> = Vec::new();
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
        let lower = match start_after {
            Some(key) => Bound::Excluded(key.clone()),
            None => Bound::Unbounded,
        };
        for entry in m.range((lower, Bound::Unbounded)).take(budget) {
            let key = entry.key().clone();
            let record = entry.value();
            scanned += 1;
            if !record.routing_in_progress
                && now.saturating_sub(record.created_at_ns) > CLIENT_MUTATION_KEY_TTL_NS
            {
                expired.push(key.clone());
            }
            last_key = Some(key);
        }
    });
    let removed = expired.len() as u32;
    if removed > 0 {
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            for key in &expired {
                m.remove(key);
            }
        });
    }
    (scanned, removed, last_key)
}

/// Drop the heavy fields of a fully completed + projected record. The resolved
/// label/property tables and the shard fan-out are never read again once replay
/// short-circuits on `completed_row_count` (ADR 0025, mechanism E); `mutation_id`,
/// `created_at_ns`, `request_fingerprint`, and `completed_row_count` remain for
/// idempotent replay and TTL eviction.
fn compact_completed_record(record: &mut RouterMutationRecord) {
    record.resolved_labels = None;
    record.resolved_properties = None;
    record.shards = Vec::new();
}

impl RouterStore {
    pub fn allocate_mutation_id(&self) -> Result<MutationId, RouterError> {
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| {
            let next = counter
                .get()
                .checked_add(1)
                .ok_or_else(|| RouterError::IdExhausted("mutation_id".into()))?;
            if next == 0 {
                return Err(RouterError::IdExhausted("mutation_id".into()));
            }
            counter.set(next);
            Ok(next)
        })
    }

    pub fn reserve_mutation_id_for_client_key(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        request_fingerprint: Vec<u8>,
    ) -> Result<ClientMutationReservation, RouterError> {
        self.reserve_mutation_id_for_client_key_at(
            caller,
            graph_id,
            client_key,
            request_fingerprint,
            ic_time_ns(),
        )
    }

    pub(crate) fn reserve_mutation_id_for_client_key_at(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        request_fingerprint: Vec<u8>,
        now: u64,
    ) -> Result<ClientMutationReservation, RouterError> {
        validate_client_mutation_key(client_key)?;
        let key = client_mutation_key(caller, graph_id, client_key);
        if let Some(mut record) = ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key)) {
            if now.saturating_sub(record.created_at_ns) > CLIENT_MUTATION_KEY_TTL_NS {
                return Err(RouterError::InvalidArgument(
                    "client_mutation_key expired; use a new key for a new mutation".into(),
                ));
            }
            if record.request_fingerprint != request_fingerprint {
                return Err(RouterError::Conflict(
                    "client_mutation_key was already used for a different request".into(),
                ));
            }
            if record.routing_in_progress {
                return Err(RouterError::Conflict(
                    "client_mutation_key is already in progress; retry later".into(),
                ));
            }
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.routing_in_progress = true;
                let mutation_id = record.mutation_id;
                ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
                    m.insert(key, record);
                });
                return Ok(ClientMutationReservation {
                    mutation_id,
                    routing_owner: true,
                });
            }
            return Ok(ClientMutationReservation {
                mutation_id: record.mutation_id,
                routing_owner: false,
            });
        }
        let mutation_id = self.allocate_mutation_id()?;
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            m.insert(
                key,
                RouterMutationRecord::new(mutation_id, now, request_fingerprint),
            );
        });
        // Amortized GC (ADR 0025, mechanism B): every new reservation evicts a bounded
        // slice of expired records, so the journal stays bounded automatically without a
        // timer or a separate time-ordered index.
        self.gc_expired_client_mutation_keys(now);
        Ok(ClientMutationReservation {
            mutation_id,
            routing_owner: true,
        })
    }

    /// Amortized, automatic eviction of expired records. Advances a heap round-robin
    /// cursor over the journal keyspace, examining [`MUTATION_GC_BUDGET`] records per
    /// call and wrapping at the end. Driven by [`reserve_mutation_id_for_client_key_at`]
    /// (the sole growth source), so the journal converges to its TTL working set.
    pub(crate) fn gc_expired_client_mutation_keys(&self, now: u64) {
        let start = MUTATION_GC_CURSOR.with_borrow(|cursor| cursor.clone());
        let (scanned, _removed, last_key) =
            evict_expired_client_mutation_keys(start.as_ref(), MUTATION_GC_BUDGET as usize, now);
        let next = if scanned < MUTATION_GC_BUDGET {
            None
        } else {
            last_key
        };
        MUTATION_GC_CURSOR.with_borrow_mut(|cursor| *cursor = next);
    }

    /// Remove expired client-mutation idempotency records in a bounded, paginated
    /// pass. The journal (`ROUTER_MUTATION_BY_CLIENT_KEY`) is keyed by
    /// `(caller, graph_id, client_key)` with no time ordering, so eviction scans a
    /// budgeted slice of the keyspace per call; the operator drives it to
    /// completion by feeding `next_cursor` back as `start_after` (the router has no
    /// timer — maintenance is operator-driven, like backfill / projection).
    ///
    /// Only records past [`CLIENT_MUTATION_KEY_TTL_NS`] that are **not**
    /// `routing_in_progress` are removed, so an in-flight reservation is never
    /// yanked. Records within the TTL window are retained for idempotent replay.
    pub fn admin_sweep_expired_client_mutation_keys(
        &self,
        caller: Principal,
        start_after: Option<ClientMutationKey>,
        max_scan: u32,
    ) -> Result<AdminSweepMutationKeysStepResult, RouterError> {
        self.admin_sweep_expired_client_mutation_keys_at(
            caller,
            start_after,
            max_scan,
            ic_time_ns(),
        )
    }

    pub(crate) fn admin_sweep_expired_client_mutation_keys_at(
        &self,
        caller: Principal,
        start_after: Option<ClientMutationKey>,
        max_scan: u32,
        now: u64,
    ) -> Result<AdminSweepMutationKeysStepResult, RouterError> {
        auth::require_admin(&caller)?;
        if max_scan == 0 {
            return Err(RouterError::InvalidArgument(
                "max_scan must be greater than zero".into(),
            ));
        }

        let (scanned, removed, last_key) =
            evict_expired_client_mutation_keys(start_after.as_ref(), max_scan as usize, now);

        // Fewer entries scanned than the budget means the range was exhausted.
        let done = scanned < max_scan;
        Ok(AdminSweepMutationKeysStepResult {
            scanned,
            removed,
            next_cursor: if done { None } else { last_key },
            done,
        })
    }

    pub fn router_mutation_record(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Option<RouterMutationRecord> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key))
    }

    pub fn record_router_mutation_shards(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        shards: Vec<RouterMutationShard>,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.resolved_labels = Some(resolved_labels);
                record.resolved_properties = Some(resolved_properties);
                record.routing_in_progress = false;
                record.shards = shards;
                m.insert(key, record);
            }
            Ok(())
        })
    }

    pub fn record_router_mutation_completed_without_shards(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        row_count: u64,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.resolved_labels = Some(resolved_labels);
                record.resolved_properties = Some(resolved_properties);
                record.completed_row_count = Some(row_count);
                record.routing_in_progress = false;
                m.insert(key, record);
            }
            Ok(())
        })
    }

    pub fn abandon_router_mutation_routing_reservation(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            record.routing_in_progress = false;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_completed(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        shard_id: ShardId,
        row_count: u64,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let shard = record
                .shards
                .iter_mut()
                .find(|shard| shard.shard_id == shard_id)
                .ok_or(RouterError::ShardNotRegistered)?;
            shard.completed = true;
            shard.projection_advanced = false;
            shard.row_count = row_count;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_projection_advanced(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let shard = record
                .shards
                .iter_mut()
                .find(|shard| shard.shard_id == shard_id)
                .ok_or(RouterError::ShardNotRegistered)?;
            shard.projection_advanced = true;
            // Once every shard is completed and projected, the mutation is fully done:
            // pin the final row count and drop the heavy fields (ADR 0025, mechanism E).
            // Subsequent replays short-circuit on completed_row_count and never read them.
            if record
                .shards
                .iter()
                .all(|shard| shard.completed && shard.projection_advanced)
            {
                let total = record
                    .shards
                    .iter()
                    .fold(0u64, |total, shard| total.saturating_add(shard.row_count));
                record.completed_row_count = Some(total);
                compact_completed_record(&mut record);
            }
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn router_mutation_completed_row_count(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Option<u64> {
        let record = self.router_mutation_record(caller, graph_id, client_key)?;
        if let Some(row_count) = record.completed_row_count {
            return Some(row_count);
        }
        if record.shards.is_empty()
            || record
                .shards
                .iter()
                .any(|shard| !shard.completed || !shard.projection_advanced)
        {
            return None;
        }
        Some(
            record
                .shards
                .iter()
                .fold(0u64, |total, shard| total.saturating_add(shard.row_count)),
        )
    }
}

fn client_mutation_key(
    caller: Principal,
    graph_id: GraphId,
    client_key: &str,
) -> ClientMutationKey {
    ClientMutationKey::new(caller, graph_id, client_key.to_owned())
}
