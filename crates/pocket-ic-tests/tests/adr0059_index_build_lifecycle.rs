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
//! # Edge-INLINE scenario-to-symbol map (GAP-2026-07-29-001)
//!
//! Verified by direct read at main `20710174e` (2026-08-23); the inline
//! scenarios below drive these committed symbols. **Status:** both scenarios
//! previously exposed GAP-2026-08-22-001 (`Failed(TargetRejected)` on directed
//! edge builds) and ran `#[ignore]`d; the identity contract fix conformed
//! graph-index seeding/sieves and the Graph read path, and these two scenarios
//! are now un-ignored as the owning cross-canister regression proof for
//! closing GAP-2026-07-29-001.
//!
//! - Inline schema registration: typed-graph DDL (`CREATE GRAPH TYPE ... NEXT
//!   CREATE GRAPH ... TYPED`) commits `ROUTER_EDGE_INLINE_PROPERTY_PROFILES`
//!   (`crates/router/src/facade/store/catalogs.rs`), which three consumers read:
//!   migration scope resolution, atomic-insert width validation, and the DML
//!   wire's resolved label table.
//! - Migration scope resolution: `resolve_inline_projection`
//!   (`facade/store/schema_migration/index.rs`) maps an eligible scalar inline
//!   slot to `CanonicalInlineProjection { source_property_id, byte_offset: 0,
//!   value_profile = source_profile }`; indexing a whole inline struct stays
//!   rejected ("index a leaf field instead").
//! - Domain selection: one physical index exports exactly one canonical domain.
//!   A registered `scope.inline` selects Graph's `export_edge_inline_page`
//!   (`crates/graph/src/index/canonical_export.rs`); without it the sidecar
//!   page runs. The dual-domain cursor walk in
//!   `edge_property_backfill.rs` belongs to the operator `advance_backfill`
//!   repair API, separate from this lifecycle.
//! - Enumeration identity: `export_edge_inline_page` visits outgoing rows only;
//!   an undirected row is kept only when `canonical_undirected_owner(owner,
//!   neighbor) == owner` (max endpoint), so each logical edge yields exactly one
//!   `CanonicalIndexableFact::Edge` and mirror double-posting cannot arise.
//!   graph-index binds every seeded fact to the registered target identity in
//!   `prepare_fact_posting` (`build_state.rs`) before inserting the posting.
//! - Read surfaces exercised post-Active: router planner stats project Active
//!   directional edge memberships (`planner_stats.rs::is_edge_indexed_for`);
//!   execution probes graph-index postings through `lookup_edge_equal_page` /
//!   `lookup_edge_range_page` (`scan/edge_index.rs`, `expand/candidates.rs`).
//! - Assertion surfaces used here: the projected catalog memberships
//!   (`get_indexed_property_catalog(...).edge_indexes`), direct
//!   `lookup_edge_equal_page` probes against both index canisters (posting
//!   truth, shard and canonical-owner identity), and GQL equality/range rows.
//! - Fixture symbols: directed seeds use `e2e_insert_directed_edge_with_inline_property`
//!   (caller-supplied `RawU16` profile, ADR 0034 convention); undirected seeds
//!   go through the router `atomic_insert` path with `directed: false` so the
//!   Router-resolved profile validates the bytes and the ordered batch inserts
//!   an undirected inline row.
//!
//! # Boundary notes
//!
//! - With current page budgets one Build advance can seed tens of thousands of
//!   facts, so fixture-scale data converges within a single advance round (this
//!   holds for edge builds as for vertex builds). The upgrade scenarios
//!   therefore prove persistence and resumption of the registered build state
//!   across the upgrade boundary rather than partial-page watermark resumption,
//!   which remains covered by graph-index worker units.

use candid::{Decode, Encode};
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_ic::GqlWireRows;
use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, RouterError, encode_global_vertex_id};
use gleaph_graph_kernel::index::{
    EdgePostingHitPage, IndexBuildStatus, IndexMaintenancePhase, IndexedEdgeMembership,
    IndexedPropertyCatalog, LookupEdgeEqualPageRequest, LookupEdgeRangePageRequest,
    LookupEqualPageRequest, LookupRangePageRequest, PhysicalIndexId, PostingHitPage,
    PostingRangeRequest,
};
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, MutationLifecyclePhase};
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, SchemaMigrationApplyStatus, SchemaMigrationGraphSelector,
    SchemaMigrationRecordState,
};
use gleaph_pocket_ic_tests::{
    DEST_SHARD, E2eRecordFieldValue, FederationEnv, GRAPH_NAME, SOURCE_SHARD,
    atomic_insert_as_admin, e2e_insert_directed_edge_with_inline_property,
    e2e_insert_directed_edge_with_property, e2e_insert_vertex_with_label,
    e2e_insert_vertex_with_label_and_property, e2e_insert_vertex_with_label_and_record,
    e2e_set_vertex_record, ensure_edge_label, ensure_property, ensure_vertex_label,
    federation_graph_element_id_encoding_key_bytes, gql_mutate_as_admin,
    gql_mutate_as_admin_expect_err, gql_query_as_admin, gql_query_as_admin_expect_err,
    install_federation, query_as_router, wasm_bytes,
};
use gleaph_router::types::{
    AtomicInsertEdgeV1, AtomicInsertEndpointV1, AtomicInsertOperationV1, AtomicInsertRequest,
    AtomicInsertRequestV1,
};

const MIGRATION_ID: &str = "000101_adr0059_age";
const AGE_INDEX_DDL: &str = "CREATE INDEX adr0059_person_age FOR (n:Person) ON (n.age)";
const MAX_DRIVE_ROUNDS: usize = 32;

const EDGE_INLINE_MIGRATION_ID: &str = "000102_adr0059_edge_inline";
const EDGE_INLINE_UPGRADE_MIGRATION_ID: &str = "000103_adr0059_edge_inline_upgrade";
const ROAD_LABEL: &str = "ROAD";
const LINK_LABEL: &str = "LINK";
const DISTANCE_PROPERTY: &str = "distance";
/// The undirected index uses its own property name so the two sub-builds never share one
/// property scope: label-less seed anchors stay unambiguous and posting probes per namespace
/// remain exact.
const LINK_DISTANCE_PROPERTY: &str = "link_distance";
/// Declares scalar `UINT16 INLINE` slots on a directed and an undirected edge
/// type and binds them to the default federation graph. The typed binding is
/// what registers the router inline schema that `resolve_inline_projection`
/// and atomic-insert validation consume.
const EDGE_INLINE_TYPE_DDL: &str = "CREATE GRAPH TYPE IF NOT EXISTS adr0059_road_type { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { distance UINT16 INLINE } CONNECTING (city -> city), UNDIRECTED EDGE Link LABEL LINK { link_distance UINT16 INLINE } CONNECTING (city ~ city) } NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED adr0059_road_type";
const ROAD_DISTANCE_DDL: &str =
    "CREATE INDEX adr0059_road_distance FOR ()-[e:ROAD]-() ON (e.distance)";
const LINK_DISTANCE_DDL: &str =
    "CREATE INDEX adr0059_link_distance FOR () ~[e:LINK]~ () ON (e.link_distance)";

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
                Value::Int32(v) => i64::from(*v),
                Value::Int16(v) => i64::from(*v),
                Value::Int8(v) => i64::from(*v),
                Value::Uint64(v) => i64::try_from(*v).expect("uint64 fits i64"),
                Value::Uint32(v) => i64::from(*v),
                // ADR 0034 scalar inline slots decode to the exact unsigned
                // width (UINT16 INLINE projects Uint16), so the edge-INLINE
                // scenarios read integer columns through this arm too.
                Value::Uint16(v) => i64::from(*v),
                Value::Uint8(v) => i64::from(*v),
                other => panic!("expected an integer in column v, got {other:?}"),
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

// ---------------------------------------------------------------------------
// Edge-INLINE fixtures and assertion surfaces (GAP-2026-07-29-001)
// ---------------------------------------------------------------------------

fn u16_distance_profile() -> EdgeInlinePropertyProfile {
    EdgeInlinePropertyProfile {
        byte_width: 2,
        encoding: EdgeInlinePropertyEncoding::RawU16,
    }
}

/// Sortable index key bytes for a `UINT16` inline literal; identical to what
/// the export emits after decoding (`INDEX_KEY_NUMERIC` is width-agnostic).
fn u16_distance_key(value: u16) -> Vec<u8> {
    value_to_index_key_bytes(&Value::Uint16(value))
        .expect("uint16 encodes")
        .expect("non-null key")
}

/// Declares the typed inline schema on the default federation graph.
fn declare_edge_inline_schema(env: &FederationEnv) {
    gql_mutate_as_admin(env, EDGE_INLINE_TYPE_DDL, "adr0059_edge_inline_schema");
}

/// Seeds one directed `ROAD` edge carrying a `UINT16` inline `distance` on
/// `shard`, between two fresh City-labeled vertices.
fn seed_directed_road_edge(env: &FederationEnv, shard: candid::Principal, distance: u16) {
    let city = ensure_vertex_label(env, "City").raw();
    let road = ensure_edge_label(env, ROAD_LABEL).raw();
    let source = e2e_insert_vertex_with_label(env, shard, city).local_vertex_id;
    let target = e2e_insert_vertex_with_label(env, shard, city).local_vertex_id;
    e2e_insert_directed_edge_with_inline_property(
        env,
        shard,
        source,
        target,
        road,
        distance.to_le_bytes().to_vec(),
        u16_distance_profile(),
    );
}

/// Seeds one undirected `LINK` edge carrying a `UINT16` inline `distance`
/// through the router atomic-insert path (`directed: false`), which validates
/// the bytes against the Router-resolved inline profile. Returns the two local
/// endpoint ids so callers can assert the canonical max-endpoint owner.
fn seed_undirected_link_edge(
    env: &FederationEnv,
    shard: candid::Principal,
    distance: u16,
    mutation_key: &str,
) -> (u32, u32) {
    let city = ensure_vertex_label(env, "City").raw();
    let low = e2e_insert_vertex_with_label(env, shard, city);
    let high = e2e_insert_vertex_with_label(env, shard, city);
    let encoding_key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(env));
    let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: mutation_key.to_owned(),
        graph_name: Some(GRAPH_NAME.to_owned()),
        operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
            source: AtomicInsertEndpointV1::Existing(
                encode_global_vertex_id(&encoding_key, low.global_vertex_id)
                    .0
                    .to_vec(),
            ),
            target: AtomicInsertEndpointV1::Existing(
                encode_global_vertex_id(&encoding_key, high.global_vertex_id)
                    .0
                    .to_vec(),
            ),
            directed: false,
            edge_label_name: Some(LINK_LABEL.to_owned()),
            inline_property: Some(distance.to_le_bytes().to_vec()),
            initial_edge_properties: Vec::new(),
        })],
    });
    let status =
        atomic_insert_as_admin(env, request).expect("undirected inline atomic insert commits");
    assert_eq!(
        status.status.phase,
        MutationLifecyclePhase::Completed,
        "undirected inline seed must complete durably"
    );
    (low.local_vertex_id, high.local_vertex_id)
}

/// Every projected edge membership from the router catalog.
fn edge_memberships(env: &FederationEnv) -> Vec<IndexedEdgeMembership> {
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
    catalog.expect("catalog query ok").edge_indexes
}

fn edge_phase_for(
    memberships: &[IndexedEdgeMembership],
    label_raw: u16,
    property_raw: u32,
) -> Option<IndexMaintenancePhase> {
    memberships
        .iter()
        .filter(|m| m.label_id == label_raw && m.property_id == property_raw)
        .map(|m| m.phase)
        .next()
}

fn projected_edge_physical_index(
    memberships: &[IndexedEdgeMembership],
    label_raw: u16,
    property_raw: u32,
) -> PhysicalIndexId {
    memberships
        .iter()
        .find(|m| m.label_id == label_raw && m.property_id == property_raw)
        .map(|m| m.physical_index_id)
        .expect("a Building-or-later edge membership must be projected")
}

/// Bare-result graph-index page query (the lookup pages do not use the
/// `Result<R, String>` envelope `query_as_router` decodes).
fn edge_lookup_page<R: candid::CandidType + serde::de::DeserializeOwned>(
    env: &FederationEnv,
    index_canister: candid::Principal,
    method: &str,
    request: impl candid::CandidType,
) -> R {
    let bytes = env
        .pic
        .query_call(
            index_canister,
            env.router,
            method,
            Encode!(&request).expect("encode edge lookup request"),
        )
        .unwrap_or_else(|e| panic!("{method} on graph-index: {e:?}"));
    Decode!(&bytes, R).unwrap_or_else(|_| panic!("decode {method}"))
}

/// All equality postings for one `(physical index, property, value)` bucket across both
/// federation index canisters (one per shard), following resume cursors to exhaustion.
/// `label_id` stays `None` so the probe observes the stored label identities directly
/// instead of assuming them.
fn edge_equal_postings(
    env: &FederationEnv,
    physical_index_id: PhysicalIndexId,
    property_raw: u32,
    value: u16,
) -> Vec<gleaph_graph_kernel::index::EdgePostingHit> {
    let mut hits = Vec::new();
    for index_canister in [env.index, env.index_dest] {
        let mut after = None;
        loop {
            let page: EdgePostingHitPage = edge_lookup_page(
                env,
                index_canister,
                "lookup_edge_equal_page",
                LookupEdgeEqualPageRequest {
                    physical_index_id,
                    property_id: property_raw,
                    value: u16_distance_key(value),
                    label_id: None,
                    after,
                    limit: 128,
                },
            );
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
    }
    hits
}

/// All range postings over the half-open encoded interval `[low, high)`, unioned across
/// both federation index canisters.
fn edge_range_postings(
    env: &FederationEnv,
    physical_index_id: PhysicalIndexId,
    property_raw: u32,
    low: u16,
    high: u16,
) -> Vec<gleaph_graph_kernel::index::EdgePostingHit> {
    let mut hits = Vec::new();
    for index_canister in [env.index, env.index_dest] {
        let mut after = None;
        loop {
            let page: EdgePostingHitPage = edge_lookup_page(
                env,
                index_canister,
                "lookup_edge_range_page",
                LookupEdgeRangePageRequest {
                    physical_index_id,
                    property_id: property_raw,
                    range: PostingRangeRequest::Between {
                        low: u16_distance_key(low),
                        high: u16_distance_key(high),
                    },
                    label_id: None,
                    after,
                    limit: 128,
                },
            );
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
    }
    hits
}

/// The combined migration statement: directed ROAD (both direction buckets) and
/// undirected-only LINK sub-builds advance sequentially in payload order.
fn edge_inline_statement() -> String {
    format!("{ROAD_DISTANCE_DDL}\nNEXT {LINK_DISTANCE_DDL}")
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

/// Drives the combined ROAD+LINK edge migration until every sub-build reaches
/// `Applied`, collecting each membership's observed phases. Returns the final
/// projected memberships so callers can resolve physical namespaces.
fn drive_edge_inline_migration_to_applied(
    env: &FederationEnv,
    args: &ApplySchemaMigrationArgs,
    road: u16,
    road_property: u32,
    link: u16,
    link_property: u32,
) -> Vec<IndexedEdgeMembership> {
    let mut road_phases: Vec<IndexMaintenancePhase> = Vec::new();
    let mut link_phases: Vec<IndexMaintenancePhase> = Vec::new();
    let mut rounds = 0_usize;
    loop {
        rounds += 1;
        assert!(
            rounds <= MAX_DRIVE_ROUNDS,
            "edge migration did not converge"
        );
        let result = apply_once(env, args);
        if let Some(phase) = edge_phase_for(&edge_memberships(env), road, road_property)
            && road_phases.last() != Some(&phase)
        {
            road_phases.push(phase);
        }
        if let Some(phase) = edge_phase_for(&edge_memberships(env), link, link_property)
            && link_phases.last() != Some(&phase)
        {
            link_phases.push(phase);
        }
        match result.status {
            SchemaMigrationApplyStatus::Progress(_) => {}
            SchemaMigrationApplyStatus::Applied => break,
            SchemaMigrationApplyStatus::Replay => {
                panic!("fresh migration cannot replay before reaching Applied")
            }
            other => panic!("edge migration terminated early: {other:?}"),
        }
    }
    // Both sequential sub-builds traversed the full observable lifecycle.
    for (name, phases) in [("ROAD", &road_phases), ("LINK", &link_phases)] {
        assert!(
            phases.contains(&IndexMaintenancePhase::Building),
            "{name} lifecycle must pass Building, observed {phases:?}"
        );
        assert!(
            phases.contains(&IndexMaintenancePhase::Sealing),
            "{name} lifecycle must pass Sealing, observed {phases:?}"
        );
        assert_eq!(
            phases.last(),
            Some(&IndexMaintenancePhase::Active),
            "{name} must end Active"
        );
    }
    edge_memberships(env)
}

#[test]
fn edge_inline_create_index_migration_converges_active_with_complete_postings() {
    let env = install_federation();
    declare_edge_inline_schema(&env);
    let road = ensure_edge_label(&env, ROAD_LABEL).raw();
    let link = ensure_edge_label(&env, LINK_LABEL).raw();
    let distance = ensure_property(&env, DISTANCE_PROPERTY).raw();
    let link_distance = ensure_property(&env, LINK_DISTANCE_PROPERTY).raw();

    // Directed ROAD inline values across both shards; 30 appears twice on the
    // source shard so equality multiplicity is observable.
    for value in [10_u16, 20, 30, 30, 40] {
        seed_directed_road_edge(&env, env.graph_source, value);
    }
    for value in [15_u16, 25, 35] {
        seed_directed_road_edge(&env, env.graph_dest, value);
    }
    // One undirected LINK pair per shard for the canonical-owner rule.
    let (src_low, src_high) =
        seed_undirected_link_edge(&env, env.graph_source, 12, "adr0059_link_src");
    let (dst_low, dst_high) =
        seed_undirected_link_edge(&env, env.graph_dest, 28, "adr0059_link_dst");

    let args = migration_args(EDGE_INLINE_MIGRATION_ID, &edge_inline_statement());
    let memberships =
        drive_edge_inline_migration_to_applied(&env, &args, road, distance, link, link_distance);

    // Posting truth on the ROAD namespace: every seeded directed inline value
    // converged exactly once per logical edge, per shard, with multiplicity.
    let road_physical = projected_edge_physical_index(&memberships, road, distance);
    for (value, expected_shard, expected_count) in [
        (10_u16, SOURCE_SHARD, 1_usize),
        (20, SOURCE_SHARD, 1),
        (30, SOURCE_SHARD, 2),
        (40, SOURCE_SHARD, 1),
        (15, DEST_SHARD, 1),
        (25, DEST_SHARD, 1),
        (35, DEST_SHARD, 1),
    ] {
        let hits = edge_equal_postings(&env, road_physical, distance, value);
        assert_eq!(
            hits.len(),
            expected_count,
            "ROAD equality postings for {value}"
        );
        assert!(
            hits.iter().all(|hit| hit.shard_id == expected_shard),
            "ROAD postings for {value} must live on the seeding shard"
        );
    }
    assert_eq!(
        edge_range_postings(&env, road_physical, distance, 0, 1000).len(),
        8,
        "the full ROAD domain converges with no extra or missing postings"
    );

    // Undirected LINK postings: exactly one hit per logical pair, owned by the
    // max endpoint — the E2E canonical-owner mirror of
    // backfill_emits_only_the_canonical_undirected_owner.
    let link_physical = projected_edge_physical_index(&memberships, link, link_distance);
    for (value, expected_shard, low, high) in [
        (12_u16, SOURCE_SHARD, src_low, src_high),
        (28, DEST_SHARD, dst_low, dst_high),
    ] {
        let hits = edge_equal_postings(&env, link_physical, link_distance, value);
        assert_eq!(
            hits.len(),
            1,
            "undirected pair {value} posts once, no mirrors"
        );
        assert_eq!(hits[0].shard_id, expected_shard);
        assert_eq!(
            hits[0].owner_vertex_id,
            low.max(high),
            "the undirected canonical owner is the max endpoint"
        );
    }
    assert_eq!(
        edge_range_postings(&env, link_physical, link_distance, 0, 1000).len(),
        2,
        "both undirected pairs converge without duplicate identities"
    );

    // Equality completeness through GQL after Active, including multiplicity.
    let mut eq30 = int_column(
        &env,
        "MATCH ()-[e:ROAD]->() WHERE e.distance = 30 RETURN e.distance AS v",
    );
    eq30.sort();
    assert_eq!(
        eq30,
        vec![30, 30],
        "both pre-existing inline 30s are visible"
    );

    // Range completeness through GQL: every seeded directed inline value inside
    // the interval, exactly once per logical edge.
    let mut range = int_column(
        &env,
        "MATCH ()-[e:ROAD]->() WHERE e.distance >= 10 AND e.distance < 40 RETURN e.distance AS v",
    );
    range.sort();
    assert_eq!(range, vec![10, 15, 20, 25, 30, 30, 35]);

    // Undirected reads observe each pair exactly once post-Active.
    let eq12 = int_column(
        &env,
        "MATCH ()~[e:LINK]~() WHERE e.link_distance = 12 RETURN e.link_distance AS v",
    );
    assert_eq!(
        eq12,
        vec![12],
        "the undirected pair is visible exactly once"
    );
    let mut undirected_range = int_column(
        &env,
        "MATCH ()~[e:LINK]~() WHERE e.link_distance >= 0 AND e.link_distance < 1000 RETURN e.link_distance AS v",
    );
    undirected_range.sort();
    assert_eq!(undirected_range, vec![12, 28]);
}

#[test]
fn edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges() {
    let env = install_federation();
    declare_edge_inline_schema(&env);
    let road = ensure_edge_label(&env, ROAD_LABEL).raw();
    let link = ensure_edge_label(&env, LINK_LABEL).raw();
    let distance = ensure_property(&env, DISTANCE_PROPERTY).raw();
    let link_distance = ensure_property(&env, LINK_DISTANCE_PROPERTY).raw();

    // Inline-heavy build: 24 directed ROAD edges per shard plus one undirected
    // LINK pair per shard.
    for shard_graph in [env.graph_source, env.graph_dest] {
        for base in [0_u16, 100] {
            for offset in 0..12_u16 {
                seed_directed_road_edge(&env, shard_graph, base + offset);
            }
        }
    }
    let (src_low, src_high) =
        seed_undirected_link_edge(&env, env.graph_source, 200, "adr0059_upg_link_src");
    let _ = seed_undirected_link_edge(&env, env.graph_dest, 220, "adr0059_upg_link_dst");

    let args = migration_args(EDGE_INLINE_UPGRADE_MIGRATION_ID, &edge_inline_statement());

    // Drive into Building and perform at least one Build advance so the
    // graph-index build holds persisted seeded-item progress. The sequential
    // payload advances ROAD first.
    let mut rounds = 0_usize;
    loop {
        rounds += 1;
        assert!(rounds <= MAX_DRIVE_ROUNDS, "never reached Building");
        apply_once(&env, &args);
        if edge_phase_for(&edge_memberships(&env), road, distance)
            == Some(IndexMaintenancePhase::Building)
        {
            break;
        }
    }
    let memberships_before = edge_memberships(&env);
    let road_physical = projected_edge_physical_index(&memberships_before, road, distance);
    loop {
        let status = index_status_as_router(&env, road_physical);
        if status.progress.seeded_items > 0 {
            break;
        }
        rounds += 1;
        assert!(rounds <= MAX_DRIVE_ROUNDS, "build never seeded any item");
        apply_once(&env, &args);
    }
    let before_upgrade = index_status_as_router(&env, road_physical);
    assert!(before_upgrade.progress.seeded_items > 0);

    // Upgrade every federation canister to the same wasm mid-drive.
    upgrade_federation_in_place(&env);

    // The registered build state survived on graph-index.
    let after_upgrade = index_status_as_router(&env, road_physical);
    assert_eq!(
        after_upgrade.progress.seeded_items, before_upgrade.progress.seeded_items,
        "persisted build watermarks survive the upgrade boundary"
    );

    // Resume from persisted state and converge both sequential sub-builds.
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

    // Completeness across both shards: all 48 directed inline values are
    // visible through one full-range query and an equality spot check.
    let mut full = int_column(
        &env,
        "MATCH ()-[e:ROAD]->() WHERE e.distance >= 0 AND e.distance < 200 RETURN e.distance AS v",
    );
    full.sort();
    let expected: Vec<i64> = (0..12).chain(100..112).collect();
    let mut expected_twice: Vec<i64> = expected
        .iter()
        .copied()
        .chain(expected.iter().copied())
        .collect();
    expected_twice.sort();
    assert_eq!(
        full, expected_twice,
        "every pre-existing inline value from both shards is visible"
    );

    let mut eq105 = int_column(
        &env,
        "MATCH ()-[e:ROAD]->() WHERE e.distance = 105 RETURN e.distance AS v",
    );
    eq105.sort();
    assert_eq!(eq105, vec![105, 105]);

    // The undirected pair resumed across the upgrade boundary too.
    let eq200 = int_column(
        &env,
        "MATCH ()~[e:LINK]~() WHERE e.link_distance = 200 RETURN e.link_distance AS v",
    );
    assert_eq!(eq200, vec![200]);
    let link_memberships = edge_memberships(&env);
    let link_physical = projected_edge_physical_index(&link_memberships, link, link_distance);
    let hits = edge_equal_postings(&env, link_physical, link_distance, 200);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].shard_id, SOURCE_SHARD);
    assert_eq!(hits[0].owner_vertex_id, src_low.max(src_high));
}

// ---------------------------------------------------------------------------
// Vertex nested-record leaf scenario (ADR 0073 slices 1-3)
// ---------------------------------------------------------------------------

const NESTED_MIGRATION_ID: &str = "000104_adr0059_vertex_nested";
const NESTED_DDL: &str =
    "CREATE INDEX adr0059_person_stats_score FOR (n:Person) ON (n.stats.score)";
const STATS_SCORE_LEAF: &str = "stats.score";

fn int64_index_key(value: i64) -> Vec<u8> {
    value_to_index_key_bytes(&Value::Int64(value))
        .expect("int64 encodes")
        .expect("non-null key")
}

/// Every projected vertex membership from the router catalog.
fn vertex_memberships(
    env: &FederationEnv,
) -> Vec<gleaph_graph_kernel::index::IndexedVertexMembership> {
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
    catalog.expect("catalog query ok").vertex_indexes
}

fn nested_phase_for(
    memberships: &[gleaph_graph_kernel::index::IndexedVertexMembership],
    leaf_raw: u32,
) -> Option<IndexMaintenancePhase> {
    memberships
        .iter()
        .filter(|m| m.property_id == leaf_raw && m.field_path == STATS_SCORE_LEAF)
        .map(|m| m.phase)
        .next()
}

fn projected_nested_physical_index(
    memberships: &[gleaph_graph_kernel::index::IndexedVertexMembership],
    leaf_raw: u32,
) -> PhysicalIndexId {
    memberships
        .iter()
        .find(|m| m.property_id == leaf_raw && m.field_path == STATS_SCORE_LEAF)
        .map(|m| m.physical_index_id)
        .expect("a Building-or-later nested membership must be projected")
}

/// All vertex equality postings for one `(physical index, leaf property, value)` bucket
/// across both federation index canisters.
fn vertex_equal_postings(
    env: &FederationEnv,
    physical_index_id: PhysicalIndexId,
    property_raw: u32,
    value: i64,
) -> Vec<gleaph_graph_kernel::index::PostingHit> {
    let mut hits = Vec::new();
    for index_canister in [env.index, env.index_dest] {
        let mut after = None;
        loop {
            let page: PostingHitPage = edge_lookup_page(
                env,
                index_canister,
                "lookup_equal_page",
                LookupEqualPageRequest {
                    physical_index_id,
                    property_id: property_raw,
                    value: int64_index_key(value),
                    after,
                    limit: 128,
                },
            );
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
    }
    hits
}

/// All vertex range postings over the half-open encoded interval `[low, high)` across both
/// federation index canisters.
fn vertex_range_postings(
    env: &FederationEnv,
    physical_index_id: PhysicalIndexId,
    property_raw: u32,
    low: i64,
    high: i64,
) -> Vec<gleaph_graph_kernel::index::PostingHit> {
    let mut hits = Vec::new();
    for index_canister in [env.index, env.index_dest] {
        let mut after = None;
        loop {
            let page: PostingHitPage = edge_lookup_page(
                env,
                index_canister,
                "lookup_range_page",
                LookupRangePageRequest {
                    physical_index_id,
                    property_id: property_raw,
                    range: PostingRangeRequest::Between {
                        low: int64_index_key(low),
                        high: int64_index_key(high),
                    },
                    after,
                    limit: 128,
                },
            );
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
    }
    hits
}

/// Seeds one Person vertex whose `stats` record is `{ score }` on `shard`, returning the
/// local vertex id for later rewrites.
fn seed_person_record(
    env: &FederationEnv,
    shard: candid::Principal,
    stats_raw: u32,
    score: i64,
) -> u32 {
    e2e_insert_vertex_with_label_and_record(
        env,
        shard,
        ensure_vertex_label(env, "Person").raw(),
        stats_raw,
        vec![("score".to_owned(), E2eRecordFieldValue::Int(score))],
    )
    .local_vertex_id
}

#[test]
fn vertex_nested_create_index_migration_converges_active_with_complete_postings() {
    let env = install_federation();
    let person = ensure_vertex_label(&env, "Person").raw();
    let other = ensure_vertex_label(&env, "Other").raw();
    let stats = ensure_property(&env, "stats").raw();
    let score_leaf = ensure_property(&env, STATS_SCORE_LEAF).raw();

    // Pre-existing records on BOTH shards; 30 appears twice on the source shard so
    // equality multiplicity is observable. The score-10 vertex is rewritten post-Active.
    let mut first_rewrite_id = None;
    for score in [10_i64, 30, 30] {
        let local = seed_person_record(&env, env.graph_source, stats, score);
        if first_rewrite_id.is_none() {
            first_rewrite_id = Some(local);
        }
    }
    let rewrite_vertex_id = first_rewrite_id.expect("rewrite fixture id");
    for score in [20_i64, 40] {
        seed_person_record(&env, env.graph_dest, stats, score);
    } // The absence shapes plus a non-Person decoy carrying a real leaf value:
    // none of them may ever produce a posting.
    // 1. missing root record entirely:
    e2e_insert_vertex_with_label(&env, env.graph_source, person);
    // 2. non-record root node:
    e2e_insert_vertex_with_label_and_property(&env, env.graph_source, person, stats, 5);
    // 3. container (list) leaf under the declared path:
    e2e_insert_vertex_with_label_and_record(
        &env,
        env.graph_source,
        person,
        stats,
        vec![("score".to_owned(), E2eRecordFieldValue::IntList(vec![1, 2]))],
    );
    // Missing leaf inside a present record:
    e2e_insert_vertex_with_label_and_record(&env, env.graph_dest, person, stats, vec![]);
    // Non-Person decoy with a real leaf value must stay excluded by label scoping.
    e2e_insert_vertex_with_label_and_record(
        &env,
        env.graph_source,
        other,
        stats,
        vec![("score".to_owned(), E2eRecordFieldValue::Int(77))],
    );

    let args = migration_args(NESTED_MIGRATION_ID, NESTED_DDL);
    let mut phases: Vec<IndexMaintenancePhase> = Vec::new();
    let mut rounds = 0_usize;
    loop {
        rounds += 1;
        assert!(
            rounds <= MAX_DRIVE_ROUNDS,
            "nested migration did not converge"
        );
        let result = apply_once(&env, &args);
        if let Some(phase) = nested_phase_for(&vertex_memberships(&env), score_leaf)
            && phases.last() != Some(&phase)
        {
            phases.push(phase);
        }
        match result.status {
            SchemaMigrationApplyStatus::Progress(_) => {}
            SchemaMigrationApplyStatus::Applied => break,
            SchemaMigrationApplyStatus::Replay => {
                panic!("fresh migration cannot replay before reaching Applied")
            }
            other => panic!("nested migration terminated early: {other:?}"),
        }
    }
    assert!(
        phases.contains(&IndexMaintenancePhase::Building),
        "lifecycle must pass Building, observed {phases:?}"
    );
    assert!(
        phases.contains(&IndexMaintenancePhase::Sealing),
        "lifecycle must pass Sealing, observed {phases:?}"
    );
    assert_eq!(phases.last(), Some(&IndexMaintenancePhase::Active));

    // Posting truth: every seeded scalar leaf converged exactly once on its own shard,
    // with equality multiplicity preserved and no absence-shape or decoy postings.
    let physical = projected_nested_physical_index(&vertex_memberships(&env), score_leaf);
    for (value, expected_shard, expected_count) in [
        (10_i64, SOURCE_SHARD, 1_usize),
        (20, DEST_SHARD, 1),
        (30, SOURCE_SHARD, 2),
        (40, DEST_SHARD, 1),
    ] {
        let hits = vertex_equal_postings(&env, physical, score_leaf, value);
        assert_eq!(
            hits.len(),
            expected_count,
            "nested equality postings for {value}"
        );
        assert!(
            hits.iter().all(|hit| hit.shard_id == expected_shard),
            "nested postings for {value} must live on the seeding shard"
        );
    }
    for absent in [5_i64, 77] {
        assert!(
            vertex_equal_postings(&env, physical, score_leaf, absent).is_empty(),
            "absence shape or decoy value {absent} must not post"
        );
    }
    assert_eq!(
        vertex_range_postings(&env, physical, score_leaf, 0, 1000).len(),
        5,
        "the full nested domain converges with no extra or missing postings"
    );

    // GQL row-level nested reads are ADR 0073 slice 4 (planner anchors), proven in
    // `router_gql_query.rs::federated_vertex_nested_leaf_index_match_equality_and_range`:
    // the Router extracts a dotted-leaf seed probe from the MATCH plan and each shard
    // revalidates seeded rows through the residual nested predicate. This scenario pins
    // slices 1-3 at the posting layer below.

    // Post-Active DML: rewriting one record swaps exactly its leaf posting through the
    // shared dotted-path resolver.
    e2e_set_vertex_record(
        &env,
        env.graph_source,
        rewrite_vertex_id,
        stats,
        vec![("score".to_owned(), E2eRecordFieldValue::Int(11))],
    );
    assert!(
        vertex_equal_postings(&env, physical, score_leaf, 10).is_empty(),
        "the old leaf posting must be removed"
    );
    assert_eq!(
        vertex_equal_postings(&env, physical, score_leaf, 11).len(),
        1,
        "the new leaf posting must be inserted"
    );
    assert_eq!(
        vertex_range_postings(&env, physical, score_leaf, 0, 1000).len(),
        5,
        "the rewrite leaves no stale or duplicate postings behind"
    );
}
