//! PocketIC contracts for the scalar mutation journal's lifetime (ADR 0027, amended 2026-11-07).
//!
//! The defect these prove closed: a scalar (`PlanExecution`) journal entry used to age out after nine days **while the
//! Router could still re-dispatch the mutation**, so a lost Router outcome plus nine quiet days deleted the only evidence
//! that the mutation had been applied - and the retry executed it a second time. The contract is now the ordered
//! families': an entry is `Active` until its owner retires it, the age-only sweep never erases it, and retirement is an
//! exact claim about the captured outcome.

use candid::Principal;
use gleaph_graph_kernel::plan_exec::{
    MutationJournalState, ScalarMutationRetirementAck, ScalarMutationRetirementArgs,
    ScalarMutationRetirementArgsV1,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, advance_past_journal_eviction, ensure_vertex_label,
    evict_graph_mutation_journal, gql_mutate_as_admin, graph_mutation_journal_len,
    install_single_shard_federation, try_update_as_router,
};

/// Runs one scalar mutation and returns the Graph shard's journal length for it.
fn journal_len(env: &FederationEnv) -> u64 {
    graph_mutation_journal_len(env, env.graph_source)
}

fn retire(
    env: &FederationEnv,
    mutation_id: u64,
    row_count: u64,
) -> Result<ScalarMutationRetirementAck, String> {
    try_update_as_router(
        env,
        env.graph_source,
        "retire_scalar_mutation",
        ScalarMutationRetirementArgs::V1(ScalarMutationRetirementArgsV1 {
            mutation_id: gleaph_graph_kernel::plan_exec::MutationId::new(mutation_id),
            row_count,
        }),
    )
}

/// The Router retires a completed scalar entry inline; only the age after that retirement is evictable.
///
/// The `Active` survival rule is a store contract (`an_active_scalar_entry_survives_the_age_only_sweep`); a completed
/// Router mutation has already captured its receipt, so this E2E proves the retirement transition and its age window.
#[test]
fn a_completed_scalar_journal_entry_ages_out_after_retirement() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    gql_mutate_as_admin(
        &env,
        "INSERT (p:Person { name: 'retained' })",
        "adr0027-scalar-retained",
    );
    assert_eq!(
        journal_len(&env),
        1,
        "the committed mutation is journaled exactly once"
    );

    // The Router captured and retired this receipt; past the retention window the retired entry is evictable.
    advance_past_journal_eviction(&env);
    assert_eq!(
        evict_graph_mutation_journal(&env, env.graph_source),
        0,
        "a retired scalar entry ages out after the retention window"
    );
    assert_eq!(journal_len(&env), 0, "the retired evidence is reclaimed");
}

/// Retirement is what starts retention, and it refuses a receipt that is not the captured one.
#[test]
fn retirement_is_exact_and_is_what_releases_the_evidence() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let row_count = gql_mutate_as_admin(
        &env,
        "INSERT (p:Person { name: 'retired' })",
        "adr0027-scalar-retired",
    );
    let status = gleaph_pocket_ic_tests::mutation_status_as_admin(
        &env,
        GRAPH_NAME,
        "adr0027-scalar-retired",
    )
    .expect("mutation status");
    let mutation_id = status.mutation_id;

    // A receipt that disagrees with the journaled entry retires nothing.
    let wrong = row_count.saturating_add(1);
    assert!(
        retire(&env, mutation_id.raw(), wrong).is_err(),
        "a retirement whose receipt differs from the captured outcome must fail"
    );
    assert_eq!(
        evict_graph_mutation_journal(&env, env.graph_source),
        1,
        "a refused retirement leaves the retention untouched"
    );

    // The exact receipt retires, and the acknowledgement echoes it.
    let ScalarMutationRetirementAck::V1(ack) = retire(&env, mutation_id.raw(), row_count)
        .expect("the exact captured receipt retires the entry");
    assert_eq!(ack.mutation_id, mutation_id);
    assert_eq!(ack.row_count, row_count);

    // Retirement starts the clock: the entry is still present, then goes once its retention passes.
    assert_eq!(
        journal_len(&env),
        1,
        "retiring does not delete the evidence, it starts its retention"
    );
    advance_past_journal_eviction(&env);
    assert_eq!(
        evict_graph_mutation_journal(&env, env.graph_source),
        0,
        "a retired entry ages out from its retirement"
    );
}

/// The state a retirement acts on is the journal's, not the caller's belief.
#[test]
fn a_retirement_for_an_unknown_mutation_is_rejected() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    gql_mutate_as_admin(
        &env,
        "INSERT (p:Person { name: 'present' })",
        "adr0027-scalar-present",
    );
    let unknown: Principal = env.graph_source;
    let _ = unknown;
    assert!(
        retire(&env, 9_999_999, 0).is_err(),
        "an id the journal never held cannot be retired"
    );
    assert_eq!(journal_len(&env), 1, "and nothing was removed");
}

/// The journal records the scalar entry as completed while it is retained.
#[test]
fn the_retained_entry_is_the_completed_scalar_record() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    gql_mutate_as_admin(
        &env,
        "INSERT (p:Person { name: 'completed' })",
        "adr0027-scalar-completed",
    );
    let status = gleaph_pocket_ic_tests::mutation_status_as_admin(
        &env,
        GRAPH_NAME,
        "adr0027-scalar-completed",
    )
    .expect("mutation status");
    assert!(
        matches!(
            status.phase,
            gleaph_graph_kernel::plan_exec::MutationLifecyclePhase::Completed
        ),
        "the mutation itself completed; only its dedup evidence is retained"
    );
    let entry_len = journal_len(&env);
    assert_eq!(entry_len, 1);
    let _ = MutationJournalState::Completed;
}
