//! PocketIC: ADR 0013 graph type catalog — catalog DDL on router stable memory.

use gleaph_pocket_ic_tests::{
    GRAPH_HOME_NAME, e2e_insert_vertex, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_as_admin_expect_err, gql_query_as_admin, gql_query_as_admin_expect_err,
    install_two_graph_federation,
};

const PERSON_KNOWS: &str = "NODE Person LABELS Person AS person, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (person -> person)";

#[test]
fn catalog_create_graph_type_returns_zero_rows() {
    let env = install_two_graph_federation();
    let ddl = format!("CREATE GRAPH TYPE gt {{ {PERSON_KNOWS} }}");
    let row_count =
        gql_execute_idempotent_as_admin(&env, &ddl, "catalog_create_graph_type_returns_zero_rows");
    assert_eq!(row_count, 0);
}

#[test]
fn catalog_typed_binding_persists_across_calls() {
    let env = install_two_graph_federation();
    let type_ddl = format!("CREATE GRAPH TYPE gt {{ {PERSON_KNOWS} }}");
    gql_execute_idempotent_as_admin(&env, &type_ddl, "catalog_typed_binding_type");
    let bind_ddl = format!("CREATE GRAPH {GRAPH_HOME_NAME} TYPED gt");
    gql_execute_idempotent_as_admin(&env, &bind_ddl, "catalog_typed_binding_graph");

    let _ = e2e_insert_vertex(&env, env.graph_source);
    let result = gql_query_as_admin(&env, "MATCH (n) RETURN n");
    assert_eq!(result.row_count, 1);
}

#[test]
fn catalog_typed_schema_rejects_undirected_match_on_directed_edge() {
    let env = install_two_graph_federation();
    let setup = format!(
        "CREATE GRAPH TYPE gt {{ {PERSON_KNOWS} }} NEXT CREATE GRAPH {GRAPH_HOME_NAME} TYPED gt"
    );
    gql_execute_idempotent_as_admin(&env, &setup, "catalog_schema_setup");
    let err = gql_query_as_admin_expect_err(&env, "MATCH ()~[e:KNOWS]~() RETURN e");
    assert!(
        matches!(
            err,
            gleaph_graph_kernel::federation::RouterError::InvalidArgument(_)
        ),
        "expected InvalidArgument for schema edge direction mismatch, got {err:?}"
    );
    assert!(
        err.to_string().contains("DIRECTED"),
        "expected directed-edge message, got {err:?}"
    );
}

#[test]
fn catalog_create_graph_unregistered_name_rejected() {
    let env = install_two_graph_federation();
    let type_ddl = format!("CREATE GRAPH TYPE gt {{ {PERSON_KNOWS} }}");
    gql_execute_idempotent_as_admin(&env, &type_ddl, "catalog_unregistered_type");
    let bind_ddl = "CREATE GRAPH missing_graph TYPED gt";
    let err =
        gql_execute_idempotent_as_admin_expect_err(&env, bind_ddl, "catalog_unregistered_bind");
    assert!(
        matches!(
            err,
            gleaph_graph_kernel::federation::RouterError::NotFound(_)
        ),
        "expected NotFound for unregistered graph name, got {err:?}"
    );
}

#[test]
fn catalog_drop_graph_type_cascades_typed_binding() {
    let env = install_two_graph_federation();
    let setup = format!(
        "CREATE GRAPH TYPE gt {{ {PERSON_KNOWS} }} NEXT CREATE GRAPH {GRAPH_HOME_NAME} TYPED gt"
    );
    gql_execute_idempotent_as_admin(&env, &setup, "catalog_drop_setup");
    gql_execute_idempotent_as_admin(&env, "DROP GRAPH TYPE gt", "catalog_drop_type");

    let bind_again = format!("CREATE GRAPH {GRAPH_HOME_NAME} TYPED gt");
    let err = gql_execute_idempotent_as_admin_expect_err(&env, &bind_again, "catalog_drop_rebind");
    assert!(
        matches!(
            err,
            gleaph_graph_kernel::federation::RouterError::NotFound(_)
                | gleaph_graph_kernel::federation::RouterError::InvalidArgument(_)
        ),
        "expected NotFound or InvalidArgument when rebinding to dropped type, got {err:?}"
    );
}
