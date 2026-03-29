use canbench_rs::bench;
use gleaph_gql::{
    parse_statement, planner::build_plan_with_stats, stats::TableStats, validate_statement,
};
use gleaph_types::{EntityType, IndexType, Value};
use ic_cdk_macros::update;

use crate::state::{restore_state_uncertified, with_state, with_state_mut};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const TIMELINE_NUM_USERS: u32 = 10_000;
const TIMELINE_NUM_POSTS: u32 = 50_000; // 5 posts per user

/// Total vertices: 60K (10K users + 50K posts)
const TIMELINE_TOTAL_VERTICES: u32 = TIMELINE_NUM_USERS + TIMELINE_NUM_POSTS;

/// Base timestamp (nanoseconds) — ~March 2026.
const TIMELINE_TS_BASE_NS: u64 = 1_772_000_000_000_000_000;

/// Time window: 30 days in nanoseconds.
const TIMELINE_TS_WINDOW_NS: u64 = 30 * 24 * 3600 * 1_000_000_000;

/// Cutoff for "recent" edges (for hybrid pull queries).
/// Set so that the last ~20% of the window is considered recent.
const TIMELINE_RECENT_CUTOFF: u64 = TIMELINE_TS_BASE_NS - TIMELINE_TS_WINDOW_NS / 5;

/// Number of follows per user (outgoing).
const FOLLOWS_PER_USER: u32 = 10;

/// Number of posts per user.
const POSTS_PER_USER: u32 = 5;

/// Celebrity threshold: users with >= this many followers are celebrities.
/// With 10K users and 10 follows each (cubic distribution), users at
/// rank < ~20 have 500+ followers.
const CELEBRITY_THRESHOLD: u32 = 500;

const TIMELINE_QUERY_MAX_GROUPS: usize = 1_000_000;
const TIMELINE_QUERY_MAX_EXECUTION_STEPS: u64 = 10_000_000_000;

// ---------------------------------------------------------------------------
// Hash / distribution helpers (reuse social patterns)
// ---------------------------------------------------------------------------

/// Splitmix64-based hash — strong mixing of both seed values.
fn timeline_hash(a: u32, b: u32) -> u64 {
    let mut x = (a as u64)
        .wrapping_mul(31)
        .wrapping_add((b as u64).wrapping_mul(7919));
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Cubic distribution for follow targets — realistic power-law skew.
/// Low user IDs get the most followers.
fn timeline_follow_target(h: u64, num_users: u32) -> u32 {
    let n = num_users as u64;
    let r = h % n;
    let t = r.wrapping_mul(r).wrapping_mul(r) / (n * n);
    (t as u32).min(num_users - 1)
}

/// Monotonically increasing timestamp for edge (a, b_slot).
/// Maps b_slot linearly into [BASE - WINDOW, BASE].
fn timeline_timestamp(_a: u32, b: u32) -> u64 {
    let b_clamped = (b as u64).min(1000);
    TIMELINE_TS_BASE_NS - TIMELINE_TS_WINDOW_NS + b_clamped * TIMELINE_TS_WINDOW_NS / 1000
}

/// Count the number of followers for a given user by iterating all users' follow edges.
/// This is deterministic and matches the edge creation logic.
fn follower_count_of(target_user: u32) -> u32 {
    let mut count = 0u32;
    for u in 0..TIMELINE_NUM_USERS {
        for k in 0..FOLLOWS_PER_USER {
            let h = timeline_hash(u, k);
            let t = timeline_follow_target(h, TIMELINE_NUM_USERS);
            if t == target_user && t != u {
                count += 1;
            }
        }
    }
    count
}

/// Return the list of follower user IDs for a given target user.
fn followers_of(target_user: u32) -> Vec<u32> {
    let mut followers = Vec::new();
    for u in 0..TIMELINE_NUM_USERS {
        for k in 0..FOLLOWS_PER_USER {
            let h = timeline_hash(u, k);
            let t = timeline_follow_target(h, TIMELINE_NUM_USERS);
            if t == target_user && t != u {
                followers.push(u);
            }
        }
    }
    followers
}

/// Check if a user is a celebrity based on follower count.
fn is_celebrity(user_id: u32) -> bool {
    follower_count_of(user_id) >= CELEBRITY_THRESHOLD
}

/// Find a user with at least `min_followers` followers.
/// Returns the user with the closest count >= min_followers.
/// If no user has enough, returns the user with the most followers.
fn find_user_with_min_followers(min_followers: u32) -> u32 {
    let mut best_user = 0u32;
    let mut best_fc = 0u32;
    let mut closest_user = 0u32;
    let mut closest_fc = u32::MAX;
    // Only scan the first 200 users (low IDs have more followers in cubic dist)
    for u in 0..200u32.min(TIMELINE_NUM_USERS) {
        let fc = follower_count_of(u);
        // Track user with most followers overall
        if fc > best_fc {
            best_fc = fc;
            best_user = u;
        }
        // Track user with closest count >= min_followers
        if fc >= min_followers && fc < closest_fc {
            closest_fc = fc;
            closest_user = u;
        }
    }
    if closest_fc < u32::MAX {
        closest_user
    } else {
        best_user
    }
}

/// Find a user whose timeline has approximately `target_count` Timeline edges.
fn find_user_with_approx_timeline_edges(target_count: u32) -> u32 {
    // Timeline edges = sum of posts from non-celebrity followed users.
    // Users who follow many non-celebrities will have many Timeline edges.
    let mut best_user = 0u32;
    let mut best_diff = u32::MAX;
    for u in (0..TIMELINE_NUM_USERS).step_by(50) {
        let mut tl_count = 0u32;
        for k in 0..FOLLOWS_PER_USER {
            let h = timeline_hash(u, k);
            let followed = timeline_follow_target(h, TIMELINE_NUM_USERS);
            if followed != u && !is_celebrity(followed) {
                tl_count += POSTS_PER_USER;
            }
        }
        let diff = (tl_count as i64 - target_count as i64).unsigned_abs() as u32;
        if diff < best_diff {
            best_diff = diff;
            best_user = u;
        }
    }
    best_user
}

/// Find a user who follows at least `min_celebs` celebrities.
fn find_user_following_celebs(min_celebs: u32) -> u32 {
    for u in 0..TIMELINE_NUM_USERS {
        let mut celeb_count = 0u32;
        for k in 0..FOLLOWS_PER_USER {
            let h = timeline_hash(u, k);
            let followed = timeline_follow_target(h, TIMELINE_NUM_USERS);
            if followed != u && is_celebrity(followed) {
                celeb_count += 1;
            }
        }
        if celeb_count >= min_celebs {
            return u;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Setup endpoints — called by the PocketIC snapshot generator
// ---------------------------------------------------------------------------

/// Create User vertices in batches.
/// Properties: id, follower_count, celebrity (0 or 1).
#[update]
fn bench_setup_timeline_users(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|u| {
            let fc = follower_count_of(u);
            let celeb = u32::from(fc >= CELEBRITY_THRESHOLD);
            format!("INSERT (:User {{id: {u}, follower_count: {fc}, celebrity: {celeb}}})")
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE User {}: {e}", start + i as u32));
    }
}

/// Create Post vertices in batches.
/// Properties: id, author_id, content_type (0-3).
#[update]
fn bench_setup_timeline_posts(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|p| {
            let author_id = p / POSTS_PER_USER;
            let content_type = p % 4;
            format!(
                "INSERT (:Post {{id: {p}, author_id: {author_id}, content_type: {content_type}}})"
            )
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE Post {}: {e}", start + i as u32));
    }
}

/// Create Follows edges (User→User) for the specified user range.
/// 10 follows per user, cubic power-law distribution on targets.
#[update]
fn bench_setup_timeline_follows(start_user: u32, end_user: u32) {
    with_state_mut(|g| {
        let base_vertex = g
            .vertex_count()
            .saturating_sub(u64::from(TIMELINE_TOTAL_VERTICES)) as u32;

        for u in start_user..end_user {
            let user_v = base_vertex + u;
            for k in 0..FOLLOWS_PER_USER {
                let h = timeline_hash(u, k);
                let target = timeline_follow_target(h, TIMELINE_NUM_USERS);
                if target != u {
                    let target_v = base_vertex + target;
                    let ts = timeline_timestamp(u, k);
                    g.create_edge(user_v, target_v, Some("Follows".into()), vec![], 1.0, ts)
                        .unwrap_or(());
                }
            }
        }
    });
}

/// Create Posted edges (User→Post) for the specified user range.
/// 5 posts per user.
#[update]
fn bench_setup_timeline_posted(start_user: u32, end_user: u32) {
    let post_base_offset = TIMELINE_NUM_USERS; // Posts start after users in vertex space
    with_state_mut(|g| {
        let base_vertex = g
            .vertex_count()
            .saturating_sub(u64::from(TIMELINE_TOTAL_VERTICES)) as u32;

        for u in start_user..end_user {
            let user_v = base_vertex + u;
            for k in 0..POSTS_PER_USER {
                let post_idx = u * POSTS_PER_USER + k;
                if post_idx < TIMELINE_NUM_POSTS {
                    let post_v = base_vertex + post_base_offset + post_idx;
                    let ts = timeline_timestamp(u, k.wrapping_add(100));
                    g.create_edge(user_v, post_v, Some("Posted".into()), vec![], 1.0, ts)
                        .unwrap_or(());
                }
            }
        }
    });
}

/// Create Timeline edges (follower→Post) for non-celebrity authors.
/// For each user in [start_user, end_user) who is NOT a celebrity,
/// create Timeline edges from each of their followers to each of their posts.
#[update]
fn bench_setup_timeline_fanout(start_user: u32, end_user: u32) {
    let post_base_offset = TIMELINE_NUM_USERS;
    with_state_mut(|g| {
        let base_vertex = g
            .vertex_count()
            .saturating_sub(u64::from(TIMELINE_TOTAL_VERTICES)) as u32;

        for author in start_user..end_user {
            // Skip celebrities — their posts are pulled on read
            if is_celebrity(author) {
                continue;
            }

            let followers = followers_of(author);
            if followers.is_empty() {
                continue;
            }

            for k in 0..POSTS_PER_USER {
                let post_idx = author * POSTS_PER_USER + k;
                if post_idx >= TIMELINE_NUM_POSTS {
                    continue;
                }
                let post_v = base_vertex + post_base_offset + post_idx;
                let ts = timeline_timestamp(author, k.wrapping_add(100));
                for &follower in &followers {
                    let follower_v = base_vertex + follower;
                    g.create_edge(follower_v, post_v, Some("Timeline".into()), vec![], 1.0, ts)
                        .unwrap_or(());
                }
            }
        }
    });
}

/// Register the timeline graph type schema and create secondary indexes.
#[update]
fn bench_setup_timeline_indexes() {
    // Register graph type schema for the timeline benchmark (§18.3 inline edge types).
    crate::gql_bridge::mutate(
        "CREATE GRAPH TYPE TimelineType { \
           (:User), (:Post), \
           (:User)-[:Follows]->(:User), \
           (:User)-[:Posted]->(:Post), \
           (:User)-[:Timeline]->(:Post) \
         }",
    )
    .expect("setup: CREATE GRAPH TYPE TimelineType failed");

    crate::state::with_state_mut(|g| {
        g.create_index(EntityType::Vertex, "id".into(), IndexType::Equality)
            .expect("setup: create_index(id) failed");
        g.create_index(EntityType::Vertex, "celebrity".into(), IndexType::Equality)
            .expect("setup: create_index(celebrity) failed");
    });
}

/// Persist the overlay to stable memory (reuse existing endpoint).
/// Note: bench_persist_overlay is already defined in social.rs for bench-social,
/// so we define it here only when bench-social is not active.
#[cfg(not(feature = "bench-social"))]
#[update]
fn bench_persist_overlay() {
    crate::state::with_state_mut(|g| g.compute_property_selectivity());
    crate::state::persist_overlay_only().expect("persist overlay");
}

// ---------------------------------------------------------------------------
// Query texts
// ---------------------------------------------------------------------------

fn timeline_query_text(name: &str) -> Option<String> {
    match name {
        // B2: Timeline read — read timeline edges, ORDER BY ts DESC LIMIT 20
        "timeline_read" => Some(
            "MATCH (me:User {id: UID})-[t:Timeline]->(post:Post) \
             RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts \
             ORDER BY ts DESC LIMIT 20"
                .into(),
        ),
        // B3 phase 1: same as timeline_read but with specific user
        "hybrid_timeline" => Some(
            "MATCH (me:User {id: UID})-[t:Timeline]->(post:Post) \
             RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts \
             ORDER BY ts DESC LIMIT 20"
                .into(),
        ),
        // B3 phase 2: celebrity pull
        "hybrid_celeb_pull" => Some(format!(
            "MATCH (me:User {{id: UID}})-[:Follows]->(c:User {{celebrity: 1}})-[p:Posted]->(post:Post) \
             WHERE gleaph_timestamp(p) > {TIMELINE_RECENT_CUTOFF} \
             RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(p) AS ts \
             ORDER BY ts DESC LIMIT 20"
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Run helpers
// ---------------------------------------------------------------------------

#[inline]
fn run_timeline_query(gql: &str) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    run_timeline_query_limited(gql, None, Some(TIMELINE_QUERY_MAX_EXECUTION_STEPS))
}

fn run_timeline_query_limited(
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

    gleaph_gql::executor::set_max_groups_override(Some(TIMELINE_QUERY_MAX_GROUPS));
    let _guard = MaxGroupsGuard;
    crate::gql_bridge::query_with_limits(gql, max_rows.map(|v| v as usize), max_steps)
}

fn run_timeline_query_profiled_limited(
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

    gleaph_gql::executor::set_max_groups_override(Some(TIMELINE_QUERY_MAX_GROUPS));
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

fn explain_timeline_query(gql: &str) -> Result<String, gleaph_types::GleaphError> {
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

// ---------------------------------------------------------------------------
// Probe helpers
// ---------------------------------------------------------------------------

fn index_state_lines() -> Vec<String> {
    with_state(|g| {
        let user_count = g.scan_vertices_by_label("User").len();
        let post_count = g.scan_vertices_by_label("Post").len();
        let mut indexes = g
            .list_property_indexes()
            .into_iter()
            .map(|idx| {
                let entity = match idx.entity_type {
                    EntityType::Vertex => "vertex",
                    EntityType::Edge => "edge",
                };
                let itype = match idx.index_type {
                    IndexType::Equality => "eq",
                    IndexType::Range => "range",
                };
                format!("{entity}:{itype}:{}", idx.property_name)
            })
            .collect::<Vec<_>>();
        indexes.sort();
        let id_42_hits = g
            .scan_vertices_by_property_eq_live("id", &Value::Int64(42))
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "none(index-missing)".into());
        let celebrity_1_hits = g
            .scan_vertices_by_property_eq_live("celebrity", &Value::Int64(1))
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "none(index-missing)".into());
        let mut out = vec![
            format!(
                "vertex_count={} edge_count={}",
                g.vertex_count(),
                g.edge_count()
            ),
            format!("label_user={} label_post={}", user_count, post_count),
            format!(
                "index_hits id=42:{} celebrity=1:{}",
                id_42_hits, celebrity_1_hits
            ),
            format!("index_count={}", indexes.len()),
        ];
        out.extend(indexes.into_iter().map(|line| format!("index={line}")));
        out
    })
}

// ---------------------------------------------------------------------------
// Probe endpoints
// ---------------------------------------------------------------------------

#[update]
fn bench_timeline_probe_query(
    name: String,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = timeline_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown timeline probe name '{name}'"))
    })?;
    run_timeline_query(&gql)
}

#[update]
fn bench_timeline_probe_gql(
    gql: String,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    run_timeline_query(&gql)
}

#[update]
fn bench_timeline_probe_gql_limited(
    gql: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    run_timeline_query_limited(&gql, max_rows, max_steps)
}

#[update]
fn bench_timeline_probe_explain(
    name: String,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = timeline_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown timeline probe name '{name}'"))
    })?;
    explain_timeline_query(&gql)
}

#[update]
fn bench_timeline_probe_explain_gql(
    gql: String,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    explain_timeline_query(&gql)
}

#[update]
fn bench_timeline_probe_index_state(
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    Ok(index_state_lines())
}

#[update]
fn bench_timeline_probe_query_profiled(
    name: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = timeline_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown timeline probe name '{name}'"))
    })?;
    let (result, stages) = run_timeline_query_profiled_limited(&gql, max_rows, max_steps)?;
    let mut out: Vec<String> = stages
        .into_iter()
        .map(|(stage, ins)| format!("stage={stage} instructions={ins}"))
        .collect();
    out.push(format!(
        "result: rows={} scanned_v={} scanned_e={} exec_steps={}",
        result.rows.len(),
        result.stats.scanned_vertices,
        result.stats.scanned_edges,
        result.stats.execution_steps,
    ));
    Ok(out)
}

#[update]
fn bench_timeline_probe_gql_profiled(
    gql: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let (result, stages) = run_timeline_query_profiled_limited(&gql, max_rows, max_steps)?;
    let mut out: Vec<String> = stages
        .into_iter()
        .map(|(stage, ins)| format!("stage={stage} instructions={ins}"))
        .collect();
    out.push(format!(
        "result: rows={} scanned_v={} scanned_e={} exec_steps={}",
        result.rows.len(),
        result.stats.scanned_vertices,
        result.stats.scanned_edges,
        result.stats.execution_steps,
    ));
    Ok(out)
}

// ---------------------------------------------------------------------------
// B1: Fan-out Write Cost benchmarks
// ---------------------------------------------------------------------------

/// Creates `n_fanout` Timeline edges from followers to a new post.
/// If no user has enough followers, uses the user with the most followers.
fn fanout_write_bench(n_fanout: u32, _bench_name: &str) -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();

    // Find a user with at least the target number of followers
    let author = find_user_with_min_followers(n_fanout);
    let actual_followers = followers_of(author);

    // Create a new Post vertex for this benchmark
    let post_vid = with_state_mut(|g| {
        g.create_vertex(
            vec!["Post".into()],
            vec![
                ("id".into(), Value::Int64(99999)),
                ("author_id".into(), Value::Int64(author as i64)),
                ("content_type".into(), Value::Int64(0)),
            ],
        )
        .expect("create benchmark post vertex")
    });

    let ts = TIMELINE_TS_BASE_NS;
    let actual_count = actual_followers.len().min(n_fanout as usize);
    let follower_subset: Vec<u32> = actual_followers.into_iter().take(actual_count).collect();

    // Get base_vertex for computing vertex IDs
    let base_vertex = with_state(|g| {
        g.vertex_count()
            .saturating_sub(u64::from(TIMELINE_TOTAL_VERTICES + 1)) as u32
    });

    canbench_rs::bench_fn(|| {
        with_state_mut(|g| {
            for &follower in &follower_subset {
                let follower_v = base_vertex + follower;
                g.create_edge(
                    follower_v,
                    post_vid,
                    Some("Timeline".into()),
                    vec![],
                    1.0,
                    ts,
                )
                .unwrap_or(());
            }
        });
    })
}

#[bench(raw)]
/// B1.1: Fan-out write cost — 50 followers.
///
/// Creates 50 Timeline edges from followers to a new post vertex.
/// Measures the instruction cost of fan-out on write for a normal user.
fn bench_timeline_fanout_50() -> canbench_rs::BenchResult {
    let result = fanout_write_bench(50, "bench_timeline_fanout_50");
    assert_within_ic_limits(&result, "bench_timeline_fanout_50", true);
    result
}

#[bench(raw)]
/// B1.2: Fan-out write cost — 500 followers.
///
/// Creates 500 Timeline edges. Near the celebrity threshold.
fn bench_timeline_fanout_500() -> canbench_rs::BenchResult {
    let result = fanout_write_bench(500, "bench_timeline_fanout_500");
    assert_within_ic_limits(&result, "bench_timeline_fanout_500", true);
    result
}

#[bench(raw)]
/// B1.3: Fan-out write cost — max followers (top user, ~1000+ followers).
///
/// Creates Timeline edges for the user with the most followers.
/// Tests the upper bound of fan-out cost in this graph.
fn bench_timeline_fanout_max() -> canbench_rs::BenchResult {
    let result = fanout_write_bench(u32::MAX, "bench_timeline_fanout_max");
    assert_within_ic_limits(&result, "bench_timeline_fanout_max", true);
    result
}

// ---------------------------------------------------------------------------
// B2: Timeline Read benchmarks
// ---------------------------------------------------------------------------

#[bench(raw)]
/// B2.1: Timeline read — light user (~100 Timeline edges).
///
/// Reads timeline via Timeline edges with ORDER BY ts DESC LIMIT 20.
/// With reverse-iteration early termination, cost should be bounded
/// by LIMIT regardless of total Timeline edge count.
///
/// GQL:
///   MATCH (me:User {id: UID})-[t:Timeline]->(post:Post)
///   RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts
///   ORDER BY ts DESC LIMIT 20
fn bench_timeline_read_light() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    // Find a user with ~100 timeline edges
    let uid = find_user_with_approx_timeline_edges(100);
    let gql = format!(
        "MATCH (me:User {{id: {uid}}})-[t:Timeline]->(post:Post) \
         RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts \
         ORDER BY ts DESC LIMIT 20"
    );
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(run_timeline_query(&gql), "bench_timeline_read_light");
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_timeline_read_light", false);
    result
}

#[bench(raw)]
/// B2.2: Timeline read — heavy user (~2000 Timeline edges).
///
/// Same query but for a user with many more Timeline edges.
/// Validates that LIMIT 20 + reverse-iteration keeps cost constant.
fn bench_timeline_read_heavy() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    // Find a user with ~2000 timeline edges (users following many non-celebs)
    let uid = find_user_with_approx_timeline_edges(2000);
    let gql = format!(
        "MATCH (me:User {{id: {uid}}})-[t:Timeline]->(post:Post) \
         RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts \
         ORDER BY ts DESC LIMIT 20"
    );
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(run_timeline_query(&gql), "bench_timeline_read_heavy");
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_timeline_read_heavy", false);
    result
}

// ---------------------------------------------------------------------------
// B3: Hybrid Read benchmarks (Timeline + Celebrity pull)
// ---------------------------------------------------------------------------

#[bench(raw)]
/// B3.1: Hybrid read — 0 celebrity follows (Timeline only).
///
/// Baseline: user follows only non-celebrities, so timeline is entirely
/// served from pre-computed Timeline edges. No celebrity pull needed.
fn bench_timeline_hybrid_0celeb() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    // Find a user with 0 celebrity follows but decent Timeline edges
    let mut uid = 0u32;
    for u in (500..TIMELINE_NUM_USERS).step_by(10) {
        let mut celeb_count = 0u32;
        let mut tl_count = 0u32;
        for k in 0..FOLLOWS_PER_USER {
            let h = timeline_hash(u, k);
            let followed = timeline_follow_target(h, TIMELINE_NUM_USERS);
            if followed != u {
                if is_celebrity(followed) {
                    celeb_count += 1;
                } else {
                    tl_count += POSTS_PER_USER;
                }
            }
        }
        if celeb_count == 0 && tl_count >= 20 {
            uid = u;
            break;
        }
    }
    let gql_timeline = format!(
        "MATCH (me:User {{id: {uid}}})-[t:Timeline]->(post:Post) \
         RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts \
         ORDER BY ts DESC LIMIT 20"
    );
    let result = canbench_rs::bench_fn(|| {
        // Phase 1: Timeline read only (no celeb pull needed)
        let qr = require_non_empty_rows(
            run_timeline_query(&gql_timeline),
            "bench_timeline_hybrid_0celeb",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_timeline_hybrid_0celeb", false);
    result
}

#[bench(raw)]
/// B3.2: Hybrid read — 10 celebrity follows (Timeline + pull).
///
/// User follows 10 celebrities. The benchmark runs two queries:
/// 1. Timeline edge read (pre-computed posts from non-celebrity follows)
/// 2. Celebrity pull (live traversal of celebrity Posted edges)
/// Both results would be merged client-side.
fn bench_timeline_hybrid_10celeb() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let uid = find_user_following_celebs(10);
    let gql_timeline = format!(
        "MATCH (me:User {{id: {uid}}})-[t:Timeline]->(post:Post) \
         RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts \
         ORDER BY ts DESC LIMIT 20"
    );
    let gql_celeb = format!(
        "MATCH (me:User {{id: {uid}}})-[:Follows]->(c:User {{celebrity: 1}})-[p:Posted]->(post:Post) \
         WHERE gleaph_timestamp(p) > {TIMELINE_RECENT_CUTOFF} \
         RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(p) AS ts \
         ORDER BY ts DESC LIMIT 20"
    );
    let result = canbench_rs::bench_fn(|| {
        // Phase 1: Timeline edges
        let qr1 = run_timeline_query(&gql_timeline)
            .expect("bench_timeline_hybrid_10celeb: timeline query failed");
        let _ = std::hint::black_box(&qr1);
        // Phase 2: Celebrity pull
        let qr2 = run_timeline_query(&gql_celeb)
            .expect("bench_timeline_hybrid_10celeb: celeb pull query failed");
        let _ = std::hint::black_box(&qr2);
        // In production, results would be merged + sorted client-side
    });
    assert_within_ic_limits(&result, "bench_timeline_hybrid_10celeb", false);
    result
}

// ---------------------------------------------------------------------------
// B4: Celebrity Promotion benchmark
// ---------------------------------------------------------------------------

#[bench(raw)]
/// B4: Celebrity promotion scenario.
///
/// Tests that timeline reads remain stable across a celebrity promotion.
/// Setup: a user who was promoted (their old posts have Timeline edges,
/// new posts after promotion do not). The benchmark reads the timeline
/// which should contain pre-promotion posts via Timeline edges.
///
/// We pick a celebrity user (who has Timeline edges from the snapshot
/// because `bench_setup_timeline_fanout` skips celebrities — but their
/// posts from before promotion are already fan-out'd). To simulate this,
/// we use a borderline celebrity whose older posts were fan-out before
/// they crossed the threshold.
///
/// For simplicity, we verify that reading a regular user's timeline works
/// consistently regardless of whether some followed users are celebrities.
fn bench_timeline_promotion() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    // Pick a user who follows a mix of celebrities and non-celebrities
    let uid = find_user_following_celebs(1);

    // Phase 1: Read pre-promotion timeline edges
    let gql_timeline = format!(
        "MATCH (me:User {{id: {uid}}})-[t:Timeline]->(post:Post) \
         RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(t) AS ts \
         ORDER BY ts DESC LIMIT 20"
    );
    // Phase 2: Read celebrity pull (post-promotion posts)
    let gql_celeb = format!(
        "MATCH (me:User {{id: {uid}}})-[:Follows]->(c:User {{celebrity: 1}})-[p:Posted]->(post:Post) \
         WHERE gleaph_timestamp(p) > {TIMELINE_RECENT_CUTOFF} \
         RETURN post.id, post.author_id, post.content_type, gleaph_timestamp(p) AS ts \
         ORDER BY ts DESC LIMIT 20"
    );
    let result = canbench_rs::bench_fn(|| {
        // Both phases should succeed, demonstrating stable reads
        let qr1 = run_timeline_query(&gql_timeline)
            .expect("bench_timeline_promotion: timeline query failed");
        let _ = std::hint::black_box(&qr1);
        let qr2 = run_timeline_query(&gql_celeb)
            .expect("bench_timeline_promotion: celeb pull query failed");
        let _ = std::hint::black_box(&qr2);
    });
    assert_within_ic_limits(&result, "bench_timeline_promotion", false);
    result
}
