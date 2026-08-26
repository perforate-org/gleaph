//! PocketIC: ADR 0084 — EXPLAIN AUTHORIZATION end to end.
//!
//! One suite proves the privileged diagnosis statement against live Router state:
//!
//! 1. Owner-explains-collaborator renders FULL row identities (`grant to <principal>`);
//!    self mode renders source classes only and never names another principal.
//! 2. Self-explain redaction matrix across own-grant / PUBLIC / tenancy sources.
//! 3. An invisible touched graph returns the indistinguishable record-shaped `NotFound`
//!    (the existence-oracle probe fails), including for capability holders.//! 4. Invariant-1 regression guard: execution-path `Forbidden` responses are Candid-
//!    byte-identical to the constant captured before this feature landed.
//! 5. Alternatives render any-of with per-arm sources; unattributed residue renders
//!    "requires graph tenancy"; revoked (absent) rows stop appearing as sources.
//!    Expired-row absence is proven at unit level (`holds` already reads expiry and the
//!    statement grammar mints only non-expiring data-plane rows), so the E2E absence
//!    proof uses revocation — the same "row absent ⇒ never a source" wiring.
//! 6. Deny-by-default adversarial walks: non-tenant `BY`, caps-holder reach, mixed
//!    statement blocks, malformed `BY` principals, and write-entrypoint misuse.
//!
//! Expected counts stated before running (per plan contract):
//! - 6 `#[test]` functions.
//! - Each test constructs one PocketIC environment and installs exactly 3 canisters
//!   (Router, Index, Graph shard).
//! - Update/query budget per environment: 1 schema-bind mutate, ≤3 seed mutates with
//!   drains (tests 1–2 only), 0–9 GRANT/REVOKE mutates, ≤3 `prepare` batches,
//!   ≤1 `admin_grant_caps`, and ≤14 probe calls.
//! - Regression reruns required alongside: `adr0074_plan_enforcement`,
//!   `adr0075_conditional_policies`, `smoke`.

use candid::{Decode, Encode, Principal};
use gleaph_gql_ic::{GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, drain_maintenance_via_timer, gql_mutate_as_admin,
    install_single_shard_federation, install_single_shard_federation_with_graph_admins,
    prepare_batch_as_admin,
};

// ──── Principals ────

/// A grantee on the shared graph: the collaborator owners diagnose.
fn collaborator() -> Principal {
    Principal::from_slice(&[0xC1; 29])
}

/// Relies purely on PUBLIC rows; holds no personal grant.
fn public_relier() -> Principal {
    Principal::from_slice(&[0xC2; 29])
}

/// No caps, no tenancy, no grants.
fn stranger() -> Principal {
    Principal::from_slice(&[0xC3; 29])
}

/// Holds `MANAGE_AUTHORIZATION` and nothing else relevant: caps confer no explain
/// authority (ADR 0084 §1).
fn caps_holder() -> Principal {
    Principal::from_slice(&[0xC4; 29])
}

/// A graph admin (tenant) that is not the registry owner.
fn tenant_admin() -> Principal {
    Principal::from_slice(&[0xC9; 29])
}

// ──── Fixture ────

const FEED_QUERY: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name";
const ALT_QUERY: &str = "MATCH (a:Person)-[:KNOWS|LIKES]->(b:Person) RETURN b.name AS name";
const WILD_QUERY: &str = "MATCH (n) RETURN n";

/// Typed schema: Person{name}, DIRECTED KNOWS, UNDIRECTED LIKES (alternation fixture).
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE gt {{ NODE Person {{ name STRING }}, \
         DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person), \
         UNDIRECTED EDGE LIKES LABEL LIKES CONNECTING (Person ~ Person) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED gt"
    );
    gql_mutate_as_admin(env, &ddl, "adr0084-bind-schema");
}

fn seed_two_persons_and_one_edge(env: &FederationEnv) {
    gql_mutate_as_admin(
        env,
        "INSERT (:Person {name: 'alice'})",
        "adr0084-seed-alice",
    );
    drain_maintenance_via_timer(env, env.graph_source);
    gql_mutate_as_admin(env, "INSERT (:Person {name: 'bob'})", "adr0084-seed-bob");
    drain_maintenance_via_timer(env, env.graph_source);
    gql_mutate_as_admin(
        env,
        "MATCH (a:Person {name: 'alice'}) RETURN a NEXT \
         MATCH (b:Person {name: 'bob'}) \
         INSERT (a)-[:KNOWS]->(b)",
        "adr0084-seed-knows",
    );
    drain_maintenance_via_timer(env, env.graph_source);
}

/// Grant the collaborator exactly the rows the feed query demands.
fn grant_feed_rows_to_collaborator(env: &FederationEnv) {
    let g = GRAPH_NAME;
    for statement in [
        format!(
            "GRANT MATCH ON GRAPH {g} NODES Person TO PRINCIPAL '{}'",
            collaborator().to_text()
        ),
        format!(
            "GRANT READ ON GRAPH {g} NODES Person {{ name }} TO PRINCIPAL '{}'",
            collaborator().to_text()
        ),
        format!(
            "GRANT TRAVERSE OUTGOING ON GRAPH {g} EDGES KNOWS TO PRINCIPAL '{}'",
            collaborator().to_text()
        ),
    ] {
        gql_mutate_as_admin(env, &statement, "adr0084-grant-collaborator");
    }
}

fn register_feed_prepared(env: &FederationEnv) {
    prepare_batch_as_admin(
        env,
        &[gleaph_prepared_api::PreparedRegistration {
            name: "feed".into(),
            query: FEED_QUERY.into(),
            metadata: None,
        }],
    )
    .expect("register feed");
}

// ──── Probes ────

/// Run `EXPLAIN AUTHORIZATION …` through the read entrypoint and decode the rendered
/// explanation lines.
fn explain(
    env: &FederationEnv,
    caller: Principal,
    statement: &str,
) -> Result<Vec<String>, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(
                &statement.to_string(),
                &Vec::<u8>::new(),
                &ReadMode::Eventual
            )
            .expect("encode gql_query"),
        )
        .expect("gql_query call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(result)) => Ok(explanation_lines(&result)),
        Ok(Err(err)) => Err(err),
        Err(err) => panic!("decode gql_query: {err}"),
    }
}

fn explanation_lines(result: &GqlQueryResult) -> Vec<String> {
    let blob = result.rows_blob.as_ref().expect("explanation rows present");
    let wire = GqlWireRows::decode_blob(blob).expect("decode explanation rows");
    assert_eq!(
        wire.rows.len() as u64,
        result.row_count,
        "row count matches blob"
    );
    wire.rows
        .iter()
        .map(|row| match &row.columns[0].1 {
            GqlWireValue::Text(text) => text.clone(),
            other => panic!("expected text explanation cell, got {other:?}"),
        })
        .collect()
}

/// Raw `gql_query` returning the raw response bytes plus the decoded typed error, so the
/// invariant-1 guard can compare wire bytes exactly.
fn raw_execution_error_bytes(
    env: &FederationEnv,
    caller: Principal,
    query: &str,
) -> (Vec<u8>, RouterError) {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(&query.to_string(), &Vec::<u8>::new(), &ReadMode::Eventual)
                .expect("encode gql_query"),
        )
        .expect("gql_query call");
    let decoded: Result<GqlQueryResult, RouterError> =
        Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query");
    match decoded {
        Err(err) => (bytes, err),
        Ok(_) => panic!("execution unexpectedly succeeded for {caller}: {query}"),
    }
}

/// Grant `MANAGE_AUTHORIZATION` (bit 6) and nothing else to [`caps_holder`].
fn grant_manage_authorization_cap(env: &FederationEnv) {
    use gleaph_router::types::GrantCapsArgs;
    let args = GrantCapsArgs {
        target: caps_holder(),
        // gleaph_auth::AdminCaps::MANAGE_AUTHORIZATION = 1 << 6.
        caps: 1 << 6,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_grant_caps",
            Encode!(&args).expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode admin_grant_caps")
        .expect("grant cap");
}

// ──── Tests ────

#[test]
fn owner_explains_collaborator_with_full_row_identities() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_two_persons_and_one_edge(&env);
    grant_feed_rows_to_collaborator(&env);
    register_feed_prepared(&env);

    // Owner mode over the collaborator: full identities, never "your grant".
    let owner_statement = format!(
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed BY PRINCIPAL '{}'",
        collaborator().to_text()
    );
    let lines = explain(&env, env.admin, &owner_statement).expect("owner explains collaborator");
    assert_eq!(lines[0], "COVERED", "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("grant to {}", collaborator().to_text()))),
        "full identity expected: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("your grant")),
        "owner mode renders identities, not classes: {lines:?}"
    );

    // The same join asked by the collaborator about themselves redacts to classes.
    let self_lines = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed",
    )
    .expect("collaborator self-explains");
    assert_eq!(self_lines[0], "COVERED");
    assert!(self_lines.iter().any(|l| l.contains("— your grant")));
    for line in &self_lines {
        assert!(
            !line.contains(&env.admin.to_text()) && !line.contains("grant to"),
            "self mode must not name principals: {line}"
        );
    }
}

#[test]
fn self_explain_redaction_matrix_own_public_and_tenancy_sources() {
    let env =
        install_single_shard_federation_with_graph_admins([tenant_admin()].into_iter().collect());
    bind_typed_schema(&env);
    grant_feed_rows_to_collaborator(&env);
    // PUBLIC receives the same rows, so a stranger-with-PUBLIC sees PUBLIC sources.
    let g = GRAPH_NAME;
    for statement in [
        format!("GRANT MATCH ON GRAPH {g} NODES Person TO PUBLIC"),
        format!("GRANT READ ON GRAPH {g} NODES Person {{ name }} TO PUBLIC"),
        format!("GRANT TRAVERSE OUTGOING ON GRAPH {g} EDGES KNOWS TO PUBLIC"),
    ] {
        gql_mutate_as_admin(&env, &statement, "adr0084-grant-public");
    }
    register_feed_prepared(&env);

    // Own-grant class.
    let own = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed",
    )
    .expect("own-grant source");
    assert_eq!(own[0], "COVERED");
    assert!(own.iter().any(|l| l.contains("— your grant")), "{own:?}");

    // PUBLIC class (public_relier holds no personal rows).
    let public = explain(
        &env,
        public_relier(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed",
    )
    .expect("PUBLIC source");
    assert_eq!(public[0], "COVERED");
    assert!(
        public.iter().any(|l| l.contains("— PUBLIC grant")),
        "{public:?}"
    );
    assert!(
        !public.iter().any(|l| l.contains("— your grant")),
        "PUBLIC-only coverage cannot show an own row"
    );

    // Tenancy class: the graph admin holds zero grant rows yet everything is covered.
    let tenancy = explain(
        &env,
        tenant_admin(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed",
    )
    .expect("tenancy source");
    assert_eq!(tenancy[0], "COVERED");
    assert!(
        tenancy.iter().any(|l| l.contains("— graph tenancy")),
        "{tenancy:?}"
    );

    // Redaction closure: no self-mode output names any principal involved here.
    for report in [own, public, tenancy] {
        for line in &report {
            for principal in [collaborator(), public_relier(), tenant_admin(), env.admin] {
                assert!(
                    !line.contains(&principal.to_text()),
                    "self-mode leak of {}: {line}",
                    principal.to_text()
                );
            }
        }
    }
}

#[test]
fn invisible_graph_probe_is_an_indistinguishable_not_found() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    register_feed_prepared(&env);

    // The record exists, but the stranger holds no visibility arm on its bound graph:
    // the ask must fail exactly like an unknown name (existence-oracle dead end).
    let stranger_ask =
        explain(&env, stranger(), "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed")
            .expect_err("invisible touched graph must fail");
    let absent_ask = explain(
        &env,
        stranger(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY definitely_absent",
    )
    .expect_err("unknown record must fail");

    // Both are the uniform NotFound carrying exactly the asked name — "present but
    // invisible" and "absent" have identical shapes.
    assert!(
        matches!(stranger_ask, RouterError::NotFound(_)),
        "{stranger_ask:?}"
    );
    assert!(
        matches!(absent_ask, RouterError::NotFound(_)),
        "{absent_ask:?}"
    );
    assert_eq!(
        format!("{stranger_ask:?}"),
        r#"NotFound("prepared query \"feed\"")"#
    );
    assert_eq!(
        format!("{absent_ask:?}"),
        r#"NotFound("prepared query \"definitely_absent\"")"#
    );

    // Capability holders get the identical answer: caps confer no explain authority.
    grant_manage_authorization_cap(&env);
    let caps_ask = explain(
        &env,
        caps_holder(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed",
    )
    .expect_err("caps holder must not see further");
    assert_eq!(format!("{caps_ask:?}"), format!("{stranger_ask:?}"));

    // Sanity anchor: a grantee whose visibility passes explains the same record fine.
    let g = GRAPH_NAME;
    gql_mutate_as_admin(
        &env,
        format!("GRANT MATCH ON GRAPH {g} NODES Person TO PRINCIPAL '{}'", collaborator().to_text())
            .as_str(),
        "adr0084-oracle-control-grant",
    );
    let visible = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed",
    );
    assert!(
        visible.is_ok(),
        "control probe should pass the gate: {:?}",
        visible.err()
    );
}

// Candid wire bytes of `gql_query` answering `Err(Forbidden)` for this probe, captured
// from the execution path before this feature landed. The table section carries the
// RouterError/GqlQueryResult type descriptors; any change to the enforcement route's
// response shape shifts these bytes and fails the guard.
const EXPECTED_FORBIDDEN_BYTES: [u8; 386] = [
    68, 73, 68, 76, 19, 107, 2, 188, 138, 1, 1, 197, 254, 210, 1, 12, 108, 5, 131, 147, 61, 2, 249,
    133, 174, 161, 1, 4, 190, 182, 179, 220, 4, 9, 170, 251, 147, 185, 5, 120, 187, 208, 164, 143,
    12, 10, 110, 3, 109, 123, 110, 5, 108, 2, 241, 140, 161, 63, 120, 213, 187, 173, 233, 1, 6,
    109, 7, 108, 2, 244, 169, 189, 144, 4, 8, 220, 215, 189, 168, 4, 121, 110, 120, 110, 126, 110,
    11, 107, 6, 232, 238, 154, 107, 127, 221, 243, 204, 228, 1, 127, 200, 159, 175, 249, 4, 127,
    230, 133, 239, 242, 5, 127, 235, 130, 174, 136, 15, 127, 227, 213, 162, 209, 15, 127, 107, 25,
    221, 198, 160, 17, 113, 215, 153, 216, 201, 1, 127, 217, 130, 229, 223, 2, 13, 225, 156, 165,
    243, 3, 14, 249, 242, 173, 175, 5, 15, 207, 134, 165, 247, 5, 113, 207, 160, 222, 242, 6, 113,
    133, 195, 222, 128, 7, 113, 144, 210, 225, 151, 7, 113, 194, 151, 180, 171, 7, 127, 238, 130,
    193, 234, 7, 127, 211, 216, 164, 240, 7, 113, 171, 157, 204, 208, 8, 16, 185, 210, 135, 216, 9,
    113, 244, 184, 149, 164, 11, 113, 207, 243, 128, 248, 12, 17, 143, 157, 191, 139, 13, 113, 142,
    242, 184, 246, 13, 113, 217, 209, 139, 175, 14, 127, 156, 185, 207, 238, 14, 127, 227, 149,
    165, 128, 15, 18, 186, 239, 204, 139, 15, 113, 167, 238, 166, 181, 15, 113, 189, 192, 153, 212,
    15, 113, 242, 140, 217, 238, 15, 113, 108, 1, 167, 136, 130, 130, 10, 113, 108, 2, 160, 189,
    206, 131, 3, 121, 238, 214, 157, 230, 14, 121, 107, 3, 184, 199, 162, 211, 2, 127, 133, 212,
    173, 164, 10, 127, 140, 208, 233, 210, 11, 127, 108, 4, 160, 236, 131, 36, 113, 158, 189, 149,
    194, 1, 113, 175, 168, 196, 250, 2, 113, 213, 207, 179, 153, 15, 113, 108, 2, 135, 185, 236,
    250, 5, 113, 169, 133, 167, 189, 9, 113, 108, 4, 220, 215, 189, 168, 4, 121, 223, 162, 138,
    147, 11, 120, 185, 184, 142, 223, 12, 120, 164, 223, 250, 128, 14, 113, 1, 0, 1, 18,
];

#[test]
fn execution_forbidden_bytes_are_identical_before_and_after() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    register_feed_prepared(&env);

    // Ad-hoc execution with PARTIAL coverage (visible via grant rows, traversal
    // uncovered): uniform Forbidden at plan time.
    let g = GRAPH_NAME;
    let c = collaborator().to_text();
    gql_mutate_as_admin(
        &env,
        format!("GRANT MATCH ON GRAPH {g} NODES Person TO PRINCIPAL '{c}'").as_str(),
        "adr0084-guard-match",
    );
    let (adhoc_bytes, adhoc_err) = raw_execution_error_bytes(&env, collaborator(), FEED_QUERY);
    assert!(matches!(adhoc_err, RouterError::Forbidden), "{adhoc_err:?}");
    assert_eq!(
        adhoc_bytes,
        EXPECTED_FORBIDDEN_BYTES.to_vec(),
        "invariant 1: ad-hoc Forbidden bytes changed"
    );

    // Prepared execution without EXECUTE publication: the same uniform Forbidden on the
    // same transport (record resolution never needs graph visibility).
    let bytes = env
        .pic
        .query_call(
            env.router,
            collaborator(),
            "prepared_query",
            Encode!(
                &"feed".to_string(),
                &Vec::<u8>::new(),
                &Option::<Vec<gleaph_prepared_api::PreparedSortSpec>>::None,
                &ReadMode::Eventual
            )
            .expect("encode prepared_query"),
        )
        .expect("prepared_query call");
    let decoded: Result<GqlQueryResult, RouterError> =
        Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode prepared_query");
    match decoded {
        Err(err) => assert!(matches!(err, RouterError::Forbidden), "{err:?}"),
        Ok(_) => panic!("prepared execution unexpectedly succeeded"),
    }

    // The diagnostic itself answers normally right next to unchanged enforcement:
    // same asker, same query — a rendered report instead of a bare verdict.
    let lines = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed",
    )
    .expect("collaborator explains own partial coverage");
    assert_eq!(lines[0], "NOT COVERED", "{lines:?}");
}

#[test]
fn alternatives_unattributed_and_revoked_rows_render_per_adr() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    prepare_batch_as_admin(
        &env,
        &[
            gleaph_prepared_api::PreparedRegistration {
                name: "altq".into(),
                query: ALT_QUERY.into(),
                metadata: None,
            },
            gleaph_prepared_api::PreparedRegistration {
                name: "wildq".into(),
                query: WILD_QUERY.into(),
                metadata: None,
            },
        ],
    )
    .expect("register alt/wild");

    // Alternatives: only ONE arm granted — the any-of group renders per-arm sources and
    // the demand stays satisfied.
    let g = GRAPH_NAME;
    let c = collaborator().to_text();
    gql_mutate_as_admin(
        &env,
        format!("GRANT MATCH ON GRAPH {g} NODES Person TO PRINCIPAL '{c}'").as_str(),
        "adr0084-alt-match",
    );
    gql_mutate_as_admin(
        &env,
        format!("GRANT READ ON GRAPH {g} NODES Person {{ name }} TO PRINCIPAL '{c}'").as_str(),
        "adr0084-alt-read",
    );
    gql_mutate_as_admin(
        &env,
        format!("GRANT TRAVERSE OUTGOING ON GRAPH {g} EDGES KNOWS TO PRINCIPAL '{c}'").as_str(),
        "adr0084-alt-knows",
    );
    let alt = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY altq",
    )
    .expect("altq explains");
    assert_eq!(alt[0], "COVERED", "{alt:?}");
    assert!(alt.iter().any(|l| l == "  ANY OF:"), "{alt:?}");
    assert!(
        alt.iter()
            .any(|l| l.contains("TRAVERSE OUTGOING ON EDGES KNOWS") && l.contains("— your grant")),
        "{alt:?}"
    );
    assert!(
        alt.iter()
            .any(|l| l.contains("TRAVERSE ON EDGES LIKES") && l.contains("UNCOVERED")),
        "{alt:?}"
    );

    // Unattributed residue: a wildcard scan demands tenancy; the non-tenant sees NOT
    // COVERED, the owner sees the same line but covered via the ownership root.
    let wild_non_tenant = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY wildq",
    )
    .expect("wildq explains for non-tenant");
    assert_eq!(wild_non_tenant[0], "NOT COVERED");
    assert!(
        wild_non_tenant
            .iter()
            .any(|l| l.contains("UNATTRIBUTED READ — requires graph tenancy (owner/admin)")),
        "{wild_non_tenant:?}"
    );
    let wild_owner = explain(
        &env,
        env.admin,
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY wildq",
    )
    .expect("wildq explains for owner");
    assert_eq!(wild_owner[0], "COVERED");
    assert!(
        wild_owner
            .iter()
            .any(|l| l.contains("requires graph tenancy (owner/admin)")),
        "{wild_owner:?}"
    );

    // Row absence: after REVOKE the revoked row stops being a source and the verdict
    // flips (expired rows follow the same wiring through the expiry-aware `holds`
    // primitive, proven at unit level).
    gql_mutate_as_admin(
        &env,
        format!("REVOKE TRAVERSE OUTGOING ON GRAPH {g} EDGES KNOWS FROM PRINCIPAL '{c}'").as_str(),
        "adr0084-alt-revoke",
    );
    let after_revoke = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY altq",
    )
    .expect("post-revoke explain");
    assert_eq!(after_revoke[0], "NOT COVERED", "{after_revoke:?}");
    assert!(
        after_revoke
            .iter()
            .any(|l| l.contains("TRAVERSE OUTGOING ON EDGES KNOWS") && l.contains("UNCOVERED")),
        "{after_revoke:?}"
    );
}

#[test]
fn deny_by_default_adversarial_walks() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    grant_feed_rows_to_collaborator(&env);
    register_feed_prepared(&env);

    // Non-tenant BY ask: owner mode requires tenancy of every touched graph.
    let by_ask = explain(
        &env,
        stranger(),
        &format!(
            "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed BY PRINCIPAL '{}'",
            collaborator().to_text()
        ),
    );
    assert!(
        matches!(by_ask, Err(RouterError::NotFound(_))),
        "{by_ask:?}"
    );

    // Caps holder in owner mode: identical refusal.
    grant_manage_authorization_cap(&env);
    let caps_by_ask = explain(
        &env,
        caps_holder(),
        &format!(
            "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed BY PRINCIPAL '{}'",
            collaborator().to_text()
        ),
    );
    assert!(matches!(caps_by_ask, Err(RouterError::NotFound(_))));

    // Mixed blocks are rejected outright (never planned, never partially explained).
    let mixed = explain(
        &env,
        collaborator(),
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed NEXT MATCH (n) RETURN n",
    );
    assert!(
        matches!(mixed, Err(RouterError::InvalidArgument(_))),
        "{mixed:?}"
    );

    // Malformed BY principal text is a distinct InvalidArgument.
    let bad_principal = explain(
        &env,
        env.admin,
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed BY PRINCIPAL 'not-a-principal'",
    );
    assert!(matches!(
        bad_principal,
        Err(RouterError::InvalidArgument(_))
    ));

    // Write-entrypoint misuse: the pure read refuses the mutate path without force.
    let bytes = env
        .pic
        .update_call(
            env.router,
            collaborator(),
            "gql_mutate",
            Encode!(
                &"EXPLAIN AUTHORIZATION FOR PREPARED QUERY feed".to_string(),
                &Vec::<u8>::new(),
                &"adr0084-mutate-misuse".to_string()
            )
            .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    let verdict: Result<GqlQueryResult, RouterError> =
        Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_mutate");
    assert!(
        matches!(verdict, Err(RouterError::ExecutionPathMismatch { .. })),
        "{verdict:?}"
    );
}
