//! PocketIC: the knowledge-demo vector ingestion flow end to end.
//!
//! Proves the demo's GraphRAG scenario on the real canister boundary: a typed graph with a
//! `Document` label and a small-dimension `document_embedding` vector index (created through
//! the GQL DDL path), vertices + edges committed by a durable bulk load, and then — through
//! exactly the CLI code path (`gleaph_cli::embed::run_with_transport`) — the receipt-walk id
//! mapping plus batch `ingest_vertex_embeddings`. A prepared scenario-3-shaped query
//! (structure + SEARCH + access constraint) must return the expected public documents ranked
//! by similarity, never the private document.
//!
//! Expected counts stated before running (per plan contract):
//! - 1 `#[test]` function.
//! - One PocketIC environment (`install_single_shard_federation`: 3 funded creations + 3
//!   installs) plus one vector canister install; no upgrades.
//! - Bulk load: 1 Start + 2 Appends + 1 Finalize, all as the bootstrap admin.
//! - Ingest: 1 receipt walk + payload-fitted `ingest_vertex_embeddings` batches driven by the
//!   CLI embed module against a direct-call transport.

use std::path::Path;

use candid::{Decode, Encode};
use gleaph_bulk_load_api::{
    AtomicInsertPropertyV1, AtomicInsertVertexV1, BulkLoadChunkV1, BulkLoadCommand, BulkLoadEdgeV1,
    BulkLoadEndpointV1, BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage,
};
use gleaph_cli::embed::{
    AdminIngestVertexEmbeddingBatchArgs, EmbedIngestTransport, IngestPlan, run_with_transport,
};
use gleaph_gql::Value;
use gleaph_gql_ic::{wire::encode_gql_params_blob, GqlWireRows};
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_pocket_ic_tests::{
    bulk_load_as_admin, bulk_load_status_as_admin, ensure_vertex_label, gql_mutate_as_admin,
    gql_query_with_params_as_admin, install_single_shard_federation, install_vector_canister,
    prepare_batch_as_admin, prepared_query_with_params_as, FederationEnv, GRAPH_NAME,
};
use gleaph_prepared_api::PreparedRegistration;
use gleaph_router::types::AdminAttachVectorIndexShardArgs;

const EMBEDDING_NAME: &str = "embedding";
const DIMS: usize = 8;
const BULK_KEY: &str = "vector-flow-initial-load-v1";

// ──── fixture helpers (ADR 0031 activation sequence, per-test-file style) ────

fn set_dispatch_activation(env: &FederationEnv, enabled: bool) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "set_vector_dispatch_enabled",
            Encode!(&enabled).expect("encode"),
        )
        .expect("activation call");
    let _: Result<(), RouterError> = Decode!(&bytes, Result<(), RouterError>).expect("decode");
}

fn attach_shard(env: &FederationEnv, vector: candid::Principal) {
    let args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id: ShardId::new(0),
        vector_canister: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "attach_vector_shard",
            Encode!(&args).expect("encode"),
        )
        .expect("router attach");
    let _: Result<(), RouterError> = Decode!(&bytes, Result<(), RouterError>).expect("decode");
}

/// The Router-allocated id of the single GQL-created vector definition.
fn first_index_id(env: &FederationEnv) -> u32 {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_vector_indexes",
            Encode!(&GRAPH_NAME.to_string()).expect("encode"),
        )
        .expect("list_vector_indexes call");
    let list: Result<Vec<gleaph_router::types::VectorIndexInfo>, RouterError> =
        Decode!(&bytes, Result<Vec<gleaph_router::types::VectorIndexInfo>, RouterError>)
            .expect("decode");
    let defs = list.expect("list");
    assert_eq!(defs.len(), 1, "exactly one vector index is registered");
    defs[0].index_id
}

fn activate(env: &FederationEnv, vector: candid::Principal, index_id: u32) {
    set_dispatch_activation(env, true);
    // Assign the dispatch target of the GQL-created (targetless) definition, then wire the
    // Graph → Vector sync path and the Router shard attach.
    let args = gleaph_router::types::SetVectorIndexTargetArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        index_id,
        target: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "set_vector_index_target",
            Encode!(&args).expect("encode"),
        )
        .expect("set target call");
    let _: Result<(), RouterError> = Decode!(&bytes, Result<(), RouterError>).expect("decode");
    // Route the graph's derived embedding sync to the vector owner and admit the Router on it.
    let bytes = env
        .pic
        .update_call(
            env.graph_source,
            env.router,
            "admin_set_vector_canister",
            Encode!(&vector).expect("encode"),
        )
        .expect("set graph vector routing");
    let _: Result<(), String> = Decode!(&bytes, Result<(), String>).expect("decode");
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_graph_id",
            Encode!(&GRAPH_NAME.to_string()).expect("encode"),
        )
        .expect("lookup graph id");
    let graph_id: gleaph_graph_kernel::entry::GraphId =
        Decode!(&bytes, Result<gleaph_graph_kernel::entry::GraphId, RouterError>)
            .expect("decode")
            .expect("graph id");
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &ShardId::new(0), &env.graph_source).expect("encode"),
        )
        .expect("vector attach");
    let _: Result<(), String> = Decode!(&bytes, Result<(), String>).expect("decode");
    attach_shard(env, vector);
}

// ──── direct-call transport: the Router endpoints behind the CLI, as the admin principal ────

struct AdminDirectTransport<'a>(&'a FederationEnv);

impl EmbedIngestTransport for AdminDirectTransport<'_> {
    fn status(
        &mut self,
        graph: Option<&str>,
        key: &str,
        cursor: Option<u32>,
        max_receipts: u32,
    ) -> Result<Result<BulkLoadStatusPage, RouterError>, String> {
        assert_eq!(graph, Some(GRAPH_NAME), "the flow targets the demo graph");
        Ok(bulk_load_status_as_admin(
            self.0,
            graph.expect("graph"),
            key,
            cursor,
            max_receipts,
        ))
    }

    fn ingest_batch(
        &mut self,
        args: &AdminIngestVertexEmbeddingBatchArgs,
    ) -> Result<
        Result<
            Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
            RouterError,
        >,
        String,
    > {
        let bytes = self
            .0
            .pic
            .update_call(
                self.0.router,
                self.0.admin,
                "ingest_vertex_embeddings",
                Encode!(&args).expect("encode ingest args"),
            )
            .map_err(|e| format!("ingest_vertex_embeddings transport: {e:?}"))?;
        Decode!(
            &bytes,
            Result<
                Vec<
                    Result<
                        gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult,
                        String,
                    >,
                >,
                RouterError,
            >
        )
        .map_err(|e| format!("decode ingest response: {e}"))
    }
}

// ──── seed content ────

fn property(name: &str, value: &Value) -> AtomicInsertPropertyV1 {
    AtomicInsertPropertyV1 {
        property_name: name.to_owned(),
        value: value.to_binary_bytes().expect("encode property value"),
    }
}

fn vertex(labels: &[&str], properties: Vec<AtomicInsertPropertyV1>) -> AtomicInsertVertexV1 {
    AtomicInsertVertexV1 {
        vertex_labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        initial_properties: properties,
    }
}

/// The concatenated vertex-row stream order: receipts allocate ids in exactly this ordinal.
const VERTEX_ORDER: [&str; 5] = ["d1", "d2", "d3", "alice", "platform"];

fn seed_vertices() -> BulkLoadChunkV1 {
    BulkLoadChunkV1::Vertices(vec![
        vertex(
            &["Document"],
            vec![
                property("title", &Value::Text("Intro to graph databases".into())),
                property("body", &Value::Text("Nodes and relationships".into())),
                property("is_public", &Value::Bool(true)),
            ],
        ),
        vertex(
            &["Document"],
            vec![
                property("title", &Value::Text("Vector search in graphs".into())),
                property("body", &Value::Text("Rank nodes by similarity".into())),
                property("is_public", &Value::Bool(true)),
            ],
        ),
        vertex(
            &["Document"],
            vec![
                property("title", &Value::Text("Secret notes".into())),
                property("body", &Value::Text("Private draft".into())),
                property("is_public", &Value::Bool(false)),
            ],
        ),
        vertex(
            &["Person"],
            vec![property("name", &Value::Text("Alice".into()))],
        ),
        vertex(
            &["Team"],
            vec![property("name", &Value::Text("Platform".into()))],
        ),
    ])
}

fn seed_edges(ids: &[Vec<u8>; 5]) -> BulkLoadChunkV1 {
    let edge = |source: &Vec<u8>, target: &Vec<u8>, label: &str| BulkLoadEdgeV1 {
        source: BulkLoadEndpointV1::Existing(source.clone()),
        target: BulkLoadEndpointV1::Existing(target.clone()),
        directed: true,
        edge_label_name: Some(label.to_owned()),
        inline_property: None,
        initial_edge_properties: Vec::new(),
    };
    BulkLoadChunkV1::Edges(vec![
        edge(&ids[3], &ids[0], "OWNS"),
        edge(&ids[3], &ids[1], "OWNS"),
        edge(&ids[3], &ids[2], "OWNS"),
        edge(&ids[3], &ids[4], "BELONGS_TO"),
    ])
}

/// Distinct unit-style embeddings; d2's vector doubles as the query vector so the exact-match
/// document must rank first.
fn embedding_values(seed: u8) -> Vec<f32> {
    (0..DIMS)
        .map(|index| if index == seed as usize { 1.0 } else { 0.0 })
        .collect()
}

fn write_embeddings_fixture(path: &Path) {
    use std::fs;

    let mut body = String::new();
    for (source_id, seed) in [("d1", 0u8), ("d2", 1), ("d3", 2)] {
        let values = embedding_values(seed)
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(",");
        body.push_str(&format!(
            r#"{{"source_id":"{source_id}","values":[{values}]}}"#
        ));
        body.push('\n');
    }
    fs::write(path, body).expect("write embeddings fixture");
}

fn write_vertices_fixture(path: &Path) {
    // Mirrors seeds/vertices.jsonl row order (VERTEX_ORDER); only source_id matters here.
    use std::fs;

    let mut body = String::new();
    for source_id in VERTEX_ORDER {
        body.push_str(&format!(
            "{{\"source_id\":\"{source_id}\",\"labels\":[\"Document\"]}}\n"
        ));
    }
    fs::write(path, body).expect("write vertices fixture");
}

// ──── the flow ────

#[test]
fn load_ingest_and_prepared_search_return_the_expected_documents() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    // Typed schema + small-dimension vector index through the GQL DDL path.
    let ddl = format!(
        "CREATE GRAPH TYPE kgt {{ NODE Document {{ title STRING, body STRING, is_public BOOL }}, \
         NODE Person {{ name STRING }}, NODE Team {{ name STRING }}, \
         DIRECTED EDGE Owns LABEL OWNS CONNECTING (Person -> Document), \
         DIRECTED EDGE BelongsTo LABEL BELONGS_TO CONNECTING (Person -> Team) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED kgt"
    );
    gql_mutate_as_admin(&env, &ddl, "vector-flow-bind-schema");
    gql_mutate_as_admin(
        &env,
        &format!(
            "CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding \
             OPTIONS {{ indexConfig: {{ dimensions: {DIMS}, similarity_function: 'cosine', \
             encoding: 'f32', algorithm: 'ivf_flat' }} }}"
        ),
        "vector-flow-create-index",
    );
    activate(&env, vector, first_index_id(&env));

    // Vocabulary the durable loader requires before the first chunk (ADR 0059).
    for label in ["Document", "Person", "Team"] {
        ensure_vertex_label(&env, label);
    }

    // Durable bulk load: Start → vertices → edges → Finalize(Completed).
    bulk_load_as_admin(
        &env,
        BulkLoadCommand::Start {
            graph_name: Some(GRAPH_NAME.into()),
            client_bulk_key: BULK_KEY.into(),
        },
    )
    .expect("start");
    let appended = bulk_load_as_admin(
        &env,
        BulkLoadCommand::Append {
            graph_name: Some(GRAPH_NAME.into()),
            client_bulk_key: BULK_KEY.into(),
            chunk_index: 0,
            chunk: seed_vertices(),
        },
    )
    .expect("vertex append");
    let vertex_ids: Vec<Vec<u8>> = match appended {
        BulkLoadResponse::Appended {
            receipt,
            next_offset,
            ..
        } => {
            assert_eq!(next_offset, 5, "all five vertices commit in one chunk");
            receipt.allocated_vertex_ids
        }
        other => panic!("unexpected Append response: {other:?}"),
    };
    let edge_ids: &[Vec<u8>; 5] = vertex_ids.as_slice().try_into().expect("five ids");
    bulk_load_as_admin(
        &env,
        BulkLoadCommand::Append {
            graph_name: Some(GRAPH_NAME.into()),
            client_bulk_key: BULK_KEY.into(),
            chunk_index: 1,
            chunk: seed_edges(edge_ids),
        },
    )
    .expect("edge append");
    let finalized = bulk_load_as_admin(
        &env,
        BulkLoadCommand::Finalize {
            graph_name: Some(GRAPH_NAME.into()),
            client_bulk_key: BULK_KEY.into(),
        },
    )
    .expect("finalize");
    assert!(matches!(
        finalized,
        BulkLoadResponse::FinalizeAccepted {
            state: BulkLoadPublicStateV1::Completed
        }
    ));

    // CLI ingest path: fixtures stand in for the checked-in demo files (identical shapes).
    let temp = std::env::temp_dir().join(format!("gleaph-vector-flow-{}", std::process::id()));
    std::fs::create_dir_all(&temp).expect("temp dir");
    write_vertices_fixture(&temp.join("vertices.jsonl"));
    write_embeddings_fixture(&temp.join("embeddings.jsonl"));
    let plan = IngestPlan {
        graph_name: GRAPH_NAME,
        embedding_name: EMBEDDING_NAME,
        key: BULK_KEY,
        vertices_path: &temp.join("vertices.jsonl"),
        embeddings_path: &temp.join("embeddings.jsonl"),
    };
    let summary = run_with_transport(&plan, &mut AdminDirectTransport(&env)).expect("cli ingest");
    assert_eq!(
        (
            summary.attempted,
            summary.applied,
            summary.pending,
            summary.failures.len()
        ),
        (3, 3, 0, 0),
        "every Document embedding must be applied"
    );

    // Register and execute the scenario-3-shaped prepared query.
    prepare_batch_as_admin(
        &env,
        &[PreparedRegistration {
            name: "team-readable-documents".into(),
            query:
                "MATCH (t:Team {name: 'Platform'})<-[:BELONGS_TO]-(p:Person)-[:OWNS]->(d:Document) \
                 WHERE d.is_public = TRUE \
                 SEARCH d IN (VECTOR INDEX document_embedding FOR $query LIMIT 3) SCORE AS similarity \
                 RETURN ELEMENT_ID(d) AS document_id, d.title AS title, similarity \
                 ORDER BY similarity DESC"
                    .into(),
            metadata: None,
        }],
    )
    .expect("register scenario-3 op");

    // Direct element ids for comparison.
    let element_id_of = |title: &str| -> gleaph_gql_ic::GqlWireValue {
        let find = encode_gql_params_blob(vec![("title".to_string(), Value::Text(title.into()))])
            .expect("encode title param");
        let result = gql_query_with_params_as_admin(
            &env,
            "MATCH (d:Document {title: $title}) RETURN ELEMENT_ID(d) AS document_id",
            find,
        );
        assert_eq!(result.row_count, 1, "{title} must resolve to one document");
        let blob = result.rows_blob.expect("rows blob");
        let rows = GqlWireRows::decode_blob(&blob).expect("decode rows");
        rows.rows[0]
            .columns
            .iter()
            .find(|(name, _)| name == "document_id")
            .expect("document_id column")
            .1
            .clone()
    };

    let expected_d1 = element_id_of("Intro to graph databases");
    let expected_d2 = element_id_of("Vector search in graphs");
    let excluded_d3 = element_id_of("Secret notes");

    let query_bytes: Vec<u8> = embedding_values(1)
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let params = encode_gql_params_blob(vec![("query".to_string(), Value::Bytes(query_bytes))])
        .expect("encode params");
    let result = prepared_query_with_params_as(&env, env.admin, "team-readable-documents", params);
    assert_eq!(
        result.row_count, 2,
        "only the public Platform-owned documents qualify"
    );
    let blob = result.rows_blob.expect("rows blob");
    let rows = GqlWireRows::decode_blob(&blob).expect("decode rows");
    let ids: Vec<gleaph_gql_ic::GqlWireValue> = rows
        .rows
        .iter()
        .map(|row| {
            row.columns
                .iter()
                .find(|(name, _)| name == "document_id")
                .expect("document_id column")
                .1
                .clone()
        })
        .collect();
    assert_eq!(
        ids.first(),
        Some(&expected_d2),
        "the exact-match document must rank first"
    );
    assert!(
        ids.contains(&expected_d1),
        "the second public owned document must be returned"
    );
    assert!(
        !ids.iter().any(|id| id == &excluded_d3),
        "the private document must never surface despite ownership"
    );
}
