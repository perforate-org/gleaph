//! PocketIC contracts for ADR 0059 index-build lifecycle convergence
//! (GAP-2026-07-29-006's remaining half: cross-canister runtime proof over a
//! two-shard federation with pre-existing data).
//!
//! # Scenario-to-symbol map
//!
//! Verified by direct read at main `3c75d2fb5` (2026-08-22):
//!
//! - Submission: Candid update `apply_schema_migration`
//!   (`crates/router/src/api/control.rs`) routes through
//!   `RouterStore::admin_apply_schema_migration_control`
//!   (`facade/store/schema_migration.rs`) to `index::apply_index_migration`
//!   when `gleaph_index_ddl::try_parse` matches.
//! - First apply: whole-payload preflight, index-name interning, one hidden
//!   `Preparing` catalog row, ledger record
//!   `SchemaMigrationRecordState::PendingIndex`; returns
//!   `SchemaMigrationApplyStatus::Progress` with no cross-canister call.
//! - Replay of the exact envelope resumes via `resume_existing` ->
//!   `advance_sequence`: one bounded step per call; the migration reaches
//!   `Applied` only after every build published `IndexLifecycleState::Active`,
//!   and terminal failures clean up to `Failed`.
//! - Status surfaces used here: the apply return value and the router query
//!   `get_indexed_property_catalog(logical_graph_name)` whose memberships
//!   project `IndexMaintenancePhase::{Building, Sealing, Active}`
//!   (`Preparing`/`Aborting` project as absent).
//! - Drive composition (`real_index_migration_driver()`): Register = graph-index
//!   `register_index_build` plus per-shard Graph
//!   `admin_register_index_export_scope`; Build = graph-index
//!   `advance_index_build` pulling up to `MAX_INDEX_BUILD_ADVANCE_PAGES = 4`
//!   canonical pages (`MAX_CANONICAL_EXPORT_PAGE_ITEMS = 10_000`) per call;
//!   Seal = freeze scopes capturing `admitted_through`, `seal_index_build`
//!   with shard watermarks, outbox drains, `index_build_status` poll, scope
//!   activation on convergence.
//! - Fence contract mirrored from the six Graph unit regressions
//!   (`crates/graph/src/facade/store/labels.rs`): an eligibility-changing label
//!   gain/loss on an indexed `(label, property)` emits the exact build envelope
//!   while `Building` and rejects before canonical mutation while `Sealing`.
//!   E2E trigger syntax: `SET n IS L` / `REMOVE n IS L`.
//! - Upgrade pattern (`canister_upgrade_persistence.rs`): same-wasm
//!   `pic.upgrade_canister`; this slice upgrades all five federation canisters
//!   mid-drive.
//!
//! # Boundary notes
//!
//! - Edge-INLINE storage is not enumerated by the edge backfill; that hole is
//!   GAP-2026-07-29-001 (Open) and is deliberately NOT asserted here. Edge
//!   values are covered through sidecar (`EDGE_PROPERTIES`) seeding only.
//! - With current page budgets one Build advance can seed tens of thousands of
//!   facts, so fixture-scale data converges within a single advance round. The
//!   upgrade scenario therefore proves persistence and resumption of the
//!   registered build state across the upgrade boundary rather than partial-page
//!   watermark resumption, which remains covered by graph-index worker units.

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_gql_ic::GqlWireRows;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::index::{
    IndexBuildStatus, IndexMaintenancePhase, IndexedPropertyCatalog, PhysicalIndexId,
};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, SchemaMigrationApplyStatus, SchemaMigrationGraphSelector,
    SchemaMigrationRecordState,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, e2e_insert_directed_edge_with_property,
    e2e_insert_vertex_with_label_and_property, ensure_edge_label, ensure_property,
    ensure_vertex_label, gql_mutate_as_admin, gql_mutate_as_admin_expect_err, gql_query_as_admin,
    gql_query_as_admin_expect_err, install_federation, query_as_router, wasm_bytes,
};

const MIGRATION_ID: &str = "000101_adr0059_age";
const AGE_INDEX_DDL: &str = "CREATE INDEX adr0059_person_age FOR (n:Person) ON (n.age)";
const MAX_DRIVE_ROUNDS: usize = 32;

fn migration_args(id: &str, statement: &str) -> ApplySchemaMigrationArgs {
    let selector = SchemaMigrationGraphSelector::Default;
    ApplySchemaMigrationArgs::V1(ApplySchemaMigrationArgsV1 {
        id: id.to_owned(),
        parent: None,
        graph_selector: selector.clone(),
        checksum: gleaph_migration_api::schema_migration_checksum(
            id,
            None,
            &selector,
            statement.as_bytes(),
        ),
        statement: statement.to_owned(),
    })
}

fn apply_once(
    env: &FederationEnv,
    args: &ApplySchemaMigrationArgs,
) -> ApplySchemaMigrationResultV1 {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "apply_schema_migration",
            Encode!(&args).expect("encode apply_schema_migration"),
        )
        .unwrap_or_else(|e| panic!("apply_schema_migration on router: {e:?}"));
    let decoded: Result<ApplySchemaMigrationResult, RouterError> =
        Decode!(&bytes, Result<ApplySchemaMigrationResult, RouterError>)
            .expect("decode apply_schema_migration");
    match decoded {
        Ok(ApplySchemaMigrationResult::V1(result)) => result,
        Err(err) => panic!("apply_schema_migration rejected: {err:?}"),
    }
}

/// First vertex-index membership phase projected by the router catalog, if any.
fn vertex_phase(env: &FederationEnv) -> Option<IndexMaintenancePhase> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_indexed_property_catalog",
            Encode!(&GRAPH_NAME.to_string()).expect("encode graph name"),
        )
        .unwrap_or_else(|e| panic!("get_indexed_property_catalog on router: {e:?}"));
    let catalog: Result<IndexedPropertyCatalog, RouterError> =
        Decode!(&bytes, Result<IndexedPropertyCatalog, RouterError>)
            .expect("decode get_indexed_property_catalog");
    catalog
        .expect("catalog query ok")
        .vertex_indexes
        .first()
        .map(|m| m.phase)
}

/// Physical index id from the projected membership (requires phase >= Building).
fn projected_physical_index(env: &FederationEnv) -> PhysicalIndexId {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_indexed_property_catalog",
            Encode!(&GRAPH_NAME.to_string()).expect("encode graph name"),
        )
        .unwrap_or_else(|e| panic!("get_indexed_property_catalog on router: {e:?}"));
    let catalog: Result<IndexedPropertyCatalog, RouterError> =
        Decode!(&bytes, Result<IndexedPropertyCatalog, RouterError>)
            .expect("decode get_indexed_property_catalog");
    catalog
        .expect("catalog query ok")
        .vertex_indexes
        .first()
        .map(|m| m.physical_index_id)
        .expect("a Building-or-later index must be projected")
}

fn index_status_as_router(
    env: &FederationEnv,
    physical_index_id: PhysicalIndexId,
) -> IndexBuildStatus {
    query_as_router(env, env.index, "index_build_status", physical_index_id)
}

fn int_column(env: &FederationEnv, query: &str) -> Vec<i64> {
    let GqlQueryResult { rows_blob, .. } = gql_query_as_admin(env, query);
    let blob = rows_blob.as_ref().expect("rows_blob present");
    let wire = GqlWireRows::decode_blob(blob).expect("decode rows_blob");
    wire.rows
        .into_iter()
        .map(|row| {
            let value_row = row.try_into_value_row().expect("wire row to value row");
            match value_row.get("v").expect("column v") {
                Value::Int64(v) => *v,
                other => panic!("expected Int64 in column v, got {other:?}"),
            }
        })
        .collect()
}

/// Seeds the convergence fixture: Person/age values on both shards (including a
/// duplicated equality value and a non-Person decoy carrying the same property)
/// plus one edge sidecar value per shard. Returns nothing; callers know the
/// fixture constants.
fn seed_convergence_fixture(env: &FederationEnv) {
    let person = ensure_vertex_label(env, "Person").raw();
    let age = ensure_property(env, "age").raw();
    let knows = ensure_edge_label(env, "KNOWS").raw();
    let weight = ensure_property(env, "weight").raw();

    for value in [10_i64, 20, 30, 30, 40] {
        e2e_insert_vertex_with_label_and_property(env, env.graph_source, person, age, value);
    }
    // Non-Person vertex carrying the same property: must never leak into
    // Person-scoped index results (label scoping proof).
    let other = ensure_vertex_label(env, "Other").raw();
    e2e_insert_vertex_with_label_and_property(env, env.graph_source, other, age, 30);
    for value in [15_i64, 25, 35] {
        e2e_insert_vertex_with_label_and_property(env, env.graph_dest, person, age, value);
    }

    // Edge sidecar values: enumerated by the backfill through EDGE_PROPERTIES;
    // their presence exercises mixed canonical export without asserting
    // edge-INLINE completeness (GAP-2026-07-29-001 boundary).
    e2e_insert_directed_edge_with_property(env, env.graph_source, 0, 1, knows, weight, 7);
    e2e_insert_directed_edge_with_property(env, env.graph_dest, 0, 1, knows, weight, 8);
}

#[test]
fn create_index_migration_converges_active_with_complete_postings() {
    let env = install_federation();
    seed_convergence_fixture(&env);

    // Before Active the planner projects no membership for the indexed
    // property. An UNLABELED read therefore has neither a property nor a label
    // anchor, so a federation dispatch fails closed. (Labeled reads keep
    // working pre-Active through the label-scan fallback.)
    let err = gql_query_as_admin_expect_err(&env, "MATCH (n) WHERE n.age = 30 RETURN n.age AS v");
    match err {
        RouterError::InvalidArgument(message) => assert_eq!(
            message, "no index anchor: single-shard graph required",
            "pre-Active anchorless federation reads fail closed with the exact dispatch error"
        ),
        other => panic!("expected InvalidArgument no-anchor error, got {other:?}"),
    }

    let args = migration_args(MIGRATION_ID, AGE_INDEX_DDL);
    let mut phases: Vec<IndexMaintenancePhase> = Vec::new();
    let mut rounds = 0_usize;
    loop {
        rounds += 1;
        assert!(rounds <= MAX_DRIVE_ROUNDS, "migration did not converge");
        let result = apply_once(&env, &args);
        if let Some(phase) = vertex_phase(&env) {
            if phases.last() != Some(&phase) {
                phases.push(phase);
            }
        }
        match result.status {
            SchemaMigrationApplyStatus::Progress(_) => {
                assert!(
                    matches!(
                        &result.record,
                        gleaph_migration_api::SchemaMigrationRecord::V1(v1)
                            if matches!(v1.state, SchemaMigrationRecordState::PendingIndex { .. })
                    ),
                    "Progress status must carry a PendingIndex record"
                );
            }
            SchemaMigrationApplyStatus::Applied => break,
            SchemaMigrationApplyStatus::Replay => {
                panic!("fresh migration cannot replay before reaching Applied")
            }
            other => panic!("migration terminated early: {other:?}"),
        }
    }
    // The full lifecycle was traversed observably.
    assert!(
        phases.contains(&IndexMaintenancePhase::Building),
        "lifecycle must pass Building, observed {phases:?}"
    );
    assert!(
        phases.contains(&IndexMaintenancePhase::Sealing),
        "lifecycle must pass Sealing, observed {phases:?}"
    );
    assert_eq!(phases.last(), Some(&IndexMaintenancePhase::Active));

    // The ledger records the terminal Applied state with CreateIndex profiles.
    let list_bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_schema_migrations",
            Encode!(&gleaph_migration_api::ListSchemaMigrationsArgs::V1(
                gleaph_migration_api::ListSchemaMigrationsArgsV1 {
                    start_after: None,
                    limit: 16,
                }
            ))
            .expect("encode list_schema_migrations"),
        )
        .unwrap_or_else(|e| panic!("list_schema_migrations on router: {e:?}"));
    let listed: Result<gleaph_migration_api::ListSchemaMigrationsResult, RouterError> =
        Decode!(&list_bytes, Result<gleaph_migration_api::ListSchemaMigrationsResult, RouterError>)
            .expect("decode list_schema_migrations");
    let listed = match listed {
        Ok(gleaph_migration_api::ListSchemaMigrationsResult::V1(v1)) => v1,
        other => panic!("unexpected list_schema_migrations result: {other:?}"),
    };
    let applied_record = listed
        .migrations
        .iter()
        .find(|record| matches!(record, gleaph_migration_api::SchemaMigrationRecord::V1(v1) if v1.id == MIGRATION_ID))
        .expect("migration record present in canonical chain");
    // SchemaMigrationRecord currently has a single V1 variant.
    let gleaph_migration_api::SchemaMigrationRecord::V1(record_v1) = applied_record;
    assert!(matches!(
        record_v1.state,
        gleaph_migration_api::SchemaMigrationRecordState::Applied { .. }
    ));
    assert!(matches!(
        record_v1.profile.as_slice(),
        [gleaph_migration_api::SchemaMigrationStatementProfile::CreateIndex]
    ));

    // Equality completeness across both shards, including multiplicity.
    let mut eq30 = int_column(&env, "MATCH (n:Person) WHERE n.age = 30 RETURN n.age AS v");
    eq30.sort();
    assert_eq!(
        eq30,
        vec![30, 30],
        "both shards' pre-existing 30s are visible"
    );

    // Range completeness: every seeded Person age inside the interval, exactly.
    let mut range = int_column(
        &env,
        "MATCH (n:Person) WHERE n.age >= 10 AND n.age < 40 RETURN n.age AS v",
    );
    range.sort();
    assert_eq!(range, vec![10, 15, 20, 25, 30, 30, 35]);

    // Active projection: the previously anchorless read is now served through
    // the equality anchor. The non-Person decoy carrying age 30 must stay
    // excluded because postings are label-scoped.
    let mut anchorless = int_column(&env, "MATCH (n) WHERE n.age = 30 RETURN n.age AS v");
    anchorless.sort();
    assert_eq!(
        anchorless,
        vec![30, 30],
        "post-Active the equality anchor serves the read and excludes non-Person values"
    );
}

#[test]
fn label_transition_fence_holds_at_e2e_building_and_sealing() {
    let env = install_federation();
    let person = ensure_vertex_label(&env, "Person").raw();
    let age = ensure_property(&env, "age").raw();
    // Eligibility-gain fixture: carries the indexed property but not the
    // indexed label yet.
    let other = ensure_vertex_label(&env, "Other").raw();
    for value in [10_i64, 20, 30] {
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, person, age, value);
    }
    e2e_insert_vertex_with_label_and_property(&env, env.graph_source, other, age, 99);
    for value in [15_i64] {
        e2e_insert_vertex_with_label_and_property(&env, env.graph_dest, person, age, value);
    }

    let args = migration_args(MIGRATION_ID, AGE_INDEX_DDL);

    // Drive one bounded step at a time into Building. The first apply only
    // creates the Preparing row; the register and Building transitions follow
    // on subsequent exact replays.
    let mut rounds = 0_usize;
    loop {
        rounds += 1;
        assert!(rounds <= MAX_DRIVE_ROUNDS, "never reached Building");
        apply_once(&env, &args);
        if vertex_phase(&env) == Some(IndexMaintenancePhase::Building) {
            break;
        }
    }

    // While Building: an eligibility GAIN must be admitted and durably emit the
    // exact build envelope (E2E mirror of building_label_gain_emits_exact_...
    // / _loss regressions). The gained vertex keeps its pre-existing property
    // value, so its posting must join the build through the emitted op.
    // RETURN-less federated mutations report row_count 0; the commit is
    // verified through the follow-up MATCH below.
    let gain_rows = gql_mutate_as_admin(
        &env,
        "MATCH (n:Other) SET n IS Person",
        "adr0059_fence_gain",
    );
    assert_eq!(gain_rows, 0, "RETURN-less mutations report zero rows");
    let mut gained = int_column(&env, "MATCH (n:Person) RETURN n.age AS v");
    gained.sort();
    assert_eq!(
        gained,
        vec![10, 15, 20, 30, 99],
        "label gain on the indexed property is admitted while Building"
    );

    // Drive into Sealing.
    loop {
        rounds += 1;
        assert!(rounds <= MAX_DRIVE_ROUNDS, "never reached Sealing");
        apply_once(&env, &args);
        if vertex_phase(&env) == Some(IndexMaintenancePhase::Sealing) {
            break;
        }
    }

    // While Sealing: an eligibility LOSS must reject before any canonical
    // mutation (mirror of sealing_label_loss_rejects_before_any_mutation).
    gql_mutate_as_admin_expect_err(
        &env,
        "MATCH (n:Person) WHERE n.age = 30 REMOVE n IS Person",
        "adr0059_fence_loss",
    );

    // Canonical state survived the rejection untouched: still four Person
    // vertices (3 + 1 gained), including both pre-existing 30s and the gained
    // 99.
    let mut persons = int_column(&env, "MATCH (n:Person) RETURN n.age AS v");
    persons.sort();
    assert_eq!(
        persons,
        vec![10, 15, 20, 30, 99],
        "rejection left canonical labels unchanged"
    );

    // Converge to Active despite the mid-build fence traffic.
    loop {
        rounds += 1;
        assert!(
            rounds <= MAX_DRIVE_ROUNDS,
            "migration did not converge after fence"
        );
        let result = apply_once(&env, &args);
        if matches!(
            result.status,
            SchemaMigrationApplyStatus::Applied | SchemaMigrationApplyStatus::Replay
        ) {
            break;
        }
        assert!(
            !matches!(result.status, SchemaMigrationApplyStatus::Failed(_)),
            "fence traffic must not fail the build"
        );
    }

    // The gained vertex's pre-existing value is served by the index; the
    // rejected removal did not lose either 30.
    let mut eq30 = int_column(&env, "MATCH (n:Person) WHERE n.age = 30 RETURN n.age AS v");
    eq30.sort();
    assert_eq!(eq30, vec![30]);
    let mut eq99 = int_column(&env, "MATCH (n:Person) WHERE n.age = 99 RETURN n.age AS v");
    eq99.sort();
    assert_eq!(
        eq99,
        vec![99],
        "Building-time eligibility gain converged into postings"
    );
}

/// Upgrades all five federation canisters (router, both index, both graph) to
/// the same wasm, mirroring `canister_upgrade_persistence::upgrade_all`.
fn upgrade_federation_in_place(env: &FederationEnv) {
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    for (principal, wasm_env_var, label) in [
        (env.router, "ROUTER_WASM", "router"),
        (env.index, "INDEX_WASM", "index_source"),
        (env.index_dest, "INDEX_WASM", "index_dest"),
        (env.graph_source, "GRAPH_WASM", "graph_source"),
        (env.graph_dest, "GRAPH_WASM", "graph_dest"),
    ] {
        env.pic
            .upgrade_canister(principal, wasm_bytes(wasm_env_var), empty.clone(), None)
            .unwrap_or_else(|e| panic!("upgrade {label}: {e:?}"));
    }
}

#[test]
fn same_wasm_upgrade_mid_build_resumes_and_converges() {
    let env = install_federation();
    let person = ensure_vertex_label(&env, "Person").raw();
    let age = ensure_property(&env, "age").raw();
    for shard_graph in [env.graph_source, env.graph_dest] {
        for base in [0_i64, 100] {
            for offset in 0..20_i64 {
                e2e_insert_vertex_with_label_and_property(
                    &env,
                    shard_graph,
                    person,
                    age,
                    base + offset,
                );
            }
        }
    }

    let args = migration_args(MIGRATION_ID, AGE_INDEX_DDL);

    // Drive into Building and perform at least one Build advance so the
    // graph-index build holds persisted seeded-item progress.
    let mut rounds = 0_usize;
    loop {
        rounds += 1;
        assert!(rounds <= MAX_DRIVE_ROUNDS, "never reached Building");
        apply_once(&env, &args);
        if vertex_phase(&env) == Some(IndexMaintenancePhase::Building) {
            break;
        }
    }
    let physical_index_id = projected_physical_index(&env);
    loop {
        let status = index_status_as_router(&env, physical_index_id);
        if status.progress.seeded_items > 0 {
            break;
        }
        rounds += 1;
        assert!(rounds <= MAX_DRIVE_ROUNDS, "build never seeded any item");
        apply_once(&env, &args);
    }
    let before_upgrade = index_status_as_router(&env, physical_index_id);
    assert!(before_upgrade.progress.seeded_items > 0);

    // Upgrade every federation canister to the same wasm mid-drive.
    upgrade_federation_in_place(&env);

    // The registered build state survived on graph-index.
    let after_upgrade = index_status_as_router(&env, physical_index_id);
    assert_eq!(
        after_upgrade.progress.seeded_items, before_upgrade.progress.seeded_items,
        "persisted build watermarks survive the upgrade boundary"
    );

    // Resume from persisted state and converge to Active with complete postings.
    loop {
        rounds += 1;
        assert!(
            rounds <= MAX_DRIVE_ROUNDS,
            "migration did not converge after upgrade"
        );
        let result = apply_once(&env, &args);
        if matches!(
            result.status,
            SchemaMigrationApplyStatus::Applied | SchemaMigrationApplyStatus::Replay
        ) {
            break;
        }
        assert!(
            !matches!(result.status, SchemaMigrationApplyStatus::Failed(_)),
            "upgrade must not fail the resumable build"
        );
    }

    // Completeness across both shards: all 80 pre-existing values are visible
    // through equality spot checks and one full-range query.
    let mut full = int_column(
        &env,
        "MATCH (n:Person) WHERE n.age >= 0 AND n.age < 200 RETURN n.age AS v",
    );
    full.sort();
    let expected: Vec<i64> = (0..20).chain(100..120).collect();
    let mut expected_twice: Vec<i64> = expected
        .iter()
        .copied()
        .chain(expected.iter().copied())
        .collect();
    expected_twice.sort();
    assert_eq!(
        full, expected_twice,
        "every pre-existing value from both shards is visible"
    );

    let mut eq115 = int_column(&env, "MATCH (n:Person) WHERE n.age = 115 RETURN n.age AS v");
    eq115.sort();
    assert_eq!(eq115, vec![115, 115]);
}
