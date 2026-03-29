use canbench_rs::bench;
use gleaph_gql::{
    executor::debug_two_hop_top_k_count_by_terminal_key_query_shape, parse_statement,
    planner::build_plan_with_stats, stats::TableStats, validate_statement,
};
use gleaph_types::Value;
use gleaph_types::{EntityType, IndexType};
use ic_cdk_macros::update;

use crate::state::{restore_state_uncertified, with_state, with_state_mut};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SOCIAL_NUM_USERS: u32 = 50_000;
const SOCIAL_NUM_POSTS: u32 = 100_000;
const SOCIAL_NUM_COMMENTS: u32 = 100_000;
const SOCIAL_NUM_HASHTAGS: u32 = 5_000;

/// Total vertices: 255K
const SOCIAL_TOTAL_VERTICES: u32 =
    SOCIAL_NUM_USERS + SOCIAL_NUM_POSTS + SOCIAL_NUM_COMMENTS + SOCIAL_NUM_HASHTAGS;

/// Base timestamp (nanoseconds) representing "now" in the benchmark dataset.
/// Fixed to a deterministic value (~March 2026).
const SOCIAL_TS_BASE_NS: u64 = 1_772_000_000_000_000_000;

/// Time window over which edge timestamps are distributed (30 days in nanoseconds).
const SOCIAL_TS_WINDOW_NS: u64 = 30 * 24 * 3600 * 1_000_000_000;

/// Cutoff for "recent" edges.  With monotonic timestamps where
/// `ts(b) = BASE - WINDOW + b * WINDOW / 550`, this sits at b ≈ 50 —
/// between follows (b = 0..7, old) and posted (b = 100+, recent).
/// Reverse-iteration early termination skips 8 follow edges per user.
const SOCIAL_RECENT_CUTOFF: u64 = SOCIAL_TS_BASE_NS - SOCIAL_TS_WINDOW_NS * 10 / 11;

const SOCIAL_QUERY_MAX_GROUPS: usize = 1_000_000;
const SOCIAL_QUERY_MAX_EXECUTION_STEPS: u64 = 10_000_000_000;

// ---------------------------------------------------------------------------
// Hash / distribution helpers
// ---------------------------------------------------------------------------

/// Splitmix64-based hash — strong mixing of both seed values.
fn social_hash(a: u32, b: u32) -> u64 {
    let mut x = (a as u64)
        .wrapping_mul(31)
        .wrapping_add((b as u64).wrapping_mul(7919));
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Monotonically increasing timestamp for edge (a, b).
/// Maps b linearly into [BASE - WINDOW, BASE].  Because edges are inserted
/// in b-order per vertex, this matches production behaviour where insertion
/// order equals timestamp order.
fn social_timestamp(_a: u32, b: u32) -> u64 {
    let b_clamped = (b as u64).min(550);
    SOCIAL_TS_BASE_NS - SOCIAL_TS_WINDOW_NS + b_clamped * SOCIAL_TS_WINDOW_NS / 550
}

/// Cubic distribution for follow targets — realistic power-law skew.
///
/// Maps a uniform hash to a user ID via `t = r³ / N²`, giving:
/// - Top 1% users → ~22% of follows
/// - Top 20% users → ~63% of follows
fn social_follow_target(h: u64, num_users: u32) -> u32 {
    let n = num_users as u64;
    let r = h % n;
    let t = r.wrapping_mul(r).wrapping_mul(r) / (n * n);
    (t as u32).min(num_users - 1)
}

/// Zipf-like distribution for hashtag selection.
/// Maps uniform hash to hashtag index with heavy head bias.
fn social_hashtag_target(h: u64, num_hashtags: u32) -> u32 {
    let n = num_hashtags as u64;
    let r = h % n;
    let t = r.wrapping_mul(r) / n;
    (t as u32).min(num_hashtags - 1)
}

// ---------------------------------------------------------------------------
// Setup endpoints — called by the PocketIC snapshot generator
// ---------------------------------------------------------------------------

/// Create User vertices in batches.
/// Properties: id, verified (0/1, 5%), region (0-4).
#[update]
fn bench_setup_social_users(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|u| {
            let verified = u32::from(u % 20 == 0); // 5% verified
            let region = u % 5;
            format!("INSERT (:User {{id: {u}, verified: {verified}, region: {region}}})")
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE User {}: {e}", start + i as u32));
    }
}

/// Create Post vertices in batches.
/// Properties: id, content_type (0-3), viral_score (0-99).
#[update]
fn bench_setup_social_posts(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|p| {
            let content_type = p % 4;
            let viral_score = (p.wrapping_mul(37).wrapping_add(13)) % 100;
            format!(
                "INSERT (:Post {{id: {p}, content_type: {content_type}, viral_score: {viral_score}}})"
            )
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE Post {}: {e}", start + i as u32));
    }
}

/// Create Comment vertices in batches.
/// Properties: id, depth (0-2).
#[update]
fn bench_setup_social_comments(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|c| {
            let depth = c % 3;
            format!("INSERT (:Comment {{id: {c}, depth: {depth}}})")
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE Comment {}: {e}", start + i as u32));
    }
}

/// Create Hashtag vertices in batches.
/// Properties: id, category (0-9).
#[update]
fn bench_setup_social_hashtags(start: u32, end: u32) {
    let stmts: Vec<String> = (start..end)
        .map(|h| {
            let category = h % 10;
            format!("INSERT (:Hashtag {{id: {h}, category: {category}}})")
        })
        .collect();
    let results = crate::gql_bridge::batch_mutate_tracked(&stmts);
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("CREATE Hashtag {}: {e}", start + i as u32));
    }
}

/// Create Follows edges (User→User) for the specified user range.
/// ~8 follows per user, power-law distribution on targets.
/// Total: ~400K edges.
#[update]
fn bench_setup_social_follow_edges(start_user: u32, end_user: u32) {
    let num_users = SOCIAL_NUM_USERS;
    with_state_mut(|g| {
        let base_vertex = g
            .vertex_count()
            .saturating_sub(u64::from(SOCIAL_TOTAL_VERTICES)) as u32;

        for u in start_user..end_user {
            let user_v = base_vertex + u;
            for k in 0..8u32 {
                let h = social_hash(u, k);
                let target = social_follow_target(h, num_users);
                if target != u {
                    let target_v = base_vertex + target;
                    let ts = social_timestamp(u, k);
                    g.create_edge(user_v, target_v, Some("Follows".into()), vec![], 1.0, ts)
                        .unwrap_or(());
                }
            }
        }
    });
}

/// Create content edges for the specified user range:
/// - Posted (User→Post): 2 per user, 100K total
/// - Liked (User→Post): 4 per user, power-law on post popularity, ~200K total
/// - Authored (User→Comment): 2 per user, 100K total
/// - ReplyTo (Comment→Post): 70% of comments
/// - ReplyToComment (Comment→Comment): 30% of comments
/// - Tagged (Post→Hashtag): ~1.5 per post, Zipf distribution, ~150K total
#[update]
fn bench_setup_social_content_edges(start_user: u32, end_user: u32) {
    let num_posts = SOCIAL_NUM_POSTS;
    let num_comments = SOCIAL_NUM_COMMENTS;
    let num_hashtags = SOCIAL_NUM_HASHTAGS;
    with_state_mut(|g| {
        let base_vertex = g
            .vertex_count()
            .saturating_sub(u64::from(SOCIAL_TOTAL_VERTICES)) as u32;
        let post_base = base_vertex + SOCIAL_NUM_USERS;
        let comment_base = post_base + num_posts;
        let hashtag_base = comment_base + num_comments;

        for u in start_user..end_user {
            let user_v = base_vertex + u;

            // Posted: 2 posts per user (deterministic mapping).
            for k in 0..2u32 {
                let post_idx = u * 2 + k;
                if post_idx < num_posts {
                    let post_v = post_base + post_idx;
                    let ts = social_timestamp(u, k.wrapping_add(100));
                    g.create_edge(user_v, post_v, Some("Posted".into()), vec![], 1.0, ts)
                        .unwrap_or(());
                }
            }

            // Liked: 4 likes per user, power-law on post popularity.
            for k in 0..4u32 {
                let h = social_hash(u, k.wrapping_add(200));
                let post_idx = social_follow_target(h, num_posts); // reuse cubic for popularity
                let post_v = post_base + post_idx;
                let ts = social_timestamp(u, k.wrapping_add(200));
                g.create_edge(user_v, post_v, Some("Liked".into()), vec![], 1.0, ts)
                    .unwrap_or(());
            }

            // Authored: 2 comments per user (deterministic mapping).
            for k in 0..2u32 {
                let comment_idx = u * 2 + k;
                if comment_idx < num_comments {
                    let comment_v = comment_base + comment_idx;
                    let ts = social_timestamp(u, k.wrapping_add(300));
                    g.create_edge(user_v, comment_v, Some("Authored".into()), vec![], 1.0, ts)
                        .unwrap_or(());

                    // ReplyTo / ReplyToComment: 70% reply to post, 30% reply to another comment.
                    let h = social_hash(u, k.wrapping_add(400));
                    if h % 10 < 7 {
                        // ReplyTo: comment → post
                        let target_post = (h as u32) % num_posts;
                        let target_v = post_base + target_post;
                        g.create_edge(comment_v, target_v, Some("ReplyTo".into()), vec![], 1.0, ts)
                            .unwrap_or(());
                    } else {
                        // ReplyToComment: comment → another comment
                        let target_comment = (h as u32) % num_comments;
                        if target_comment != comment_idx {
                            let target_v = comment_base + target_comment;
                            g.create_edge(
                                comment_v,
                                target_v,
                                Some("ReplyToComment".into()),
                                vec![],
                                1.0,
                                ts,
                            )
                            .unwrap_or(());
                        }
                    }
                }
            }

            // Tagged: ~1.5 tags per post (posts owned by this user).
            for k in 0..2u32 {
                let post_idx = u * 2 + k;
                if post_idx < num_posts {
                    let post_v = post_base + post_idx;
                    // First tag always present
                    let h1 = social_hash(post_idx, 500);
                    let tag1 = social_hashtag_target(h1, num_hashtags);
                    let tag1_v = hashtag_base + tag1;
                    let ts = social_timestamp(u, k.wrapping_add(500));
                    g.create_edge(post_v, tag1_v, Some("Tagged".into()), vec![], 1.0, ts)
                        .unwrap_or(());

                    // Second tag 50% of the time → average 1.5 tags/post
                    let h2 = social_hash(post_idx, 501);
                    if h2.is_multiple_of(2) {
                        let tag2 = social_hashtag_target(h2, num_hashtags);
                        if tag2 != tag1 {
                            let tag2_v = hashtag_base + tag2;
                            g.create_edge(post_v, tag2_v, Some("Tagged".into()), vec![], 1.0, ts)
                                .unwrap_or(());
                        }
                    }
                }
            }
        }
    });
}

/// Register the social graph type schema and create secondary indexes.
#[update]
fn bench_setup_social_indexes() {
    // Register graph type schema for the social benchmark (§18.3 inline edge types).
    crate::gql_bridge::mutate(
        "CREATE GRAPH TYPE SocialType { \
           (:User), (:Post), (:Comment), (:Hashtag), \
           (:User)-[:Follows]->(:User), \
           (:User)-[:Posted]->(:Post), \
           (:User)-[:Liked]->(:Post), \
           (:User)-[:Authored]->(:Comment), \
           (:Comment)-[:ReplyTo]->(:Post), \
           (:Comment)-[:ReplyToComment]->(:Comment), \
           (:Post)-[:Tagged]->(:Hashtag) \
         }",
    )
    .expect("setup: CREATE GRAPH TYPE SocialType failed");

    crate::state::with_state_mut(|g| {
        g.create_index(EntityType::Vertex, "id".into(), IndexType::Equality)
            .expect("setup: create_index(id) failed");
        g.create_index(EntityType::Vertex, "verified".into(), IndexType::Equality)
            .expect("setup: create_index(verified) failed");
        g.create_index(EntityType::Vertex, "region".into(), IndexType::Equality)
            .expect("setup: create_index(region) failed");
    });
}

/// Persist the overlay to stable memory.
#[update]
fn bench_persist_overlay() {
    crate::state::with_state_mut(|g| g.compute_property_selectivity());
    crate::state::persist_overlay_only().expect("persist overlay");
}

// ---------------------------------------------------------------------------
// Query texts
// ---------------------------------------------------------------------------

fn social_query_text(name: &str) -> Option<String> {
    match name {
        // Q1: Feed — recent posts from followed users
        "feed" => Some(format!(
            "MATCH (me:User {{id: 42}})-[:Follows]->(f:User)-[e:Posted]->(p:Post) \
             WHERE gleaph_timestamp(e) > {SOCIAL_RECENT_CUTOFF} \
             RETURN p.id, p.viral_score, gleaph_timestamp(e) AS ts \
             ORDER BY ts DESC LIMIT 20"
        )),
        // Q2: Follower activity — followed users and their post counts
        "follower_activity" => Some(
            "MATCH (:User {id: 42})-[:Follows]->(f:User)-[:Posted]->(p:Post) \
             RETURN f.id, f.verified, COUNT(*) AS post_count \
             ORDER BY post_count DESC"
                .into(),
        ),
        // Q3: Influencer ranking — top users by follower count (WITH LIMIT 500)
        "influencer" => Some(
            "MATCH (a:User {verified: 1}) \
             WITH a LIMIT 500 \
             MATCH (a)-[:Follows]->(b:User) \
             RETURN b.id, COUNT(*) AS followers \
             ORDER BY followers DESC LIMIT 10"
                .into(),
        ),
        // Q4: Trending posts — most-liked posts recently (WITH LIMIT 500)
        "trending_posts" => Some(format!(
            "MATCH (u:User {{verified: 1}}) \
             WITH u LIMIT 500 \
             MATCH (u)-[e:Liked]->(p:Post) \
             WHERE gleaph_timestamp(e) > {SOCIAL_RECENT_CUTOFF} \
             RETURN p.id, COUNT(*) AS likes \
             ORDER BY likes DESC LIMIT 10"
        )),
        // Q5: Engagement rate — single verified user's posts + like fan-in.
        // Uses user 500 (verified, mid-range post popularity) rather than user 0
        // whose posts sit at the peak of the cubic power-law Liked distribution.
        "engagement_rate" => Some(
            "MATCH (u:User {id: 500})-[:Posted]->(p:Post) \
             OPTIONAL MATCH (liker:User)-[:Liked]->(p) \
             RETURN u.id, COUNT(DISTINCT p) AS posts, COUNT(DISTINCT liker) AS likers"
                .into(),
        ),
        // Q6: Friend-of-friend recommendation
        "fof_recommend" => Some(
            "MATCH (me:User {id: 42})-[:Follows]->(f:User)-[:Follows]->(rec:User) \
             WHERE rec.id <> 42 \
             RETURN rec.id, COUNT(*) AS mutual \
             ORDER BY mutual DESC LIMIT 10"
                .into(),
        ),
        // Q7: Hashtag co-occurrence — which categories appear alongside a hashtag
        "hashtag_cooccurrence" => Some(
            "MATCH (h:Hashtag {id: 0})<-[:Tagged]-(p:Post)-[:Tagged]->(other:Hashtag) \
             WHERE other.id <> 0 \
             RETURN other.category, COUNT(*) AS co_count \
             ORDER BY co_count DESC LIMIT 10"
                .into(),
        ),
        // Q8: Content virality — variable-length path from user to post via likes/follows
        "content_virality" => Some(
            "MATCH (u:User {id: 42})-[:Follows|Liked*1..3]->(p:Post) \
             RETURN p.id, p.viral_score LIMIT 20"
                .into(),
        ),
        // Q9: Community bridge — shortest path between two users
        "community_bridge" => Some(
            "MATCH SHORTEST p = (a:User {id: 42})-[*1..5]->(b:User {id: 999}) \
             RETURN p"
                .into(),
        ),
        // Q10: User segmentation — engagement by content type (WITH LIMIT 500)
        "user_segmentation" => Some(
            "MATCH (u:User {verified: 1}) \
             WITH u LIMIT 500 \
             MATCH (u)-[:Posted]->(p:Post) \
             RETURN p.content_type, COUNT(DISTINCT u) AS users, \
               COUNT(DISTINCT p) AS total_posts, \
               SUM(CASE WHEN p.viral_score > 80 THEN 1 ELSE 0 END) AS viral_posts, \
               AVG(p.viral_score) AS avg_score \
             ORDER BY users DESC"
                .into(),
        ),
        // Q11: Thread depth — posts with deep comment reply chains (verified subset)
        "thread_depth" => Some(
            "MATCH (u:User {verified: 1})-[:Posted]->(p:Post)<-[:ReplyTo]-(c:Comment)<-[:ReplyToComment*1..3]-(deep:Comment) \
             RETURN p.id, COUNT(DISTINCT deep) AS chain_length \
             ORDER BY chain_length DESC LIMIT 10"
                .into(),
        ),
        // Q12: Cross engagement — union of different engagement types
        "cross_engagement" => Some(
            "MATCH (u:User {id: 42})-[e:Posted]->(p:Post) \
             RETURN p.id AS target_id, 'posted' AS action, gleaph_timestamp(e) AS ts \
             UNION ALL \
             MATCH (u:User {id: 42})-[e:Liked]->(p:Post) \
             RETURN p.id AS target_id, 'liked' AS action, gleaph_timestamp(e) AS ts \
             UNION ALL \
             MATCH (u:User {id: 42})-[e:Authored]->(c:Comment) \
             RETURN c.id AS target_id, 'commented' AS action, gleaph_timestamp(e) AS ts"
                .into(),
        ),
        // Q13: Verified influence — verified users and their distinct hashtag reach (WITH LIMIT 500)
        "verified_influence" => Some(
            "MATCH (u:User {verified: 1}) \
             WITH u LIMIT 500 \
             MATCH (u)-[:Posted]->(p:Post)-[:Tagged]->(h:Hashtag) \
             RETURN u.id, COLLECT(DISTINCT h.category) AS categories, \
               COUNT(DISTINCT h) AS hashtag_reach \
             ORDER BY hashtag_reach DESC LIMIT 10"
                .into(),
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Run helpers
// ---------------------------------------------------------------------------

#[inline]
fn run_social_query(gql: &str) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    run_social_query_limited(gql, None, Some(SOCIAL_QUERY_MAX_EXECUTION_STEPS))
}

fn run_social_query_limited(
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

    gleaph_gql::executor::set_max_groups_override(Some(SOCIAL_QUERY_MAX_GROUPS));
    let _guard = MaxGroupsGuard;
    crate::gql_bridge::query_with_limits(gql, max_rows.map(|v| v as usize), max_steps)
}

fn run_social_query_profiled_limited(
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

    gleaph_gql::executor::set_max_groups_override(Some(SOCIAL_QUERY_MAX_GROUPS));
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

fn explain_social_query(gql: &str) -> Result<String, gleaph_types::GleaphError> {
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
    with_state(|g| {
        let user_count = g.scan_vertices_by_label("User").len();
        let post_count = g.scan_vertices_by_label("Post").len();
        let comment_count = g.scan_vertices_by_label("Comment").len();
        let hashtag_count = g.scan_vertices_by_label("Hashtag").len();
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
        let verified_1_hits = g
            .scan_vertices_by_property_eq_live("verified", &Value::Int64(1))
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "none(index-missing)".into());
        let region_0_hits = g
            .scan_vertices_by_property_eq_live("region", &Value::Int64(0))
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "none(index-missing)".into());
        let mut out = vec![
            format!(
                "vertex_count={} edge_count={}",
                g.vertex_count(),
                g.edge_count()
            ),
            format!(
                "label_user={} label_post={} label_comment={} label_hashtag={}",
                user_count, post_count, comment_count, hashtag_count
            ),
            format!(
                "index_hits id=42:{} verified=1:{} region=0:{}",
                id_42_hits, verified_1_hits, region_0_hits
            ),
            format!("index_count={}", indexes.len()),
        ];
        out.extend(indexes.into_iter().map(|line| format!("index={line}")));
        out
    })
}

// ---------------------------------------------------------------------------
// Probe helpers
// ---------------------------------------------------------------------------

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
        _ => format!("{v:?}"),
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
        "{step}: rows={} first={} scanned_v={} scanned_e={} steps={} rows_after_match={} rows_after_with={} rows_before_projection={} groups={} full_sort={} top_k={} limit_truncate={} index_used={} agg_used={} recent_2hop_fast={} var_len_fast={} selectivity_refresh={}",
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
        b.recent_two_hop_projection_fast_path_used,
        b.var_len_terminal_projection_fast_path_used,
        b.selectivity_refresh_ran,
    )
}

fn social_probe_pipeline(name: &str) -> Option<Vec<(&'static str, String)>> {
    match name {
        "feed" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (me:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "followed_users",
                "MATCH (me:User {id: 42})-[:Follows]->(f:User) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("feed").unwrap(),
            ),
        ]),
        "follower_activity" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (me:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "outgoing_follows",
                "MATCH (:User {id: 42})-[:Follows]->(f:User) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("follower_activity").unwrap(),
            ),
        ]),
        "influencer" => Some(vec![
            (
                "total_follow_edges",
                "MATCH (a:User {verified: 1}) WITH a LIMIT 500 MATCH (a)-[:Follows]->(b:User) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("influencer").unwrap(),
            ),
        ]),
        "trending_posts" => Some(vec![
            (
                "total_like_edges",
                "MATCH (u:User {verified: 1}) WITH u LIMIT 500 MATCH (u)-[:Liked]->(p:Post) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("trending_posts").unwrap(),
            ),
        ]),
        "engagement_rate" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (u:User {id: 500}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "user_posts",
                "MATCH (u:User {id: 500})-[:Posted]->(p:Post) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("engagement_rate").unwrap(),
            ),
        ]),
        "fof_recommend" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (me:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "first_hop",
                "MATCH (me:User {id: 42})-[:Follows]->(f:User) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("fof_recommend").unwrap(),
            ),
        ]),
        "hashtag_cooccurrence" => Some(vec![
            (
                "seed_hashtag_exists",
                "MATCH (h:Hashtag {id: 0}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "tagged_posts",
                "MATCH (h:Hashtag {id: 0})<-[:Tagged]-(p:Post) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("hashtag_cooccurrence").unwrap(),
            ),
        ]),
        "content_virality" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (u:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("content_virality").unwrap(),
            ),
        ]),
        "community_bridge" => Some(vec![
            (
                "source_exists",
                "MATCH (a:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "target_exists",
                "MATCH (b:User {id: 999}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("community_bridge").unwrap(),
            ),
        ]),
        "user_segmentation" => Some(vec![
            (
                "total_posted_edges",
                "MATCH (u:User {verified: 1}) WITH u LIMIT 500 MATCH (u)-[:Posted]->(p:Post) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("user_segmentation").unwrap(),
            ),
        ]),
        "thread_depth" => Some(vec![
            (
                "reply_to_count",
                "MATCH (c:Comment)-[:ReplyTo]->(p:Post) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("thread_depth").unwrap(),
            ),
        ]),
        "cross_engagement" => Some(vec![
            (
                "seed_user_exists",
                "MATCH (u:User {id: 42}) RETURN COUNT(*) AS c".into(),
            ),
            (
                "final",
                social_query_text("cross_engagement").unwrap(),
            ),
        ]),
        "verified_influence" => Some(vec![
            (
                "verified_users_count",
                "MATCH (u:User {verified: 1}) WITH u LIMIT 500 RETURN COUNT(*) AS c".into(),
            ),
            (
                "verified_posts_tagged",
                "MATCH (u:User {verified: 1}) WITH u LIMIT 500 \
                 MATCH (u)-[:Posted]->(p:Post)-[:Tagged]->(h:Hashtag) \
                 RETURN COUNT(*) AS c"
                    .into(),
            ),
            (
                "final",
                social_query_text("verified_influence").unwrap(),
            ),
        ]),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Probe endpoints
// ---------------------------------------------------------------------------

#[update]
fn bench_social_probe_query(
    name: String,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = social_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown social probe name '{name}'"))
    })?;
    run_social_query(&gql)
}

#[update]
fn bench_social_probe_gql(
    gql: String,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    run_social_query(&gql)
}

#[update]
fn bench_social_probe_gql_limited(
    gql: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    run_social_query_limited(&gql, max_rows, max_steps)
}

#[update]
fn bench_social_probe_query_limited(
    name: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<gleaph_types::QueryResult, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = social_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown social probe name '{name}'"))
    })?;
    run_social_query_limited(&gql, max_rows, max_steps)
}

#[update]
fn bench_social_probe_query_profiled(
    name: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = social_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown social probe name '{name}'"))
    })?;
    let (result, stages) = run_social_query_profiled_limited(&gql, max_rows, max_steps)?;
    let two_hop_shape = match parse_statement(&gql) {
        Ok(gleaph_gql::ast::Statement::Query(q)) => {
            debug_two_hop_top_k_count_by_terminal_key_query_shape(&q)
        }
        _ => "parse_failed",
    };
    let mut out: Vec<String> = stages
        .into_iter()
        .map(|(stage, ins)| format!("stage={stage} instructions={ins}"))
        .collect();
    out.push(format!(
        "result: rows={} scanned_v={} scanned_e={} exec_steps={} rows_after_match={} rows_after_with={} rows_before_projection={} groups={} full_sort={} top_k={} limit_truncate={} aggregate_fast={} compiled_fast={} recent_2hop_fast={} var_len_fast={} two_hop_count_fast={} edge_label_calls={} edge_record_calls={} is_edge_tombstoned_calls={} reverse_callbacks={} var_len_dfs_calls={} compiled_match_records={} var_len_binding_clones={} var_len_path_contains_checks={} var_len_node_match_checks={} reverse_row_clones={} reverse_node_match_checks={} compiled_group_key_evals={} compiled_group_bucket_probes={} compiled_agg_updates={} compiled_projection_fast_calls={} compiled_projection_input_rows={} compiled_projection_empty_returns={} with_continuation_match_calls={} with_continuation_match_input_rows={} with_continuation_match_output_rows={} joined_match_start_candidates={} joined_match_local_rows_before_inline_where={} joined_match_local_rows_after_inline_where={} with_cont_joined_match_start_candidates={} with_cont_joined_local_rows_before_inline_where={} with_cont_joined_local_rows_after_inline_where={} with_cont_scanned_edges={} with_cont_execution_steps={} outgoing_hop_candidates={} incoming_hop_candidates={} hop_label_rejects={} outgoing_hop_label_rejects={} incoming_hop_label_rejects={} hop_node_rejects={} hop_edge_property_rejects={} hop_where_pushdown_rejects={} var_len_cycle_rejects={} with_cont_hop_label_rejects={} with_cont_outgoing_hop_candidates={} with_cont_incoming_hop_candidates={} with_cont_outgoing_hop_label_rejects={} with_cont_incoming_hop_label_rejects={} with_cont_hop_node_rejects={} with_cont_hop_edge_property_rejects={} with_cont_hop_where_pushdown_rejects={} with_cont_var_len_cycle_rejects={} two_hop_shape={}",
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
        result.stats.breakdown.aggregate_fast_path_used,
        result.stats.breakdown.aggregate_compiled_fast_path_used,
        result.stats.breakdown.recent_two_hop_projection_fast_path_used,
        result.stats.breakdown.var_len_terminal_projection_fast_path_used,
        result.stats.breakdown.two_hop_top_k_count_fast_path_used,
        result.stats.breakdown.edge_label_calls,
        result.stats.breakdown.edge_record_calls,
        result.stats.breakdown.is_edge_tombstoned_calls,
        result.stats.breakdown.reverse_neighbor_callbacks,
        result.stats.breakdown.var_len_dfs_calls,
        result.stats.breakdown.compiled_match_records,
        result.stats.breakdown.var_len_binding_clones,
        result.stats.breakdown.var_len_path_contains_checks,
        result.stats.breakdown.var_len_node_match_checks,
        result.stats.breakdown.reverse_row_clones,
        result.stats.breakdown.reverse_node_match_checks,
        result.stats.breakdown.compiled_group_key_evals,
        result.stats.breakdown.compiled_group_bucket_probes,
        result.stats.breakdown.compiled_agg_updates,
        result.stats.breakdown.compiled_projection_fast_calls,
        result.stats.breakdown.compiled_projection_input_rows,
        result.stats.breakdown.compiled_projection_empty_returns,
        result.stats.breakdown.with_continuation_match_calls,
        result.stats.breakdown.with_continuation_match_input_rows,
        result.stats.breakdown.with_continuation_match_output_rows,
        result.stats.breakdown.joined_match_start_candidates,
        result.stats.breakdown.joined_match_local_rows_before_inline_where,
        result.stats.breakdown.joined_match_local_rows_after_inline_where,
        result.stats.breakdown.with_continuation_joined_match_start_candidates,
        result.stats.breakdown.with_continuation_joined_local_rows_before_inline_where,
        result.stats.breakdown.with_continuation_joined_local_rows_after_inline_where,
        result.stats.breakdown.with_continuation_scanned_edges,
        result.stats.breakdown.with_continuation_execution_steps,
        result.stats.breakdown.outgoing_hop_candidates,
        result.stats.breakdown.incoming_hop_candidates,
        result.stats.breakdown.hop_label_rejects,
        result.stats.breakdown.outgoing_hop_label_rejects,
        result.stats.breakdown.incoming_hop_label_rejects,
        result.stats.breakdown.hop_node_rejects,
        result.stats.breakdown.hop_edge_property_rejects,
        result.stats.breakdown.hop_where_pushdown_rejects,
        result.stats.breakdown.var_len_cycle_rejects,
        result.stats.breakdown.with_continuation_hop_label_rejects,
        result.stats.breakdown.with_continuation_outgoing_hop_candidates,
        result.stats.breakdown.with_continuation_incoming_hop_candidates,
        result.stats.breakdown.with_continuation_outgoing_hop_label_rejects,
        result.stats.breakdown.with_continuation_incoming_hop_label_rejects,
        result.stats.breakdown.with_continuation_hop_node_rejects,
        result.stats.breakdown.with_continuation_hop_edge_property_rejects,
        result.stats.breakdown.with_continuation_hop_where_pushdown_rejects,
        result.stats.breakdown.with_continuation_var_len_cycle_rejects,
        two_hop_shape,
    ));
    Ok(out)
}

#[update]
fn bench_social_probe_gql_profiled(
    gql: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let (result, stages) = run_social_query_profiled_limited(&gql, max_rows, max_steps)?;
    let mut out: Vec<String> = stages
        .into_iter()
        .map(|(stage, ins)| format!("stage={stage} instructions={ins}"))
        .collect();
    out.push(format!(
        "result: rows={} scanned_v={} scanned_e={} exec_steps={} rows_after_match={} rows_after_with={} rows_before_projection={} groups={} full_sort={} top_k={} limit_truncate={} aggregate_fast={} compiled_fast={} recent_2hop_fast={} var_len_fast={} two_hop_count_fast={} edge_label_calls={} edge_record_calls={} is_edge_tombstoned_calls={} reverse_callbacks={} var_len_dfs_calls={} compiled_match_records={} var_len_binding_clones={} var_len_path_contains_checks={} var_len_node_match_checks={} reverse_row_clones={} reverse_node_match_checks={} compiled_group_key_evals={} compiled_group_bucket_probes={} compiled_agg_updates={} compiled_projection_fast_calls={} compiled_projection_input_rows={} compiled_projection_empty_returns={} with_continuation_match_calls={} with_continuation_match_input_rows={} with_continuation_match_output_rows={} joined_match_start_candidates={} joined_match_local_rows_before_inline_where={} joined_match_local_rows_after_inline_where={} with_cont_joined_match_start_candidates={} with_cont_joined_local_rows_before_inline_where={} with_cont_joined_local_rows_after_inline_where={} with_cont_scanned_edges={} with_cont_execution_steps={} outgoing_hop_candidates={} incoming_hop_candidates={} hop_label_rejects={} outgoing_hop_label_rejects={} incoming_hop_label_rejects={} hop_node_rejects={} hop_edge_property_rejects={} hop_where_pushdown_rejects={} var_len_cycle_rejects={} with_cont_hop_label_rejects={} with_cont_outgoing_hop_candidates={} with_cont_incoming_hop_candidates={} with_cont_outgoing_hop_label_rejects={} with_cont_incoming_hop_label_rejects={} with_cont_hop_node_rejects={} with_cont_hop_edge_property_rejects={} with_cont_hop_where_pushdown_rejects={} with_cont_var_len_cycle_rejects={}",
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
        result.stats.breakdown.aggregate_fast_path_used,
        result.stats.breakdown.aggregate_compiled_fast_path_used,
        result.stats.breakdown.recent_two_hop_projection_fast_path_used,
        result.stats.breakdown.var_len_terminal_projection_fast_path_used,
        result.stats.breakdown.two_hop_top_k_count_fast_path_used,
        result.stats.breakdown.edge_label_calls,
        result.stats.breakdown.edge_record_calls,
        result.stats.breakdown.is_edge_tombstoned_calls,
        result.stats.breakdown.reverse_neighbor_callbacks,
        result.stats.breakdown.var_len_dfs_calls,
        result.stats.breakdown.compiled_match_records,
        result.stats.breakdown.var_len_binding_clones,
        result.stats.breakdown.var_len_path_contains_checks,
        result.stats.breakdown.var_len_node_match_checks,
        result.stats.breakdown.reverse_row_clones,
        result.stats.breakdown.reverse_node_match_checks,
        result.stats.breakdown.compiled_group_key_evals,
        result.stats.breakdown.compiled_group_bucket_probes,
        result.stats.breakdown.compiled_agg_updates,
        result.stats.breakdown.compiled_projection_fast_calls,
        result.stats.breakdown.compiled_projection_input_rows,
        result.stats.breakdown.compiled_projection_empty_returns,
        result.stats.breakdown.with_continuation_match_calls,
        result.stats.breakdown.with_continuation_match_input_rows,
        result.stats.breakdown.with_continuation_match_output_rows,
        result.stats.breakdown.joined_match_start_candidates,
        result.stats.breakdown.joined_match_local_rows_before_inline_where,
        result.stats.breakdown.joined_match_local_rows_after_inline_where,
        result.stats.breakdown.with_continuation_joined_match_start_candidates,
        result.stats.breakdown.with_continuation_joined_local_rows_before_inline_where,
        result.stats.breakdown.with_continuation_joined_local_rows_after_inline_where,
        result.stats.breakdown.with_continuation_scanned_edges,
        result.stats.breakdown.with_continuation_execution_steps,
        result.stats.breakdown.outgoing_hop_candidates,
        result.stats.breakdown.incoming_hop_candidates,
        result.stats.breakdown.hop_label_rejects,
        result.stats.breakdown.outgoing_hop_label_rejects,
        result.stats.breakdown.incoming_hop_label_rejects,
        result.stats.breakdown.hop_node_rejects,
        result.stats.breakdown.hop_edge_property_rejects,
        result.stats.breakdown.hop_where_pushdown_rejects,
        result.stats.breakdown.var_len_cycle_rejects,
        result.stats.breakdown.with_continuation_hop_label_rejects,
        result.stats.breakdown.with_continuation_outgoing_hop_candidates,
        result.stats.breakdown.with_continuation_incoming_hop_candidates,
        result.stats.breakdown.with_continuation_outgoing_hop_label_rejects,
        result.stats.breakdown.with_continuation_incoming_hop_label_rejects,
        result.stats.breakdown.with_continuation_hop_node_rejects,
        result.stats.breakdown.with_continuation_hop_edge_property_rejects,
        result.stats.breakdown.with_continuation_hop_where_pushdown_rejects,
        result.stats.breakdown.with_continuation_var_len_cycle_rejects,
    ));
    Ok(out)
}

#[update]
fn bench_social_probe_explain(
    name: String,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let gql = social_query_text(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!("unknown social probe name '{name}'"))
    })?;
    explain_social_query(&gql)
}

#[update]
fn bench_social_probe_explain_gql(
    gql: String,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    explain_social_query(&gql)
}

#[update]
fn bench_social_probe_index_state(restore: bool) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    Ok(index_state_lines())
}

#[update]
fn bench_social_probe_pipeline(
    name: String,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = social_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown social probe pipeline name '{name}'"
        ))
    })?;
    let mut out = Vec::with_capacity(steps.len());
    for (step, gql) in steps {
        match run_social_query(&gql) {
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
fn bench_social_probe_pipeline_limited(
    name: String,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<Vec<String>, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = social_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown social probe pipeline name '{name}'"
        ))
    })?;
    let mut out = Vec::with_capacity(steps.len());
    for (step, gql) in steps {
        match run_social_query_limited(&gql, max_rows, max_steps) {
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
fn bench_social_probe_pipeline_step(
    name: String,
    step_index: u32,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = social_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown social probe pipeline name '{name}'"
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
    let result = run_social_query(gql)?;
    Ok(summarize_probe_step(step, &result))
}

#[update]
fn bench_social_probe_pipeline_step_limited(
    name: String,
    step_index: u32,
    max_rows: Option<u32>,
    max_steps: Option<u64>,
    restore: bool,
) -> Result<String, gleaph_types::GleaphError> {
    if restore {
        restore_state_uncertified()?;
    }
    let steps = social_probe_pipeline(&name).ok_or_else(|| {
        gleaph_types::GleaphError::ValidationError(format!(
            "unknown social probe pipeline name '{name}'"
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
    let result = run_social_query_limited(gql, max_rows, max_steps)?;
    Ok(summarize_probe_step(step, &result))
}

// ---------------------------------------------------------------------------
// Benchmark functions (13 queries)
// ---------------------------------------------------------------------------

#[bench(raw)]
/// Social: user feed — recent posts from followed users (50K users, 100K posts).
///
/// Index seek on User.id, 2-hop traversal (Follows→Posted), temporal pushdown
/// on Posted edge timestamp, ORDER BY + LIMIT.
///
/// GQL:
///   MATCH (me:User {id: 42})-[:Follows]->(f:User)-[e:Posted]->(p:Post)
///   WHERE gleaph_timestamp(e) > {SOCIAL_RECENT_CUTOFF}
///   RETURN p.id, p.viral_score, gleaph_timestamp(e) AS ts
///   ORDER BY ts DESC LIMIT 20
fn bench_social_feed() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(&format!(
                "MATCH (me:User {{id: 42}})-[:Follows]->(f:User)-[e:Posted]->(p:Post) \
                 WHERE gleaph_timestamp(e) > {SOCIAL_RECENT_CUTOFF} \
                 RETURN p.id, p.viral_score, gleaph_timestamp(e) AS ts \
                 ORDER BY ts DESC LIMIT 20"
            )),
            "bench_social_feed",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_feed", false);
    result
}

#[bench(raw)]
/// Social: follower activity — followed users and their post counts (50K users, 100K posts).
///
/// Index seek on User.id, 2-hop (Follows→Posted), GROUP BY + COUNT, ORDER BY.
///
/// GQL:
///   MATCH (:User {id: 42})-[:Follows]->(f:User)-[:Posted]->(p:Post)
///   RETURN f.id, f.verified, COUNT(*) AS post_count
///   ORDER BY post_count DESC
fn bench_social_follower_activity() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (:User {id: 42})-[:Follows]->(f:User)-[:Posted]->(p:Post) \
                 RETURN f.id, f.verified, COUNT(*) AS post_count \
                 ORDER BY post_count DESC",
            ),
            "bench_social_follower_activity",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_follower_activity", false);
    result
}

#[bench(raw)]
/// Social: influencer ranking — top 10 users by follower count (500 users, ~4K edges).
///
/// WITH LIMIT pre-caps source vertices, then fan-out + COUNT aggregation.
///
/// GQL:
///   MATCH (a:User {verified: 1}) WITH a LIMIT 500
///   MATCH (a)-[:Follows]->(b:User)
///   RETURN b.id, COUNT(*) AS followers
///   ORDER BY followers DESC LIMIT 10
fn bench_social_influencer() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (a:User {verified: 1}) \
                 WITH a LIMIT 500 \
                 MATCH (a)-[:Follows]->(b:User) \
                 RETURN b.id, COUNT(*) AS followers \
                 ORDER BY followers DESC LIMIT 10",
            ),
            "bench_social_influencer",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_influencer", false);
    result
}

#[bench(raw)]
/// Social: trending posts — most-liked posts in recent window (500 users, ~2K likes).
///
/// WITH LIMIT pre-caps source vertices. Timestamp pushdown on Liked edges, COUNT + top-K sort.
///
/// GQL:
///   MATCH (u:User {verified: 1}) WITH u LIMIT 500
///   MATCH (u)-[e:Liked]->(p:Post)
///   WHERE gleaph_timestamp(e) > {SOCIAL_RECENT_CUTOFF}
///   RETURN p.id, COUNT(*) AS likes
///   ORDER BY likes DESC LIMIT 10
fn bench_social_trending_posts() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(&format!(
                "MATCH (u:User {{verified: 1}}) \
                 WITH u LIMIT 500 \
                 MATCH (u)-[e:Liked]->(p:Post) \
                 WHERE gleaph_timestamp(e) > {SOCIAL_RECENT_CUTOFF} \
                 RETURN p.id, COUNT(*) AS likes \
                 ORDER BY likes DESC LIMIT 10"
            )),
            "bench_social_trending_posts",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_trending_posts", false);
    result
}

#[bench(raw)]
/// Social: engagement rate for a single user (user 500, 2 posts).
///
/// Index anchor on id, OPTIONAL MATCH with reverse-chain-anchor like fan-in,
/// COUNT(DISTINCT) multi-aggregate.  User 500 sits in the mid-range of the
/// cubic power-law Liked distribution (~60 likes per post).
///
/// GQL:
///   MATCH (u:User {id: 500})-[:Posted]->(p:Post)
///   OPTIONAL MATCH (liker:User)-[:Liked]->(p)
///   RETURN u.id, COUNT(DISTINCT p) AS posts, COUNT(DISTINCT liker) AS likers
fn bench_social_engagement_rate() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (u:User {id: 500})-[:Posted]->(p:Post) \
                 OPTIONAL MATCH (liker:User)-[:Liked]->(p) \
                 RETURN u.id, COUNT(DISTINCT p) AS posts, COUNT(DISTINCT liker) AS likers",
            ),
            "bench_social_engagement_rate",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_engagement_rate", false);
    result
}

#[bench(raw)]
/// Social: friend-of-friend recommendation (50K users).
///
/// 2-hop expand via Follows, GROUP BY dedup on recommended user, COUNT, LIMIT.
///
/// GQL:
///   MATCH (me:User {id: 42})-[:Follows]->(f:User)-[:Follows]->(rec:User)
///   WHERE rec.id <> 42
///   RETURN rec.id, COUNT(*) AS mutual
///   ORDER BY mutual DESC LIMIT 10
fn bench_social_fof_recommend() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (me:User {id: 42})-[:Follows]->(f:User)-[:Follows]->(rec:User) \
                 WHERE rec.id <> 42 \
                 RETURN rec.id, COUNT(*) AS mutual \
                 ORDER BY mutual DESC LIMIT 10",
            ),
            "bench_social_fof_recommend",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_fof_recommend", false);
    result
}

#[bench(raw)]
/// Social: hashtag co-occurrence (5K hashtags, 100K posts, ~150K tags).
///
/// Reverse + forward fan-out from seed hashtag via Tagged edges,
/// GROUP BY on second-hop hashtag category.
///
/// GQL:
///   MATCH (h:Hashtag {id: 0})<-[:Tagged]-(p:Post)-[:Tagged]->(other:Hashtag)
///   WHERE other.id <> 0
///   RETURN other.category, COUNT(*) AS co_count
///   ORDER BY co_count DESC LIMIT 10
fn bench_social_hashtag_cooccurrence() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (h:Hashtag {id: 0})<-[:Tagged]-(p:Post)-[:Tagged]->(other:Hashtag) \
                 WHERE other.id <> 0 \
                 RETURN other.category, COUNT(*) AS co_count \
                 ORDER BY co_count DESC LIMIT 10",
            ),
            "bench_social_hashtag_cooccurrence",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_hashtag_cooccurrence", false);
    result
}

#[bench(raw)]
/// Social: content virality — variable-length path via mixed edge labels (50K users, 100K posts).
///
/// Uses label expression `Follows|Liked` with variable-length path `*1..3`.
///
/// GQL:
///   MATCH (u:User {id: 42})-[:Follows|Liked*1..3]->(p:Post)
///   RETURN p.id, p.viral_score LIMIT 20
fn bench_social_content_virality() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (u:User {id: 42})-[:Follows|Liked*1..3]->(p:Post) \
                 RETURN p.id, p.viral_score LIMIT 20",
            ),
            "bench_social_content_virality",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_content_virality", false);
    result
}

#[bench(raw)]
/// Social: community bridge — shortest path between two users (50K users).
///
/// BFS with variable-length path `*1..5`, dual-anchor constraints.
///
/// GQL:
///   MATCH SHORTEST p = (a:User {id: 42})-[*1..5]->(b:User {id: 999})
///   RETURN p
fn bench_social_community_bridge() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH SHORTEST p = (a:User {id: 42})-[*1..5]->(b:User {id: 999}) \
                 RETURN p",
            ),
            "bench_social_community_bridge",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_community_bridge", false);
    result
}

#[bench(raw)]
/// Social: user segmentation by content type (500 users, ~1K posts).
///
/// WITH LIMIT pre-caps source vertices.
/// CASE WHEN in SUM, multi-aggregate (COUNT + SUM + AVG), GROUP BY content_type.
///
/// GQL:
///   MATCH (u:User {verified: 1}) WITH u LIMIT 500
///   MATCH (u)-[:Posted]->(p:Post)
///   RETURN p.content_type, COUNT(DISTINCT u) AS users,
///     COUNT(DISTINCT p) AS total_posts,
///     SUM(CASE WHEN p.viral_score > 80 THEN 1 ELSE 0 END) AS viral_posts,
///     AVG(p.viral_score) AS avg_score
///   ORDER BY users DESC
fn bench_social_user_segmentation() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (u:User {verified: 1}) \
                 WITH u LIMIT 500 \
                 MATCH (u)-[:Posted]->(p:Post) \
                 RETURN p.content_type, COUNT(DISTINCT u) AS users, \
                   COUNT(DISTINCT p) AS total_posts, \
                   SUM(CASE WHEN p.viral_score > 80 THEN 1 ELSE 0 END) AS viral_posts, \
                   AVG(p.viral_score) AS avg_score \
                 ORDER BY users DESC",
            ),
            "bench_social_user_segmentation",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_user_segmentation", false);
    result
}

#[bench(raw)]
/// Social: thread depth — posts with deep comment reply chains (2.5K verified users, 5K posts).
///
/// Chained fixed + variable-length hops (reverse direction), aggregation on intermediate.
/// Single MATCH required for correct anchor planning across reverse + var-length hops.
///
/// GQL:
///   MATCH (u:User {verified: 1})-[:Posted]->(p:Post)<-[:ReplyTo]-(c:Comment)<-[:ReplyToComment*1..3]-(deep:Comment)
///   RETURN p.id, COUNT(DISTINCT deep) AS chain_length
///   ORDER BY chain_length DESC LIMIT 10
fn bench_social_thread_depth() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (u:User {verified: 1})-[:Posted]->(p:Post)<-[:ReplyTo]-(c:Comment)<-[:ReplyToComment*1..3]-(deep:Comment) \
                 RETURN p.id, COUNT(DISTINCT deep) AS chain_length \
                 ORDER BY chain_length DESC LIMIT 10",
            ),
            "bench_social_thread_depth",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_thread_depth", false);
    result
}

#[bench(raw)]
/// Social: cross engagement — UNION ALL across 3 engagement types (50K users).
///
/// 3-branch UNION ALL with heterogeneous edge hops (Posted, Liked, Authored).
///
/// GQL:
///   MATCH (u:User {id: 42})-[e:Posted]->(p:Post)
///   RETURN p.id AS target_id, 'posted' AS action, gleaph_timestamp(e) AS ts
///   UNION ALL
///   MATCH (u:User {id: 42})-[e:Liked]->(p:Post)
///   RETURN p.id AS target_id, 'liked' AS action, gleaph_timestamp(e) AS ts
///   UNION ALL
///   MATCH (u:User {id: 42})-[e:Authored]->(c:Comment)
///   RETURN c.id AS target_id, 'commented' AS action, gleaph_timestamp(e) AS ts
fn bench_social_cross_engagement() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (u:User {id: 42})-[e:Posted]->(p:Post) \
                 RETURN p.id AS target_id, 'posted' AS action, gleaph_timestamp(e) AS ts \
                 UNION ALL \
                 MATCH (u:User {id: 42})-[e:Liked]->(p:Post) \
                 RETURN p.id AS target_id, 'liked' AS action, gleaph_timestamp(e) AS ts \
                 UNION ALL \
                 MATCH (u:User {id: 42})-[e:Authored]->(c:Comment) \
                 RETURN c.id AS target_id, 'commented' AS action, gleaph_timestamp(e) AS ts",
            ),
            "bench_social_cross_engagement",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_cross_engagement", false);
    result
}

#[bench(raw)]
/// Social: verified influence — verified users and their hashtag reach (500 users, 5K hashtags).
///
/// WITH LIMIT pre-caps source vertices. 2-hop (Posted→Tagged), COLLECT(DISTINCT), COUNT(DISTINCT).
///
/// GQL:
///   MATCH (u:User {verified: 1}) WITH u LIMIT 500
///   MATCH (u)-[:Posted]->(p:Post)-[:Tagged]->(h:Hashtag)
///   RETURN u.id, COLLECT(DISTINCT h.category) AS categories,
///     COUNT(DISTINCT h) AS hashtag_reach
///   ORDER BY hashtag_reach DESC LIMIT 10
fn bench_social_verified_influence() -> canbench_rs::BenchResult {
    restore_and_warm_planner_stats();
    let result = canbench_rs::bench_fn(|| {
        let qr = require_non_empty_rows(
            run_social_query(
                "MATCH (u:User {verified: 1}) \
                 WITH u LIMIT 500 \
                 MATCH (u)-[:Posted]->(p:Post)-[:Tagged]->(h:Hashtag) \
                 RETURN u.id, COLLECT(DISTINCT h.category) AS categories, \
                   COUNT(DISTINCT h) AS hashtag_reach \
                 ORDER BY hashtag_reach DESC LIMIT 10",
            ),
            "bench_social_verified_influence",
        );
        let _ = std::hint::black_box(qr);
    });
    assert_within_ic_limits(&result, "bench_social_verified_influence", false);
    result
}
