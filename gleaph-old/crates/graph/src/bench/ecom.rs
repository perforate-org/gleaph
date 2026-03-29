use canbench_rs::bench;
use gleaph_gql::{
    parse_statement, planner::build_plan_with_stats, stats::TableStats, validate_statement,
};
use gleaph_types::Value;
use gleaph_types::{EntityType, IndexType};
use ic_cdk_macros::update;

use crate::state::{restore_state_uncertified, with_state, with_state_mut};

// Benchmark scale — kept small enough for PocketIC's ~500 MB blob transfer
// limit (canbench loads stable_memory.bin via set_stable_memory internally).
// 300K vertices + ~700K edges → ~200 MB stable memory.
const ECOM_NUM_USERS: u32 = 50_000;
const ECOM_NUM_PRODUCTS: u32 = 50_000;
const ECOM_INTERACTIONS_PER_USER: u32 = 10;
/// Each user's 10 interactions: k%3==0 → purchase (via Order), so 4 orders per user (k=0,3,6,9).
const ECOM_ORDERS_PER_USER: u32 = 4;
const ECOM_NUM_ORDERS: u32 = ECOM_NUM_USERS * ECOM_ORDERS_PER_USER; // 200K

/// Base timestamp (nanoseconds) representing "now" in the benchmark dataset.
/// Fixed to a deterministic value (~March 2026).
const ECOM_TS_BASE_NS: u64 = 1_772_000_000_000_000_000;

/// Time window over which edge timestamps are distributed (30 days in nanoseconds).
const ECOM_TS_WINDOW_NS: u64 = 30 * 24 * 3600 * 1_000_000_000;

/// Cutoff for "recent" interactions = last 10% of the window (~3 days).
const ECOM_RECENT_CUTOFF: u64 = ECOM_TS_BASE_NS - ECOM_TS_WINDOW_NS / 10;
const ECOM_QUERY_MAX_GROUPS: usize = 1_000_000;
const ECOM_QUERY_MAX_EXECUTION_STEPS: u64 = 10_000_000_000;

// ---------------------------------------------------------------------------
// Setup endpoints — called by the PocketIC snapshot generator
// ---------------------------------------------------------------------------

/// Create User vertices in batches via `batch_mutate_tracked`.
///
/// Called by the PocketIC snapshot generator with (start, end) ranges
/// to stay within the per-message instruction limit.
#[update]
fn bench_setup_ecom_users(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|u| {
            let tier = match u % 10 {
                0 => 2,
                1..=3 => 1,
                _ => 0,
            };
            format!("INSERT (:User {{id: {u}, tier: {tier}}})")
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE User {}: {e}", start + i as u32));
    }
}

/// Create Product vertices in batches via `batch_mutate_tracked`.
#[update]
fn bench_setup_ecom_products(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|p| {
            let price = (p.wrapping_mul(17).wrapping_add(10)) % 490 + 10;
            let category = p % 5;
            format!("INSERT (:Product {{id: {p}, price: {price}, category: {category}}})")
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE Product {}: {e}", start + i as u32));
    }
}

/// Create Order vertices in batches via `batch_mutate_tracked`.
///
/// Called after users and products, before edges.
/// Each user has `ECOM_ORDERS_PER_USER` orders.
#[update]
fn bench_setup_ecom_orders(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|o| format!("INSERT (:Order {{id: {o}}})"))
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE Order {}: {e}", start + i as u32));
    }
}

/// Splitmix64-based hash — strong mixing of both user and interaction index.
///
/// The previous hash `u * 2654435761 + k * 1000003 ^ (k >> 3)` had weak
/// k-mixing (XOR only affected 3 bits), causing 95% of users to interact
/// with only 1 unique product across all 10 interactions.
fn ecom_hash(u: u32, k: u32) -> u64 {
    let mut x = (u as u64)
        .wrapping_mul(31)
        .wrapping_add((k as u64).wrapping_mul(7919));
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Monotonically increasing timestamp for edge (a, k).
/// Maps k linearly into [BASE - WINDOW, BASE].  Because edges are inserted
/// in k-order per vertex, this matches production behaviour where insertion
/// order equals timestamp order.
fn ecom_timestamp(_a: u32, k: u32) -> u64 {
    let max_k = (ECOM_INTERACTIONS_PER_USER - 1) as u64; // 0..9
    let k_clamped = (k as u64).min(max_k);
    ECOM_TS_BASE_NS - ECOM_TS_WINDOW_NS + k_clamped * ECOM_TS_WINDOW_NS / max_k
}

/// Cubic distribution for product selection — realistic Pareto-like skew.
///
/// Maps a uniform hash to a product ID via `p = r³ / N²`, giving:
/// - Top 1% products → ~22% of edges
/// - Top 20% products → ~63% of edges
/// - ~61% of products receive ≥1 edge
///
/// The previous `num_products / (r + 1)` was extremely heavy-tailed:
/// product 0 alone received 50% of all edges, and only 0.9% of products
/// had any edges at all.
fn ecom_product(h: u64, num_products: u32) -> u32 {
    let n = num_products as u64;
    let r = h % n;
    let p = r.wrapping_mul(r).wrapping_mul(r) / (n * n);
    (p as u32).min(num_products - 1)
}

/// Create interaction edges via low-level PMA API for the specified user range.
///
/// Uses the Order vertex model:
/// - k%3==0 (purchase): User -[:Placed]-> Order -[:Contains]-> Product
/// - k%3==1 (cart):     User -[:Carted]-> Product (direct edge)
/// - k%3==2 (fav):      User -[:Favorited]-> Product (direct edge)
///
/// Carted/Favorited are deduped per `(product, label)` per user.
/// Purchases go through intermediate Order vertices — no product dedup
/// (same product can be purchased multiple times via separate Orders).
#[update]
fn bench_setup_ecom_edges(start_user: u32, end_user: u32) {
    let num_products = ECOM_NUM_PRODUCTS;
    let interactions_per_user = ECOM_INTERACTIONS_PER_USER;
    with_state_mut(|g| {
        // Vertex ID layout (all created before this function):
        // base + 0..NUM_USERS                              → Users
        // base + NUM_USERS..NUM_USERS+NUM_PRODUCTS         → Products
        // base + NUM_USERS+NUM_PRODUCTS..+NUM_ORDERS       → Orders
        let base_vertex = g.vertex_count().saturating_sub(u64::from(
            ECOM_NUM_USERS + ECOM_NUM_PRODUCTS + ECOM_NUM_ORDERS,
        )) as u32;
        let product_base = base_vertex + ECOM_NUM_USERS;
        let order_base = product_base + num_products;

        for u in start_user..end_user {
            let mut carted_products = Vec::<u32>::with_capacity(4);
            let mut favorited_products = Vec::<u32>::with_capacity(4);
            let mut bought_idx: u32 = 0;

            for k in 0..interactions_per_user {
                let h = ecom_hash(u, k);
                let p = ecom_product(h, num_products);
                let ts = ecom_timestamp(u, k);
                let user_v = base_vertex + u;
                let product_v = product_base + p;

                match k % 3 {
                    0 => {
                        // Purchase via intermediate Order vertex.
                        let order_v = order_base + u * ECOM_ORDERS_PER_USER + bought_idx;
                        bought_idx += 1;
                        g.create_edge(user_v, order_v, Some("Placed".into()), vec![], 1.0, ts)
                            .unwrap_or_else(|e| panic!("Placed {user_v}->{order_v}: {e}"));
                        g.create_edge(order_v, product_v, Some("Contains".into()), vec![], 1.0, ts)
                            .unwrap_or_else(|e| panic!("Contains {order_v}->{product_v}: {e}"));
                    }
                    1 => {
                        // Carted: direct edge, dedup on (product, "Carted") per user.
                        if !carted_products.contains(&p) {
                            carted_products.push(p);
                            g.create_edge(
                                user_v,
                                product_v,
                                Some("Carted".into()),
                                vec![],
                                1.0,
                                ts,
                            )
                            .unwrap_or_else(|e| panic!("Carted {user_v}->{product_v}: {e}"));
                        }
                    }
                    _ => {
                        // Favorited: direct edge, dedup on (product, "Favorited") per user.
                        // Same product with "Carted" label is allowed ((src,dst,label) uniqueness).
                        if !favorited_products.contains(&p) {
                            favorited_products.push(p);
                            g.create_edge(
                                user_v,
                                product_v,
                                Some("Favorited".into()),
                                vec![],
                                1.0,
                                ts,
                            )
                            .unwrap_or_else(|e| panic!("Favorited {user_v}->{product_v}: {e}"));
                        }
                    }
                }
            }
        }
    });
}

/// Register the ecom graph type schema and create secondary indexes.
///
/// Uses PMA's `create_index` directly instead of `api::create_index` to avoid
/// allocating the stable ABP secondary-index region.  This keeps
/// `persist_state_metadata` (called during `pre_upgrade`) lightweight —
/// it only serializes the overlay without rebuilding any ABP trees.
///
/// At query time, `scan_vertices_by_property_eq_auto` falls back to the
/// in-memory equality index, which is fully populated from the overlay's
/// `property_indexes` + `backfill_vertex_equality_index` during restore.
#[update]
fn bench_setup_ecom_indexes() {
    // Register graph type schema for the ecom benchmark (§18.3 inline edge types).
    crate::gql_bridge::mutate(
        "CREATE GRAPH TYPE EcomType { \
           (:User), (:Product), (:Order), \
           (:User)-[:Placed]->(:Order), \
           (:Order)-[:Contains]->(:Product), \
           (:User)-[:Carted]->(:Product), \
           (:User)-[:Favorited]->(:Product) \
         }",
    )
    .expect("setup: CREATE GRAPH TYPE EcomType failed");

    crate::state::with_state_mut(|g| {
        g.create_index(
            gleaph_types::EntityType::Vertex,
            "id".into(),
            gleaph_types::IndexType::Equality,
        )
        .expect("setup: create_index(id) failed");
        g.create_index(
            gleaph_types::EntityType::Vertex,
            "tier".into(),
            gleaph_types::IndexType::Equality,
        )
        .expect("setup: create_index(tier) failed");
    });
}

/// Persist the overlay (header + vertex labels/props + runtime metadata) to
/// stable memory without triggering a full canister upgrade.
#[update]
fn bench_persist_overlay() {
    crate::state::with_state_mut(|g| g.compute_property_selectivity());
    crate::state::persist_overlay_only().expect("persist overlay");
}

fn ecom_query_text(name: &str) -> Option<String> {
    match name {
        "collab_filter" => Some(
            "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(t:Product {id: 5}) \
             WITH u LIMIT 200 \
             MATCH (u)-[:Placed]->(:Order)-[:Contains]->(rec:Product) \
             WHERE rec.id <> 5 \
             RETURN rec.id, COUNT(*) AS rec_score \
             ORDER BY rec_score DESC LIMIT 10"
                .into(),
        ),
        "trending_products" => Some(
            "MATCH (u:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN p.id, COUNT(*) AS popularity \
             ORDER BY popularity DESC LIMIT 10"
                .into(),
        ),
        "co_purchase" => Some(
            "MATCH (:User {id: 42})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             WITH DISTINCT p \
             MATCH (p)<-[:Contains]-(:Order)<-[:Placed]-(other:User) \
             WHERE other.id <> 42 \
             RETURN other.id, COUNT(DISTINCT p) AS shared \
             ORDER BY shared DESC LIMIT 10"
                .into(),
        ),
        "buyer_segment" => Some(
            "MATCH (p:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User) \
             RETURN u.tier, COUNT(*) AS cnt \
             ORDER BY cnt DESC"
                .into(),
        ),
        "vip_popular_products" => Some(format!(
            "MATCH (u:User {{tier: 2}})-[e:Placed]->(:Order)-[:Contains]->(p:Product) \
             WHERE gleaph_timestamp(e) > {ECOM_RECENT_CUTOFF} \
             RETURN p.id, COUNT(*) AS vip_score \
             ORDER BY vip_score DESC LIMIT 5"
        )),
        "cart_abandonment" => Some(
            "MATCH (u:User)-[:Carted]->(p:Product {id: 5}) \
             OPTIONAL MATCH (u)-[:Placed]->(o:Order)-[:Contains]->(p) \
             WHERE o IS NULL \
             RETURN u.id \
             LIMIT 20"
                .into(),
        ),
        "user_activity" => Some(
            "MATCH (:User {id: 42})-[e:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts \
             UNION ALL \
             MATCH (:User {id: 42})-[e:Carted]->(p:Product) \
             RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts \
             UNION ALL \
             MATCH (:User {id: 42})-[e:Favorited]->(p:Product) \
             RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts"
                .into(),
        ),
        "category_revenue" => Some(
            "MATCH (:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN p.category, COUNT(*) AS purchases, SUM(p.price) AS revenue, AVG(p.price) AS avg_price \
             ORDER BY revenue DESC"
                .into(),
        ),
        "cross_sell" => Some(
            "MATCH (:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User)-[:Favorited]->(rec:Product) \
             WHERE rec.id <> 5 \
             RETURN rec.id, COUNT(DISTINCT u) AS buyer_favorites \
             ORDER BY buyer_favorites DESC LIMIT 10"
                .into(),
        ),
        "multi_touch" => Some(
            "MATCH (u:User)-[e:Carted|Favorited]->(p:Product {id: 5}) \
             RETURN u.id, COUNT(*) AS touch_count \
             ORDER BY touch_count DESC LIMIT 20"
                .into(),
        ),
        "high_value_buyers" => Some(
            "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN u.id, COUNT(*) AS purchases, \
               SUM(CASE WHEN p.price > 300 THEN 1 ELSE 0 END) AS premium_count, \
               SUM(p.price) AS total_spent \
             ORDER BY total_spent DESC LIMIT 10"
                .into(),
        ),
        "wishlist_hot" => Some(
            "MATCH (u:User {tier: 2})-[:Favorited]->(p:Product) \
             RETURN u.id, COLLECT(p.id) AS favorites, COUNT(*) AS fav_count \
             ORDER BY fav_count DESC LIMIT 10"
                .into(),
        ),
        "shortest_path" => Some(
            "MATCH SHORTEST p = (u:User {id: 42})-[*1..4]->(target:Product {id: 4364}) \
             RETURN p"
                .into(),
        ),
        _ => None,
    }
}

#[inline]
fn run_ecom_query(gql: &str) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    // Benchmark path: disable row cap and raise execution/group guardrails so
    // instruction deltas reflect query shape instead of framework defaults.
    run_ecom_query_limited(gql, None, Some(ECOM_QUERY_MAX_EXECUTION_STEPS))
}

fn run_ecom_query_limited(
    gql: &str,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    struct MaxGroupsGuard;
    impl Drop for MaxGroupsGuard {
        fn drop(&mut self) {
            gleaph_gql::executor::set_max_groups_override(None);
        }
    }

    gleaph_gql::executor::set_max_groups_override(Some(ECOM_QUERY_MAX_GROUPS));
    let _guard = MaxGroupsGuard;
    crate::gql_bridge::query_with_limits(gql, max_rows.map(|v| v as usize), max_steps)
}

fn run_ecom_query_profiled_limited(
    gql: &str,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
) -> Result<(gleaph_types::QueryResult, Vec<(String, u64)>), gleaph_types::GleaphError> {
    struct MaxGroupsGuard;
    impl Drop for MaxGroupsGuard {
        fn drop(&mut self) {
            gleaph_gql::executor::set_max_groups_override(None);
        }
    }

    gleaph_gql::executor::set_max_groups_override(Some(ECOM_QUERY_MAX_GROUPS));
    let _guard = MaxGroupsGuard;
    crate::gql_bridge::query_with_limits_profiled(gql, max_rows.map(|v| v as usize), max_steps)
}

#[inline]
fn restore_and_warm_planner_stats() {
    restore_state_uncertified().unwrap();
    // Selectivity data is fresh from the snapshot; counter is reset to 0 on
    // restore so no pre-warmup is needed.  This lets the benchmark include the
    // same cold-start cost that production queries would see.
}

fn assert_within_ic_limits(result: &canbench_rs::BenchResult, name: &str, is_mutation: bool) {
    let limit = if is_mutation {
        crate::gql_bridge::IC_UPDATE_INSTRUCTION_LIMIT
    } else {
        crate::gql_bridge::IC_QUERY_INSTRUCTION_LIMIT
    };
    assert!(
        result.total.instructions <= limit,
        "{name}: {ins} instructions exceeds IC {kind} limit {limit}",
        ins = result.total.instructions,
        kind = if is_mutation { "update" } else { "query" },
    );
}

#[inline]
fn require_non_empty_rows(
    result: Result<gleaph_types::QueryResult, gleaph_types::GleaphError>,
    bench_name: &str,
) -> gleaph_types::QueryResult {
    let qr = result.unwrap_or_else(|e| panic!("{bench_name}: query failed: {e}"));
    assert!(
        !qr.rows.is_empty(),
        "{bench_name}: query unexpectedly returned no rows (scanned_v={} scanned_e={} exec_steps={})",
        qr.stats.scanned_vertices,
        qr.stats.scanned_edges,
        qr.stats.execution_steps
    );
    qr
}

fn planner_stats_for_explain() -> TableStats {
    with_state_mut(|g| {
        let _ = g.refresh_selectivity_if_stale_with_flag();
        let vertex_count = g.vertex_count();
        let edge_count = g.edge_count();
        let mut stats = TableStats {
            vertex_count,
            edge_count,
            avg_degree: if vertex_count == 0 {
                1.0
            } else {
                (edge_count as f64 / vertex_count as f64).max(1.0)
            },
            label_cardinality: g.label_cardinalities(),
            ..TableStats::default()
        };
        for (key, &sel) in g.get_property_selectivity() {
            stats.property_selectivity.insert(key.clone(), sel);
        }
        for idx in g.list_property_indexes() {
            if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
                stats
                    .indexed_vertex_properties
                    .insert(idx.property_name.clone());
                stats
                    .property_selectivity
                    .entry(format!("vertex:{}", idx.property_name))
                    .or_insert(0.1);
            }
            if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Range {
                stats
                    .range_indexed_vertex_properties
                    .insert(idx.property_name.clone());
            }
            if idx.entity_type == EntityType::Edge && idx.index_type == IndexType::Equality {
                stats
                    .indexed_edge_properties
                    .insert(idx.property_name.clone());
                stats
                    .property_selectivity
                    .entry(format!("edge:{}", idx.property_name))
                    .or_insert(0.1);
            }
        }
        stats
    })
}

fn explain_ecom_query(gql: &str) -> Result<String, gleaph_types::GleaphError> {
    let stmt = parse_statement(gql)?;
    validate_statement(&stmt)?;
    let stats = planner_stats_for_explain();
    let plan = build_plan_with_stats(&stmt, Some(&stats))?;
    let ann = &plan.annotations;
    let chosen_anchor = ann.chosen_anchor.clone().unwrap_or_else(|| "-".into());
    let card_source = ann
        .estimated_cardinality_source
        .clone()
        .unwrap_or_else(|| "-".into());
    let est_rows = ann.estimated_rows.unwrap_or(0.0);
    let est_instr = ann.estimated_instructions.unwrap_or(0.0);
    let filter_stages = ann.filter_pushdown_stages.clone().unwrap_or_default();
    let join_order = ann.join_order.clone().unwrap_or_default();
    let ops = plan
        .ops
        .iter()
        .map(|op| format!("{op:?}"))
        .collect::<Vec<_>>()
        .join(" -> ");
    let cond = ann
        .conditional_scan
        .as_ref()
        .map(|c| {
            let descs: Vec<_> = c
                .candidates
                .iter()
                .map(|cand| {
                    let op = match cand.cmp_op {
                        gleaph_gql::plan::ConditionalCmpOp::Eq => "=",
                        gleaph_gql::plan::ConditionalCmpOp::Ge => ">=",
                        gleaph_gql::plan::ConditionalCmpOp::Gt => ">",
                        gleaph_gql::plan::ConditionalCmpOp::Le => "<=",
                        gleaph_gql::plan::ConditionalCmpOp::Lt => "<",
                    };
                    format!(
                        "${}/{op}{}({})",
                        cand.param_name, cand.property, cand.variable
                    )
                })
                .collect();
            format!(" conditional_scan=[{}]", descs.join(", "))
        })
        .unwrap_or_default();
    Ok(format!(
        "ops=[{ops}] chosen_anchor={chosen_anchor} card_source={card_source} est_rows={est_rows:.1} est_instr={est_instr:.1} limit_pushdown={} join_order={join_order:?} filter_stages={filter_stages:?}{cond}",
        ann.limit_pushdown_applied,
    ))
}

fn index_state_lines() -> Vec<String> {
    fn entity_name(v: EntityType) -> &'static str {
        match v {
            EntityType::Vertex => "vertex",
            EntityType::Edge => "edge",
        }
    }
    fn index_name(v: IndexType) -> &'static str {
        match v {
            IndexType::Equality => "eq",
            IndexType::Range => "range",
        }
    }
    with_state(|g| {
        let user_count = g.scan_vertices_by_label("User").len();
        let product_count = g.scan_vertices_by_label("Product").len();
        let order_count = g.scan_vertices_by_label("Order").len();
        let mut indexes = g
            .list_property_indexes()
            .into_iter()
            .map(|idx| {
                format!(
                    "{}:{}:{}",
                    entity_name(idx.entity_type),
                    index_name(idx.index_type),
                    idx.property_name
                )
            })
            .collect::<Vec<_>>();
        indexes.sort();
        let id_42_hits = g
            .scan_vertices_by_property_eq_live("id", &Value::Int64(42))
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "none(index-missing)".into());
        let id_5_hits = g
            .scan_vertices_by_property_eq_live("id", &Value::Int64(5))
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "none(index-missing)".into());
        let tier_2_hits = g
            .scan_vertices_by_property_eq_live("tier", &Value::Int64(2))
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "none(index-missing)".into());
        let mut out = vec![
            format!(
                "vertex_count={} edge_count={}",
                g.vertex_count(),
                g.edge_count()
            ),
            format!(
                "label_user={} label_product={} label_order={}",
                user_count, product_count, order_count
            ),
            format!(
                "index_hits id=42:{} id=5:{} tier=2:{}",
                id_42_hits, id_5_hits, tier_2_hits
            ),
            format!("index_count={}", indexes.len()),
        ];
        out.extend(indexes.into_iter().map(|line| format!("index={line}")));
        out
    })
}

fn ecom_probe_pipeline(name: &str) -> Option<Vec<(&'static str, String)>> {
    match name {
        "collab_filter" => Some(vec![
            (
                "seed_product_exists",
                "MATCH (t:Product {id: 5}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "first_hop_any_buyer",
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(t:Product {id: 5}) \
                 RETURN u.id LIMIT 1"
                    .into(),
            ),
            (
                "first_hop_buyers_limit_200",
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(t:Product {id: 5}) \
                 WITH u LIMIT 200 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "second_hop_any_candidate",
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(t:Product {id: 5}) \
                 WITH u LIMIT 200 \
                 MATCH (u)-[:Placed]->(:Order)-[:Contains]->(rec:Product) \
                 WHERE rec.id <> 5 \
                 RETURN rec.id LIMIT 1"
                    .into(),
            ),
            (
                "final",
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(t:Product {id: 5}) \
                 WITH u LIMIT 200 \
                 MATCH (u)-[:Placed]->(:Order)-[:Contains]->(rec:Product) \
                 WHERE rec.id <> 5 \
                 RETURN rec.id, COUNT(*) AS rec_score \
                 ORDER BY rec_score DESC LIMIT 10"
                    .into(),
            ),
        ]),
        "trending_products" => Some(vec![
            (
                "tier2_users_count",
                "MATCH (u:User {tier: 2}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                "MATCH (u:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN p.id, COUNT(*) AS popularity \
                 ORDER BY popularity DESC LIMIT 10"
                    .into(),
            ),
        ]),
        "co_purchase" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (u:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "seed_user_orders",
                "MATCH (:User {id: 42})-[:Placed]->(o:Order) RETURN COUNT(*) AS c".into(),
            ),
            (
                "seed_user_purchased_products",
                "MATCH (:User {id: 42})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "distinct_products",
                "MATCH (:User {id: 42})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN COUNT(DISTINCT p) AS c"
                    .into(),
            ),
            (
                "final",
                "MATCH (:User {id: 42})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 WITH DISTINCT p \
                 MATCH (p)<-[:Contains]-(:Order)<-[:Placed]-(other:User) \
                 WHERE other.id <> 42 \
                 RETURN other.id, COUNT(DISTINCT p) AS shared \
                 ORDER BY shared DESC LIMIT 10"
                    .into(),
            ),
        ]),
        "buyer_segment" => Some(vec![
            (
                "seed_product_exists",
                "MATCH (p:Product {id: 5}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "buyers_via_orders",
                "MATCH (p:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User) \
                 RETURN u.id LIMIT 1"
                    .into(),
            ),
            (
                "buyers_count",
                "MATCH (p:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "final",
                "MATCH (p:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User) \
                 RETURN u.tier, COUNT(*) AS cnt \
                 ORDER BY cnt DESC"
                    .into(),
            ),
        ]),
        "vip_popular_products" => Some(vec![
            (
                "vip_users_count",
                "MATCH (u:User {tier: 2}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "vip_recent_purchases_count",
                format!(
                    "MATCH (u:User {{tier: 2}})-[e:Placed]->(:Order)-[:Contains]->(:Product) \
                     WHERE gleaph_timestamp(e) > {ECOM_RECENT_CUTOFF} \
                     RETURN COUNT(*) AS c"
                ),
            ),
            (
                "final",
                format!(
                    "MATCH (u:User {{tier: 2}})-[e:Placed]->(:Order)-[:Contains]->(p:Product) \
                     WHERE gleaph_timestamp(e) > {ECOM_RECENT_CUTOFF} \
                     RETURN p.id, COUNT(*) AS vip_score \
                     ORDER BY vip_score DESC LIMIT 5"
                ),
            ),
        ]),
        "cart_abandonment" => Some(vec![
            (
                "seed_product_exists",
                "MATCH (p:Product {id: 5}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "carted_users",
                "MATCH (u:User)-[:Carted]->(p:Product {id: 5}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                "MATCH (u:User)-[:Carted]->(p:Product {id: 5}) \
                 OPTIONAL MATCH (u)-[:Placed]->(o:Order)-[:Contains]->(p) \
                 WHERE o IS NULL \
                 RETURN u.id \
                 LIMIT 20"
                    .into(),
            ),
        ]),
        "user_activity" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (u:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "purchases",
                "MATCH (:User {id: 42})-[e:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "final",
                "MATCH (:User {id: 42})-[e:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts \
                 UNION ALL \
                 MATCH (:User {id: 42})-[e:Carted]->(p:Product) \
                 RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts \
                 UNION ALL \
                 MATCH (:User {id: 42})-[e:Favorited]->(p:Product) \
                 RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts"
                    .into(),
            ),
        ]),
        "category_revenue" => Some(vec![
            (
                "tier2_users_count",
                "MATCH (u:User {tier: 2}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "tier2_purchases",
                "MATCH (:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "final",
                "MATCH (:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN p.category, COUNT(*) AS purchases, SUM(p.price) AS revenue, AVG(p.price) AS avg_price \
                 ORDER BY revenue DESC"
                    .into(),
            ),
        ]),
        "cross_sell" => Some(vec![
            (
                "seed_product_buyers",
                "MATCH (:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "buyer_favorites",
                "MATCH (:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User)-[:Favorited]->(rec:Product) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "final",
                "MATCH (:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User)-[:Favorited]->(rec:Product) \
                 WHERE rec.id <> 5 \
                 RETURN rec.id, COUNT(DISTINCT u) AS buyer_favorites \
                 ORDER BY buyer_favorites DESC LIMIT 10"
                    .into(),
            ),
        ]),
        "multi_touch" => Some(vec![
            (
                "seed_product_exists",
                "MATCH (p:Product {id: 5}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "carted_count",
                "MATCH (u:User)-[:Carted]->(p:Product {id: 5}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                "MATCH (u:User)-[e:Carted|Favorited]->(p:Product {id: 5}) \
                 RETURN u.id, COUNT(*) AS touch_count \
                 ORDER BY touch_count DESC LIMIT 20"
                    .into(),
            ),
        ]),
        "high_value_buyers" => Some(vec![
            (
                "total_purchase_edges",
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "final",
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(p:Product) \
                 RETURN u.id, COUNT(*) AS purchases, \
                   SUM(CASE WHEN p.price > 300 THEN 1 ELSE 0 END) AS premium_count, \
                   SUM(p.price) AS total_spent \
                 ORDER BY total_spent DESC LIMIT 10"
                    .into(),
            ),
        ]),
        "wishlist_hot" => Some(vec![
            (
                "tier2_favorites",
                "MATCH (u:User {tier: 2})-[:Favorited]->(p:Product) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "final",
                "MATCH (u:User {tier: 2})-[:Favorited]->(p:Product) \
                 RETURN u.id, COLLECT(p.id) AS favorites, COUNT(*) AS fav_count \
                 ORDER BY fav_count DESC LIMIT 10"
                    .into(),
            ),
        ]),
        "shortest_path" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (u:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "seed_product_exists",
                "MATCH (p:Product {id: 4364}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                "MATCH SHORTEST p = (u:User {id: 42})-[*1..4]->(target:Product {id: 4364}) \
                 RETURN p"
                    .into(),
            ),
        ]),
        _ => None,
    }
}

fn first_scalar_text(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int64(i) => i.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Timestamp(ts) => ts.to_string(),
        Value::List(xs) => format!("list(len={})", xs.len()),
        Value::Path(p) => format!("path(len={})", p.len()),
        Value::Bytes(b) => format!("bytes(len={})", b.len()),
        Value::Date(d) => format!("date({d})"),
        Value::Time(t) => format!("time({t})"),
        Value::DateTime(secs, nanos) => format!("datetime({secs},{nanos})"),
        Value::Duration(months, nanos) => format!("duration({months},{nanos})"),
        Value::Principal(p) => format!("principal({p})"),
        other => format!("{other:?}"),
    }
}

fn summarize_probe_step(step: &str, result: &gleaph_types::QueryResult) -> String {
    let scalar = result
        .rows
        .first()
        .and_then(|r| r.first())
        .map(first_scalar_text)
        .unwrap_or_else(|| "-".into());
    let stats = &result.stats;
    let b = &stats.breakdown;
    format!(
        "{step}: rows={} first={} scanned_v={} scanned_e={} steps={} rows_after_match={} rows_after_with={} rows_before_projection={} groups={} full_sort={} top_k={} limit_truncate={} index_used={} agg_used={} selectivity_refresh={}",
        result.rows.len(),
        scalar,
        stats.scanned_vertices,
        stats.scanned_edges,
        stats.execution_steps,
        b.rows_after_match,
        b.rows_after_with,
        b.rows_before_projection,
        b.groups_formed,
        b.full_sort_calls,
        b.top_k_calls,
        b.limit_truncate_calls,
        b.index_fast_path_used,
        b.aggregate_fast_path_used,
        b.selectivity_refresh_ran,
    )
}

#[update]
/// Debug endpoint for e-commerce benchmark queries.
///
/// Returns `QueryResult.stats.breakdown` so you can inspect where execution
/// expanded (MATCH/WITH/projection) and whether fast-paths were used.
///
/// `name`:
/// - collab_filter
/// - trending_products
/// - co_purchase
/// - buyer_segment
/// - vip_popular_products
/// - cart_abandonment
/// - user_activity
/// - category_revenue
/// - cross_sell
/// - multi_touch
/// - high_value_buyers
/// - wishlist_hot
/// - shortest_path
///
/// `restore`:
/// - true: run from the stable snapshot baseline each call
/// - false: run against current in-memory state
fn bench_ecom_probe_query(
    name: String,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = ecom_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown ecom probe name '{name}'"))
    })?;
    run_ecom_query(&gql)
}

#[update]
/// Executes arbitrary diagnostic GQL for e-commerce benchmark debugging.
///
/// Uses the same execution path as benchmark probes (`query_paged`) so heavy
/// read queries can be inspected under update-message instruction limits.
fn bench_ecom_probe_gql(
    gql: String,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    run_ecom_query(&gql)
}

#[update]
/// Executes arbitrary diagnostic GQL with custom runtime limits.
///
/// Use this to inspect where row/step caps trigger for heavy benchmark queries.
///
/// - `max_rows = null` disables row-count cap
/// - `max_steps = null` uses the default bridge cap
fn bench_ecom_probe_gql_limited(
    gql: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    run_ecom_query_limited(&gql, max_rows, max_steps)
}

#[update]
/// Executes a named benchmark query with custom runtime limits.
///
/// - `max_rows = null` disables row-count cap
/// - `max_steps = null` uses the default bridge cap
fn bench_ecom_probe_query_limited(
    name: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = ecom_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown ecom probe name '{name}'"))
    })?;
    run_ecom_query_limited(&gql, max_rows, max_steps)
}

#[update]
/// Executes a named benchmark query and returns stage-level instruction deltas.
///
/// This helps identify whether bottlenecks live in parsing/planning or in
/// executor runtime.
fn bench_ecom_probe_query_profiled(
    name: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = ecom_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown ecom probe name '{name}'"))
    })?;
    let (result, stages) = run_ecom_query_profiled_limited(&gql, max_rows, max_steps)?;
    let mut out: Vec<String> = stages
        .into_iter()
        .map(|(stage, ins)| format!("stage={stage} instructions={ins}"))
        .collect();
    out.push(format!(
        "result: rows={} scanned_v={} scanned_e={} exec_steps={} rows_after_match={} rows_after_with={} rows_before_projection={} groups={} full_sort={} top_k={} limit_truncate={} compiled_fast={}",
        result.rows.len(),
        result.stats.scanned_vertices,
        result.stats.scanned_edges,
        result.stats.execution_steps,
        result.stats.breakdown.rows_after_match,
        result.stats.breakdown.rows_after_with,
        result.stats.breakdown.rows_before_projection,
        result.stats.breakdown.groups_formed,
        result.stats.breakdown.full_sort_calls,
        result.stats.breakdown.top_k_calls,
        result.stats.breakdown.limit_truncate_calls,
        result.stats.breakdown.aggregate_compiled_fast_path_used,
    ));
    Ok(out)
}

#[update]
/// Executes arbitrary GQL and returns stage-level instruction deltas.
fn bench_ecom_probe_gql_profiled(
    gql: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let (result, stages) = run_ecom_query_profiled_limited(&gql, max_rows, max_steps)?;
    let mut out: Vec<String> = stages
        .into_iter()
        .map(|(stage, ins)| format!("stage={stage} instructions={ins}"))
        .collect();
    out.push(format!(
        "result: rows={} scanned_v={} scanned_e={} exec_steps={} rows_after_match={} rows_after_with={} rows_before_projection={} groups={} full_sort={} top_k={} limit_truncate={} compiled_fast={}",
        result.rows.len(),
        result.stats.scanned_vertices,
        result.stats.scanned_edges,
        result.stats.execution_steps,
        result.stats.breakdown.rows_after_match,
        result.stats.breakdown.rows_after_with,
        result.stats.breakdown.rows_before_projection,
        result.stats.breakdown.groups_formed,
        result.stats.breakdown.full_sort_calls,
        result.stats.breakdown.top_k_calls,
        result.stats.breakdown.limit_truncate_calls,
        result.stats.breakdown.aggregate_compiled_fast_path_used,
    ));
    Ok(out)
}

#[update]
/// Returns planner decisions for a named e-commerce benchmark query without
/// executing it (anchor/index choice, estimated rows/instructions, operator list).
fn bench_ecom_probe_explain(
    name: String,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = ecom_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown ecom probe name '{name}'"))
    })?;
    explain_ecom_query(&gql)
}

#[update]
/// Returns planner decisions for an arbitrary diagnostic GQL without executing it.
fn bench_ecom_probe_explain_gql(
    gql: String,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    explain_ecom_query(&gql)
}

#[update]
/// Returns index/label state used by e-commerce benchmark queries.
fn bench_ecom_probe_index_state(restore: bool) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    Ok(index_state_lines())
}

#[update]
/// Runs step-by-step diagnostics for one e-commerce benchmark query shape and
/// returns compact human-readable summaries per step.
///
/// `name`:
/// - collab_filter
/// - trending_products
/// - co_purchase
/// - buyer_segment
/// - vip_popular_products
/// - cart_abandonment
/// - user_activity
/// - category_revenue
/// - cross_sell
/// - multi_touch
/// - high_value_buyers
/// - wishlist_hot
/// - shortest_path
///
/// `restore`:
/// - true: run from the stable snapshot baseline each call
/// - false: run against current in-memory state
fn bench_ecom_probe_pipeline(
    name: String,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = ecom_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown ecom probe pipeline name '{name}'"
        ))
    })?;
    let mut out = Vec::with_capacity(steps.len());
    for (step, gql) in steps {
        match run_ecom_query(&gql) {
            Ok(result) => out.push(summarize_probe_step(step, &result)),
            Err(e) => {
                out.push(format!("{step}: error={e}"));
                return Ok(out);
            }
        }
    }
    Ok(out)
}

#[update]
/// Runs step-by-step diagnostics with custom runtime limits.
///
/// `max_rows = null` disables row-count cap.
/// `max_steps = null` keeps the default bridge cap.
fn bench_ecom_probe_pipeline_limited(
    name: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = ecom_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown ecom probe pipeline name '{name}'"
        ))
    })?;
    let mut out = Vec::with_capacity(steps.len());
    for (step, gql) in steps {
        match run_ecom_query_limited(&gql, max_rows, max_steps) {
            Ok(result) => out.push(summarize_probe_step(step, &result)),
            Err(e) => {
                out.push(format!("{step}: error={e}"));
                return Ok(out);
            }
        }
    }
    Ok(out)
}

#[update]
/// Runs a single step from `bench_ecom_probe_pipeline` by index.
///
/// Useful when the full pipeline exceeds single-message instruction limits.
fn bench_ecom_probe_pipeline_step(
    name: String,
    step_index: u32,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = ecom_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown ecom probe pipeline name '{name}'"
        ))
    })?;
    let idx = step_index as usize;
    if idx >= steps.len() {
        return Err(gleaph_types::GleaphError::ValidationError(format!(
            "step_index out of range: {} (len={})",
            step_index,
            steps.len()
        )));
    }
    let (step, gql) = &steps[idx];
    let result = run_ecom_query(gql)?;
    Ok(summarize_probe_step(step, &result))
}

#[update]
/// Runs a single pipeline step with custom runtime limits.
///
/// Useful when a specific step exceeds default row/step caps.
fn bench_ecom_probe_pipeline_step_limited(
    name: String,
    step_index: u32,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = ecom_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown ecom probe pipeline name '{name}'"
        ))
    })?;
    let idx = step_index as usize;
    if idx >= steps.len() {
        return Err(gleaph_types::GleaphError::ValidationError(format!(
            "step_index out of range: {} (len={})",
            step_index,
            steps.len()
        )));
    }
    let (step, gql) = &steps[idx];
    let result = run_ecom_query_limited(gql, max_rows, max_steps)?;
    Ok(summarize_probe_step(step, &result))
}

#[update]
/// Compares planner cost estimates against actual execution for all ecom queries.
/// Returns one line per query: `name: est_units=X actual_steps=Y ratio=Z`.
/// The ratio is `estimated_instructions / (actual_steps / COST_UNIT_IC)` where
/// COST_UNIT_IC ≈ 8,528 IC instructions (1 cost unit).
fn bench_ecom_probe_accuracy(restore: bool) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    // One cost-model unit ≈ 8,528 IC instructions (see stats.rs).
    const COST_UNIT_IC: f64 = 8_528.0;
    let query_names = [
        "collab_filter",
        "trending_products",
        "co_purchase",
        "buyer_segment",
        "vip_popular_products",
        "cart_abandonment",
        "user_activity",
        "category_revenue",
        "cross_sell",
        "multi_touch",
        "high_value_buyers",
        "wishlist_hot",
        "shortest_path",
    ];
    let mut lines = Vec::new();
    for name in query_names {
        let gql = match ecom_query_text(name) {
            Some(q) => q,
            None => continue,
        };
        // Get planner estimate.
        let explain = explain_ecom_query(&gql);
        let est_instr = match &explain {
            Ok(s) => {
                // Parse est_instr=N.N from the explain string.
                s.split("est_instr=")
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0)
            }
            Err(_) => 0.0,
        };
        // Execute and get actual steps.
        let actual_steps = match run_ecom_query(&gql) {
            Ok(result) => result.stats.execution_steps,
            Err(e) => {
                lines.push(format!("{name}: ERROR {e}"));
                continue;
            }
        };
        let actual_units = actual_steps as f64 / COST_UNIT_IC;
        let ratio = if actual_units > 0.0 {
            est_instr / actual_units
        } else {
            0.0
        };
        lines.push(format!(
            "{name}: est_units={est_instr:.1} actual_steps={actual_steps} actual_units={actual_units:.1} ratio={ratio:.3}"
        ));
    }
    Ok(lines)
}

// ---------------------------------------------------------------------------
// E-commerce benchmarks — each restores from a pre-built stable memory
// snapshot, avoiding the expensive graph setup entirely.
// ---------------------------------------------------------------------------

#[bench(raw)]
/// E-commerce: item-to-item collaborative filtering (50K users, 50K products, 200K orders).
///
/// Finds users who purchased the seed product (2-hop via Orders),
/// caps to 200 purchasers, then counts their other purchases.
///
/// GQL:
///   MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(t:Product {id: 5})
///   WITH u LIMIT 200
///   MATCH (u)-[:Placed]->(:Order)-[:Contains]->(rec:Product)
///   WHERE rec.id <> 5
///   RETURN rec.id, COUNT(*) AS rec_score
///   ORDER BY rec_score DESC LIMIT 10
fn bench_ecom_collab_filter() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(t:Product {id: 5}) \
             WITH u LIMIT 200 \
             MATCH (u)-[:Placed]->(:Order)-[:Contains]->(rec:Product) \
             WHERE rec.id <> 5 \
             RETURN rec.id, COUNT(*) AS rec_score \
             ORDER BY rec_score DESC LIMIT 10",
            ),
            "bench_ecom_collab_filter",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_collab_filter", false);
    result
}

#[bench(raw)]
/// E-commerce: trending products via tier-sampled users (50K users, 50K products, 200K orders).
///
/// 10% sample via tier=2 index scan (5K users). 2-hop via Orders.
/// Tier is assigned by `u % 10` — independent of product selection — so any
/// tier subset is a statistically representative sample of product popularity.
///
/// GQL:
///   MATCH (u:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product)
///   RETURN p.id, COUNT(*) AS popularity
///   ORDER BY popularity DESC LIMIT 10
fn bench_ecom_trending_products() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (u:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN p.id, COUNT(*) AS popularity \
             ORDER BY popularity DESC LIMIT 10",
            ),
            "bench_ecom_trending_products",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_trending_products", false);
    result
}

#[bench(raw)]
/// E-commerce: co-purchase similarity — find users who purchased the same
/// products as a given user (50K users, 50K products, 200K orders).
///
/// Uses WITH DISTINCT to deduplicate intermediate products before the
/// reverse fan-out, avoiding redundant traversal when the same product is
/// reached via multiple orders.
///
/// GQL:
///   MATCH (:User {id: 42})-[:Placed]->(:Order)-[:Contains]->(p:Product)
///   WITH DISTINCT p
///   MATCH (p)<-[:Contains]-(:Order)<-[:Placed]-(other:User)
///   WHERE other.id <> 42
///   RETURN other.id, COUNT(DISTINCT p) AS shared
///   ORDER BY shared DESC LIMIT 10
fn bench_ecom_co_purchase() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (:User {id: 42})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             WITH DISTINCT p \
             MATCH (p)<-[:Contains]-(:Order)<-[:Placed]-(other:User) \
             WHERE other.id <> 42 \
             RETURN other.id, COUNT(DISTINCT p) AS shared \
             ORDER BY shared DESC LIMIT 10",
            ),
            "bench_ecom_co_purchase",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_co_purchase", false);
    result
}

#[bench(raw)]
/// E-commerce: buyer segment analysis for a product (50K users, 50K products, 200K orders).
///
/// Anchors on Product {id: 5}, reverse-traverses via Orders to Users,
/// and aggregates by user tier.
///
/// GQL:
///   MATCH (p:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User)
///   RETURN u.tier, COUNT(*) AS cnt
///   ORDER BY cnt DESC
fn bench_ecom_buyer_segment() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (p:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User) \
             RETURN u.tier, COUNT(*) AS cnt \
             ORDER BY cnt DESC",
            ),
            "bench_ecom_buyer_segment",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_buyer_segment", false);
    result
}

#[bench(raw)]
/// E-commerce: recent VIP purchase engagement (50K users, 50K products, 200K orders).
///
/// Uses a `tier` equality index to jump straight to VIP users (tier=2, 10 %).
/// 2-hop via Orders with temporal filter on the Placed edge.
///
/// GQL:
///   MATCH (u:User {tier: 2})-[e:Placed]->(:Order)-[:Contains]->(p:Product)
///   WHERE gleaph_timestamp(e) > {ECOM_RECENT_CUTOFF}
///   RETURN p.id, COUNT(*) AS vip_score
///   ORDER BY vip_score DESC LIMIT 5
fn bench_ecom_vip_popular_products() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(&format!(
                "MATCH (u:User {{tier: 2}})-[e:Placed]->(:Order)-[:Contains]->(p:Product) \
             WHERE gleaph_timestamp(e) > {ECOM_RECENT_CUTOFF} \
             RETURN p.id, COUNT(*) AS vip_score \
             ORDER BY vip_score DESC LIMIT 5"
            )),
            "bench_ecom_vip_popular_products",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_vip_popular_products", false);
    result
}

#[bench(raw)]
/// E-commerce: cart abandonment detection (50K users, 50K products, 200K orders).
///
/// Users who carted product 5 but never purchased it — retargeting candidates.
/// OPTIONAL MATCH produces NULL for `o` when no purchase path exists;
/// WHERE o IS NULL filters to abandoned carts.
///
/// Tests: OPTIONAL MATCH, IS NULL filtering.
///
/// GQL:
///   MATCH (u:User)-[:Carted]->(p:Product {id: 5})
///   OPTIONAL MATCH (u)-[:Placed]->(o:Order)-[:Contains]->(p)
///   WHERE o IS NULL
///   RETURN u.id
///   LIMIT 20
fn bench_ecom_cart_abandonment() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (u:User)-[:Carted]->(p:Product {id: 5}) \
             OPTIONAL MATCH (u)-[:Placed]->(o:Order)-[:Contains]->(p) \
             WHERE o IS NULL \
             RETURN u.id \
             LIMIT 20",
            ),
            "bench_ecom_cart_abandonment",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_cart_abandonment", false);
    result
}

#[bench(raw)]
/// E-commerce: complete user activity timeline (50K users, 50K products, 200K orders).
///
/// Combines purchases, cart additions, and favorites for user 42 via UNION ALL.
/// Each branch anchors on User {id: 42} via equality index.
///
/// Tests: UNION ALL across 3 branches with different edge labels.
///
/// GQL:
///   MATCH (:User {id: 42})-[e:Placed]->(:Order)-[:Contains]->(p:Product)
///   RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts
///   UNION ALL
///   MATCH (:User {id: 42})-[e:Carted]->(p:Product)
///   RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts
///   UNION ALL
///   MATCH (:User {id: 42})-[e:Favorited]->(p:Product)
///   RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts
fn bench_ecom_user_activity() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (:User {id: 42})-[e:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts \
             UNION ALL \
             MATCH (:User {id: 42})-[e:Carted]->(p:Product) \
             RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts \
             UNION ALL \
             MATCH (:User {id: 42})-[e:Favorited]->(p:Product) \
             RETURN p.id AS product_id, p.price AS price, gleaph_timestamp(e) AS ts",
            ),
            "bench_ecom_user_activity",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_user_activity", false);
    result
}

#[bench(raw)]
/// E-commerce: category-level revenue analysis for VIP segment (50K users, 50K products, 200K orders).
///
/// Anchors on tier=2 users (5K, 10% sample), traverses 2-hop via Orders,
/// aggregates with SUM + AVG multi-aggregate by product category.
///
/// Tests: SUM, AVG multi-aggregate in a single RETURN.
///
/// GQL:
///   MATCH (:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product)
///   RETURN p.category, COUNT(*) AS purchases, SUM(p.price) AS revenue, AVG(p.price) AS avg_price
///   ORDER BY revenue DESC
fn bench_ecom_category_revenue() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (:User {tier: 2})-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN p.category, COUNT(*) AS purchases, SUM(p.price) AS revenue, AVG(p.price) AS avg_price \
             ORDER BY revenue DESC",
            ),
            "bench_ecom_category_revenue",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_category_revenue", false);
    result
}

#[bench(raw)]
/// E-commerce: cross-sell recommendations (50K users, 50K products, 200K orders).
///
/// "Buyers of X also like Y" — 3-hop reverse traversal from Product {id: 5}
/// through Orders to Users, then 1-hop forward via Favorited edges.
///
/// Tests: Mixed-label multi-hop traversal (Contains, Placed, Favorited).
///
/// GQL:
///   MATCH (:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User)-[:Favorited]->(rec:Product)
///   WHERE rec.id <> 5
///   RETURN rec.id, COUNT(DISTINCT u) AS buyer_favorites
///   ORDER BY buyer_favorites DESC LIMIT 10
fn bench_ecom_cross_sell() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (:Product {id: 5})<-[:Contains]-(:Order)<-[:Placed]-(u:User)-[:Favorited]->(rec:Product) \
             WHERE rec.id <> 5 \
             RETURN rec.id, COUNT(DISTINCT u) AS buyer_favorites \
             ORDER BY buyer_favorites DESC LIMIT 10",
            ),
            "bench_ecom_cross_sell",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_cross_sell", false);
    result
}

#[bench(raw)]
/// E-commerce: multi-channel engagement (50K users, 50K products, 200K orders).
///
/// Which users engaged with product 5 via cart AND/OR favorites.
/// Uses edge label expression `Carted|Favorited` to match both edge types
/// in a single pattern.
///
/// Tests: Edge label expression (`[e:Carted|Favorited]`).
///
/// GQL:
///   MATCH (u:User)-[e:Carted|Favorited]->(p:Product {id: 5})
///   RETURN u.id, COUNT(*) AS touch_count
///   ORDER BY touch_count DESC LIMIT 20
fn bench_ecom_multi_touch() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (u:User)-[e:Carted|Favorited]->(p:Product {id: 5}) \
             RETURN u.id, COUNT(*) AS touch_count \
             ORDER BY touch_count DESC LIMIT 20",
            ),
            "bench_ecom_multi_touch",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_multi_touch", false);
    result
}

#[bench(raw)]
/// E-commerce: highest-value customers by spending (50K users, 50K products, 200K orders).
///
/// Full-graph fan-out across all Users → Orders → Products with conditional
/// aggregation via CASE WHEN to count premium purchases (price > 300).
///
/// Tests: CASE WHEN inside SUM aggregation.
///
/// GQL:
///   MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(p:Product)
///   RETURN u.id, COUNT(*) AS purchases,
///     SUM(CASE WHEN p.price > 300 THEN 1 ELSE 0 END) AS premium_count,
///     SUM(p.price) AS total_spent
///   ORDER BY total_spent DESC LIMIT 10
fn bench_ecom_high_value_buyers() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (u:User)-[:Placed]->(:Order)-[:Contains]->(p:Product) \
             RETURN u.id, COUNT(*) AS purchases, \
               SUM(CASE WHEN p.price > 300 THEN 1 ELSE 0 END) AS premium_count, \
               SUM(p.price) AS total_spent \
             ORDER BY total_spent DESC LIMIT 10",
            ),
            "bench_ecom_high_value_buyers",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_high_value_buyers", false);
    result
}

#[bench(raw)]
/// E-commerce: VIP wishlist analysis (50K users, 50K products, 200K orders).
///
/// Anchors on tier=2 users (5K), traverses Favorited edges, builds per-user
/// list of favorited product IDs via COLLECT.
///
/// Tests: COLLECT list aggregation.
///
/// GQL:
///   MATCH (u:User {tier: 2})-[:Favorited]->(p:Product)
///   RETURN u.id, COLLECT(p.id) AS favorites, COUNT(*) AS fav_count
///   ORDER BY fav_count DESC LIMIT 10
fn bench_ecom_wishlist_hot() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH (u:User {tier: 2})-[:Favorited]->(p:Product) \
             RETURN u.id, COLLECT(p.id) AS favorites, COUNT(*) AS fav_count \
             ORDER BY fav_count DESC LIMIT 10",
            ),
            "bench_ecom_wishlist_hot",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_wishlist_hot", false);
    result
}

#[bench(raw)]
/// E-commerce: shortest path from user to product (50K users, 50K products, 200K orders).
///
/// Finds the shortest connection from user 42 to product 4364 through any
/// combination of Placed/Contains/Carted/Favorited edges (1–4 hops).
/// Exercises the BFS engine with variable-length path matching.
///
/// Product 4364 is specifically chosen because user 42 reaches it only via a
/// 2-hop path (User -[:Placed]-> Order -[:Contains]-> Product), ensuring the
/// BFS must traverse at least one intermediate node.
///
/// Tests: SHORTEST with variable-length path `[*1..4]` and dual-anchor constraints.
///
/// GQL:
///   MATCH SHORTEST p = (u:User {id: 42})-[*1..4]->(target:Product {id: 4364})
///   RETURN p
fn bench_ecom_shortest_path() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_ecom_query(
                "MATCH SHORTEST p = (u:User {id: 42})-[*1..4]->(target:Product {id: 4364}) \
             RETURN p",
            ),
            "bench_ecom_shortest_path",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_ecom_shortest_path", false);
    result
}
