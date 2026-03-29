#[cfg(feature = "bench-ecom")]
mod ecom;

#[cfg(feature = "bench-social")]
mod social;

#[cfg(feature = "bench-timeline")]
mod timeline;

#[cfg(not(any(
    feature = "bench-ecom",
    feature = "bench-social",
    feature = "bench-timeline"
)))]
mod core_benchmarks {
    use canbench_rs::bench;
    use gleaph_algo::{
        bfs, budget::CountingBudget, pagerank, pagerank::PageRankConfig,
        recommend::RecommendConfig, sssp, sssp::SsspConfig,
    };

    use crate::state::{
        init_state, persist_state_metadata, restore_state_uncertified, with_state, with_state_mut,
    };

    // ---------------------------------------------------------------------------
    // Insert benchmarks
    // ---------------------------------------------------------------------------

    #[bench(raw)]
    /// Benchmarks inserting a single edge into an empty graph.
    fn bench_add_edge_empty() -> canbench_rs::BenchResult {
        init_state(1024, 0).unwrap();
        canbench_rs::bench_fn(|| {
            with_state_mut(|g| {
                g.insert(0, 1, 0, 1.0, 100).unwrap();
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks inserting one edge into a graph pre-populated with ~1K edges.
    fn bench_add_edge_populated_1k() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        with_state_mut(|g| {
            for i in 0..1000u32 {
                let src = i % 256;
                let dst = (i * 7 + 3) % 256;
                g.insert(src, dst, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            with_state_mut(|g| {
                g.insert(100, 200, 0, 2.5, 99999).unwrap();
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks inserting 100 edges in a tight loop.
    fn bench_bulk_insert_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        canbench_rs::bench_fn(|| {
            with_state_mut(|g| {
                for i in 0..100u32 {
                    let src = i % 256;
                    let dst = (i * 13 + 5) % 256;
                    g.insert(src, dst, 0, 1.0, i as u64).unwrap();
                }
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks inserting 1,000 edges in a tight loop.
    fn bench_bulk_insert_1k() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        canbench_rs::bench_fn(|| {
            with_state_mut(|g| {
                for i in 0..1_000u32 {
                    let src = i % 256;
                    let dst = (i * 13 + 5) % 256;
                    g.insert(src, dst, 0, 1.0, i as u64).unwrap();
                }
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks inserting 10,000 edges in a tight loop.
    fn bench_bulk_insert_10k() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        canbench_rs::bench_fn(|| {
            with_state_mut(|g| {
                for i in 0..10_000u32 {
                    let src = i % 512;
                    let dst = (i * 7 + 3) % 512;
                    g.insert(src, dst, 0, 1.0, i as u64).unwrap();
                }
            });
        })
    }

    // ---------------------------------------------------------------------------
    // GQL bulk edge creation benchmarks
    // ---------------------------------------------------------------------------

    #[bench(raw)]
    /// Benchmarks creating 100 edges via individual GQL CREATE statements.
    /// Baseline for comparison with the bulk `batch_mutate` path.
    fn bench_gql_create_edge_sequential_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        // Pre-create vertices so the CREATE only inserts edges.
        setup_social_users(256);
        canbench_rs::bench_fn(|| {
            for i in 0..100u32 {
                let src = i % 256;
                let dst = (i * 13 + 5) % 256;
                let _ = std::hint::black_box(crate::gql_bridge::mutate(&format!(
                    "MATCH (a:User {{id: {src}}}), (b:User {{id: {dst}}}) INSERT (a)-[:FOLLOWS {{idx: {i}}}]->(b)"
                )));
            }
        })
    }

    #[bench(raw)]
    /// Benchmarks creating 100 edges via GQL `batch_mutate` (one CREATE per statement, batched).
    fn bench_gql_create_edge_batch_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        setup_social_users(256);
        let stmts: Vec<String> = (0..100u32)
            .map(|i| {
                let src = i % 256;
                let dst = (i * 13 + 5) % 256;
                format!(
                    "MATCH (a:User {{id: {src}}}), (b:User {{id: {dst}}}) INSERT (a)-[:FOLLOWS {{idx: {i}}}]->(b)"
                )
            })
            .collect();
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::batch_mutate_tracked(&stmts));
        })
    }

    #[bench(raw)]
    /// Benchmarks creating 100 vertex+edge pairs via individual GQL INSERT (inline pattern).
    fn bench_gql_create_vertex_edge_sequential_100() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        canbench_rs::bench_fn(|| {
            for i in 0..100u32 {
                let score_a = (i.wrapping_mul(37).wrapping_add(13)) % 100;
                let score_b = (i.wrapping_mul(59).wrapping_add(7)) % 100;
                let _ = std::hint::black_box(crate::gql_bridge::mutate(&format!(
                    "INSERT (:User {{id: {}, score: {score_a}}})-[:FOLLOWS]->(:User {{id: {}, score: {score_b}}})",
                    i * 2,
                    i * 2 + 1
                )));
            }
        })
    }

    #[bench(raw)]
    /// Benchmarks creating 100 vertex+edge pairs via GQL `batch_mutate` (inline pattern).
    fn bench_gql_create_vertex_edge_batch_100() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        let stmts: Vec<String> = (0..100u32)
            .map(|i| {
                let score_a = (i.wrapping_mul(37).wrapping_add(13)) % 100;
                let score_b = (i.wrapping_mul(59).wrapping_add(7)) % 100;
                format!(
                    "INSERT (:User {{id: {}, score: {score_a}}})-[:FOLLOWS]->(:User {{id: {}, score: {score_b}}})",
                    i * 2, i * 2 + 1
                )
            })
            .collect();
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::batch_mutate_tracked(&stmts));
        })
    }

    #[bench(raw)]
    /// Benchmarks creating 500 edges (with properties) via GQL `batch_mutate` on a
    /// pre-populated social graph of 300 users. Measures realistic GQL mutation throughput.
    fn bench_gql_create_edge_batch_500_on_populated() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_follow_edges(300, 1000);
        let stmts: Vec<String> = (0..500u32)
            .map(|i| {
                let src = (i.wrapping_mul(1_000_003)) % 300;
                let dst = (i.wrapping_mul(2_654_435_761).wrapping_add(i)) % 300;
                let dst = if dst == src { (dst + 1) % 300 } else { dst };
                format!(
                    "MATCH (a:User {{id: {src}}}), (b:User {{id: {dst}}}) INSERT (a)-[:LIKES {{ts: {i}}}]->(b)"
                )
            })
            .collect();
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::batch_mutate_tracked(&stmts));
        })
    }

    // ---------------------------------------------------------------------------
    // Query benchmarks
    // ---------------------------------------------------------------------------

    #[bench(raw)]
    /// Benchmarks neighbor lookup for a vertex with degree about 10.
    fn bench_get_neighbors_degree_10() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        with_state_mut(|g| {
            for i in 1..=10u32 {
                g.insert(0, i, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let _ = std::hint::black_box(g.collect_neighbors(0));
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks neighbor lookup for a vertex with degree about 100.
    fn bench_get_neighbors_degree_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        with_state_mut(|g| {
            for i in 1..=100u32 {
                g.insert(0, i % 256, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let _ = std::hint::black_box(g.collect_neighbors(0));
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks neighbor lookup when recent edges are buffered in segment logs.
    fn bench_get_neighbors_with_log() -> canbench_rs::BenchResult {
        init_state(64, 0).unwrap();
        with_state_mut(|g| {
            for i in 1..=30u32 {
                g.insert(0, i % 64, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let _ = std::hint::black_box(g.collect_neighbors(0));
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks producing a graph statistics snapshot.
    fn bench_get_stats() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        with_state_mut(|g| {
            for i in 0..100u32 {
                g.insert(i % 256, (i + 1) % 256, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let _ = std::hint::black_box(g.stats());
            });
        })
    }

    // ---------------------------------------------------------------------------
    // Structural operation benchmarks
    // ---------------------------------------------------------------------------

    #[bench(raw)]
    /// Benchmarks a direct rebalance operation on a moderately populated graph.
    fn bench_rebalance() -> canbench_rs::BenchResult {
        init_state(64, 0).unwrap();
        with_state_mut(|g| {
            for i in 0..200u32 {
                let src = i % 64;
                let dst = (i * 7 + 1) % 64;
                g.insert(src, dst, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            with_state_mut(|g| {
                g.rebalance_wrapper(0).unwrap();
            });
        })
    }

    #[bench(raw)]
    /// Benchmarks PMA resize on a populated graph.
    fn bench_resize() -> canbench_rs::BenchResult {
        init_state(128, 0).unwrap();
        with_state_mut(|g| {
            for i in 0..500u32 {
                let src = i % 128;
                let dst = (i * 7 + 1) % 128;
                g.insert(src, dst, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            with_state_mut(|g| {
                g.resize().unwrap();
            });
        })
    }

    // ---------------------------------------------------------------------------
    // Realistic GQL workload benchmarks
    // ---------------------------------------------------------------------------

    /// Create `n` User vertices: id, score (0..99), verified (0 or 1 every 5th).
    fn setup_social_users(n: u32) {
        for i in 0..n {
            let score = (i.wrapping_mul(37).wrapping_add(13)) % 100;
            let verified = u32::from(i % 5 == 0);
            crate::gql_bridge::mutate(&format!(
                "INSERT (:User {{id: {i}, score: {score}, verified: {verified}}})"
            ))
            .expect("setup: CREATE User failed");
        }
    }

    /// Create `n` Product vertices: id, price (5..999), rating (1..5).
    fn setup_product_catalog(n: u32) {
        for i in 0..n {
            let price = (i.wrapping_mul(17).wrapping_add(5)) % 995 + 5;
            let rating = (i % 5) + 1;
            crate::gql_bridge::mutate(&format!(
                "INSERT (:Product {{id: {i}, price: {price}, rating: {rating}}})"
            ))
            .expect("setup: CREATE Product failed");
        }
    }

    /// Insert edges with a Zipf-like (power-law) in-degree distribution.
    fn build_follow_edges(num_vertices: u32, num_edges: u32) {
        with_state_mut(|g| {
            for i in 0..num_edges {
                let src = (i.wrapping_mul(1_000_003)) % num_vertices;
                let r = (i.wrapping_mul(2_654_435_761).wrapping_add(i ^ (i >> 5))) % num_vertices;
                let dst = num_vertices / (r + 1);
                if dst < num_vertices && dst != src {
                    g.insert(src, dst, 0, 1.0, i as u64).unwrap_or(());
                }
            }
        });
    }

    /// Insert labeled follow edges with unit weight.
    fn build_labeled_follow_edges(num_vertices: u32, num_edges: u32) {
        with_state_mut(|g| {
            for i in 0..num_edges {
                let src = (i.wrapping_mul(1_000_003)) % num_vertices;
                let r = (i.wrapping_mul(2_654_435_761).wrapping_add(i ^ (i >> 5))) % num_vertices;
                let dst = num_vertices / (r + 1);
                if dst < num_vertices && dst != src {
                    g.create_edge(src, dst, Some("FOLLOWS".into()), vec![], 1.0, i as u64)
                        .unwrap_or(());
                }
            }
        });
    }

    /// Insert labeled follow edges with a small deterministic weight range.
    fn build_weighted_follow_edges(num_vertices: u32, num_edges: u32) {
        with_state_mut(|g| {
            for i in 0..num_edges {
                let src = (i.wrapping_mul(1_000_003)) % num_vertices;
                let r = (i.wrapping_mul(2_654_435_761).wrapping_add(i ^ (i >> 5))) % num_vertices;
                let dst = num_vertices / (r + 1);
                if dst < num_vertices && dst != src {
                    let weight = ((i % 7) + 1) as f32;
                    g.create_edge(src, dst, Some("FOLLOWS".into()), vec![], weight, i as u64)
                        .unwrap_or(());
                }
            }
        });
    }

    #[bench(raw)]
    /// Realistic: follower scan for a specific user, with scored results.
    fn bench_realistic_social_follower_lookup() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_follow_edges(300, 1_000);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "id".into(),
            gleaph_types::IndexType::Equality,
        )
        .expect("setup: create_index failed");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (me:User {id: 50})-[]->(f:User) RETURN f.id, f.score",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: verified-follower filter (scan + filter + multi-prop return).
    fn bench_realistic_social_verified_followers() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_follow_edges(300, 1_000);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "id".into(),
            gleaph_types::IndexType::Equality,
        )
        .expect("setup: create_index failed");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (me:User {id: 50})-[]->(f:User) WHERE f.verified = 1 \
                 RETURN f.id, f.score ORDER BY f.score DESC",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: top-10 influencers by follower count (full expand + aggregate).
    fn bench_realistic_social_top_influencers() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_follow_edges(300, 1_000);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (a:User)-[]->(b:User) \
                 RETURN b.id, COUNT(*) ORDER BY COUNT(*) DESC LIMIT 10",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: top-10 influencers by follower count on labeled edges.
    fn bench_realistic_social_top_influencers_labeled() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_labeled_follow_edges(300, 1_000);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (a:User)-[:FOLLOWS]->(b:User) \
                 RETURN b.id, COUNT(*) ORDER BY COUNT(*) DESC LIMIT 10",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: top follower buckets grouped by terminal property instead of id.
    fn bench_realistic_social_top_influencers_by_score() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_follow_edges(300, 1_000);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (a:User)-[]->(b:User) \
                 RETURN b.score, COUNT(*) ORDER BY COUNT(*) DESC LIMIT 10",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: top outbound groups grouped by the start-side property.
    fn bench_realistic_social_top_outbound_by_start_score() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_follow_edges(300, 1_000);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (a:User)-[]->(b:User) \
                 RETURN a.score, COUNT(*) ORDER BY COUNT(*) DESC LIMIT 10",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: top-10 influencers by weighted incoming sum.
    fn bench_realistic_social_top_influencers_by_weight() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(300);
        build_weighted_follow_edges(300, 1_000);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (a:User)-[e:FOLLOWS]->(b:User) \
                 RETURN b.id, SUM(gleaph_weight(e)) ORDER BY SUM(gleaph_weight(e)) DESC LIMIT 10",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: friend-of-friend recommendation (2-hop expand + aggregate).
    fn bench_realistic_fof_recommendation() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(200);
        build_follow_edges(200, 500);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "id".into(),
            gleaph_types::IndexType::Equality,
        )
        .expect("setup: create_index failed");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (me:User {id: 0})-[]->(a:User)-[]->(rec:User) \
                 RETURN rec.id, COUNT(*) ORDER BY COUNT(*) DESC LIMIT 5",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: friend-of-friend recommendation through WITH aggregation.
    ///
    /// This isolates the `WITH ... COUNT(*) AS ... ORDER BY ... RETURN ...` path
    /// used by the general executor instead of the direct aggregate projection path.
    fn bench_realistic_fof_with_aggregate() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_social_users(200);
        build_follow_edges(200, 500);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "id".into(),
            gleaph_types::IndexType::Equality,
        )
        .expect("setup: create_index failed");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (me:User {id: 0})-[]->(a:User)-[]->(rec:User) \
                 WITH rec.id AS rec_id, COUNT(*) AS mutual \
                 ORDER BY mutual DESC \
                 RETURN rec_id, mutual LIMIT 5",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: direct algorithm recommendation API on a medium social subgraph.
    ///
    /// Uses unlabeled edges and wildcard label matching so the benchmark isolates
    /// the `recommend()` traversal/scoring data structures rather than label checks.
    fn bench_realistic_recommend_api() -> canbench_rs::BenchResult {
        init_state(1_024, 0).unwrap();
        setup_social_users(512);
        build_follow_edges(512, 4_096);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::api::recommend_query(
                0,
                RecommendConfig {
                    edge_label: String::new(),
                    max_hops: 2,
                    limit: 10,
                    ts_range: None,
                    exclude_known: true,
                },
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: direct BFS kernel on a medium social subgraph.
    ///
    /// The traversal runs without a target to keep the frontier and distance maps
    /// hot across a wider portion of the graph.
    fn bench_realistic_bfs_api() -> canbench_rs::BenchResult {
        init_state(1_024, 0).unwrap();
        setup_social_users(512);
        build_follow_edges(512, 4_096);
        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let mut budget = CountingBudget::new(50_000_000);
                let _ = std::hint::black_box(bfs::bfs(
                    g,
                    0,
                    &bfs::BfsConfig {
                        max_depth: Some(4),
                        max_visited: Some(5_000),
                        ..Default::default()
                    },
                    &mut budget,
                ));
            });
        })
    }

    #[bench(raw)]
    /// Realistic: direct Dijkstra kernel on a medium social subgraph.
    ///
    /// Uses uniform weights but still exercises the shortest-path frontier,
    /// distance, and predecessor maps across a non-trivial reachable set.
    fn bench_realistic_sssp_api() -> canbench_rs::BenchResult {
        init_state(1_024, 0).unwrap();
        setup_social_users(512);
        build_follow_edges(512, 4_096);
        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let mut budget = CountingBudget::new(50_000_000);
                let _ = std::hint::black_box(sssp::dijkstra(
                    g,
                    0,
                    &SsspConfig {
                        max_visited: Some(5_000),
                        ..Default::default()
                    },
                    &mut budget,
                ));
            });
        })
    }

    #[bench(raw)]
    /// Realistic: direct PageRank kernel on a medium social subgraph.
    ///
    /// The graph is small enough to finish well within IC limits while still
    /// exercising the dense rank/adjaency structures in the PageRank kernel.
    fn bench_realistic_pagerank_api() -> canbench_rs::BenchResult {
        init_state(1_024, 0).unwrap();
        setup_social_users(512);
        build_follow_edges(512, 4_096);
        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let mut budget = CountingBudget::new(50_000_000);
                let _ = std::hint::black_box(pagerank::pagerank(
                    g,
                    &PageRankConfig {
                        max_iterations: 10,
                        convergence_threshold: 1e-6,
                        ..Default::default()
                    },
                    &mut budget,
                ));
            });
        })
    }

    #[bench(raw)]
    /// Realistic: catalog filter + sort + limit.
    fn bench_realistic_catalog_filter_sort() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_product_catalog(300);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (p:Product) WHERE p.rating >= 4 \
                 RETURN p.id, p.price ORDER BY p.price LIMIT 20",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: rating histogram (scan + group-by aggregation, 5 groups).
    fn bench_realistic_catalog_rating_histogram() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_product_catalog(200);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (p:Product) RETURN p.rating, COUNT(*) ORDER BY p.rating",
            ));
        })
    }

    #[bench(raw)]
    /// Realistic: GQL bulk vertex creation with properties (100 records).
    fn bench_realistic_gql_bulk_create_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        canbench_rs::bench_fn(|| {
            for i in 0u32..100 {
                let score = (i.wrapping_mul(37).wrapping_add(13)) % 100;
                let _ = std::hint::black_box(crate::gql_bridge::mutate(&format!(
                    "INSERT (:User {{id: {i}, score: {score}}})"
                )));
            }
        })
    }

    // ---------------------------------------------------------------------------
    // Lifecycle benchmarks
    // ---------------------------------------------------------------------------

    #[bench(raw)]
    /// Benchmarks persisting and restoring state across an upgrade round-trip.
    fn bench_upgrade_roundtrip() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        with_state_mut(|g| {
            for i in 0..500u32 {
                let src = i % 256;
                let dst = (i * 7 + 1) % 256;
                g.insert(src, dst, 0, 1.0, i as u64).unwrap();
            }
        });

        canbench_rs::bench_fn(|| {
            persist_state_metadata().unwrap();
            restore_state_uncertified().unwrap();
        })
    }

    // ---------------------------------------------------------------------------
    // GQL query benchmarks (cost-model calibration)
    // ---------------------------------------------------------------------------

    /// Insert `n` labeled vertices, each with an `id` and `score` property.
    fn setup_labeled_vertices(n: u32, label: &str) {
        for i in 0..n {
            crate::gql_bridge::mutate(&format!("INSERT (:{label} {{id: {i}, score: {i}}})"))
                .expect("setup: CREATE vertex failed");
        }
    }

    #[bench(raw)]
    /// GQL full scan: iterate all 100 unlabeled vertices and project one property.
    fn bench_gql_full_scan_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        setup_labeled_vertices(100, "Person");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query("MATCH (n:Person) RETURN n.id"));
        })
    }

    #[bench(raw)]
    /// GQL label scan: 200 vertices, 50 labelled `Rare`, scan only those.
    fn bench_gql_label_scan_50_of_200() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_labeled_vertices(150, "Common");
        setup_labeled_vertices(50, "Rare");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query("MATCH (n:Rare) RETURN n.id"));
        })
    }

    #[bench(raw)]
    /// GQL expand: single hub vertex with 10 outgoing edges, traverse all.
    fn bench_gql_expand_degree_10() -> canbench_rs::BenchResult {
        init_state(64, 0).unwrap();
        setup_labeled_vertices(10, "Target");
        with_state_mut(|g| {
            for dst in 1u32..=10 {
                g.insert(0, dst, 0, 1.0, dst as u64).unwrap_or(());
            }
        });
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (a)-[]->(b:Target) RETURN b.id",
            ));
        })
    }

    #[bench(raw)]
    /// GQL property filter: 100 vertices, WHERE predicate on a scalar property.
    fn bench_gql_property_filter_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        setup_labeled_vertices(100, "Item");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Item) WHERE n.score >= 0 RETURN n.id",
            ));
        })
    }

    #[bench(raw)]
    /// GQL sort: 50 vertices, ORDER BY a single scalar property.
    fn bench_gql_sort_50() -> canbench_rs::BenchResult {
        init_state(128, 0).unwrap();
        setup_labeled_vertices(50, "Sorted");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Sorted) RETURN n.id ORDER BY n.id",
            ));
        })
    }

    #[bench(raw)]
    /// GQL aggregate: 50 vertices, COUNT(*) grouped by a property value.
    fn bench_gql_aggregate_50() -> canbench_rs::BenchResult {
        init_state(128, 0).unwrap();
        setup_labeled_vertices(50, "Agg");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Agg) RETURN n.score, COUNT(*)",
            ));
        })
    }

    #[bench(raw)]
    /// GQL constant project: 100 vertices, RETURN 1 (no property lookup).
    fn bench_gql_project_constant_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        setup_labeled_vertices(100, "Person");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query("MATCH (n:Person) RETURN 1"));
        })
    }

    #[bench(raw)]
    /// GQL LIMIT overhead: 100 vertices, LIMIT 200 (all rows pass).
    fn bench_gql_limit_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        setup_labeled_vertices(100, "Person");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Person) RETURN n.id LIMIT 200",
            ));
        })
    }

    #[bench(raw)]
    /// GQL index seek: 100 indexed vertices, equality lookup returning 1 row.
    fn bench_gql_index_seek_1_of_100() -> canbench_rs::BenchResult {
        init_state(256, 0).unwrap();
        setup_labeled_vertices(100, "Person");
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "id".into(),
            gleaph_types::IndexType::Equality,
        )
        .expect("setup: create_index failed");
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Person {id: 42}) RETURN n.score",
            ));
        })
    }

    #[bench(raw)]
    /// GQL SHORTEST path: 6-node linear chain, BFS depth 5.
    fn bench_gql_shortest_path_chain() -> canbench_rs::BenchResult {
        init_state(64, 0).unwrap();
        for (i, label) in [
            (0u32, "Start"),
            (1, "Step"),
            (2, "Step"),
            (3, "Step"),
            (4, "Step"),
            (5, "End"),
        ] {
            crate::gql_bridge::mutate(&format!("INSERT (:{label} {{id: {i}}})"))
                .expect("setup: CREATE vertex failed");
        }
        with_state_mut(|g| {
            for i in 0u32..5 {
                g.insert(i, i + 1, 0, 1.0, i as u64).unwrap_or(());
            }
        });
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH SHORTEST (s:Start)-[*1..9]->(e:End) RETURN e.id",
            ));
        })
    }

    // ---------------------------------------------------------------------------
    // Planner extension benchmarks (Phase 4–11)
    // ---------------------------------------------------------------------------

    /// Insert `n` items with `id`, `score` (0..n-1), and `country` (cycle JP/US/DE).
    fn setup_planner_bench_items(n: u32) {
        for i in 0..n {
            let country = match i % 3 {
                0 => "JP",
                1 => "US",
                _ => "DE",
            };
            crate::gql_bridge::mutate(&format!(
                "INSERT (:Item {{id: {i}, score: {i}, country: '{country}', email: 'u{i}@test.com'}})"
            ))
            .expect("setup: CREATE Item failed");
        }
    }

    // ── Phase 4: Direct Range IndexScan ──

    #[bench(raw)]
    /// BEFORE: range filter without range index (full scan + post-filter).
    fn bench_gql_range_filter_no_index_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        // No range index created.
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Item) WHERE n.score >= 200 RETURN n.id",
            ));
        })
    }

    #[bench(raw)]
    /// AFTER: range filter with range index → IndexScan.
    fn bench_gql_range_index_scan_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "score".into(),
            gleaph_types::IndexType::Range,
        )
        .unwrap();
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Item) WHERE n.score >= 200 RETURN n.id",
            ));
        })
    }

    #[bench(raw)]
    /// BEFORE: range filter < without range index.
    fn bench_gql_range_filter_lt_no_index_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Item) WHERE n.score < 50 RETURN n.id",
            ));
        })
    }

    #[bench(raw)]
    /// AFTER: range filter < with range index.
    fn bench_gql_range_index_scan_lt_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "score".into(),
            gleaph_types::IndexType::Range,
        )
        .unwrap();
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Item) WHERE n.score < 50 RETURN n.id",
            ));
        })
    }

    // ── Phase 5: Parameter-Based IndexScan ──

    #[bench(raw)]
    /// BEFORE: param equality without index (full scan + filter).
    fn bench_gql_param_eq_no_index_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        let mut params = std::collections::HashMap::new();
        params.insert("id".into(), gleaph_types::Value::Int64(42));
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE n.id = $id RETURN n.score",
                &params,
            ));
        })
    }

    #[bench(raw)]
    /// AFTER: param equality with index → IndexScan.
    fn bench_gql_param_eq_index_scan_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "id".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("id".into(), gleaph_types::Value::Int64(42));
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE n.id = $id RETURN n.score",
                &params,
            ));
        })
    }

    #[bench(raw)]
    /// AFTER: param range with range index → IndexScan.
    fn bench_gql_param_range_index_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "score".into(),
            gleaph_types::IndexType::Range,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("min".into(), gleaph_types::Value::Int64(200));
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE n.score >= $min RETURN n.id",
                &params,
            ));
        })
    }

    // ── Phase 6: Compound Range Intersection ──

    #[bench(raw)]
    /// BEFORE: single-bound conditional range scan (only $min used).
    fn bench_gql_compound_range_single_bound_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "score".into(),
            gleaph_types::IndexType::Range,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("min".into(), gleaph_types::Value::Int64(100));
        params.insert("max".into(), gleaph_types::Value::Null);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE ($min IS NULL OR n.score >= $min) AND ($max IS NULL OR n.score <= $max) RETURN n.id",
                &params,
            ));
        })
    }

    #[bench(raw)]
    /// AFTER: compound range scan (both bounds → single B+ tree traversal).
    fn bench_gql_compound_range_both_bounds_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "score".into(),
            gleaph_types::IndexType::Range,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("min".into(), gleaph_types::Value::Int64(100));
        params.insert("max".into(), gleaph_types::Value::Int64(200));
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE ($min IS NULL OR n.score >= $min) AND ($max IS NULL OR n.score <= $max) RETURN n.id",
                &params,
            ));
        })
    }

    #[bench(raw)]
    /// Compound range: narrow window (score 100..110, ~3.3% selectivity).
    fn bench_gql_compound_range_narrow_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "score".into(),
            gleaph_types::IndexType::Range,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("min".into(), gleaph_types::Value::Int64(100));
        params.insert("max".into(), gleaph_types::Value::Int64(110));
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE ($min IS NULL OR n.score >= $min) AND ($max IS NULL OR n.score <= $max) RETURN n.id",
                &params,
            ));
        })
    }

    // ── Phase 7: Multi-Predicate Anchor Selection ──

    #[bench(raw)]
    /// BEFORE/AFTER: two equality predicates, low-selectivity first in AST.
    /// Phase 7 should pick the most selective (email) regardless of order.
    fn bench_gql_multi_pred_low_sel_first_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "country".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "email".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Item) WHERE n.country = 'JP' AND n.email = 'u42@test.com' RETURN n.id",
            ));
        })
    }

    #[bench(raw)]
    /// Baseline: high-selectivity predicate first in AST (already optimal).
    fn bench_gql_multi_pred_high_sel_first_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "country".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "email".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (n:Item) WHERE n.email = 'u42@test.com' AND n.country = 'JP' RETURN n.id",
            ));
        })
    }

    // ── Phase 11: Cost-Based Candidate Ordering ──

    #[bench(raw)]
    /// Two conditional candidates, suboptimal order (country first, email second).
    /// Phase 11 reorders so email (more selective) is tried first.
    fn bench_gql_candidate_order_suboptimal_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "country".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "email".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("country".into(), gleaph_types::Value::Text("JP".into()));
        params.insert(
            "email".into(),
            gleaph_types::Value::Text("u42@test.com".into()),
        );
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE ($country IS NULL OR n.country = $country) AND ($email IS NULL OR n.email = $email) RETURN n.id",
                &params,
            ));
        })
    }

    #[bench(raw)]
    /// Same as above but optimal order (email first in WHERE).
    fn bench_gql_candidate_order_optimal_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_planner_bench_items(300);
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "country".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        crate::api::create_index(
            gleaph_types::EntityType::Vertex,
            "email".into(),
            gleaph_types::IndexType::Equality,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("country".into(), gleaph_types::Value::Text("JP".into()));
        params.insert(
            "email".into(),
            gleaph_types::Value::Text("u42@test.com".into()),
        );
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query_paged_with_params(
                "MATCH (n:Item) WHERE ($email IS NULL OR n.email = $email) AND ($country IS NULL OR n.country = $country) RETURN n.id",
                &params,
            ));
        })
    }

    // ---------------------------------------------------------------------------
    // Phase 10: Join Order (label-based mid-chain anchor) benchmarks
    // ---------------------------------------------------------------------------

    /// Setup: n_common Common vertices, n_rare Rare vertices, edges Common→Rare→Common.
    fn setup_join_order_graph(n_common: u32, n_rare: u32) {
        with_state_mut(|g| {
            for i in 0..n_common {
                g.create_vertex(
                    vec!["Common".into()],
                    vec![("id".into(), gleaph_types::Value::Int64(i as i64))],
                )
                .unwrap();
            }
            for i in 0..n_rare {
                g.create_vertex(
                    vec!["Rare".into()],
                    vec![(
                        "id".into(),
                        gleaph_types::Value::Int64((n_common + i) as i64),
                    )],
                )
                .unwrap();
            }
            // Edges: Common[i]→Rare[i]→Common[(i+1)%n_common]
            for i in 0..n_rare {
                let c_src = i % n_common;
                let r = n_common + i;
                let c_dst = (i + 1) % n_common;
                g.create_edge(c_src, r, Some("E".into()), vec![], 1.0, (i * 2 + 1) as u64)
                    .unwrap();
                g.create_edge(r, c_dst, Some("E".into()), vec![], 1.0, (i * 2 + 2) as u64)
                    .unwrap();
            }
        });
    }

    #[bench(raw)]
    /// Join order: MATCH (a:Common)-[:E]->(b:Rare)-[:E]->(c:Common) — with stats
    /// (label-anchor fast path starts from Rare).
    fn bench_gql_join_order_label_anchor_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_join_order_graph(200, 20);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (a:Common)-[:E]->(b:Rare)-[:E]->(c:Common) RETURN a.id, b.id, c.id",
            ));
        })
    }

    #[bench(raw)]
    /// Join order baseline: same query but manually rewritten to start from Rare.
    fn bench_gql_join_order_manual_rare_start_300() -> canbench_rs::BenchResult {
        init_state(512, 0).unwrap();
        setup_join_order_graph(200, 20);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (b:Rare)<-[:E]-(a:Common) MATCH (b)-[:E]->(c:Common) RETURN a.id, b.id, c.id",
            ));
        })
    }

    // ---------------------------------------------------------------------------
    // Edge property index benchmarks (supernode patterns)
    // ---------------------------------------------------------------------------

    /// Setup a supernode graph: 1 popular vertex (id=0) with `fan_count` incoming
    /// edges from distinct fan vertices, each edge carrying `{rating: fan_id % 5}`.
    /// Creates an edge equality index on "rating".
    fn setup_edge_index_supernode(fan_count: u32) {
        with_state_mut(|g| {
            // Create the supernode (popular product)
            g.create_vertex(
                vec!["Product".into()],
                vec![("id".into(), gleaph_types::Value::Int64(0))],
            )
            .unwrap();
            // Create fan vertices and edges
            for i in 0..fan_count {
                let fan_id = i + 1;
                g.create_vertex(
                    vec!["User".into()],
                    vec![("id".into(), gleaph_types::Value::Int64(fan_id as i64))],
                )
                .unwrap();
                let rating = (i % 5) as i64;
                g.create_edge(
                    fan_id,
                    0, // all point to supernode
                    Some("RATED".into()),
                    vec![("rating".into(), gleaph_types::Value::Int64(rating))],
                    1.0,
                    i as u64,
                )
                .unwrap();
            }
            g.create_index(
                gleaph_types::EntityType::Edge,
                "rating".into(),
                gleaph_types::IndexType::Equality,
            )
            .unwrap();
        });
    }

    #[bench(raw)]
    /// Edge index: targets_for_src — 20 lookups on index with 100 matching entries.
    /// Tests BTreeSet range scan performance.
    fn bench_edge_index_targets_for_src_500() -> canbench_rs::BenchResult {
        init_state(512, 1024).unwrap();
        setup_edge_index_supernode(500);
        let val = gleaph_types::Value::Int64(0); // rating=0 → 100 edges
        canbench_rs::bench_fn(|| {
            with_state(|g| {
                for src in 1..=20u32 {
                    let _ = std::hint::black_box(g.edge_index_targets_for_src("rating", &val, src));
                }
            });
        })
    }

    #[bench(raw)]
    /// Edge index: sources_for_dst on supernode (dst=0) with 100 matching edges.
    /// Tests linear scan O(n) over all 100 (src,dst) pairs in the index for rating=0.
    fn bench_edge_index_sources_for_dst_500() -> canbench_rs::BenchResult {
        init_state(512, 1024).unwrap();
        setup_edge_index_supernode(500);
        let val = gleaph_types::Value::Int64(0); // rating=0 → 100 edges
        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let _ = std::hint::black_box(g.edge_index_sources_for_dst("rating", &val, 0));
            });
        })
    }

    #[bench(raw)]
    /// Edge index: sources_for_dst on a 5K-fan supernode (dst=0).
    /// Exercises the real IC stable-memory path after canister init/re-init.
    fn bench_edge_index_sources_for_dst_5000() -> canbench_rs::BenchResult {
        init_state(8192, 16_384).unwrap();
        setup_edge_index_supernode(5_000);
        let val = gleaph_types::Value::Int64(0); // rating=0 → 1000 edges
        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let _ = std::hint::black_box(g.edge_index_sources_for_dst("rating", &val, 0));
            });
        })
    }

    #[bench(raw)]
    /// Edge index: sources_for_dst on a 10K-fan supernode (dst=0).
    /// Exercises the real IC stable-memory path after canister init/re-init.
    fn bench_edge_index_sources_for_dst_10000() -> canbench_rs::BenchResult {
        init_state(16_384, 32_768).unwrap();
        setup_edge_index_supernode(10_000);
        let val = gleaph_types::Value::Int64(0); // rating=0 → 2000 edges
        canbench_rs::bench_fn(|| {
            with_state(|g| {
                let _ = std::hint::black_box(g.edge_index_sources_for_dst("rating", &val, 0));
            });
        })
    }

    #[bench(raw)]
    /// Edge index: GQL query using edge property filter on supernode.
    /// MATCH (u:User)-[:RATED {rating: 0}]->(p:Product {id: 0}) RETURN u.id
    fn bench_gql_edge_prop_filter_supernode_500() -> canbench_rs::BenchResult {
        init_state(512, 1024).unwrap();
        setup_edge_index_supernode(500);
        canbench_rs::bench_fn(|| {
            let _ = std::hint::black_box(crate::gql_bridge::query(
                "MATCH (u:User)-[:RATED {rating: 0}]->(p:Product {id: 0}) RETURN u.id",
            ));
        })
    }
}
