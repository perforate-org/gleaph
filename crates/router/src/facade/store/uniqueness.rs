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

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql_ic::{UniqueKeyOutcome, UniqueKeyRejection, encode_unique_value};
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::plan::PlanOp;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::{
    MutationId, ResolvedLabelTable, ResolvedPropertyTable, UniqueClaimDispatch,
};

use crate::facade::stable::constraint_catalog::find_unique_constraint;
use crate::facade::stable::reservation_catalog::{self, ProofShard, ReservationClaim};
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
                    let Some((constraint_id, _)) =
                        find_unique_constraint(graph_id, vertex_label_id, property_id)
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

    /// No-`await` Try: reserves every claim against the cross-shard reservation table before the
    /// canonical write is dispatched (ADR 0030). `dispatches` is the resolved target set; its
    /// `(shard_id, graph_canister)` identities are persisted as the reservation's `proof_scope` so
    /// slice-6 recovery can read the `Acquire` proof from the exact canister the claim may commit on.
    pub(crate) fn try_reserve_unique(
        &self,
        graph_id: GraphId,
        mutation_id: MutationId,
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
        reservation_catalog::try_reserve(
            graph_id,
            mutation_id,
            &reservation_claims,
            &proof_scope,
            ic_time_ns(),
        )
    }

    /// Confirm one claim, stamping the canonical owner (ADR 0030). Runs after the shard's `Acquire`
    /// proof is durable; idempotent and best-effort (it never errors). Returns `true` when the value
    /// is committed *by this claim* — a fresh `Reserved → Committed` move **or** a replay of an
    /// already-`Committed` claim (so a Confirm retried after a failed ack still reports committed and
    /// the effect is re-acked); `false` when the record is missing/`Reclaiming`/owned by another
    /// claim. See [`reservation_catalog::confirm_reservation`].
    pub(crate) fn confirm_unique_claim(
        &self,
        graph_id: GraphId,
        mutation_id: MutationId,
        claim: &UniqueClaimDispatch,
        owner_element_id: Vec<u8>,
    ) -> bool {
        let claim_id =
            gleaph_graph_kernel::federation::ClaimId::new(mutation_id, claim.claim_ordinal);
        reservation_catalog::confirm_reservation(
            graph_id,
            claim_id,
            claim.constraint_id,
            &claim.encoded_value,
            owner_element_id,
        )
    }
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
}
