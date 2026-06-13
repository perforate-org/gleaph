//! Mutation idempotency and client mutation journal.

use super::super::stable::label_telemetry::{
    ClientMutationKey, RouterMutationRecord, RouterMutationShard,
};
use super::super::stable::{ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_MUTATION_COUNTER};
use super::{
    CLIENT_MUTATION_KEY_TTL_NS, ClientMutationReservation, RouterStore, ic_time_ns,
    validate_client_mutation_key,
};
use crate::state::RouterError;
use crate::types::ShardId;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::LabelTelemetryEventWire;
use gleaph_graph_kernel::plan_exec::{MutationId, ResolvedLabelTable, ResolvedPropertyTable};

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
        Ok(ClientMutationReservation {
            mutation_id,
            routing_owner: true,
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
        events: Vec<LabelTelemetryEventWire>,
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
            shard.telemetry_acked = false;
            shard.row_count = row_count;
            shard.label_telemetry_events = events;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_telemetry_acked(
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
            shard.telemetry_acked = true;
            shard.label_telemetry_events.clear();
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
                .any(|shard| !shard.completed || !shard.telemetry_acked)
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
