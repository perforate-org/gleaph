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
    ensure_property, ensure_vertex_label, gql_mutate_as_admin, gql_query_as_admin,
    index_vertex_property, install_single_shard_federation, install_two_graph_federation,
    mutation_status_as_admin, seed_bulk_load_gc_fixture_as_admin, start_graph_shard,
    stop_graph_shard, sweep_mutation_keys, test_declare_unique_constraint, wasm_bytes,
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

fn drive_to_completed(env: &FederationEnv, graph: &str, key: &str, chunks: Vec<BulkLoadChunkV1>) {
    bulk_load_as_admin(env, start(graph, key)).expect("start");
    for (index, chunk) in chunks.into_iter().enumerate() {
        bulk_load_as_admin(env, append(graph, key, index as u32, chunk)).expect("append");
    }
    bulk_load_as_admin(env, finalize(graph, key)).expect("finalize");
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

/// Catalog setup for a graph named explicitly instead of the single-shard fixture default.
fn ensure_property_in(env: &FederationEnv, graph: &str, name: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_properties",
            Encode!(&graph.to_string(), &vec![name.to_string()]).expect("encode ensure_properties"),
        )
        .unwrap_or_else(|e| panic!("ensure_properties on {graph}: {e:?}"));
    Decode!(
        &bytes,
        Result<Vec<gleaph_graph_kernel::entry::PropertyId>, RouterError>
    )
    .expect("decode ensure_properties")
    .unwrap_or_else(|e| panic!("ensure_properties on {graph}: {e:?}"));
}

fn ensure_vertex_label_in(env: &FederationEnv, graph: &str, name: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_vertex_label",
            Encode!(&graph.to_string(), &name.to_string()).expect("encode ensure_vertex_label"),
        )
        .unwrap_or_else(|e| panic!("ensure_vertex_label on {graph}: {e:?}"));
    Decode!(&bytes, Result<gleaph_graph_kernel::entry::VertexLabelId, RouterError>)
        .expect("decode ensure_vertex_label")
        .unwrap_or_else(|e| panic!("ensure_vertex_label on {graph}: {e:?}"));
}

fn index_vertex_property_in(env: &FederationEnv, graph: &str, label: &str, property: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "index_vertex_property",
            Encode!(
                &graph.to_string(),
                &label.to_string(),
                &property.to_string()
            )
            .expect("encode index_vertex_property"),
        )
        .unwrap_or_else(|e| panic!("index_vertex_property on {graph}: {e:?}"));
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("index_vertex_property on {graph} rejected: {err:?}"),
        Err(err) => panic!("decode index_vertex_property: {err}"),
    }
}

/// Read one vertex property inside one explicitly named logical graph.
fn prop_of_in(env: &FederationEnv, graph: &str, name: &str, property: &str) -> Option<String> {
    let result = gql_query_as_admin(
        env,
        &format!(
            "SESSION SET GRAPH {graph} MATCH (p:Person) WHERE p.name = '{name}' RETURN p.`{property}` AS hit"
        ),
    );
    assert_eq!(result.row_count, 1, "one row for name `{name}` in {graph}");
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

/// A durable bulk-load update establishes its own authorized graph before admission. The
/// generated `MATCH … SET` statement must therefore execute against that graph, not against the
/// caller's HOME/session graph re-resolved at GQL ingress: with two graphs holding the same
/// label/property/endpoint values, only the commanded graph may change.
#[test]
fn bulk_load_update_writes_only_the_commanded_graph() {
    let env = install_two_graph_federation();
    for graph in [GRAPH_HOME_NAME, GRAPH_REMOTE_NAME] {
        ensure_vertex_label_in(&env, graph, "Person");
        ensure_property_in(&env, graph, "name");
        ensure_property_in(&env, graph, "nick");
        index_vertex_property_in(&env, graph, "Person", "name");
        let seed_key = format!("adr0057-update-graph-scope-seed-{graph}");
        drive_to_completed(
            &env,
            graph,
            &seed_key,
            vec![BulkLoadChunkV1::Vertices(vec![named_vertex("alice")])],
        );
    }

    let key = "adr0057-update-graph-scope";
    bulk_load_as_admin(&env, start(GRAPH_REMOTE_NAME, key)).expect("start remote update");
    assert_eq!(
        bulk_load_as_admin(
            &env,
            append(
                GRAPH_REMOTE_NAME,
                key,
                0,
                BulkLoadChunkV1::Updates(vec![update_row("alice", "ally")]),
            ),
        ),
        Ok(BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 1,
            updated_row_count: 1,
        })
    );
    bulk_load_as_admin(&env, finalize(GRAPH_REMOTE_NAME, key)).expect("finalize remote update");

    assert_eq!(
        prop_of_in(&env, GRAPH_REMOTE_NAME, "alice", "nick"),
        Some("ally".to_owned()),
        "the commanded graph must receive the update"
    );
    assert_eq!(
        prop_of_in(&env, GRAPH_HOME_NAME, "alice", "nick"),
        None,
        "the caller's HOME graph must not be mutated by a remote bulk-load update"
    );
}

/// Update-chunk durability contract: a row failure after the chunk was admitted keeps the
/// committed prefix durable and the job resumable/abortable at that prefix, and a completed chunk/// R1: an admitted-but-unfinished update chunk is resumable. The public projection hides its
/// partial prefix, a resuming client re-sends the identical payload at `next_chunk_index`, and
/// the Router re-resolves only the uncommitted suffix — so a prefix row may rewrite its own match
/// property without making the chunk unresolvable.
#[test]
fn bulk_load_update_resumes_an_admitted_chunk_after_its_committed_prefix() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "name");
    ensure_property(&env, "nick");
    index_vertex_property(&env, "Person", "name");
    drive_to_completed(
        &env,
        GRAPH_NAME,
        "adr0057-update-resume-seed",
        vec![BulkLoadChunkV1::Vertices(vec![
            named_vertex("alice"),
            named_vertex("bob"),
        ])],
    );

    // Row 0 rewrites its *own* match property, so a retry that re-resolved the applied prefix
    // would find no `name = 'alice'` vertex and could never resume. Row 1 targets `bob`.
    let chunk = BulkLoadChunkV1::Updates(vec![
        BulkLoadUpdateV1 {
            vertex_label: "Person".to_owned(),
            property_name: "name".to_owned(),
            match_value: Value::Text("alice".to_owned())
                .to_binary_bytes()
                .expect("encode match value"),
            set_properties: vec![AtomicInsertPropertyV1 {
                property_name: "name".to_owned(),
                value: Value::Text("ally".to_owned())
                    .to_binary_bytes()
                    .expect("encode name"),
            }],
            remove_properties: Vec::new(),
        },
        update_row("bob", "bobby"),
    ]);

    let key = "adr0057-update-resume";
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start update");
    // Injected post-admission row failure: row 0 commits, then the Router returns a recoverable
    // error instead of dispatching row 1, leaving an admitted chunk with a committed prefix.
    arm_router_fault(&env, 10);
    let failed = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
        .expect_err("the injected row failure must surface as a recoverable Append error");
    arm_router_fault(&env, 0);
    assert!(
        format!("{failed:?}").contains("injected fault"),
        "unexpected failure: {failed:?}"
    );
    assert_eq!(
        maybe_prop_of(&env, "ally", "name"),
        Some("ally".to_owned()),
        "the committed prefix row must be applied"
    );

    let pending =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("pending status");
    assert_state(&pending, BulkLoadPublicStateV1::AppendPending);
    assert_eq!(
        pending.next_chunk_index, 0,
        "an unfinished chunk must not advance the accepted chunk index"
    );
    assert_eq!(pending.committed_chunk_count, 0);
    assert_eq!(pending.completed_chunk_count, 0);
    assert!(
        pending.receipts.is_empty(),
        "an unfinished update chunk must not project a partial receipt: {:?}",
        pending.receipts
    );

    // A different payload for the same index is rejected: the retry contract requires the
    // identical authored chunk, not merely a compatible one.
    let conflicting = bulk_load_as_admin(
        &env,
        append(
            GRAPH_NAME,
            key,
            0,
            BulkLoadChunkV1::Updates(vec![update_row("bob", "other")]),
        ),
    )
    .expect_err("a different payload for a pending chunk must conflict");
    assert!(
        matches!(conflicting, RouterError::Conflict(ref message) if message.contains("fingerprint")),
        "unexpected conflict: {conflicting:?}"
    );

    // Mutate the uncommitted row's key too. Retry must use its admitted ID, not require the
    // original property value to remain resolvable.
    gql_mutate_as_admin(
        &env,
        "MATCH (p:Person) WHERE p.name = 'bob' SET p.name = 'robert'",
        "adr0057-rename-admitted-bob",
    );
    let resumed = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
        .expect("the identical payload must resume the admitted chunk");
    assert_eq!(
        resumed,
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 2,
            updated_row_count: 2,
        }
    );
    assert_eq!(
        maybe_prop_of(&env, "robert", "nick"),
        Some("bobby".to_owned()),
        "the uncommitted suffix must be applied exactly once to its original vertex"
    );
    assert_eq!(
        maybe_prop_of(&env, "ally", "name"),
        Some("ally".to_owned()),
        "the resumed chunk must not re-apply or roll back its committed prefix"
    );

    bulk_load_as_admin(&env, finalize(GRAPH_NAME, key)).expect("finalize resumed update");
    let status = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("resumed status");
    assert_state(&status, BulkLoadPublicStateV1::Completed);
    assert_eq!(status.receipts.len(), 1);
    assert_eq!(status.receipts[0].chunk_index, 0);
    assert_eq!(
        status.receipts[0].updated_row_count, 2,
        "the resumed chunk records its complete authored row count once"
    );
    let total: u64 = status
        .receipts
        .iter()
        .map(|row| row.updated_row_count)
        .sum();
    assert_eq!(total, 2, "exactly the two authored rows are accounted for");

    // A completed chunk replays its stored receipt and is not re-resolved or re-applied.
    let replay = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk))
        .expect("completed chunk replay must return the stored receipt");
    assert_eq!(replay, resumed);
    assert_eq!(maybe_prop_of(&env, "ally", "name"), Some("ally".to_owned()));
}

/// A retry holding an admitted chunk's stale snapshot must not start an unwritten row after
/// Abort closes the job. A one-shot owner-boundary hook runs real Abort before re-admission;
/// retry no longer performs a property lookup. Exact receipt assertions protect the dispatch
/// grant separately from completed replay.
#[test]
fn bulk_load_update_abort_before_readmission_blocks_the_suspended_row_write() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "name");
    ensure_property(&env, "nick");
    index_vertex_property(&env, "Person", "name");
    drive_to_completed(
        &env,
        GRAPH_NAME,
        "adr0057-update-resolve-race-seed",
        vec![BulkLoadChunkV1::Vertices(vec![
            named_vertex("alice"),
            named_vertex("bob"),
        ])],
    );

    let key = "adr0057-update-resolve-race";
    let chunk = BulkLoadChunkV1::Updates(vec![
        update_row("alice", "ally"),
        update_row("bob", "bobby"),
    ]);
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start update");

    // Leave an admitted chunk whose prefix is exactly row 0 (row 0 commits, row 1 does not run).
    arm_router_fault(&env, 10);
    bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
        .expect_err("the injected row failure must leave row 1 unwritten");
    arm_router_fault(&env, 0);
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally".to_owned())
    );
    assert_eq!(maybe_prop_of(&env, "bob", "nick"), None);

    // Reach admission with a stale snapshot, not an unrelated index/transport error.
    arm_router_fault(&env, 12);
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
            .expect("late callback replays the receipt after Abort"),
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 1,
            updated_row_count: 1,
        }
    );
    let aborted =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("aborted status");
    assert_state(&aborted, BulkLoadPublicStateV1::Aborted);
    assert_eq!(aborted.committed_chunk_count, 1);
    assert_eq!(aborted.receipts.len(), 1);
    assert_eq!(
        aborted.receipts[0].updated_row_count, 1,
        "the terminal receipt must claim exactly the rows that were written"
    );

    assert_eq!(
        maybe_prop_of(&env, "bob", "nick"),
        None,
        "the never-written row must not be applied by the suspended retry"
    );

    // The terminal state and its receipt are unchanged by the late caller.
    let terminal =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("terminal status");
    assert_state(&terminal, BulkLoadPublicStateV1::Aborted);
    assert_eq!(terminal.receipts.len(), 1);
    assert_eq!(terminal.receipts[0].updated_row_count, 1);
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally".to_owned())
    );

    // A fresh retry of the same payload replays the receipt and still writes nothing.
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk))
            .expect("post-abort retry must replay the stored receipt"),
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 1,
            updated_row_count: 1,
        }
    );
    assert_eq!(maybe_prop_of(&env, "bob", "nick"), None);
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally".to_owned())
    );
}

/// T3: a genuinely rejected row (the shard's Graph call is refused while the shard is stopped) is
/// a *real* failure, not an injected one, and the row keeps a durable Router saga record. The
/// classification must therefore treat it as unsettled rather than write-free. Two contracts:
///
/// (a) without an Abort, the identical re-`Append` settles the row through its journal key and
///     completes the chunk, applying each row exactly once;
/// (b) once an `Abort` is admitted, the unwritten suffix is never started: the `Abort` is
///     retryably `Busy` while a row is unsettled, a re-`Append` may still settle the row that was
///     already dispatched, and the next `Abort` terminalizes at exactly that settled prefix.
///
/// This tests retryable transport failure. Deterministic constrained-SET rejection is covered
/// separately, without a transport failure or fault hook.
#[test]
fn bulk_load_update_settles_a_rejected_row_and_aborts_at_its_true_prefix() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "name");
    ensure_property(&env, "nick");
    index_vertex_property(&env, "Person", "name");
    drive_to_completed(
        &env,
        GRAPH_NAME,
        "adr0057-update-rejected-seed",
        vec![BulkLoadChunkV1::Vertices(vec![
            named_vertex("alice"),
            named_vertex("bob"),
            named_vertex("carol"),
            named_vertex("dave"),
        ])],
    );
    let chunk = BulkLoadChunkV1::Updates(vec![
        update_row("alice", "ally"),
        update_row("bob", "bobby"),
    ]);
    // Scenario (b) runs on its own vertices so scenario (a)'s writes cannot be mistaken for its
    // verdicts.
    let abort_chunk = BulkLoadChunkV1::Updates(vec![
        update_row("carol", "caz"),
        update_row("dave", "davey"),
    ]);

    // ── (a) rejection then a plain retry: the chunk completes exactly once ──
    let retry_key = "adr0057-update-rejected-retry";
    bulk_load_as_admin(&env, start(GRAPH_NAME, retry_key)).expect("start retry job");
    stop_graph_shard(&env, env.graph_source);
    let rejected_message = submit_bulk_load(&env, append(GRAPH_NAME, retry_key, 0, chunk.clone()));
    env.pic.tick();
    start_graph_shard(&env, env.graph_source);
    let rejected = await_bulk_load(&env, rejected_message);
    let RouterError::InvalidArgument(message) = rejected.as_ref().expect_err("rejected") else {
        panic!("the stopped shard must reject the row's Graph call: {rejected:?}");
    };
    assert!(
        message.contains("execute_plan_update") && message.contains("is stopped"),
        "exact Graph rejection changed: {message}"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        None,
        "a rejected row must not have applied its write"
    );
    let settled = bulk_load_as_admin(&env, append(GRAPH_NAME, retry_key, 0, chunk.clone()))
        .expect("the identical payload must settle the rejected row and finish the chunk");
    assert_eq!(
        settled,
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 2,
            updated_row_count: 2,
        }
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally".to_owned())
    );
    assert_eq!(maybe_prop_of(&env, "bob", "nick"), Some("bobby".to_owned()));
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, retry_key, 0, chunk.clone()))
            .expect("completed replay"),
        settled,
        "a completed chunk replays its stored receipt"
    );
    bulk_load_as_admin(&env, finalize(GRAPH_NAME, retry_key)).expect("finalize retry job");
    let retry_status =
        bulk_load_status_as_admin(&env, GRAPH_NAME, retry_key, None, 8).expect("retry status");
    assert_state(&retry_status, BulkLoadPublicStateV1::Completed);
    assert_eq!(
        retry_status
            .receipts
            .iter()
            .map(|row| row.updated_row_count)
            .sum::<u64>(),
        2,
        "each authored row must be counted exactly once"
    );

    // ── (b) rejection then an Abort: the unwritten suffix is never started ──
    let abort_key = "adr0057-update-rejected-abort";
    bulk_load_as_admin(&env, start(GRAPH_NAME, abort_key)).expect("start abort job");
    stop_graph_shard(&env, env.graph_source);
    let failed_message =
        submit_bulk_load(&env, append(GRAPH_NAME, abort_key, 0, abort_chunk.clone()));
    env.pic.tick();
    let failed = await_bulk_load(&env, failed_message).expect_err("stopped Graph rejects dispatch");
    let RouterError::InvalidArgument(message) = failed else {
        panic!("unexpected dispatch rejection: {failed:?}");
    };
    assert!(
        message.contains("is stopped"),
        "unexpected rejection: {message}"
    );

    // Unknown (journal unreadable) and then Unresolved (readable but absent) must both keep
    // the child pending. No successful prefix or terminal receipt may be invented in either case.
    for journal_readable in [false, true] {
        if journal_readable {
            start_graph_shard(&env, env.graph_source);
        }
        assert_eq!(
            bulk_load_as_admin(&env, abort(GRAPH_NAME, abort_key)),
            Err(RouterError::Busy {
                operation: "bulk_load.append".into()
            }),
            "journal_readable={journal_readable}"
        );
        let aborting = bulk_load_status_as_admin(&env, GRAPH_NAME, abort_key, None, 8)
            .expect("status while aborting");
        assert_state(&aborting, BulkLoadPublicStateV1::AbortPending);
        assert_eq!(aborting.next_chunk_index, 0);
        assert_eq!(aborting.committed_chunk_count, 0);
        assert_eq!(aborting.completed_chunk_count, 0);
        assert!(aborting.receipts.is_empty());
    }

    // The re-Append settles the row that was already dispatched and refuses to start the row that
    // never was; the job stays aborting.
    let settling = bulk_load_as_admin(&env, append(GRAPH_NAME, abort_key, 0, abort_chunk.clone()))
        .expect_err("a winding-down job must not start the unwritten row");
    assert!(
        matches!(
            settling,
            RouterError::Busy { ref operation } if operation == "bulk_load.append"
        ),
        "unexpected settle error: {settling:?}"
    );
    assert_eq!(
        maybe_prop_of(&env, "carol", "nick"),
        Some("caz".to_owned()),
        "the already-dispatched row must be settled exactly once"
    );
    assert_eq!(
        maybe_prop_of(&env, "dave", "nick"),
        None,
        "the never-dispatched row must not be started after Abort"
    );

    // The next Abort terminalizes at the settled prefix, and its receipt matches the writes.
    assert_eq!(
        bulk_load_as_admin(&env, abort(GRAPH_NAME, abort_key)).expect("second abort"),
        BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted,
        }
    );
    let terminal =
        bulk_load_status_as_admin(&env, GRAPH_NAME, abort_key, None, 8).expect("terminal status");
    assert_state(&terminal, BulkLoadPublicStateV1::Aborted);
    assert_eq!(terminal.receipts.len(), 1);
    assert_eq!(
        terminal.receipts[0].updated_row_count, 1,
        "the terminal receipt must count exactly the applied row"
    );
    let applied = u64::from(maybe_prop_of(&env, "carol", "nick").is_some())
        + u64::from(maybe_prop_of(&env, "dave", "nick").is_some());
    assert_eq!(
        terminal
            .receipts
            .iter()
            .map(|row| row.updated_row_count)
            .sum::<u64>(),
        applied,
        "a terminal abort must never leave a write outside its receipts"
    );
    assert_eq!(applied, 1);
    // A further retry replays the terminal receipt and still writes nothing.
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, abort_key, 0, abort_chunk.clone()))
            .expect("post-abort replay"),
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 1,
            updated_row_count: 1,
        }
    );
    assert_eq!(maybe_prop_of(&env, "dave", "nick"), None);
    assert_eq!(
        bulk_load_as_admin(&env, abort(GRAPH_NAME, abort_key)).expect("repeat abort"),
        BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted,
        }
    );
}

/// A constrained SET is rejected by the real GQL admission owner after a valid prefix.
/// The released routing reservation proves the failed row never dispatched; Abort closes at
/// the exact prefix without changing the constraint, clearing a fault, or changing the payload.
#[test]
fn bulk_load_update_deterministic_row_failure_converges_to_a_terminal_abort() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, "person_name", "Person", "name");
    ensure_property(&env, "nick");
    index_vertex_property(&env, "Person", "name");
    for name in ["alice", "bob"] {
        gql_mutate_as_admin(
            &env,
            &format!("INSERT (:Person {{name: '{name}'}})"),
            &format!("adr0057-update-deterministic-seed-{name}"),
        );
    }

    let key = "adr0057-update-deterministic";
    let mut rejected_row = update_row("bob", "bobby");
    rejected_row.set_properties[0].property_name = "name".to_owned();
    let chunk = BulkLoadChunkV1::Updates(vec![update_row("alice", "ally"), rejected_row]);
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start update");
    let first = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
        .expect_err("the constrained row must be rejected");
    assert_eq!(
        first,
        RouterError::NotImplemented(
            "SET on a uniqueness-constrained property or label requires the two-phase \
         acquire/release protocol, which is not yet implemented (ADR 0030); refused \
         rather than risk writing a duplicate value"
                .to_owned()
        )
    );
    let row_status = mutation_status_as_admin(&env, GRAPH_NAME, &format!("{key}:0:u1"))
        .expect("rejected row retains its released routing reservation");
    assert_eq!(
        row_status.phase,
        gleaph_graph_kernel::plan_exec::MutationLifecyclePhase::Failed
    );
    let (journal,): (Option<gleaph_graph_kernel::plan_exec::GraphMutationJournalEntryWire>,) =
        pocket_ic::update_candid_as(
            &env.pic,
            env.graph_source,
            env.router,
            "get_mutation_journal_entry",
            (row_status.mutation_id,),
        )
        .expect("read the Graph-owned journal");
    assert!(
        journal.is_none(),
        "the rejected row must not have a Graph write"
    );
    assert_eq!(maybe_prop_of(&env, "bob", "name"), Some("bob".to_owned()));
    assert_eq!(
        gql_query_as_admin(
            &env,
            "MATCH (p:Person) WHERE p.name = 'bobby' RETURN p.name"
        )
        .row_count,
        0
    );
    // The valid prefix row is applied, and the failing row did not change anything.
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally".to_owned())
    );
    assert_eq!(maybe_prop_of(&env, "bob", "nick"), None);
    // The same payload (still failing) reports the same deterministic error, never a different
    // fingerprint/conflict error and never a silent success.
    let second = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
        .expect_err("the same failing payload must report the same deterministic error");
    assert_eq!(second, first, "a deterministic row failure must be stable");

    // Abort converges finitely: the failing row is proven write-free, so the chunk closes at its
    // one-row prefix.
    let aborted = bulk_load_as_admin(&env, abort(GRAPH_NAME, key)).expect("abort must terminate");
    assert_eq!(
        aborted,
        BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted,
        }
    );
    let status = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("aborted status");
    assert_state(&status, BulkLoadPublicStateV1::Aborted);
    assert_eq!(status.committed_chunk_count, 1);
    assert_eq!(status.completed_chunk_count, 1);
    assert_eq!(status.receipts.len(), 1);
    assert_eq!(
        status.receipts[0].updated_row_count, 1,
        "the abort must record exactly the committed prefix"
    );
    assert_eq!(
        maybe_prop_of(&env, "alice", "nick"),
        Some("ally".to_owned())
    );
    assert_eq!(
        maybe_prop_of(&env, "bob", "nick"),
        None,
        "the write-free row must stay unapplied"
    );
    // Re-Abort and a replayed Append stay consistent with the terminal state.
    assert_eq!(
        bulk_load_as_admin(&env, abort(GRAPH_NAME, key)).expect("re-abort"),
        BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted,
        }
    );
    let replayed = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk))
        .expect("the accepted chunk stays replayable after abort");
    assert_eq!(
        replayed,
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 1,
            updated_row_count: 1,
        }
    );
    assert_eq!(maybe_prop_of(&env, "bob", "name"), Some("bob".to_owned()));
    assert_eq!(
        gql_query_as_admin(
            &env,
            "MATCH (p:Person) WHERE p.name = 'bobby' RETURN p.name"
        )
        .row_count,
        0,
        "replay after a terminal abort must not resurrect the rejected SET"
    );
}

/// S1 scenario 2 (counterexample B): the Graph canonical write landed but the Router lost the
/// bulk prefix record, and the row rewrote its own match property. A resend must settle the row
/// from the durable journal instead of re-resolving the stale property, apply it exactly once,
/// and record it exactly once.
#[test]
fn bulk_load_update_resumes_a_row_whose_canonical_write_lost_its_prefix_record() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "name");
    ensure_property(&env, "nick");
    index_vertex_property(&env, "Person", "name");
    drive_to_completed(
        &env,
        GRAPH_NAME,
        "adr0057-update-lost-prefix-seed",
        vec![BulkLoadChunkV1::Vertices(vec![named_vertex("alice")])],
    );

    // The single row rewrites the property it matches on: after the write there is no vertex
    // with `name = 'alice'`, so any implementation that re-resolves before consulting the journal
    // fails here instead of converging.
    let key = "adr0057-update-lost-prefix";
    let chunk = BulkLoadChunkV1::Updates(vec![BulkLoadUpdateV1 {
        vertex_label: "Person".to_owned(),
        property_name: "name".to_owned(),
        match_value: Value::Text("alice".to_owned())
            .to_binary_bytes()
            .expect("encode match value"),
        set_properties: vec![AtomicInsertPropertyV1 {
            property_name: "name".to_owned(),
            value: Value::Text("ally".to_owned())
                .to_binary_bytes()
                .expect("encode name"),
        }],
        remove_properties: Vec::new(),
    }]);
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start update");

    // The Graph write commits, then the enclosing callback traps before the prefix record, so the
    // Router keeps only the pre-dispatch saga record while the Graph holds the write.
    arm_router_fault(&env, 11);
    bulk_load_as_admin_expect_trap(&env, append(GRAPH_NAME, key, 0, chunk.clone()));
    arm_router_fault(&env, 0);
    let row_status = mutation_status_as_admin(&env, GRAPH_NAME, &format!("{key}:0:u0"))
        .expect("pre-dispatch row journal survives the callback trap");
    let (journal,): (Option<gleaph_graph_kernel::plan_exec::GraphMutationJournalEntryWire>,) =
        pocket_ic::update_candid_as(
            &env.pic,
            env.graph_source,
            env.router,
            "get_mutation_journal_entry",
            (row_status.mutation_id,),
        )
        .expect("read the Graph-owned journal");
    assert_eq!(
        journal
            .expect("canonical write has a journal receipt")
            .state(),
        gleaph_graph_kernel::plan_exec::MutationJournalState::Completed,
        "scalar execute_plan_update persists only a completed receipt, not Incomplete"
    );
    assert_eq!(
        maybe_prop_of(&env, "ally", "name"),
        Some("ally".to_owned()),
        "the trap must leave the Graph canonical write durable"
    );
    let lost = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8)
        .expect("status after the lost prefix record");
    assert_eq!(
        lost.next_chunk_index, 0,
        "the chunk must still be pending at index 0"
    );
    assert!(
        lost.receipts.is_empty(),
        "no receipt may claim the unrecorded prefix: {:?}",
        lost.receipts
    );

    // The identical payload must converge from the journal: no re-resolution of `name = 'alice'`,
    // exactly one application, and exactly one receipt row.
    let resumed = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
        .expect("the journal must settle the row instead of re-resolving the rewritten property");
    assert_eq!(
        resumed,
        BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 1,
            updated_row_count: 1,
        }
    );
    assert_eq!(
        maybe_prop_of(&env, "ally", "name"),
        Some("ally".to_owned()),
        "the settled row must not be applied a second time"
    );
    bulk_load_as_admin(&env, finalize(GRAPH_NAME, key)).expect("finalize resumed update");
    let status = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("resumed status");
    assert_state(&status, BulkLoadPublicStateV1::Completed);
    assert_eq!(status.receipts.len(), 1);
    assert_eq!(
        status
            .receipts
            .iter()
            .map(|row| row.updated_row_count)
            .sum::<u64>(),
        1,
        "the settled row must be counted exactly once"
    );
    // Replay of the completed chunk returns the stored receipt and applies nothing.
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk)).expect("completed replay"),
        resumed
    );
    assert_eq!(maybe_prop_of(&env, "ally", "name"), Some("ally".to_owned()));
}

/// R2/S1 scenario 3: an update chunk that was admitted and partially applied must not be silently
/// abandoned. Abort cannot terminalize at the recorded prefix while the running append can still
/// commit a row; it stays `AbortPending` (exact retryable diagnostic) and the documented
/// convergence is that same-payload re-Append, whose completion terminalizes the abort at the
/// true prefix. The terminal invariant is asserted as `recorded == applied`, so an abort can
/// never leave a write outside the receipts.
#[test]
fn bulk_load_update_abort_does_not_claim_a_partially_applied_chunk() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "name");
    ensure_property(&env, "nick");
    index_vertex_property(&env, "Person", "name");
    drive_to_completed(
        &env,
        GRAPH_NAME,
        "adr0057-update-abort-seed",
        vec![BulkLoadChunkV1::Vertices(vec![
            named_vertex("alice"),
            named_vertex("bob"),
        ])],
    );

    let key = "adr0057-update-abort-race";
    let chunk = BulkLoadChunkV1::Updates(vec![
        update_row("alice", "ally"),
        update_row("bob", "bobby"),
    ]);
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start update");

    // Suspend the Append inside its first row's Graph call, then interleave an Abort ingress.
    stop_graph_shard(&env, env.graph_source);
    let append_message = submit_bulk_load(&env, append(GRAPH_NAME, key, 0, chunk.clone()));
    env.pic.tick();
    let suspended = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8)
        .expect("status while the update row is in flight");
    assert_state(&suspended, BulkLoadPublicStateV1::AppendPending);
    assert!(
        suspended.receipts.is_empty(),
        "an in-flight update chunk must not project a partial receipt"
    );

    let abort_error = await_bulk_load(&env, submit_bulk_load(&env, abort(GRAPH_NAME, key)))
        .expect_err("Abort must not terminalize a chunk whose row may still commit");
    assert!(
        matches!(
            abort_error,
            RouterError::Busy { ref operation } if operation == "bulk_load.append"
        ),
        "unexpected abort error: {abort_error:?}"
    );
    let abort_pending = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8)
        .expect("status while abort is pending");
    assert_state(&abort_pending, BulkLoadPublicStateV1::AbortPending);
    assert!(
        abort_pending.receipts.is_empty(),
        "a pending abort must not claim an uncommitted prefix"
    );
    assert!(matches!(
        bulk_load_as_admin(&env, finalize(GRAPH_NAME, key)),
        Err(RouterError::Busy { .. })
    ));

    // Resume the shard so the suspended row can settle, then await the Append outcome.
    start_graph_shard(&env, env.graph_source);
    let append_outcome = await_bulk_load(&env, append_message);
    let after_append = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8)
        .expect("status after the suspended append settled");
    let mut recorded: u64 = after_append
        .receipts
        .iter()
        .map(|row| row.updated_row_count)
        .sum();
    if !matches!(after_append.state, BulkLoadPublicStateV1::Aborted) {
        // The suspended row's Graph call was rejected while the shard was stopped, so this
        // attempt committed nothing. Assert that positively rather than treating "Err" as proof
        // of no commit.
        assert!(
            append_outcome.is_err(),
            "the stopped shard must reject the suspended append: {append_outcome:?}"
        );
        assert_state(&after_append, BulkLoadPublicStateV1::AbortPending);
        assert_eq!(
            after_append
                .receipts
                .iter()
                .map(|row| row.updated_row_count)
                .sum::<u64>(),
            0,
            "no receipt may claim rows from a rejected append attempt"
        );
        // The row does have a durable saga record, so it is unsettled rather than write-free: the
        // job may only terminalize once that settles or is proven write-free.
        let settling = bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
            .expect_err("a winding-down job must not start its never-dispatched first row");
        assert!(
            matches!(
                settling,
                RouterError::Busy { ref operation } if operation == "bulk_load.append"
            ),
            "unexpected settle outcome: {settling:?}"
        );
        // The already-dispatched first row may still be settled (that is the intended
        // reconciliation), but the never-dispatched second row must not be started.
        assert_eq!(
            maybe_prop_of(&env, "bob", "nick"),
            None,
            "the never-dispatched row must not be started while the job winds down"
        );
        let converged = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8)
            .expect("status after the settle attempt");
        assert_state(&converged, BulkLoadPublicStateV1::AbortPending);
        // With the rejected row proven write-free by the owners, the next Abort terminalizes at
        // the zero prefix.
        let terminal_abort = bulk_load_as_admin(&env, abort(GRAPH_NAME, key))
            .expect("the second Abort must terminalize the write-free chunk");
        assert_eq!(
            terminal_abort,
            BulkLoadResponse::AbortAccepted {
                state: BulkLoadPublicStateV1::Aborted,
            }
        );
        let converged = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8)
            .expect("status after the converged abort");
        assert_state(&converged, BulkLoadPublicStateV1::Aborted);
        assert_eq!(
            converged.receipts.len(),
            1,
            "the converged abort records exactly one completed chunk"
        );
        recorded = converged
            .receipts
            .iter()
            .map(|row| row.updated_row_count)
            .sum();
    }

    // The invariant: every applied row is counted, and the terminal state is Aborted.
    let alice = maybe_prop_of(&env, "alice", "nick");
    let bob = maybe_prop_of(&env, "bob", "nick");
    let applied = u64::from(alice.is_some()) + u64::from(bob.is_some());
    assert_eq!(
        recorded, applied,
        "an abort must never leave a write outside the recorded prefix (recorded {recorded}, applied {applied})"
    );
    // The terminal prefix must describe exactly the applied rows: a row count that exceeds what
    // is visible (or vice versa) is the defect this contract exists to prevent.
    assert_eq!(
        (alice.as_deref(), bob.as_deref()),
        match recorded {
            0 => (None, None),
            1 => (Some("ally"), None),
            2 => (Some("ally"), Some("bobby")),
            other => panic!("terminal prefix {other} has no consistent applied state"),
        },
        "the terminal prefix must match the applied values (recorded {recorded})"
    );
    let terminal =
        bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("terminal status");
    assert_state(&terminal, BulkLoadPublicStateV1::Aborted);
    assert_eq!(
        bulk_load_as_admin(&env, abort(GRAPH_NAME, key)),
        Ok(BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted,
        })
    );

    // Reuse this fixture for deletion after admission, both with an acknowledged zero-effect
    // receipt and with that response lost. A replacement Bob is a decoy, never a new target.
    for lose_response in [false, true] {
        let key = if lose_response {
            "adr0057-deleted-target-lost"
        } else {
            "adr0057-deleted-target"
        };
        let chunk = BulkLoadChunkV1::Updates(vec![
            update_row("alice", "kept-prefix"),
            update_row("bob", "must-not-land"),
        ]);
        bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start deletion case");
        arm_router_fault(&env, 10);
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone()))
            .expect_err("stop before row one");
        arm_router_fault(&env, 0);
        assert_eq!(nick_of(&env, "alice"), "kept-prefix");
        gql_mutate_as_admin(
            &env,
            "MATCH (p:Person) WHERE p.name = 'bob' DELETE p",
            &format!("{key}-delete"),
        );
        gql_mutate_as_admin(
            &env,
            "INSERT (:Person {name: 'bob'})",
            &format!("{key}-decoy"),
        );
        let rejected =
            RouterError::NotFound("bulk-load update target is no longer eligible".into());
        if lose_response {
            arm_router_fault(&env, 11);
            bulk_load_as_admin_expect_trap(&env, append(GRAPH_NAME, key, 0, chunk.clone()));
            arm_router_fault(&env, 0);
        } else {
            for _ in 0..2 {
                assert_eq!(
                    bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone())),
                    Err(rejected.clone())
                );
            }
        }
        let row = mutation_status_as_admin(&env, GRAPH_NAME, &format!("{key}:0:u1"))
            .expect("row journal");
        let (journal,): (Option<gleaph_graph_kernel::plan_exec::GraphMutationJournalEntryWire>,) =
            pocket_ic::update_candid_as(
                &env.pic,
                env.graph_source,
                env.router,
                "get_mutation_journal_entry",
                (row.mutation_id,),
            )
            .expect("Graph journal");
        let journal = journal.expect("durable zero-effect receipt");
        assert_eq!(
            journal.state(),
            gleaph_graph_kernel::plan_exec::MutationJournalState::Completed
        );
        assert_eq!(
            journal.row_count(),
            0,
            "a deleted target is not a completed bulk row"
        );
        assert_eq!(
            maybe_prop_of(&env, "bob", "nick"),
            None,
            "the replacement Bob must be untouched"
        );
        let pending = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("pending");
        assert_state(&pending, BulkLoadPublicStateV1::AppendPending);
        assert_eq!(
            (
                pending.next_chunk_index,
                pending.committed_chunk_count,
                pending.completed_chunk_count
            ),
            (0, 0, 0)
        );
        assert!(pending.receipts.is_empty());
        // No re-Append is needed in the lost-response case: the Graph's zero receipt is itself
        // conclusive evidence. The never-written row cannot become eligible after this Abort.
        for _ in 0..2 {
            assert_eq!(
                bulk_load_as_admin(&env, abort(GRAPH_NAME, key)),
                Ok(BulkLoadResponse::AbortAccepted {
                    state: BulkLoadPublicStateV1::Aborted
                })
            );
        }
        let terminal = bulk_load_status_as_admin(&env, GRAPH_NAME, key, None, 8).expect("terminal");
        assert_state(&terminal, BulkLoadPublicStateV1::Aborted);
        assert_eq!(
            (
                terminal.next_chunk_index,
                terminal.committed_chunk_count,
                terminal.completed_chunk_count
            ),
            (1, 1, 1)
        );
        assert_eq!(terminal.receipts.len(), 1);
        assert_eq!(terminal.receipts[0].updated_row_count, 1);
        assert_eq!(
            bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk)),
            Ok(BulkLoadResponse::Updated {
                chunk_index: 0,
                next_offset: 1,
                updated_row_count: 1
            })
        );
        assert_eq!(maybe_prop_of(&env, "bob", "nick"), None);
        assert_eq!(nick_of(&env, "alice"), "kept-prefix");
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

    // Both keys initially resolve uniquely. Renaming Alice to Bob in row zero must not broaden
    // row one's target from the original Bob to both vertices. Compare values by immutable ID.
    let key = "adr0057-update-pinned-target";
    let chunk = BulkLoadChunkV1::Updates(vec![
        remove_row("alice", vec![("name", "bob")], vec![]),
        update_row("bob", "only-original-bob"),
    ]);
    bulk_load_as_admin(&env, start(GRAPH_NAME, key)).expect("start pinned-target job");
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk.clone())),
        Ok(BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 2,
            updated_row_count: 2
        })
    );
    let result = gql_query_as_admin(
        &env,
        "MATCH (p:Person) RETURN element_id(p) AS id, p.nick AS nick",
    );
    let wire = GqlWireRows::decode_blob(result.rows_blob.as_ref().expect("identity projection"))
        .expect("decode identity projection");
    let actual: std::collections::BTreeMap<_, _> = wire
        .rows
        .into_iter()
        .map(|row| {
            let row = row.try_into_value_row().expect("value row");
            let Value::Bytes(id) = row.get("id").expect("id") else {
                panic!("binary vertex ID required")
            };
            (id.clone(), row.get("nick").expect("nick").clone())
        })
        .collect();
    let ids = &seed.receipts[0].receipt.allocated_vertex_ids;
    assert_eq!(
        actual,
        std::collections::BTreeMap::from([
            (ids[0].clone(), Value::Text("ally".into())),
            (ids[1].clone(), Value::Text("only-original-bob".into())),
            (ids[2].clone(), Value::Null),
        ]),
        "renaming a match key must not change another row's admitted target"
    );
    bulk_load_as_admin(&env, finalize(GRAPH_NAME, key)).expect("finalize pinned-target job");
    assert_eq!(
        bulk_load_as_admin(&env, append(GRAPH_NAME, key, 0, chunk)),
        Ok(BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 2,
            updated_row_count: 2
        })
    );
    assert_bulk_update_policy_and_expired_row_replay(&env);
}

/// Property-policy lowering and row retention share this fixture so time advancement does not
/// require another federation. The blocked row becomes eligible only after its zero receipt.
fn assert_bulk_update_policy_and_expired_row_replay(env: &FederationEnv) {
    let caller = candid::Principal::from_slice(&[0xD1; 29]);
    drive_to_completed(
        env,
        GRAPH_NAME,
        "policy-row-seed",
        vec![BulkLoadChunkV1::Vertices(vec![
            named_vertex("policy-allowed"),
            named_vertex("policy-blocked"),
        ])],
    );
    for (i, statement) in [
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Person FOR (p:Person) WHERE p.name = 'policy-allowed' TO PRINCIPAL '{}'", caller.to_text()),
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Person {{ name, nick }} TO PRINCIPAL '{}'", caller.to_text()),
        format!("GRANT UPDATE ON GRAPH {GRAPH_NAME} NODES Person TO PRINCIPAL '{}'", caller.to_text()),
    ].into_iter().enumerate() {
        gql_mutate_as_admin(env, &statement, &format!("policy-row-grant-{i}"));
    }
    let submit = |command| {
        let bytes = env
            .pic
            .update_call(env.router, caller, "bulk_load", Encode!(&command).unwrap())
            .unwrap();
        Decode!(&bytes, Result<BulkLoadResponse, RouterError>).unwrap()
    };
    let allowed = "policy-row-allowed";
    submit(start(GRAPH_NAME, allowed)).unwrap();
    assert_eq!(
        submit(append(
            GRAPH_NAME,
            allowed,
            0,
            BulkLoadChunkV1::Updates(vec![update_row("policy-allowed", "allowed-write"),])
        )),
        Ok(BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 1,
            updated_row_count: 1
        })
    );
    assert_eq!(nick_of(env, "policy-allowed"), "allowed-write");
    submit(finalize(GRAPH_NAME, allowed)).unwrap();

    let blocked = "policy-row-blocked";
    let chunk = BulkLoadChunkV1::Updates(vec![update_row("policy-blocked", "must-stay-empty")]);
    submit(start(GRAPH_NAME, blocked)).unwrap();
    let zero = Err(RouterError::NotFound(
        "bulk-load update target is no longer eligible".into(),
    ));
    assert_eq!(submit(append(GRAPH_NAME, blocked, 0, chunk.clone())), zero);
    assert_eq!(maybe_prop_of(env, "policy-blocked", "nick"), None);
    let row_key = format!("{blocked}:0:u0");
    let status = || {
        let bytes = env
            .pic
            .query_call(
                env.router,
                caller,
                "mutation_status",
                Encode!(&Some(GRAPH_NAME.to_owned()), &row_key).unwrap(),
            )
            .unwrap();
        Decode!(&bytes, Result<gleaph_router::types::MutationStatus, RouterError>)
            .unwrap()
            .unwrap()
    };
    let before = status();
    assert_eq!(
        before.phase,
        gleaph_graph_kernel::plan_exec::MutationLifecyclePhase::Completed
    );
    env.pic.advance_time(Duration::from_secs(8 * 24 * 60 * 60));
    assert!(
        sweep_mutation_keys(env, 1000) > 0,
        "expired ordinary records must actually be swept"
    );
    let retained = status();
    assert_eq!(retained.mutation_id, before.mutation_id);
    assert_eq!(retained.phase, before.phase);
    // Change eligibility without changing the grant or authored row payload. Fixed-ID replay
    // must return the saved zero, not select the other allowed vertex or apply a fresh write.
    gql_mutate_as_admin(
        env,
        "MATCH (p:Person {name: 'policy-blocked'}) SET p.name = 'policy-allowed'",
        "policy-row-make-eligible",
    );
    assert_eq!(submit(append(GRAPH_NAME, blocked, 0, chunk.clone())), zero);
    assert_eq!(status().mutation_id, before.mutation_id);
    let query = gql_query_as_admin(
        env,
        "MATCH (p:Person) WHERE p.name = 'policy-allowed' RETURN p.nick AS nick",
    );
    let wire = GqlWireRows::decode_blob(query.rows_blob.as_ref().unwrap()).unwrap();
    let values: Vec<_> = wire
        .rows
        .into_iter()
        .map(|row| {
            row.try_into_value_row()
                .unwrap()
                .get("nick")
                .unwrap()
                .clone()
        })
        .collect();
    assert_eq!(values.len(), 2);
    assert!(values.contains(&Value::Null));
    assert!(values.contains(&Value::Text("allowed-write".into())));
    assert_eq!(
        submit(abort(GRAPH_NAME, blocked)),
        Ok(BulkLoadResponse::AbortAccepted {
            state: BulkLoadPublicStateV1::Aborted
        })
    );
    assert_eq!(
        submit(append(GRAPH_NAME, blocked, 0, chunk)),
        Ok(BulkLoadResponse::Updated {
            chunk_index: 0,
            next_offset: 0,
            updated_row_count: 0
        })
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
