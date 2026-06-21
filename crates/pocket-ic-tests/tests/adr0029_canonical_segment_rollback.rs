//! PocketIC proof for ADR 0029 Phase 1: a graph-shard canonical mutation segment
//! that traps after a local write rolls back the **whole** message.
//!
//! A single linear DML statement (`MATCH ... INSERT ... DELETE ...`) binds the
//! matched hub in the read phase, then the shard applies the write tail
//! (`INSERT` then `DELETE`) as one shard-local canonical segment with no
//! inter-canister `await` inside it (ADR 0029 §1). The segment runs `INSERT` (a
//! canonical write) before `DELETE`; deleting the matched hub traps inside the
//! same segment (it still has an incident edge). IC message-execution atomicity
//! must then discard every canonical write in that message — the orphan inserted
//! before the trap must not survive.
//!
//! Two guards make the result unambiguous:
//!   * A plain `INSERT` of the same orphan label commits and is observable, so the
//!     write mechanism is real (not a silent no-op).
//!   * The router surfaces the graph trap with a "DML atomic section" marker, and
//!     the trap fires at the `DELETE` op — which the plan reaches only *after* the
//!     `INSERT` op — so execution provably entered the segment and wrote first.
//!
//! Together they show the empty-after result is rollback, not a write that never happened.

use candid::{Decode, Encode};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_vertex_label, gql_execute_idempotent_as_admin, gql_query_as_admin,
    install_single_shard_federation,
};

/// Issue a router `gql_execute_idempotent` that must NOT commit because the graph
/// shard traps inside its DML atomic section. Asserting the surfaced error carries
/// the "DML atomic section" marker proves execution actually entered the canonical
/// segment (a mid-segment trap), not a pre-execution parse/plan rejection that
/// would never have written anything.
fn gql_execute_expect_segment_trap(env: &FederationEnv, query: &str, client_mutation_key: &str) {
    let outcome = env.pic.update_call(
        env.router,
        env.admin,
        "gql_execute_idempotent",
        Encode!(
            &query.to_string(),
            &Vec::<u8>::new(),
            &client_mutation_key.to_string()
        )
        .expect("encode gql_execute_idempotent"),
    );
    let message = match outcome {
        Ok(reply) => match Decode!(&reply, Result<GqlQueryResult, RouterError>) {
            Ok(Err(err)) => format!("{err:?}"),
            Ok(Ok(result)) => panic!(
                "trapping DML must not commit, got row_count {}",
                result.row_count
            ),
            Err(err) => panic!("decode gql_execute_idempotent: {err}"),
        },
        // A graph trap that propagates as a raw call rejection also carries the
        // canister trap message.
        Err(reject) => format!("{reject:?}"),
    };
    assert!(
        message.contains("DML atomic section"),
        "expected a mid-segment trap, got error: {message}"
    );
}

fn count(env: &FederationEnv, query: &str) -> u64 {
    gql_query_as_admin(env, query).row_count
}

#[test]
fn canonical_segment_trap_rolls_back_whole_message() {
    let env = install_single_shard_federation();

    // Pre-intern the orphan label whose only writer is the rolled-back segment.
    // Catalog interning lives in the router independently of the graph shard's
    // canonical state, so the verification query stays resolvable after rollback.
    admin_intern_vertex_label(&env, "RollbackOrphan");

    // Setup: an attached hub (a vertex with an out-edge), so a plain `DELETE` of
    // it traps inside the segment.
    let _ = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:AttachedHub)-[:TrapRel]->(:TrapSink)",
        "adr0029_setup_attached",
    );
    assert_eq!(count(&env, "MATCH (n:AttachedHub) RETURN n"), 1);
    assert_eq!(count(&env, "MATCH (n:TrapSink) RETURN n"), 1);

    // Vacuity guard: a committed INSERT of the same orphan label persists and is
    // observable by label scan. This proves the write mechanism is real, so the
    // trap case's empty-after result is attributable to rollback.
    let _ = gql_execute_idempotent_as_admin(&env, "INSERT (:CtrlOrphan)", "adr0029_ctrl_commit");
    assert_eq!(
        count(&env, "MATCH (n:CtrlOrphan) RETURN n"),
        1,
        "control: a committed INSERT must persist and be observable by label scan"
    );
    assert_eq!(count(&env, "MATCH (n:RollbackOrphan) RETURN n"), 0);

    // Trap: one statement, one shard-local canonical segment. The plan inserts the
    // orphan, then `DELETE h` traps (the matched hub has an incident edge). The
    // trap fires at the DELETE op, which the plan reaches only after the INSERT op
    // already wrote the orphan, so whole-message rollback must erase the orphan.
    gql_execute_expect_segment_trap(
        &env,
        "MATCH (h:AttachedHub) INSERT (:RollbackOrphan) DELETE h",
        "adr0029_trap_rollback",
    );

    assert_eq!(
        count(&env, "MATCH (n:RollbackOrphan) RETURN n"),
        0,
        "whole-message rollback: the orphan written before the trap must not persist"
    );
    assert_eq!(
        count(&env, "MATCH (n:AttachedHub) RETURN n"),
        1,
        "whole-message rollback: the matched hub must survive the failed DELETE"
    );
    assert_eq!(
        count(&env, "MATCH (n:TrapSink) RETURN n"),
        1,
        "whole-message rollback must not disturb pre-existing canonical state"
    );
}
