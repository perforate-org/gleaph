//! PocketIC contracts for the ADR 0057 durable Router bulk-load boundary.

use std::time::Duration;

use candid::{Decode, Encode};
use gleaph_gql::value::Value;
use gleaph_gql_ic::GqlWireRows;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_HOME_NAME, GRAPH_NAME, GRAPH_REMOTE_NAME, arm_router_fault,
    bulk_load_as_admin, bulk_load_as_admin_expect_trap, bulk_load_gc_probe_as_admin,
    bulk_load_gc_step_as_admin, bulk_load_start_probe_as_admin, bulk_load_status_as_admin,
    ensure_property, ensure_vertex_label, gql_query_as_admin, index_vertex_property,
    install_single_shard_federation, install_two_graph_federation,
    seed_bulk_load_gc_fixture_as_admin, start_graph_shard, stop_graph_shard, sweep_mutation_keys,
    wasm_bytes,
};
use gleaph_router::types::{
    AtomicInsertPropertyV1, AtomicInsertVertexV1, BulkLoadChunkV1, BulkLoadCommand,
    BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadUpdateV1,
};

fn vertices(labels: &[&str], count: usize) -> BulkLoadChunkV1 {
    BulkLoadChunkV1::Vertices(
        (0..count)
            .map(|_| AtomicInsertVertexV1 {
                vertex_labels: labels.iter().map(|label| (*label).to_owned()).collect(),
                initial_properties: Vec::new(),
            })
            .collect(),
    )
}

fn start(graph: &str, key: &str) -> BulkLoadCommand {
    BulkLoadCommand::Start {
        graph_name: Some(graph.to_owned()),
        client_bulk_key: key.to_owned(),
    }
}

fn append(graph: &str, key: &str, chunk_index: u32, chunk: BulkLoadChunkV1) -> BulkLoadCommand {
    BulkLoadCommand::Append {
        graph_name: Some(graph.to_owned()),
        client_bulk_key: key.to_owned(),
        chunk_index,
        chunk,
    }
}

fn finalize(graph: &str, key: &str) -> BulkLoadCommand {
    BulkLoadCommand::Finalize {
        graph_name: Some(graph.to_owned()),
        client_bulk_key: key.to_owned(),
    }
}

fn abort(graph: &str, key: &str) -> BulkLoadCommand {
    BulkLoadCommand::Abort {
        graph_name: Some(graph.to_owned()),
        client_bulk_key: key.to_owned(),
    }
}

fn submit_bulk_load(
    env: &FederationEnv,
    command: BulkLoadCommand,
) -> pocket_ic::common::rest::RawMessageId {
    env.pic
        .submit_call(
            env.router,
            env.admin,
            "bulk_load",
            Encode!(&command).expect("encode submitted bulk_load"),
        )
        .unwrap_or_else(|error| panic!("submit bulk_load: {error:?}"))
}

fn await_bulk_load(
    env: &FederationEnv,
    message_id: pocket_ic::common::rest::RawMessageId,
) -> Result<BulkLoadResponse, RouterError> {
    let bytes = env
        .pic
        .await_call(message_id)
        .unwrap_or_else(|error| panic!("await submitted bulk_load: {error:?}"));
    Decode!(&bytes, Result<BulkLoadResponse, RouterError>).expect("decode submitted bulk_load")
}

fn assert_state(
    status: &gleaph_router::types::BulkLoadStatusPage,
    expected: BulkLoadPublicStateV1,
) {
    assert_eq!(status.state, expected);
}

#[test]
fn bulk_load_lifecycle_replay_preserves_prefix_and_pages_receipts() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let key = "adr0057-lifecycle-replay";
    let chunk0 = vertices(&["Person"], 2);
    let chunk1 = vertices(&["Person"], 1);

    let invalid_key = "";
    let before_invalid = bulk_load_start_probe_as_admin(&env, GRAPH_NAME, invalid_key);
    assert!(matches!(
        bulk_load_as_admin(&env, start(GRAPH_NAME, invalid_key)),
        Err(RouterError::InvalidArgument(_))
    ));
    assert_eq!(
        bulk_load_start_probe_as_admin(&env, GRAPH_NAME, invalid_key),
        before_invalid,
        "typed Start validation must fail before counter or client binding writes"
    );

    let counter_key = "adr0057-start-counter-rollback";
    let parent_key = "adr0057-start-parent-rollback";
    let baseline = bulk_load_start_probe_as_admin(&env, GRAPH_NAME, counter_key).0;

    arm_router_fault(&env, 6);
    bulk_load_as_admin_expect_trap(&env, start(GRAPH_NAME, counter_key));
    arm_router_fault(&env, 0);
    assert_eq!(
        bulk_load_start_probe_as_admin(&env, GRAPH_NAME, counter_key),
        (baseline, None, false),
        "a counter-boundary trap must roll back the counter and client binding"
    );
    assert_eq!(
        bulk_load_as_admin(&env, start(GRAPH_NAME, counter_key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 0
        })
    );
    assert_eq!(
        bulk_load_start_probe_as_admin(&env, GRAPH_NAME, counter_key),
        (baseline + 1, Some(baseline + 1), true)
    );

    arm_router_fault(&env, 7);
    bulk_load_as_admin_expect_trap(&env, start(GRAPH_NAME, parent_key));
    arm_router_fault(&env, 0);
    assert_eq!(
        bulk_load_start_probe_as_admin(&env, GRAPH_NAME, parent_key),
        (baseline + 1, None, false),
        "a parent-boundary trap must roll back both durable writes"
    );
    assert_eq!(
        bulk_load_as_admin(&env, start(GRAPH_NAME, parent_key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 0
        })
    );
    assert_eq!(
        bulk_load_as_admin(&env, start(GRAPH_NAME, parent_key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 0
        })
    );
    assert_eq!(
        bulk_load_start_probe_as_admin(&env, GRAPH_NAME, parent_key),
        (baseline + 2, Some(baseline + 2), true),
        "exact Start replay must not allocate again"
    );

    assert_eq!(
        bulk_load_as_admin(&env, start(GRAPH_NAME, key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 0
        })
    );
    let first = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk0.clone()))
        .expect("first chunk must commit");
    let BulkLoadResponse::Appended {
        chunk_index: first_index,
        next_offset: first_offset,
        receipt: first_receipt,
    } = first
    else {
        panic!("first append must return a receipt");
    };
    assert_eq!(first_index, 0);
    assert_eq!(first_offset, 2);
    assert_eq!(first_receipt.logical_vertex_count, 2);
    assert_eq!(first_receipt.allocated_vertex_ids.len(), 2);

    let replay = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk0))
        .expect("same chunk must replay");
    assert_eq!(
        replay,
        BulkLoadResponse::Appended {
            chunk_index: 0,
            next_offset: first_offset,
            receipt: first_receipt.clone(),
        }
    );

    let conflict = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk1.clone()));
    assert!(matches!(
        conflict,
        Err(RouterError::Conflict(message)) if message.contains("fingerprint")
    ));

    let second = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 1, chunk1))
        .expect("second chunk must commit without rolling back the prefix");
    let BulkLoadResponse::Appended {
        chunk_index: second_index,
        next_offset: second_offset,
        receipt: second_receipt,
    } = second
    else {
        panic!("second append must return a receipt");
    };
    assert_eq!(second_index, 1);
    assert_eq!(second_offset, 1);
    assert_eq!(second_receipt.logical_vertex_count, 1);

    let first_page =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1).expect("first status page");
    assert_state(&first_page, BulkLoadPublicStateV1::Open);
    assert_eq!(first_page.next_chunk_index, 2);
    assert_eq!(first_page.committed_chunk_count, 2);
    assert_eq!(first_page.completed_chunk_count, 2);
    assert_eq!(first_page.receipts.len(), 1);
    assert_eq!(first_page.receipts[0].chunk_index, 0);
    assert_eq!(first_page.receipts[0].receipt, first_receipt);
    assert_eq!(first_page.next_receipt_cursor, Some(1));

    let second_page =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, first_page.next_receipt_cursor, 1)
            .expect("second status page");
    assert_eq!(second_page.receipts.len(), 1);
    assert_eq!(second_page.receipts[0].chunk_index, 1);
    assert_eq!(second_page.receipts[0].receipt, second_receipt);
    assert_eq!(second_page.next_receipt_cursor, None);

    let finalized = bulk_load_as_admin(&env, finalize(GRAPH_NAME, key))
        .expect("finalize must verify the completed prefix");
    assert_eq!(
        finalized,
        BulkLoadResponse::FinalizeAccepted {
            state: BulkLoadPublicStateV1::Completed,
        }
    );
    let completed =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 2).expect("completed status");
    assert_state(&completed, BulkLoadPublicStateV1::Completed);
    assert_eq!(completed.next_chunk_index, 2);

    // Exact replay remains available after terminal completion and never allocates another row.
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, vertices(&["Person"], 2)),),
        Ok(BulkLoadResponse::Appended {
            chunk_index: 0,
            next_offset: 2,
            receipt: completed.receipts[0].receipt.clone(),
        })
    );
}

#[test]
fn bulk_load_abort_drives_active_child_and_rejects_finalize_while_busy() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let key = "adr0057-abort-active-child";
    let chunk = vertices(&["Person"], 1);

    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start bulk job");
    stop_graph_shard(&env, env.graph_source);

    let append_message = submit_bulk_load(&env, append(GRAPH_NAME, key, 0, chunk.clone()));
    env.pic.tick();
    let pending = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1)
        .expect("status while append is pending");
    assert_state(&pending, BulkLoadPublicStateV1::AppendPending);
    assert_eq!(pending.committed_chunk_count, 0);
    assert!(
        env.pic.ingress_status(append_message.clone()).is_none(),
        "Append ingress must still be suspended at the Graph await"
    );

    let finalize_message = submit_bulk_load(&env, finalize(GRAPH_NAME, key));
    let finalize_error = await_bulk_load(&env, finalize_message)
        .expect_err("Finalize must not overtake the suspended Append ingress");
    assert!(matches!(
        finalize_error,
        RouterError::Busy { operation } if operation == "bulk_load.append"
    ));

    let abort_message = submit_bulk_load(&env, abort(GRAPH_NAME, key));
    let abort_error = await_bulk_load(&env, abort_message)
        .expect_err("Abort must retain the exact active child while Graph is stopped");
    assert!(matches!(abort_error, RouterError::Internal(_)));
    let append_error = await_bulk_load(&env, append_message)
        .expect_err("stopped Graph must reject the suspended Append callback");
    assert!(matches!(append_error, RouterError::Internal(_)));
    let abort_pending = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1)
        .expect("status while abort is pending");
    assert_state(&abort_pending, BulkLoadPublicStateV1::AbortPending);
    assert_eq!(abort_pending.committed_chunk_count, 0);

    env.pic
        .advance_time(Duration::from_secs(7 * 24 * 60 * 60 + 1));
    let _ = sweep_mutation_keys(&env, 100_000);
    let retained = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1)
        .expect("non-terminal child must survive retention sweep");
    assert_state(&retained, BulkLoadPublicStateV1::AbortPending);
    assert_eq!(retained.terminal_at_ns, None);

    // A delayed Append cannot replace or fork the active child during AbortPending.
    let conflicting_append = bulk_load_as_admin(
        &env,
        append(GRAPH_NAME, key, 0, vertices(&["Different"], 1)),
    )
    .expect_err("conflicting delayed Append must be fenced");
    assert!(matches!(
        conflicting_append,
        RouterError::Conflict(message) if message.contains("fingerprint")
    ));

    start_graph_shard(&env, env.graph_source);
    let aborted = bulk_load_as_admin(&env, abort(GRAPH_NAME, key))
        .expect("Abort retry must replay and retire the same child");
    assert_eq!(
        aborted,
        BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted,
        }
    );
    let terminal =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1).expect("aborted status");
    assert_state(&terminal, BulkLoadPublicStateV1::Aborted);
    assert_eq!(terminal.committed_chunk_count, 1);
    assert_eq!(terminal.completed_chunk_count, 1);
    assert!(terminal.terminal_at_ns.is_some());

    // The exact accepted chunk remains replayable, while a different payload cannot dispatch.
    let replay = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk))
        .expect("exact append replay after Abort");
    assert!(matches!(
        replay,
        BulkLoadResponse::Appended { chunk_index: 0, .. }
    ));
    assert_eq!(
        bulk_load_as_admin(&env, abort(GRAPH_NAME, key)),
        Ok(BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted,
        })
    );
}

#[test]
fn bulk_load_status_and_receipts_survive_router_graph_index_upgrade() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let key = "adr0057-upgrade-reopen";

    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start bulk job");
    bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, vertices(&["Person"], 1)))
        .expect("append bulk chunk");
    bulk_load_as_admin(&env, finalize(GRAPH_NAME, key)).expect("finalize bulk job");
    let before =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1).expect("status before upgrade");
    assert_state(&before, BulkLoadPublicStateV1::Completed);

    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty.clone(), None)
        .expect("upgrade Router");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty.clone(), None)
        .expect("upgrade Index");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade Graph");

    let after =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1).expect("status after upgrade");
    assert_eq!(after, before);
    assert_eq!(
        bulk_load_as_admin(&env, start(GRAPH_NAME, key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 1,
        })
    );

    // Public Start/Append/Finalize created the real placement and first Graph-backed receipt. The
    // feature-gated setup seam expands only that expensive repeated child range at its actual stable
    // owner, then pauses autonomous recovery so every production GC step is exactly observable.
    seed_bulk_load_gc_fixture_as_admin(&env, GRAPH_NAME, key);
    assert_eq!(
        bulk_load_gc_probe_as_admin(&env, GRAPH_NAME, key),
        (true, None, 65, Some("Completed".to_owned()))
    );
    let seeded = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 64)
        .expect("seeded completed status");
    assert_state(&seeded, BulkLoadPublicStateV1::Completed);
    assert_eq!(seeded.next_chunk_index, 65);
    assert_eq!(seeded.receipts.len(), 64);
    assert_eq!(seeded.next_receipt_cursor, Some(64));

    // Seven-day expiry is strict. Advance to the exact durable expiration timestamp rather than
    // assuming setup ingresses did not move PocketIC time by a few nanoseconds.
    let expires_at_ns = seeded.expires_at_ns.expect("terminal expiration anchor");
    let now_ns = env.pic.get_time().as_nanos_since_unix_epoch();
    env.pic
        .advance_time(Duration::from_nanos(expires_at_ns - now_ns));
    let at_boundary = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1)
        .expect("terminal job must remain available at the exact seven-day boundary");
    assert_state(&at_boundary, BulkLoadPublicStateV1::Completed);
    assert_eq!(at_boundary.next_chunk_index, 65);

    env.pic.advance_time(Duration::from_nanos(1));
    assert_eq!(
        bulk_load_gc_step_as_admin(&env, GRAPH_NAME, key),
        (32, 32, false)
    );
    assert_eq!(
        bulk_load_gc_probe_as_admin(&env, GRAPH_NAME, key),
        (true, Some(32), 33, Some("Completed".to_owned()))
    );
    let partial = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 64)
        .expect("terminal public outcome must survive partial receipt GC");
    assert_state(&partial, BulkLoadPublicStateV1::Completed);
    assert_eq!(partial.receipts.len(), 33);

    for fenced in [
        start(GRAPH_NAME, key),
        append(GRAPH_NAME, key, 0, vertices(&["Person"], 1)),
        finalize(GRAPH_NAME, key),
        abort(GRAPH_NAME, key),
    ] {
        assert!(matches!(
            bulk_load_as_admin(&env, fenced),
            Err(RouterError::Conflict(message)) if message.contains("expired")
        ));
    }

    // Reopen at the exact 32-row cursor boundary. The heap pause resets, but the explicit GC-step
    // endpoint re-pauses before the post-upgrade timer is due.
    env.pic
        .upgrade_canister(
            env.router,
            wasm_bytes("ROUTER_WASM"),
            Encode!(&()).expect("encode partial-GC Router upgrade arg"),
            None,
        )
        .expect("upgrade Router during partial bulk GC");
    assert_eq!(
        bulk_load_gc_probe_as_admin(&env, GRAPH_NAME, key),
        (true, Some(32), 33, Some("Completed".to_owned()))
    );
    let reopened = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 64)
        .expect("partial GC terminal status after reopen");
    assert_eq!(reopened, partial);

    assert_eq!(
        bulk_load_gc_step_as_admin(&env, GRAPH_NAME, key),
        (32, 32, false)
    );
    assert_eq!(
        bulk_load_gc_probe_as_admin(&env, GRAPH_NAME, key),
        (true, Some(64), 1, Some("Completed".to_owned()))
    );
    assert_eq!(
        bulk_load_gc_step_as_admin(&env, GRAPH_NAME, key),
        (1, 1, false)
    );
    assert_eq!(
        bulk_load_gc_probe_as_admin(&env, GRAPH_NAME, key),
        (true, Some(65), 0, Some("Completed".to_owned()))
    );
    let empty_terminal = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 64)
        .expect("parent remains terminal until empty-range proof");
    assert_state(&empty_terminal, BulkLoadPublicStateV1::Completed);
    assert!(empty_terminal.receipts.is_empty());

    assert_eq!(
        bulk_load_gc_step_as_admin(&env, GRAPH_NAME, key),
        (0, 0, true)
    );
    assert_eq!(
        bulk_load_gc_probe_as_admin(&env, GRAPH_NAME, key),
        (false, None, 0, None)
    );
    assert!(matches!(
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 1),
        Err(RouterError::NotFound(found)) if found == key
    ));
}

#[test]
fn bulk_load_same_textual_key_is_independent_per_graph() {
    let env = install_two_graph_federation();
    let key = "adr0057-same-key-two-graphs";
    let home_chunk = vertices(&[], 1);
    let remote_chunk = vertices(&[], 1);

    assert_eq!(
        bulk_load_as_admin(&env, start(GRAPH_HOME_NAME, key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 0,
        })
    );
    assert_eq!(
        bulk_load_as_admin(&env, start(GRAPH_REMOTE_NAME, key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 0,
        })
    );

    bulk_load_as_admin(&env, append(GRAPH_HOME_NAME, key, 0, home_chunk))
        .expect("home graph append");
    bulk_load_as_admin(&env, append(GRAPH_REMOTE_NAME, key, 0, remote_chunk))
        .expect("remote graph append");
    bulk_load_as_admin(&env, finalize(GRAPH_HOME_NAME, key)).expect("home finalize");
    bulk_load_as_admin(&env, finalize(GRAPH_REMOTE_NAME, key)).expect("remote finalize");

    let home = bulk_load_status_as_admin(&env, GRAPH_HOME_NAME, key, None, 1).expect("home status");
    let remote =
        bulk_load_status_as_admin(&env, GRAPH_REMOTE_NAME, key, None, 1).expect("remote status");
    assert_state(&home, BulkLoadPublicStateV1::Completed);
    assert_state(&remote, BulkLoadPublicStateV1::Completed);
    assert_eq!(home.committed_chunk_count, 1);
    assert_eq!(remote.committed_chunk_count, 1);
}

fn named_vertex(name: &str) -> AtomicInsertVertexV1 {
    AtomicInsertVertexV1 {
        vertex_labels: vec!["Person".to_owned()],
        initial_properties: vec![AtomicInsertPropertyV1 {
            property_name: "name".to_owned(),
            value: Value::Text(name.to_owned())
                .to_binary_bytes()
                .expect("encode name property"),
        }],
    }
}

fn update_row(name: &str, nick: &str) -> BulkLoadUpdateV1 {
    BulkLoadUpdateV1 {
        vertex_label: "Person".to_owned(),
        property_name: "name".to_owned(),
        match_value: Value::Text(name.to_owned())
            .to_binary_bytes()
            .expect("encode match value"),
        set_properties: vec![AtomicInsertPropertyV1 {
            property_name: "nick".to_owned(),
            value: Value::Text(nick.to_owned())
                .to_binary_bytes()
                .expect("encode set value"),
        }],
        remove_properties: Vec::new(),
    }
}

fn nick_of(env: &FederationEnv, name: &str) -> String {
    let result = gql_query_as_admin(
        env,
        &format!("MATCH (p:Person) WHERE p.name = '{name}' RETURN p.nick AS nick"),
    );
    assert_eq!(result.row_count, 1, "one row for name `{name}`");
    let wire =
        GqlWireRows::decode_blob(result.rows_blob.as_ref().expect("rows_blob for nick query"))
            .expect("decode rows_blob");
    let row = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row");
    match row.get("nick").expect("nick column") {
        Value::Text(nick) => nick.clone(),
        other => panic!("expected nick text, got {other:?}"),
    }
}

/// Read one vertex property as text, returning `None` when the property is absent (GQL
/// projects a missing property as NULL). Used to assert REMOVE cleared the value.
fn maybe_prop_of(env: &FederationEnv, name: &str, property: &str) -> Option<String> {
    let result = gql_query_as_admin(
        env,
        &format!("MATCH (p:Person) WHERE p.name = '{name}' RETURN p.`{property}` AS hit"),
    );
    assert_eq!(result.row_count, 1, "one row for name `{name}`");
    let wire =
        GqlWireRows::decode_blob(result.rows_blob.as_ref().expect("rows_blob for prop query"))
            .expect("decode rows_blob");
    let row = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row");
    match row.get("hit").expect("hit column") {
        Value::Text(text) => Some(text.clone()),
        Value::Null => None,
        other => panic!("expected text or NULL for {property}, got {other:?}"),
    }
}

fn remove_row(name: &str, set: Vec<(&str, &str)>, remove: Vec<&str>) -> BulkLoadUpdateV1 {
    BulkLoadUpdateV1 {
        vertex_label: "Person".to_owned(),
        property_name: "name".to_owned(),
        match_value: Value::Text(name.to_owned())
            .to_binary_bytes()
            .expect("encode match value"),
        set_properties: set
            .into_iter()
            .map(|(property, value)| AtomicInsertPropertyV1 {
                property_name: property.to_owned(),
                value: Value::Text(value.to_owned())
                    .to_binary_bytes()
                    .expect("encode set value"),
            })
            .collect(),
        remove_properties: remove.into_iter().map(str::to_owned).collect(),
    }
}

/// `gleaph load --mode update` runtime contract: update chunks resolve every match key through
/// the converged property index before any row executes, apply absolute SET assignments, and
/// record the committed row count in the durable receipt (resume skips by that count).
#[test]
fn bulk_load_update_applies_vertex_set_and_rejects_missing_match() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "name");
    ensure_property(&env, "nick");
    index_vertex_property(&env, "Person", "name");

    // Seed three indexed vertices through a plain insert job.
    let seed_key = "adr0057-update-seed";
    bulk_load_as_admin(&env, start(GRAPH_NAME, seed_key)).expect("start seed");
    bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            seed_key,
            0,
            BulkLoadChunkV1::Vertices(vec![
                named_vertex("alice"),
                named_vertex("bob"),
                named_vertex("cara"),
            ]),
        ),
    )
    .expect("append seed");
    bulk_load_as_admin(&env, finalize(GRAPH_NAME, seed_key)).expect("finalize seed");
    let seed = bulk_load_status_as_admin(&env, GRAPH_NAME, seed_key, None, 8).expect("seed status");
    assert_state(&seed, BulkLoadPublicStateV1::Completed);

    // DEBUG probe: read-WHERE vs mutate-WHERE to isolate the filter path.
    // The update chunk commits both rows with one absolute SET each.
    let key = "adr0057-update-lifecycle";
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start update");
    let updated = bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            key,
            0,
            BulkLoadChunkV1::Updates(vec![
                update_row("alice", "ally"),
                update_row("bob", "bobby"),
            ]),
        ),
    )
    .expect("append update");
    assert_eq!(
        updated,
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 2,
            updated_row_count: 2,
        },
        "update Append must report the committed row count"
    );
    assert_eq!(nick_of(&env, "alice"), "ally");
    assert_eq!(nick_of(&env, "bob"), "bobby");

    // A match key that resolves to no vertex rejects the whole chunk before any row executes.
    let missing = bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            key,
            1,
            BulkLoadChunkV1::Updates(vec![update_row("ghost", "spooky")]),
        ),
    )
    .expect_err("missing match value must reject the whole chunk");
    let RouterError::InvalidArgument(missing_message) = &missing else {
        panic!("missing match value must reject with InvalidArgument: {missing:?}");
    };
    assert!(
        missing_message.contains("does not resolve"),
        "{missing_message}"
    );

    bulk_load_as_admin(&env, finalize(GRAPH_NAME, key)).expect("finalize update");
    let status = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("update status");
    assert_state(&status, BulkLoadPublicStateV1::Completed);
    let updated_total: u64 = status
        .receipts
        .iter()
        .map(|row| row.updated_row_count)
        .sum();
    assert_eq!(
        updated_total, 2,
        "durable receipts must record the committed update rows for resume"
    );
}

/// REMOVE contract for `--mode update` rows: a row clears listed vertex properties through the
/// same `RemoveProperties` primitive as single-statement GQL `REMOVE`, so removing a
/// registered-but-absent value is a no-op success while a never-registered name rejects with
/// `NotFound` (plan-declared `ReadExisting` seed resolution, identical to single `REMOVE`);
/// SET+REMOVE mix on distinct properties applies atomically to the matched vertex;
/// re-execution converges (REMOVE is a no-op on the second pass, SET is absolute); and naming
/// one property in both clauses rejects the chunk at the wire boundary.
#[test]
fn bulk_load_update_remove_clears_properties_and_mixes_with_set() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "name");
    ensure_property(&env, "nick");
    ensure_property(&env, "temp");
    // `spare` stays registered but is never set on any vertex: removing it must no-op.
    ensure_property(&env, "spare");
    // NB: `never_existed` is deliberately never registered: removing it must reject with
    // `NotFound`, exactly like single-statement GQL `REMOVE`.
    index_vertex_property(&env, "Person", "name");

    let seed_key = "adr0057-update-remove-seed";
    bulk_load_as_admin(&env, start(GRAPH_NAME, seed_key)).expect("start seed");
    bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            seed_key,
            0,
            BulkLoadChunkV1::Vertices(vec![named_vertex("alice"), named_vertex("bob")]),
        ),
    )
    .expect("append seed");
    bulk_load_as_admin(&env, finalize(GRAPH_NAME, seed_key)).expect("finalize seed");

    // Give both vertices removable state through a plain SET update first.
    let key = "adr0057-update-remove-lifecycle";
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start update");
    bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            key,
            0,
            BulkLoadChunkV1::Updates(vec![
                remove_row("alice", vec![("nick", "ally"), ("temp", "t1")], vec![]),
                remove_row("bob", vec![("nick", "bobby"), ("temp", "t2")], vec![]),
            ]),
        ),
    )
    .expect("append set baseline");

    // Mixed row (SET nick + REMOVE present temp + REMOVE registered-but-absent spare) and
    // remove-only row.
    let chunk = BulkLoadChunkV1::Updates(vec![
        remove_row("alice", vec![("nick", "ally2")], vec!["temp", "spare"]),
        remove_row("bob", vec![], vec!["nick", "temp"]),
    ]);
    let updated =
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 1, chunk.clone())).expect("append remove");
    assert_eq!(
        updated,
        BulkLoadResponse::Updated {
            chunk_index: 1,
            next_offset: 2,
            updated_row_count: 2,
        },
        "remove rows count toward the committed row count like SET rows"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally2".to_owned()),
        "SET in a mixed row still applies"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "temp"),
        None,
        "REMOVE clears a present property"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "spare"),
        None,
        "REMOVE of a registered-but-absent value is a no-op success"
    );
    assert_eq!(
        maybe_prop_of(&env, "bob", "nick"),
        None,
        "remove-only row clears the property"
    );
    assert_eq!(
        maybe_prop_of(&env, "bob", "temp"),
        None,
        "remove-only row clears every listed property"
    );

    // Re-execution with the same fingerprint converges: REMOVE is a no-op on the second pass.
    let replayed =
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 1, chunk)).expect("replay same chunk");
    assert_eq!(
        replayed,
        BulkLoadResponse::Updated {
            chunk_index: 1,
            next_offset: 2,
            updated_row_count: 2,
        },
        "replayed chunk must converge to the same receipt"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally2".to_owned()),
        "replay must not disturb converged state"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "temp"),
        None,
        "replay keeps the removed property absent"
    );

    // A never-registered remove name rejects with `NotFound`, like single-statement REMOVE.
    let unknown = bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            key,
            2,
            BulkLoadChunkV1::Updates(vec![remove_row("alice", vec![], vec!["never_existed"])]),
        ),
    )
    .expect_err("never-registered remove name must reject");
    let RouterError::NotFound(unknown_message) = &unknown else {
        panic!("never-registered remove name must reject with NotFound: {unknown:?}");
    };
    assert!(
        unknown_message.contains("never_existed"),
        "{unknown_message}"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally2".to_owned()),
        "rejected chunk must leave state untouched"
    );

    // One property named in both clauses rejects the whole chunk at the wire boundary.
    let overlap = bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            key,
            3,
            BulkLoadChunkV1::Updates(vec![remove_row(
                "alice",
                vec![("nick", "ally3")],
                vec!["nick"],
            )]),
        ),
    )
    .expect_err("set/remove overlap must reject the whole chunk");
    let RouterError::InvalidArgument(overlap_message) = &overlap else {
        panic!("set/remove overlap must reject with InvalidArgument: {overlap:?}");
    };
    assert!(
        overlap_message.contains("both set and removed"),
        "{overlap_message}"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally2".to_owned()),
        "rejected chunk must leave state untouched"
    );

    bulk_load_as_admin(&env, finalize(GRAPH_NAME, key)).expect("finalize update");
    let status = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("update status");
    assert_state(&status, BulkLoadPublicStateV1::Completed);
    let updated_total: u64 = status
        .receipts
        .iter()
        .map(|row| row.updated_row_count)
        .sum();
    assert_eq!(
        updated_total, 4,
        "receipts record SET and REMOVE rows alike for resume"
    );
}
