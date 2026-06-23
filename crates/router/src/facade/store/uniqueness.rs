//! Write-path wiring for cross-shard uniqueness (ADR 0030 slice 5a).
//!
//! This is the Router's enforcement seam for `INSERT`: it (1) statically detects which constrained
//! `(vertex label, property)` an admitted `INSERT` touches and canonicalizes the claimed values,
//! (2) reserves them through the no-`await` Try, and (3) Confirms each reservation once the shard's
//! `Acquire` proof is durable. The reservation table and Try/Confirm transitions live in
//! [`crate::facade::stable::reservation_catalog`]; this module owns the *plan → claims* admission
//! and the facade methods the GQL dispatch calls.
//!
//! Admission first cut (slice 5a): only a **statically single-element** `INSERT` of a vertex whose
//! constrained property value is a literal or bound parameter is enforced. The value must evaluate
//! identically on the Router and the shard, so only the transformation-free expression forms
//! (`Literal`, `Parameter`, parenthesized) are admitted; anything that would require evaluation, or
//! any multi-element insert, is refused (`NotImplemented`) rather than admitted unguarded — silently
//! skipping enforcement would break the very invariant the constraint promises.

use std::collections::{BTreeMap, BTreeSet};

use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql_ic::{UniqueKeyOutcome, UniqueKeyRejection, encode_unique_value};
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::plan::{PlanOp, SetPlanItem};
use gleaph_graph_kernel::entry::{GraphId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::plan_exec::{
    ConstrainedPropertyDispatch, MutationId, ResolvedLabelTable, ResolvedPropertyTable,
    UniqueClaimDispatch,
};

use candid::Principal;

use crate::facade::stable::constraint_catalog::{
    active_constrained_properties_for_graph, bump_drop_scan_generation,
    constrained_properties_for_graph, find_active_unique_constraint,
};
use crate::facade::stable::label_stats::ClientMutationKey;
use crate::facade::stable::reservation_catalog::{
    self, ConfirmOutcome, ProofShard, ReclaimCandidate, ReclaimTicket, ReleaseOutcome,
    ReservationClaim, UniqueReservationKey,
};
use crate::federation::ShardDispatch;
use crate::state::RouterError;

use super::{RouterStore, ic_time_ns};

/// Evaluates an expression to a concrete value **iff** it needs no transformation — a literal, a
/// bound parameter, or either wrapped in parentheses. These evaluate identically on the Router and
/// the shard, so the Router's reserved `encoded_value` is guaranteed to key the value the shard
/// actually stores. Any other form yields `None` (the caller refuses the insert).
fn static_value(expr: &Expr, params: &BTreeMap<String, Value>) -> Option<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Some(value.clone()),
        // The parser stores parameter names with the `$` sigil (`Parameter("$e")`), but the Router's
        // `pmap` is keyed by the bare name — strip the sigil before lookup, matching the convention
        // in `seed.rs` / `aggregate_index_fast_path.rs`.
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name);
            params.get(key).cloned()
        }
        ExprKind::Paren(inner) => static_value(inner, params),
        _ => None,
    }
}

impl RouterStore {
    /// Detects the cross-shard uniqueness claims an admitted `INSERT` makes (ADR 0030 slice 5a).
    ///
    /// Returns the claims in deterministic order (`claim_ordinal` = position), or an empty vector
    /// when the insert touches no constrained property (or only sets it to `NULL`, which makes no
    /// claim). Errors:
    /// - [`RouterError::NotImplemented`]: a constrained insert outside the first-cut envelope — more
    ///   than one created vertex, or a constrained value that is not a literal/bound parameter.
    /// - [`RouterError::InvalidArgument`]: a constrained value that cannot be a uniqueness key
    ///   (non-finite float, unsupported type, or over the length bound).
    pub(crate) fn plan_unique_claims(
        &self,
        graph_id: GraphId,
        plans: &[PhysicalPlan],
        params: &BTreeMap<String, Value>,
        resolved_labels: &ResolvedLabelTable,
        resolved_properties: &ResolvedPropertyTable,
    ) -> Result<Vec<UniqueClaimDispatch>, RouterError> {
        let insert_vertices: Vec<(
            &[gleaph_gql_planner::plan::NodeLabelRef],
            &[gleaph_gql_planner::plan::PropertyAssignment],
        )> = plans
            .iter()
            .flat_map(|plan| plan.ops.iter())
            .filter_map(|op| match op {
                PlanOp::InsertVertex {
                    labels, properties, ..
                } => Some((labels.as_slice(), properties.as_slice())),
                _ => None,
            })
            .collect();

        let mut claims: Vec<UniqueClaimDispatch> = Vec::new();
        for (labels, properties) in &insert_vertices {
            for assignment in *properties {
                let Some(property_id) = resolved_properties
                    .properties
                    .iter()
                    .find(|entry| entry.name == assignment.name.as_ref())
                    .map(|entry| entry.id)
                else {
                    continue;
                };
                for label in *labels {
                    let Some(vertex_label_id) = resolved_labels
                        .vertex
                        .iter()
                        .find(|entry| entry.name == label.name.as_ref())
                        .map(|entry| entry.id)
                    else {
                        continue;
                    };
                    // ADR 0030 slice 9: a `Dropping` constraint is absent for new acquires, so the
                    // active-only lookup makes a new INSERT proceed unconstrained while the
                    // constraint drains.
                    let Some((constraint_id, _)) =
                        find_active_unique_constraint(graph_id, vertex_label_id, property_id)
                    else {
                        continue;
                    };
                    let Some(value) = static_value(&assignment.value, params) else {
                        return Err(RouterError::NotImplemented(format!(
                            "uniqueness constraint on '{}' requires a literal or parameter value; \
                             computed expressions are not yet supported (ADR 0030 slice 5a)",
                            assignment.name
                        )));
                    };
                    match encode_unique_value(&value) {
                        UniqueKeyOutcome::Claim(encoded_value) => {
                            let claim_ordinal = claims.len() as u32;
                            claims.push(UniqueClaimDispatch {
                                claim_ordinal,
                                constraint_id,
                                encoded_value,
                            });
                        }
                        // SQL semantics: a NULL/absent constrained value reserves nothing.
                        UniqueKeyOutcome::NoClaim => {}
                        UniqueKeyOutcome::Rejected(reason) => {
                            return Err(RouterError::InvalidArgument(format!(
                                "value for unique property '{}' cannot be a uniqueness key: {}",
                                assignment.name,
                                describe_rejection(reason)
                            )));
                        }
                    }
                }
            }
        }

        // Admission gate for the slice-5a write path. A claim makes the shard attach an `Acquire` to
        // the single vertex it creates, so the program must create *exactly one* vertex from *no*
        // existing-state read — otherwise the owner is ambiguous or the value is amplified across
        // rows. Two independent refusals:
        //
        // 1. `is_pure_insert`: rejects any read-prefix or row producer (`MATCH ... INSERT`,
        //    `UNWIND ... INSERT`), which would run the single `InsertVertex` op once per upstream
        //    row and claim the same value many times under one owner assumption.
        // 2. single `InsertVertex`: rejects a literal multi-vertex insert (`INSERT (a), (b)`), which
        //    has no single owner for the claim.
        if !claims.is_empty() {
            let read_prefixed_or_amplified =
                insert_vertices.len() != 1 || !plans.iter().all(PhysicalPlan::is_pure_insert);
            if read_prefixed_or_amplified {
                return Err(RouterError::NotImplemented(
                    "INSERT under a uniqueness constraint must be a single-vertex pure INSERT with \
                     no MATCH/UNWIND prefix (ADR 0030 slice 5a)"
                        .to_string(),
                ));
            }
        }
        Ok(claims)
    }

    /// Refuses `SET` writes that would touch a uniqueness-constrained value before the two-phase
    /// acquire/release protocol exists (ADR 0030). `plan_unique_claims` only enforces `INSERT`, and
    /// `plan_can_release` only emits `Release` for `DELETE`/`REMOVE`; a `SET` that re-keys a
    /// constrained value (`SET n.email = …`), replaces all properties (`SET n = {…}`), or adds a
    /// constrained label (`SET n IS User`) would otherwise reach the canonical write **unguarded** and
    /// could create a duplicate once `CREATE CONSTRAINT` is published. Such writes need an
    /// acquire-new-and-release-old handshake that is not yet built, so they are refused
    /// **non-retryably** (`NotImplemented`) rather than admitted unsafely. No-op when the graph
    /// declares no constraint, or when every `SET` item targets an unconstrained property/label.
    pub(crate) fn reject_unsupported_constrained_writes(
        &self,
        graph_id: GraphId,
        plans: &[PhysicalPlan],
    ) -> Result<(), RouterError> {
        // ADR 0030 slice 9: only `Active` constraints gate new constrained writes. A `Dropping`
        // constraint must not refuse a `SET` — new DML proceeds unconstrained while it drains.
        let constrained = active_constrained_properties_for_graph(graph_id);
        if constrained.is_empty() {
            return Ok(());
        }
        let constrained_properties: BTreeSet<PropertyId> = constrained
            .iter()
            .map(|(_, property, _)| *property)
            .collect();
        let constrained_labels: BTreeSet<VertexLabelId> =
            constrained.iter().map(|(label, _, _)| *label).collect();

        for op in plans.iter().flat_map(|plan| plan.ops.iter()) {
            let PlanOp::SetProperties { items } = op else {
                continue;
            };
            for item in items {
                let touches_constraint = match item {
                    // `SET n = {…}` can write any property, including a constrained one.
                    SetPlanItem::AllProperties { .. } => true,
                    SetPlanItem::Property { property, .. } => self
                        .lookup_property_id(graph_id, property.as_ref())
                        .is_ok_and(|property_id| constrained_properties.contains(&property_id)),
                    // Adding a constrained label makes its `(label, property)` constraint apply.
                    SetPlanItem::Label { label, .. } => self
                        .lookup_vertex_label_id(graph_id, label.name.as_ref())
                        .is_ok_and(|label_id| constrained_labels.contains(&label_id)),
                };
                if touches_constraint {
                    return Err(RouterError::NotImplemented(
                        "SET on a uniqueness-constrained property or label requires the two-phase \
                         acquire/release protocol, which is not yet implemented (ADR 0030); refused \
                         rather than risk writing a duplicate value"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// No-`await` Try: reserves every claim against the cross-shard reservation table before the
    /// canonical write is dispatched (ADR 0030). `dispatches` is the resolved target set; its
    /// `(shard_id, graph_canister)` identities are persisted as the reservation's `proof_scope` so
    /// slice-6 recovery can read the `Acquire` proof from the exact canister the claim may commit on.
    ///
    /// The reverse index that GC-pins the owning mutation record and resolves a reservation back to
    /// it (ADR 0030 slice 6) is bumped here in lockstep. On the IC an `Err` does not roll back, so
    /// both fallible checks run as read-only preflights *before* either write: the reservation
    /// conflict scan inside [`reservation_catalog::try_reserve`] and the reverse-index count overflow
    /// here. The overflow preflight uses the claim count as a conservative upper bound on fresh
    /// inserts, so once it clears, the post-Try slot bump (by the real, `<=` fresh count) cannot
    /// overflow and is infallible — leaving no path that writes a reservation without bumping the
    /// count, or vice versa.
    pub(crate) fn try_reserve_unique(
        &self,
        caller: Principal,
        graph_id: GraphId,
        mutation_id: MutationId,
        client_key: &str,
        claims: &[UniqueClaimDispatch],
        dispatches: &[ShardDispatch],
    ) -> Result<(), RouterError> {
        let reservation_claims: Vec<ReservationClaim> = claims
            .iter()
            .map(|claim| ReservationClaim {
                constraint_id: claim.constraint_id,
                encoded_value: claim.encoded_value.clone(),
                claim_ordinal: claim.claim_ordinal,
            })
            .collect();
        let proof_scope: Vec<ProofShard> = dispatches
            .iter()
            .map(|dispatch| ProofShard::new(dispatch.shard_id, dispatch.graph_canister))
            .collect();
        // Preflight (read-only): reject a reverse-index count overflow before any reservation is
        // written. `claims.len()` is the maximum a Try could freshly insert.
        let fresh_upper = u32::try_from(claims.len())
            .map_err(|_| RouterError::Internal("unique claim count exceeds u32".to_string()))?;
        self.preflight_reservation_slots(mutation_id, fresh_upper)?;
        // Apply: `try_reserve` does its own conflict preflight, then inserts; on success it reports
        // the real fresh-insert count. No `await` separates it from the slot bump, so the overflow
        // guarantee from the preflight still holds and the bump is infallible.
        let fresh = reservation_catalog::try_reserve(
            graph_id,
            mutation_id,
            &reservation_claims,
            &proof_scope,
            ic_time_ns(),
        )?;
        let key = ClientMutationKey::new(caller, graph_id, client_key.to_owned());
        self.apply_reservation_slots(mutation_id, &key, fresh);
        Ok(())
    }

    /// Decrement the non-terminal reservation count for `mutation_id` after a `FreshlyCommitted`
    /// Confirm (ADR 0030 slice 6): that reservation has left the non-terminal set, so it no longer
    /// pins the owning record. Infallible; a missing reverse row is a no-op.
    pub(crate) fn release_unique_reservation_slot(&self, mutation_id: MutationId) {
        self.release_reservation_slot(mutation_id);
    }

    /// Register the slice-6 discovery row for a dispatch that may emit a unique effect (ADR 0030
    /// slice 6). Called once per dispatch shard **before the first dispatch `await`**, so it
    /// co-commits with the reservation/envelope; idempotent on replay. The pinned `canister` is
    /// stored verbatim so recovery reaches the exact canister even after the shard is unregistered;
    /// `client_key` is stored so Driver 2 can resolve the owning record for any effect kind.
    pub(crate) fn register_pending_unique_effect(
        &self,
        graph_id: GraphId,
        mutation_id: MutationId,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        canister: candid::Principal,
        client_key: crate::facade::stable::label_stats::ClientMutationKey,
    ) {
        crate::facade::stable::unique_effect_pending::register(
            graph_id,
            mutation_id,
            shard_id,
            canister,
            client_key,
        );
        // ADR 0030 slice 9: a new pending-effect row may have been registered behind the drop-drain
        // driver's scan cursor. Bump the scan-invalidation token on every `Dropping` constraint in
        // this graph so the driver re-laps rather than reaching a false-clean completion. A no-op
        // when no constraint is `Dropping`; conservative bumps only cost an extra lap.
        bump_drop_scan_generation(graph_id);
    }

    /// Remove one slice-6 discovery row (Driver 2): called only after a fresh `cursor=None`
    /// re-enumeration of the shard's effects for the mutation came back empty.
    #[cfg_attr(
        not(target_family = "wasm"),
        allow(
            dead_code,
            reason = "driven by the wasm recovery timer (Driver 2); resolvers are unit-tested"
        )
    )]
    pub(crate) fn remove_pending_unique_effect(
        &self,
        graph_id: GraphId,
        mutation_id: MutationId,
        shard_id: gleaph_graph_kernel::federation::ShardId,
    ) {
        crate::facade::stable::unique_effect_pending::remove(graph_id, mutation_id, shard_id);
    }

    /// Park a slice-6 discovery row (Driver 2) as a quarantined orphan with a re-check backoff and a
    /// persistent diagnostic — the row is kept, never acked.
    #[cfg_attr(
        not(target_family = "wasm"),
        allow(
            dead_code,
            reason = "driven by the wasm recovery timer (Driver 2); resolvers are unit-tested"
        )
    )]
    pub(crate) fn quarantine_pending_unique_effect(
        &self,
        graph_id: GraphId,
        mutation_id: MutationId,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        next_retry_ns: u64,
        diagnostic: String,
    ) {
        crate::facade::stable::unique_effect_pending::quarantine(
            graph_id,
            mutation_id,
            shard_id,
            next_retry_ns,
            diagnostic,
        );
    }

    /// `true` iff a reservation for `(graph, constraint, encoded_value)` exists **and** is held by
    /// `claim_id`. Driver 2 uses this to tell a delegated `Acquire` (a reservation Driver 1 still
    /// owns) from an orphan `Acquire` (no reservation will ever ack it).
    pub(crate) fn reservation_exists_for_claim(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
        claim_id: gleaph_graph_kernel::federation::ClaimId,
    ) -> bool {
        reservation_catalog::claim_has_reservation(graph_id, constraint_id, encoded_value, claim_id)
    }

    /// Confirm one claim, stamping the canonical owner (ADR 0030). Runs after the shard's `Acquire`
    /// proof is durable; idempotent and best-effort (it never errors). Returns `true` when the value
    /// is committed *by this claim* — a fresh `Reserved → Committed` move **or** a replay of an
    /// already-`Committed` claim (so a Confirm retried after a failed ack still reports committed and
    /// the effect is re-acked); `false` when the record is missing/`Reclaiming`/owned by another
    /// claim. `effect_id` is the proven `Acquire` effect; it is stamped as `pending_acquire_ack` in
    /// the same `→ Committed` write so a crash before the ack leaves the still-pinned `Acquire`
    /// re-discoverable by slice-6 recovery. The returned [`ConfirmOutcome`] tells the caller whether
    /// to ack (`FreshlyCommitted`/`AlreadyCommitted`) and whether to decrement the mutation's
    /// non-terminal count (`FreshlyCommitted` only). See [`reservation_catalog::confirm_reservation`].
    pub(crate) fn confirm_unique_claim(
        &self,
        graph_id: GraphId,
        mutation_id: MutationId,
        claim: &UniqueClaimDispatch,
        owner_element_id: Vec<u8>,
        effect_id: gleaph_graph_kernel::federation::EffectId,
    ) -> ConfirmOutcome {
        let claim_id =
            gleaph_graph_kernel::federation::ClaimId::new(mutation_id, claim.claim_ordinal);
        reservation_catalog::confirm_reservation(
            graph_id,
            claim_id,
            claim.constraint_id,
            &claim.encoded_value,
            owner_element_id,
            effect_id,
        )
    }

    /// Clear `pending_acquire_ack` after the `Acquire` effect has been acked (unpinned). Idempotent
    /// and claim-fenced; a no-op (`false`) for a missing/non-`Committed`/foreign record or one whose
    /// ack was already cleared. See [`reservation_catalog::clear_acquire_ack`].
    pub(crate) fn clear_unique_acquire_ack(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
        claim_id: gleaph_graph_kernel::federation::ClaimId,
    ) -> bool {
        reservation_catalog::clear_acquire_ack(graph_id, constraint_id, encoded_value, claim_id)
    }
}

impl RouterStore {
    /// The constrained `(vertex_label, property, constraint)` set to dispatch to a shard whose
    /// mutation can delete/remove a constrained element (ADR 0030 slice 5b), so it can pin one
    /// `Release` per freed value. Empty when the graph declares no constraint.
    pub(crate) fn constrained_property_dispatch(
        &self,
        graph_id: GraphId,
    ) -> Vec<ConstrainedPropertyDispatch> {
        constrained_properties_for_graph(graph_id)
            .into_iter()
            .map(
                |(vertex_label_id, property_id, constraint_id)| ConstrainedPropertyDispatch {
                    vertex_label_id,
                    property_id,
                    constraint_id,
                },
            )
            .collect()
    }

    /// Begin a generation-fenced reclaim proof for a `Reserved` value (ADR 0030 slice 6 §Timeout):
    /// `Reserved → Reclaiming`, checked-incrementing the persistent generation, returning the fence.
    /// Bounded, cursor-based work discovery for the reclaim reconciler (ADR 0030 slice 6, Driver 1):
    /// the next slice of reservations needing reconciliation (`Reserved` past TTL, any `Reclaiming`,
    /// or `Committed` with a pending ack), the next cursor, and the count scanned. Read-only.
    pub(crate) fn scan_unique_reclaim_candidates(
        &self,
        start_after: Option<&UniqueReservationKey>,
        budget: usize,
        now: u64,
    ) -> (Vec<ReclaimCandidate>, Option<UniqueReservationKey>, u32) {
        reservation_catalog::scan_reclaim_candidates(start_after, budget, now)
    }

    /// `None` aborts the proof (the entry is absent or not `Reserved`). No `await`.
    pub(crate) fn begin_unique_reclaim(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
    ) -> Option<ReclaimTicket> {
        reservation_catalog::begin_reclaim(graph_id, constraint_id, encoded_value)
    }

    /// Test-only (`pocket-ic-e2e`): drive a `Reserved` reservation for the `(label, property, text
    /// value)` of `graph_id` into `Reclaiming` and commit it, so the failure-injection suite can prove
    /// a same-`ClaimId` retry is fenced while a reclaim proof is in flight. Returns whether the
    /// transition happened. The value is encoded exactly as the write path encodes a literal `Text`
    /// claim, so the key byte-matches a reservation made by an `INSERT (:Label {property: 'value'})`.
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn test_force_reclaiming_text(
        &self,
        graph_id: GraphId,
        label: &str,
        property: &str,
        value: &str,
    ) -> Result<bool, RouterError> {
        let vertex_label_id = self.lookup_vertex_label_id(graph_id, label)?;
        let property_id = self.lookup_property_id(graph_id, property)?;
        let (constraint_id, _) = find_active_unique_constraint(
            graph_id,
            vertex_label_id,
            property_id,
        )
        .ok_or_else(|| {
            RouterError::InvalidArgument("no unique constraint for (label, property)".to_string())
        })?;
        let encoded_value = match encode_unique_value(&Value::Text(value.to_string())) {
            UniqueKeyOutcome::Claim(encoded) => encoded,
            UniqueKeyOutcome::NoClaim => {
                return Err(RouterError::InvalidArgument(
                    "value makes no uniqueness claim".to_string(),
                ));
            }
            UniqueKeyOutcome::Rejected(_) => {
                return Err(RouterError::InvalidArgument(
                    "value cannot be a uniqueness key".to_string(),
                ));
            }
        };
        Ok(self
            .begin_unique_reclaim(graph_id, constraint_id, &encoded_value)
            .is_some())
    }

    /// Resume an interrupted reclaim proof (entry already `Reclaiming`) under its current generation,
    /// without bumping it (ADR 0030 slice 6). `None` if not `Reclaiming`.
    pub(crate) fn resume_unique_reclaim(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
    ) -> Option<ReclaimTicket> {
        reservation_catalog::resume_reclaim(graph_id, constraint_id, encoded_value)
    }

    /// Apply a reclaim proof's **commit** outcome under the fence (`Reclaiming@g → Committed`,
    /// stamping the owner). Returns `true` iff applied — the caller may then ack the `Acquire`.
    pub(crate) fn apply_unique_reclaim_commit(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
        claim_id: gleaph_graph_kernel::federation::ClaimId,
        generation: u64,
        owner_element_id: Vec<u8>,
        effect_id: gleaph_graph_kernel::federation::EffectId,
    ) -> bool {
        reservation_catalog::apply_reclaim_commit(
            graph_id,
            constraint_id,
            encoded_value,
            claim_id,
            generation,
            owner_element_id,
            effect_id,
        )
    }

    /// Atomically cancel an uncommitted reclaim (`Reclaiming@g → removed`) under the **full** ADR 0030
    /// slice-6 cancel authority, with no `await` and **no partial state** on any failure. On the IC a
    /// non-trapping early return does not roll back, so every fallible condition is checked in a
    /// read-only preflight *before* any mutation; only when all hold is the unified apply performed,
    /// and the apply provably cannot fail:
    ///
    /// Preflight (read-only, no mutation): the reverse-index row resolves the owning record (its
    /// presence also proves the non-terminal count is `>= 1`); the reservation is `Reclaiming@g`
    /// owned by `claim_id`; and the record is terminal-failure eligible — `mutation_id` matches and
    /// it is either already terminally failed (idempotent for a sibling reservation) or an
    /// uncommitted dispatch (envelope present, no canonical shard completed, routing released).
    ///
    /// Apply (only after every preflight passed): record the irreversible terminal failure, remove
    /// the reservation (its fence was just preflighted, so this cannot fail — asserted, and a trap
    /// would roll back the whole message rather than leave partial state), and decrement the
    /// non-terminal count (the row was just preflighted, so the fail-closed release cannot trap).
    ///
    /// Returns `true` iff cancelled. `false` (any preflight failed) means the caller must `hold`; no
    /// terminal failure, reservation removal, or count change has occurred.
    pub(crate) fn reclaim_cancel_uncommitted(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
        claim_id: gleaph_graph_kernel::federation::ClaimId,
        generation: u64,
        error: String,
    ) -> bool {
        let mutation_id = claim_id.mutation_id;
        // Preflight: owner + count>=1 (the row exists iff count>=1) and the reservation fence.
        let Some(client_key) = self.reservation_index_client_key(mutation_id) else {
            return false;
        };
        if !reservation_catalog::is_reclaiming_at(
            graph_id,
            constraint_id,
            encoded_value,
            claim_id,
            generation,
        ) {
            return false;
        }
        // Apply: terminal-fail mutates ONLY when it returns true (record-side eligibility preflight
        // and apply are one fenced op); a false leaves no state to undo.
        if !self.terminally_fail_uncommitted_dispatch(&client_key, mutation_id, error) {
            return false;
        }
        let removed = reservation_catalog::cancel_reclaim(
            graph_id,
            constraint_id,
            encoded_value,
            claim_id,
            generation,
        );
        assert!(
            removed,
            "preflighted reclaim cancel must apply atomically (ADR 0030 slice 6)"
        );
        self.release_reservation_slot(mutation_id);
        true
    }

    /// Release the reclaim fence without resolving (`Reclaiming@g → Reserved`, keeping `g`), for an
    /// unreachable/unknown shard or a non-terminal/missing owning mutation (ADR 0030 slice 6
    /// §Timeout step 7). Fenced on `claim_id` + generation for the same ABA reason as
    /// [`Self::reclaim_cancel_uncommitted`]. Returns `true` iff reverted.
    pub(crate) fn hold_unique_reclaim(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
        claim_id: gleaph_graph_kernel::federation::ClaimId,
        generation: u64,
    ) -> bool {
        reservation_catalog::hold_reclaim(
            graph_id,
            constraint_id,
            encoded_value,
            claim_id,
            generation,
        )
    }

    /// Reconcile one shard-emitted `Release` effect against the reservation table (ADR 0030 slice
    /// 5b). Returns `true` when the Router may **ack** the effect (the value's reservation was
    /// removed, was already gone, or the `Release` is stale because a different element took the
    /// value over); `false` when the effect must be **held** (Release-before-Acquire: the value is
    /// still `Reserved`/`Reclaiming` or its owner is undetermined). Best-effort and idempotent — it
    /// never errors. See [`reservation_catalog::release_reservation`].
    pub(crate) fn release_unique_effect(
        &self,
        graph_id: GraphId,
        constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
        encoded_value: &[u8],
        owner_element_id: &[u8],
    ) -> bool {
        match reservation_catalog::release_reservation(
            graph_id,
            constraint_id,
            encoded_value,
            owner_element_id,
        ) {
            ReleaseOutcome::Applied => true,
            ReleaseOutcome::Held => false,
        }
    }
}

/// Whether any plan in the program can free a constrained value — i.e. carries a vertex
/// delete/detach-delete or a property remove (ADR 0030 slice 5b). Used to decide whether the
/// constrained-property set must ride the dispatch so the shard can emit `Release` effects.
pub(crate) fn plan_can_release(plans: &[PhysicalPlan]) -> bool {
    plans.iter().flat_map(|plan| plan.ops.iter()).any(|op| {
        matches!(
            op,
            PlanOp::DeleteVertex { .. }
                | PlanOp::DetachDeleteVertex { .. }
                | PlanOp::RemoveProperties { .. }
        )
    })
}

fn describe_rejection(reason: UniqueKeyRejection) -> String {
    match reason {
        UniqueKeyRejection::NonFinite => "non-finite float (NaN/±∞) has no stable equality".into(),
        UniqueKeyRejection::Unsupported => "type has no canonical key encoding".into(),
        UniqueKeyRejection::TooLong { len, max } => {
            format!("encoded length {len} exceeds the {max}-byte bound")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::store::catalog_test_support::setup;
    use gleaph_graph_kernel::plan_exec::{ResolvedProperty, ResolvedVertexLabel};

    /// Plans a single statement block from GQL text (no stats / no schema), mirroring the dispatch
    /// path's plan source closely enough to exercise admission against real planner output.
    fn plans_for(query: &str) -> Vec<PhysicalPlan> {
        let program = gleaph_gql::parser::parse(query).expect("parse");
        let tx = program
            .transaction_activity
            .as_ref()
            .expect("transaction activity");
        let block = tx.body.as_ref().expect("statement block");
        let plan = gleaph_gql_planner::build_block_plan_with_schema(
            block,
            None,
            &gleaph_gql::type_check::NoSchema,
        )
        .expect("build plan");
        vec![plan]
    }

    /// Declares the `User.email` UNIQUE constraint and returns the resolved-name tables the dispatch
    /// path would hand `plan_unique_claims` (name → interned id for the constrained label/property).
    fn setup_user_email_constraint() -> (
        RouterStore,
        GraphId,
        ResolvedLabelTable,
        ResolvedPropertyTable,
    ) {
        let (store, _admin, graph_id) = setup();
        store
            .create_unique_constraint(graph_id, "user_email", false, "User", "email")
            .expect("create constraint");
        let label_id = store
            .lookup_vertex_label_id(graph_id, "User")
            .expect("User interned");
        let property_id = store
            .lookup_property_id(graph_id, "email")
            .expect("email interned");
        let resolved_labels = ResolvedLabelTable {
            vertex: vec![ResolvedVertexLabel {
                name: "User".into(),
                id: label_id,
            }],
            edge: vec![],
        };
        let resolved_properties = ResolvedPropertyTable {
            properties: vec![ResolvedProperty {
                name: "email".into(),
                id: property_id,
            }],
        };
        (store, graph_id, resolved_labels, resolved_properties)
    }

    #[test]
    fn literal_insert_makes_one_claim_with_canonical_value() {
        let (store, graph_id, labels, properties) = setup_user_email_constraint();
        let plans = plans_for("INSERT (n:User {email: 'a@example.com'})");
        let claims = store
            .plan_unique_claims(graph_id, &plans, &BTreeMap::new(), &labels, &properties)
            .expect("admitted");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].claim_ordinal, 0);
        let expected = match encode_unique_value(&Value::Text("a@example.com".into())) {
            UniqueKeyOutcome::Claim(bytes) => bytes,
            other => panic!("expected a claim, got {other:?}"),
        };
        assert_eq!(claims[0].encoded_value, expected);
    }

    #[test]
    fn parameter_insert_makes_a_claim() {
        let (store, graph_id, labels, properties) = setup_user_email_constraint();
        let plans = plans_for("INSERT (n:User {email: $e})");
        let mut params = BTreeMap::new();
        params.insert("e".to_string(), Value::Text("p@example.com".into()));
        let claims = store
            .plan_unique_claims(graph_id, &plans, &params, &labels, &properties)
            .expect("admitted");
        assert_eq!(claims.len(), 1);
        let expected = match encode_unique_value(&Value::Text("p@example.com".into())) {
            UniqueKeyOutcome::Claim(bytes) => bytes,
            other => panic!("expected a claim, got {other:?}"),
        };
        assert_eq!(claims[0].encoded_value, expected);
    }

    #[test]
    fn null_constrained_value_makes_no_claim() {
        let (store, graph_id, labels, properties) = setup_user_email_constraint();
        let plans = plans_for("INSERT (n:User {email: NULL})");
        let claims = store
            .plan_unique_claims(graph_id, &plans, &BTreeMap::new(), &labels, &properties)
            .expect("admitted");
        assert!(claims.is_empty(), "NULL reserves nothing, got {claims:?}");
    }

    #[test]
    fn unconstrained_property_makes_no_claim() {
        let (store, graph_id, labels, properties) = setup_user_email_constraint();
        // `name` carries no constraint, so the insert claims nothing even though it touches `User`.
        let plans = plans_for("INSERT (n:User {name: 'x'})");
        let claims = store
            .plan_unique_claims(graph_id, &plans, &BTreeMap::new(), &labels, &properties)
            .expect("admitted");
        assert!(claims.is_empty(), "unconstrained property, got {claims:?}");
    }

    #[test]
    fn computed_constrained_value_is_rejected() {
        let (store, graph_id, labels, properties) = setup_user_email_constraint();
        // A non-literal/non-parameter value would not evaluate identically on the Router and shard.
        let plans = plans_for("INSERT (n:User {email: 'a' || 'b'})");
        let err = store
            .plan_unique_claims(graph_id, &plans, &BTreeMap::new(), &labels, &properties)
            .expect_err("computed value refused");
        assert!(matches!(err, RouterError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn multi_vertex_constrained_insert_is_rejected() {
        let (store, graph_id, labels, properties) = setup_user_email_constraint();
        let plans = plans_for("INSERT (a:User {email: 'a@x'}), (b:User {email: 'b@x'})");
        let err = store
            .plan_unique_claims(graph_id, &plans, &BTreeMap::new(), &labels, &properties)
            .expect_err("multi-vertex refused");
        assert!(matches!(err, RouterError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn read_prefixed_constrained_insert_is_rejected() {
        let (store, graph_id, labels, properties) = setup_user_email_constraint();
        // A MATCH prefix makes the plan non-pure-insert; the single `InsertVertex` op could run once
        // per matched row, amplifying the same claimed value under one owner assumption.
        let plans = plans_for("MATCH (u:User) INSERT (n:User {email: 'a@x'})");
        let err = store
            .plan_unique_claims(graph_id, &plans, &BTreeMap::new(), &labels, &properties)
            .expect_err("read-prefixed insert refused");
        assert!(matches!(err, RouterError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn insert_without_constraint_makes_no_claim() {
        // No constraint declared at all: the same insert is fully admitted with zero claims.
        let (store, _admin, graph_id) = setup();
        let label_resolved = ResolvedLabelTable {
            vertex: vec![],
            edge: vec![],
        };
        let property_resolved = ResolvedPropertyTable { properties: vec![] };
        let plans = plans_for("INSERT (n:User {email: 'a@x'})");
        let claims = store
            .plan_unique_claims(
                graph_id,
                &plans,
                &BTreeMap::new(),
                &label_resolved,
                &property_resolved,
            )
            .expect("admitted");
        assert!(claims.is_empty());
    }

    #[test]
    fn plan_can_release_detects_delete_and_remove() {
        // Release-only shapes carry the constrained-property set so the shard can emit `Release`.
        assert!(plan_can_release(&plans_for("MATCH (n:User) DELETE n")));
        assert!(plan_can_release(&plans_for(
            "MATCH (n:User) DETACH DELETE n"
        )));
        assert!(plan_can_release(&plans_for(
            "MATCH (n:User) REMOVE n.email"
        )));
        // A pure INSERT or a read acquires/touches nothing to release.
        assert!(!plan_can_release(&plans_for(
            "INSERT (n:User {email: 'a@x'})"
        )));
        assert!(!plan_can_release(&plans_for("MATCH (n:User) RETURN n")));
    }

    #[test]
    fn constrained_property_dispatch_lists_declared_constraints() {
        let (store, graph_id, _labels, _properties) = setup_user_email_constraint();
        let dispatched = store.constrained_property_dispatch(graph_id);
        assert_eq!(dispatched.len(), 1);
        let entry = &dispatched[0];
        assert_eq!(
            entry.vertex_label_id,
            store.lookup_vertex_label_id(graph_id, "User").unwrap()
        );
        assert_eq!(
            entry.property_id,
            store.lookup_property_id(graph_id, "email").unwrap()
        );
    }

    #[test]
    fn constrained_property_dispatch_is_empty_without_constraints() {
        let (store, _admin, graph_id) = setup();
        assert!(store.constrained_property_dispatch(graph_id).is_empty());
    }

    #[test]
    fn set_constrained_property_is_rejected() {
        // `SET n.email = …` re-keys a constrained value with no acquire/release handshake.
        let (store, graph_id, _labels, _properties) = setup_user_email_constraint();
        let plans = plans_for("MATCH (n:User) SET n.email = 'x@x' RETURN n");
        let err = store
            .reject_unsupported_constrained_writes(graph_id, &plans)
            .expect_err("constrained SET refused");
        assert!(matches!(err, RouterError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn set_all_properties_is_rejected_when_a_constraint_exists() {
        // `SET n = {…}` can overwrite the constrained property, so it is refused conservatively.
        let (store, graph_id, _labels, _properties) = setup_user_email_constraint();
        let plans = plans_for("MATCH (n:User) SET n = {email: 'x@x'} RETURN n");
        let err = store
            .reject_unsupported_constrained_writes(graph_id, &plans)
            .expect_err("SET-all refused");
        assert!(matches!(err, RouterError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn set_constrained_label_is_rejected() {
        // Adding the constrained label makes `(User, email)` apply to a vertex that may already hold
        // `email`, so it needs an acquire — refused until the protocol lands.
        let (store, graph_id, _labels, _properties) = setup_user_email_constraint();
        let plans = plans_for("MATCH (n) SET n IS User RETURN n");
        let err = store
            .reject_unsupported_constrained_writes(graph_id, &plans)
            .expect_err("constrained label add refused");
        assert!(matches!(err, RouterError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn set_unconstrained_property_is_admitted() {
        // `name` carries no constraint, so the SET is admitted even though a constraint exists.
        let (store, graph_id, _labels, _properties) = setup_user_email_constraint();
        let plans = plans_for("MATCH (n:User) SET n.name = 'x' RETURN n");
        store
            .reject_unsupported_constrained_writes(graph_id, &plans)
            .expect("unconstrained SET admitted");
    }

    #[test]
    fn set_constrained_property_without_any_constraint_is_admitted() {
        // No constraint declared: even `SET n.email` is fully admitted.
        let (store, _admin, graph_id) = setup();
        let plans = plans_for("MATCH (n:User) SET n.email = 'x@x' RETURN n");
        store
            .reject_unsupported_constrained_writes(graph_id, &plans)
            .expect("admitted without constraints");
    }
}
