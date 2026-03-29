use gleaph_gql::{
    executor::{execute_mutation, execute_plan, execute_query_statement},
    parse_statement,
    planner::build_plan,
    validate_statement,
};
use gleaph_pma::{PmaGraph, VecMemory};
use gleaph_types::Value;

// ── helpers ───────────────────────────────────────────────────────────────────

fn run_query<M: gleaph_pma::Memory + Clone>(
    g: &PmaGraph<M>,
    gql: &str,
) -> gleaph_types::QueryResult {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    let plan = build_plan(&stmt).unwrap_or_else(|e| panic!("plan '{gql}': {e}"));
    execute_plan(&plan, g).unwrap_or_else(|e| panic!("execute '{gql}': {e}"))
}

fn run_compound<M: gleaph_pma::Memory + Clone>(
    g: &PmaGraph<M>,
    gql: &str,
) -> gleaph_types::QueryResult {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    execute_query_statement(&stmt, g).unwrap_or_else(|e| panic!("execute '{gql}': {e}"))
}

fn run_mutation<M: gleaph_pma::Memory>(
    g: &mut PmaGraph<M>,
    gql: &str,
) -> gleaph_types::MutationResult {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    execute_mutation(&stmt, g).unwrap_or_else(|e| panic!("execute '{gql}': {e}"))
}

fn user(g: &mut PmaGraph<VecMemory>, name: &str) -> u32 {
    g.create_vertex(
        vec!["User".into()],
        vec![("name".into(), Value::Text(name.into()))],
    )
    .unwrap()
}

fn knows(g: &mut PmaGraph<VecMemory>, src: u32, dst: u32) {
    g.create_edge(src, dst, Some("KNOWS".into()), vec![], 1.0, 0)
        .unwrap();
}

fn new_graph() -> PmaGraph<VecMemory> {
    PmaGraph::new_with_initial_edge_capacity(VecMemory::default(), 100, 400).unwrap()
}

// ════════════════════════════════════════════════════════════════════════
// §16.4 KEEP: KEEP clause tests
// ════════════════════════════════════════════════════════════════════════

#[test]
fn keep_star_retains_all_bindings() {
    let mut g = new_graph();
    let a = user(&mut g, "Alice");
    let b = user(&mut g, "Bob");
    knows(&mut g, a, b);

    let r = run_query(
        &g,
        "MATCH (a:User)-[:KNOWS]->(b:User) KEEP * RETURN a.name, b.name",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("Alice".into()));
    assert_eq!(r.rows[0][1], Value::Text("Bob".into()));
}

#[test]
fn keep_specific_vars_restricts_scope() {
    let mut g = new_graph();
    let a = user(&mut g, "Alice");
    let b = user(&mut g, "Bob");
    knows(&mut g, a, b);

    // KEEP only `b` — `a` should become unavailable.
    let r = run_query(&g, "MATCH (a:User)-[:KNOWS]->(b:User) KEEP b RETURN b.name");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("Bob".into()));
}

#[test]
fn keep_drops_unmentioned_vars() {
    let mut g = new_graph();
    let a = user(&mut g, "Alice");
    let b = user(&mut g, "Bob");
    knows(&mut g, a, b);

    // After KEEP b, `a` is dropped — referencing it should give Null.
    let r = run_query(
        &g,
        "MATCH (a:User)-[:KNOWS]->(b:User) KEEP b RETURN b.name, a.name",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("Bob".into()));
    assert_eq!(r.rows[0][1], Value::Null);
}

#[test]
fn keep_multiple_vars() {
    let mut g = new_graph();
    let a = user(&mut g, "Alice");
    let b = user(&mut g, "Bob");
    let c = user(&mut g, "Carol");
    knows(&mut g, a, b);
    knows(&mut g, b, c);

    // Two-hop traversal, KEEP only a and c (drop b).
    let r = run_query(
        &g,
        "MATCH (a:User)-[:KNOWS]->(b:User)-[:KNOWS]->(c:User) KEEP a, c RETURN a.name, c.name, b.name",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("Alice".into()));
    assert_eq!(r.rows[0][1], Value::Text("Carol".into()));
    assert_eq!(r.rows[0][2], Value::Null);
}

// ════════════════════════════════════════════════════════════════════════
// BYTES/BINARY: BYTES/BINARY type tests
// ════════════════════════════════════════════════════════════════════════

#[test]
fn bytes_literal_parse() {
    let r = run_compound(&new_graph(), "RETURN X'48454C4C4F'");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Bytes(b"HELLO".to_vec()));
}

#[test]
fn bytes_literal_lowercase_hex() {
    let r = run_compound(&new_graph(), "RETURN x'deadbeef'");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
}

#[test]
fn bytes_empty() {
    let r = run_compound(&new_graph(), "RETURN X''");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Bytes(vec![]));
}

#[test]
fn bytes_to_hex_function() {
    let r = run_compound(&new_graph(), "RETURN to_hex(X'CAFE')");
    assert_eq!(r.rows[0][0], Value::Text("cafe".into()));
}

#[test]
fn bytes_from_hex_function() {
    let r = run_compound(&new_graph(), "RETURN from_hex('cafe')");
    assert_eq!(r.rows[0][0], Value::Bytes(vec![0xca, 0xfe]));
}

#[test]
fn bytes_from_hex_with_prefix() {
    let r = run_compound(&new_graph(), "RETURN from_hex('0xCAFE')");
    assert_eq!(r.rows[0][0], Value::Bytes(vec![0xca, 0xfe]));
}

#[test]
fn bytes_byte_length() {
    let r = run_compound(&new_graph(), "RETURN byte_length(X'010203')");
    assert_eq!(r.rows[0][0], Value::Int64(3));
}

#[test]
fn bytes_cast_to_text() {
    let r = run_compound(&new_graph(), "RETURN CAST(X'4142' AS TEXT)");
    assert_eq!(r.rows[0][0], Value::Text("4142".into()));
}

#[test]
fn bytes_cast_text_to_bytes() {
    let r = run_compound(&new_graph(), "RETURN CAST('4142' AS BYTES)");
    assert_eq!(r.rows[0][0], Value::Bytes(vec![0x41, 0x42]));
}

#[test]
fn bytes_is_type_check() {
    let r = run_compound(&new_graph(), "RETURN X'00' IS :: BYTES");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn bytes_comparison() {
    let r = run_compound(&new_graph(), "RETURN X'01' < X'02'");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn bytes_equality() {
    let r = run_compound(&new_graph(), "RETURN X'AB' = X'AB'");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn bytes_property_store_roundtrip() {
    let mut g = new_graph();
    run_mutation(&mut g, "INSERT (:Data {payload: X'DEADBEEF'})");
    let r = run_query(&g, "MATCH (n:Data) RETURN n.payload");
    assert_eq!(r.rows[0][0], Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
}

#[test]
fn bytes_tostring_function() {
    let r = run_compound(&new_graph(), "RETURN tostring(X'FF')");
    assert_eq!(r.rows[0][0], Value::Text("ff".into()));
}

#[test]
fn bytes_cast_list_to_bytes() {
    let r = run_compound(&new_graph(), "RETURN CAST([65, 66, 67] AS BYTES)");
    assert_eq!(r.rows[0][0], Value::Bytes(b"ABC".to_vec()));
}

// ════ W-D2: Structured Temporal Types (Date, Time, DateTime, Duration) ════

// ── Literal parsing ──

#[test]
fn temporal_date_literal() {
    let r = run_compound(&new_graph(), "RETURN DATE '2024-03-15'");
    assert_eq!(r.rows[0][0], Value::Date(19797));
}

#[test]
fn temporal_time_literal() {
    let r = run_compound(&new_graph(), "RETURN TIME '14:30:00'");
    assert_eq!(r.rows[0][0], Value::Time(52_200_000_000_000));
}

#[test]
fn temporal_time_literal_fractional() {
    let r = run_compound(&new_graph(), "RETURN TIME '14:30:00.5'");
    assert_eq!(r.rows[0][0], Value::Time(52_200_500_000_000));
}

#[test]
fn temporal_datetime_literal_utc() {
    let r = run_compound(&new_graph(), "RETURN DATETIME '2024-03-15T14:30:00Z'");
    let expected_secs = 19797i64 * 86400 + 14 * 3600 + 30 * 60;
    assert_eq!(r.rows[0][0], Value::DateTime(expected_secs, 0));
}

#[test]
fn temporal_datetime_literal_offset() {
    let r = run_compound(&new_graph(), "RETURN DATETIME '2024-03-15T14:30:00+09:00'");
    let utc_secs = 19797i64 * 86400 + 14 * 3600 + 30 * 60 - 9 * 3600;
    assert_eq!(r.rows[0][0], Value::DateTime(utc_secs, 0));
}

#[test]
fn temporal_duration_literal() {
    let r = run_compound(&new_graph(), "RETURN DURATION 'P1Y2M3DT4H5M6S'");
    let expected_nanos =
        3 * 86_400_000_000_000i64 + 4 * 3_600_000_000_000 + 5 * 60_000_000_000 + 6_000_000_000;
    assert_eq!(r.rows[0][0], Value::Duration(14, expected_nanos));
}

#[test]
fn temporal_duration_literal_month_only() {
    let r = run_compound(&new_graph(), "RETURN DURATION 'P1M'");
    assert_eq!(r.rows[0][0], Value::Duration(1, 0));
}

// ── Property store roundtrip ──

#[test]
fn temporal_date_property_roundtrip() {
    let mut g = new_graph();
    run_mutation(&mut g, "INSERT (:Event {day: DATE '2024-03-15'})");
    let r = run_compound(&g, "MATCH (n:Event) RETURN n.day");
    assert_eq!(r.rows[0][0], Value::Date(19797));
}

#[test]
fn temporal_time_property_roundtrip() {
    let mut g = new_graph();
    run_mutation(&mut g, "INSERT (:Event {t: TIME '08:30:00'})");
    let r = run_compound(&g, "MATCH (n:Event) RETURN n.t");
    assert_eq!(r.rows[0][0], Value::Time(30_600_000_000_000));
}

#[test]
fn temporal_datetime_property_roundtrip() {
    let mut g = new_graph();
    run_mutation(
        &mut g,
        "INSERT (:Event {dt: DATETIME '2024-03-15T14:30:00Z'})",
    );
    let r = run_compound(&g, "MATCH (n:Event) RETURN n.dt");
    let expected_secs = 19797i64 * 86400 + 14 * 3600 + 30 * 60;
    assert_eq!(r.rows[0][0], Value::DateTime(expected_secs, 0));
}

#[test]
fn temporal_duration_property_roundtrip() {
    let mut g = new_graph();
    run_mutation(&mut g, "INSERT (:Event {d: DURATION 'PT1H30M'})");
    let r = run_compound(&g, "MATCH (n:Event) RETURN n.d");
    assert_eq!(
        r.rows[0][0],
        Value::Duration(0, 3_600_000_000_000 + 30 * 60_000_000_000)
    );
}

// ── CAST ──

#[test]
fn temporal_cast_text_to_date() {
    let r = run_compound(&new_graph(), "RETURN CAST('2024-03-15' AS DATE)");
    assert_eq!(r.rows[0][0], Value::Date(19797));
}

#[test]
fn temporal_cast_date_to_text() {
    let r = run_compound(&new_graph(), "RETURN CAST(DATE '2024-03-15' AS TEXT)");
    assert_eq!(r.rows[0][0], Value::Text("2024-03-15".into()));
}

#[test]
fn temporal_cast_date_to_datetime() {
    let r = run_compound(&new_graph(), "RETURN CAST(DATE '2024-03-15' AS DATETIME)");
    assert_eq!(r.rows[0][0], Value::DateTime(19797 * 86400, 0));
}

#[test]
fn temporal_cast_datetime_to_date() {
    let r = run_compound(
        &new_graph(),
        "RETURN CAST(DATETIME '2024-03-15T14:30:00Z' AS DATE)",
    );
    assert_eq!(r.rows[0][0], Value::Date(19797));
}

#[test]
fn temporal_cast_datetime_to_time() {
    let r = run_compound(
        &new_graph(),
        "RETURN CAST(DATETIME '2024-03-15T14:30:00Z' AS TIME)",
    );
    assert_eq!(r.rows[0][0], Value::Time(52_200_000_000_000));
}

#[test]
fn temporal_cast_text_to_duration() {
    let r = run_compound(&new_graph(), "RETURN CAST('P1Y2M' AS DURATION)");
    assert_eq!(r.rows[0][0], Value::Duration(14, 0));
}

#[test]
fn temporal_cast_datetime_to_timestamp() {
    let r = run_compound(
        &new_graph(),
        "RETURN CAST(DATETIME '1970-01-01T00:00:01Z' AS TIMESTAMP)",
    );
    assert_eq!(r.rows[0][0], Value::Timestamp(1_000_000_000));
}

// ── Comparison ──

#[test]
fn temporal_date_comparison() {
    let r = run_compound(&new_graph(), "RETURN DATE '2024-01-01' < DATE '2024-12-31'");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn temporal_time_comparison() {
    let r = run_compound(&new_graph(), "RETURN TIME '08:00:00' < TIME '14:30:00'");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn temporal_datetime_comparison() {
    let r = run_compound(
        &new_graph(),
        "RETURN DATETIME '2024-01-01T00:00:00Z' < DATETIME '2024-12-31T23:59:59Z'",
    );
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

// ── Arithmetic ──

#[test]
fn temporal_date_plus_duration() {
    // Jan 31 + 1 month = Feb 29 (2024 is a leap year)
    let r = run_compound(&new_graph(), "RETURN DATE '2024-01-31' + DURATION 'P1M'");
    assert_eq!(
        r.rows[0][0],
        Value::Date(gleaph_gql::temporal::ymd_to_days(2024, 2, 29).unwrap())
    );
}

#[test]
fn temporal_date_minus_date() {
    let r = run_compound(&new_graph(), "RETURN DATE '2024-03-15' - DATE '2024-03-10'");
    assert_eq!(r.rows[0][0], Value::Duration(0, 5 * 86_400_000_000_000));
}

#[test]
fn temporal_datetime_plus_duration() {
    let r = run_compound(
        &new_graph(),
        "RETURN DATETIME '2024-03-15T12:00:00Z' + DURATION 'PT2H'",
    );
    let expected_secs = 19797i64 * 86400 + 14 * 3600;
    assert_eq!(r.rows[0][0], Value::DateTime(expected_secs, 0));
}

#[test]
fn temporal_duration_add() {
    let r = run_compound(&new_graph(), "RETURN DURATION 'P1M' + DURATION 'P2M'");
    assert_eq!(r.rows[0][0], Value::Duration(3, 0));
}

#[test]
fn temporal_duration_multiply() {
    let r = run_compound(&new_graph(), "RETURN DURATION 'PT1H' * 3");
    assert_eq!(r.rows[0][0], Value::Duration(0, 3 * 3_600_000_000_000));
}

// ── Extraction functions ──

#[test]
fn temporal_year_function() {
    let r = run_compound(&new_graph(), "RETURN year(DATE '2024-03-15')");
    assert_eq!(r.rows[0][0], Value::Int64(2024));
}

#[test]
fn temporal_month_function() {
    let r = run_compound(&new_graph(), "RETURN month(DATE '2024-03-15')");
    assert_eq!(r.rows[0][0], Value::Int64(3));
}

#[test]
fn temporal_day_function() {
    let r = run_compound(&new_graph(), "RETURN day(DATE '2024-03-15')");
    assert_eq!(r.rows[0][0], Value::Int64(15));
}

#[test]
fn temporal_hour_function() {
    let r = run_compound(&new_graph(), "RETURN hour(TIME '14:30:45')");
    assert_eq!(r.rows[0][0], Value::Int64(14));
}

#[test]
fn temporal_minute_function() {
    let r = run_compound(&new_graph(), "RETURN minute(TIME '14:30:45')");
    assert_eq!(r.rows[0][0], Value::Int64(30));
}

#[test]
fn temporal_second_function() {
    let r = run_compound(&new_graph(), "RETURN second(TIME '14:30:45')");
    assert_eq!(r.rows[0][0], Value::Int64(45));
}

// ── Construction functions ──

#[test]
fn temporal_make_date() {
    let r = run_compound(&new_graph(), "RETURN make_date(2024, 3, 15)");
    assert_eq!(r.rows[0][0], Value::Date(19797));
}

#[test]
fn temporal_make_time() {
    let r = run_compound(&new_graph(), "RETURN make_time(14, 30, 0)");
    assert_eq!(r.rows[0][0], Value::Time(52_200_000_000_000));
}

// ── Epoch conversion ──

#[test]
fn temporal_to_epoch_millis() {
    let r = run_compound(
        &new_graph(),
        "RETURN to_epoch_millis(DATETIME '1970-01-01T00:00:01Z')",
    );
    assert_eq!(r.rows[0][0], Value::Int64(1000));
}

#[test]
fn temporal_from_epoch_millis() {
    let r = run_compound(&new_graph(), "RETURN from_epoch_millis(1000)");
    assert_eq!(r.rows[0][0], Value::DateTime(1, 0));
}

// ── IS :: type ──

#[test]
fn temporal_is_type_date() {
    let r = run_compound(&new_graph(), "RETURN DATE '2024-01-01' IS :: DATE");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn temporal_is_type_time() {
    let r = run_compound(&new_graph(), "RETURN TIME '12:00:00' IS :: TIME");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn temporal_is_type_datetime() {
    let r = run_compound(
        &new_graph(),
        "RETURN DATETIME '2024-01-01T00:00:00Z' IS :: DATETIME",
    );
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn temporal_is_type_duration() {
    let r = run_compound(&new_graph(), "RETURN DURATION 'P1M' IS :: DURATION");
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

// ── tostring ──

#[test]
fn temporal_tostring_date() {
    let r = run_compound(&new_graph(), "RETURN tostring(DATE '2024-03-15')");
    assert_eq!(r.rows[0][0], Value::Text("2024-03-15".into()));
}

#[test]
fn temporal_tostring_duration() {
    let r = run_compound(&new_graph(), "RETURN tostring(DURATION 'PT1H30M')");
    assert_eq!(r.rows[0][0], Value::Text("PT1H30M".into()));
}

// ── Extraction from DateTime ──

#[test]
fn temporal_year_from_datetime() {
    let r = run_compound(&new_graph(), "RETURN year(DATETIME '2024-03-15T14:30:00Z')");
    assert_eq!(r.rows[0][0], Value::Int64(2024));
}

#[test]
fn temporal_hour_from_datetime() {
    let r = run_compound(&new_graph(), "RETURN hour(DATETIME '2024-03-15T14:30:00Z')");
    assert_eq!(r.rows[0][0], Value::Int64(14));
}
