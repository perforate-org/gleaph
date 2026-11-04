//! Labeled graph `compact` implementation.

#[cfg(test)]
mod physical_layout_tests {
    use super::{
        LeafPhysicalDestination, PhysicalLayoutEvent, PhysicalLayoutOperation,
        VertexPhysicalPlacement,
    };
    use crate::lara::edge::free_span::{
        FreeSpan, FreeSpanAllocatorStats, FreeSpanError, FreeSpanStore, PhysicalSpanSelection,
        free_span_release_calls, reset_free_span_release_calls,
    };
    use crate::{
        VertexId,
        labeled::{BucketLabelKey, InitialCapacities, LabeledVertex, record::LabelBucket},
        traits::CsrVertex,
    };
    use ic_stable_structures::{Memory, VectorMemory};

    type PhysicalTestGraph<E> = super::LabeledLaraGraph<E, CopyAdmissionMemory>;
    type CacheEntry = (VertexId, BucketLabelKey, u64, u32, u64);

    fn empty_physical_fixture<E: crate::traits::CsrEdge>(
        capacities: InitialCapacities,
        segment: u32,
    ) -> (PhysicalTestGraph<E>, [CopyAdmissionMemory; 16]) {
        let m = std::array::from_fn(|_| CopyAdmissionMemory::default());
        let graph = super::LabeledLaraGraph::new_with_segment_size(
            m[0].clone(),
            m[1].clone(),
            m[2].clone(),
            m[3].clone(),
            m[4].clone(),
            m[5].clone(),
            m[6].clone(),
            m[7].clone(),
            m[8].clone(),
            m[9].clone(),
            m[10].clone(),
            m[11].clone(),
            m[12].clone(),
            m[13].clone(),
            m[14].clone(),
            m[15].clone(),
            capacities,
            BucketLabelKey::from_raw(9),
            segment,
        )
        .unwrap();
        (graph, m)
    }

    fn physical_fixture<E: crate::traits::CsrEdge>()
    -> (PhysicalTestGraph<E>, [CopyAdmissionMemory; 16]) {
        let (graph, m) = empty_physical_fixture(InitialCapacities::uniform(256), 16);
        for ordinal in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
            graph.vertices.set(
                VertexId::from(ordinal),
                &LabeledVertex::from_parts(ordinal as u64, 1, 8, 0),
            );
            let (origin, degree, width, words) = if ordinal == 0 {
                (100, 3, 4, vec![7u32, 0, u32::MAX, 7])
            } else {
                (68, 2, 3, vec![11u32, u32::MAX, 11])
            };
            graph
                .publish_bucket_descriptor(
                    ordinal as u64,
                    LabelBucket::from_parts(BucketLabelKey::from_raw(2), origin, degree, width),
                )
                .unwrap();
            graph
                .edges
                .write_slots_contiguous_bytes(
                    origin,
                    &words
                        .into_iter()
                        .flat_map(u32::to_le_bytes)
                        .collect::<Vec<_>>(),
                )
                .unwrap();
        }
        graph
            .edges
            .set_segment_physical_start(crate::SegmentId::from(0), 64)
            .unwrap();
        graph
            .edges
            .bump_vertex_segment_counts(VertexId::from(0), 5, 64)
            .unwrap();
        graph.edges.set_num_edges(5);
        (graph, m)
    }

    fn reopen_physical<E: crate::traits::CsrEdge>(
        m: &[CopyAdmissionMemory; 16],
    ) -> PhysicalTestGraph<E> {
        super::LabeledLaraGraph::init(
            m[0].clone(),
            m[1].clone(),
            m[2].clone(),
            m[3].clone(),
            m[4].clone(),
            m[5].clone(),
            m[6].clone(),
            m[7].clone(),
            m[8].clone(),
            m[9].clone(),
            m[10].clone(),
            m[11].clone(),
            m[12].clone(),
            m[13].clone(),
            m[14].clone(),
            m[15].clone(),
            InitialCapacities::uniform(256),
            BucketLabelKey::from_raw(9),
        )
        .unwrap()
    }

    fn canonical_bytes(m: &[CopyAdmissionMemory; 16]) -> Vec<Vec<u8>> {
        m.iter()
            .map(|memory| memory.bytes.borrow().clone())
            .collect()
    }

    fn assert_canonical_bytes(m: &[CopyAdmissionMemory; 16], expected: &[Vec<u8>]) {
        for (memory, bytes) in m.iter().zip(expected) {
            assert_eq!(&memory.bytes.borrow()[..bytes.len()], bytes);
            assert!(
                memory.bytes.borrow()[bytes.len()..]
                    .iter()
                    .all(|byte| *byte == 0)
            );
        }
    }

    fn append_physical_source_run<E: crate::traits::CsrEdge>(
        graph: &PhysicalTestGraph<E>,
    ) -> LabelBucket {
        let src = VertexId::from(0);
        let bucket = graph.buckets.read_label_bucket_slot(0).unwrap();
        let access = crate::labeled::access::LabelEdgeSpanAccess::with_bucket(
            &graph.buckets,
            0,
            bucket,
            bucket.edge_start() + u64::from(bucket.stored_slots()),
            src,
        );
        for target in [31u32, 0, 13, 31] {
            assert!(matches!(
                graph
                    .edges
                    .insert_edge_without_logical_slot(
                        &access,
                        src,
                        E::read_from(&target.to_le_bytes())
                    )
                    .unwrap(),
                crate::lara::edge::InsertLocation::Spill { .. }
            ));
        }
        let populated = graph.buckets.read_label_bucket_slot(0).unwrap();
        let run = graph.edges.spill_mint(populated.edge_spill_run()).unwrap();
        assert_eq!(run.get(), 0);
        assert_eq!(graph.edges.spill_used_len(run), 4);
        graph
            .edges
            .spill_write_row(run, 2, &u32::MAX.to_le_bytes())
            .unwrap();
        let source = populated.with_degree_field(bucket.degree() + 3);
        graph.publish_bucket_descriptor(0, source).unwrap();
        graph.edges.bump_vertex_segment_counts(src, -1, 0).unwrap();
        graph
            .edges
            .set_num_edges(graph.edges.header().num_edges - 1);
        assert_eq!(source.stored_slots(), bucket.stored_slots());
        source
    }

    fn seed_mixed_physical_residents<E: crate::traits::CsrEdge>(
        graph: &PhysicalTestGraph<E>,
    ) -> [LabelBucket; 3] {
        use crate::labeled::ltb_raw_block_store::{BlockHeader, BlockKind};
        let mut roots = Vec::new();
        for ordinal in 0..2u32 {
            let id = graph.ltb.mint().unwrap();
            assert_eq!(id, ordinal);
            graph.ltb.write_block_header(
                id,
                &BlockHeader {
                    kind: if ordinal == 0 {
                        BlockKind::Edge
                    } else {
                        BlockKind::InlineProperty
                    },
                    bucket_label_key_wire: 3,
                    owner_or_next_free: 0,
                    ordinal: 0,
                    level: 0,
                    tombstone_count: 0,
                    reserved: [0],
                },
            );
            let mut payload = [0u8; 4096];
            for (index, word) in payload.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                word.copy_from_slice(&(1000 + ordinal * 2000 + index as u32).to_le_bytes());
            }
            graph.ltb.write_payload(id, &payload).unwrap();
            roots.extend(id.to_le_bytes());
        }
        let slab = append_physical_source_run(graph);
        let tree = LabelBucket::from_parts_with_inline_property(
            BucketLabelKey::from_raw(3),
            112,
            512,
            512,
            4,
            0,
            0,
            -1,
            0,
        )
        .with_tree_mode(true);
        let tiny = LabelBucket::from_parts(BucketLabelKey::from_raw(4), 114, 0, 0)
            .try_enable_tiny_mode(4)
            .unwrap()
            .with_tiny_target(0, 55)
            .with_degree_field(1)
            .with_tiny_used_width(1);
        graph
            .edges
            .write_slots_contiguous_bytes(112, &roots)
            .unwrap();
        graph
            .buckets
            .write_label_bucket_slots_contiguous(0, &[slab, tree, tiny])
            .unwrap();
        graph
            .vertices
            .set(VertexId::from(0), &LabeledVertex::from_parts(0, 3, 16, 0));
        let peer = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 68, 2, 3);
        graph.publish_bucket_descriptor(3, peer).unwrap();
        graph
            .vertices
            .set(VertexId::from(1), &LabeledVertex::from_parts(3, 1, 8, 0));
        graph.edges.set_num_edges(521);
        [slab, tree, tiny]
    }

    fn cache_snapshot<E: crate::traits::CsrEdge>(
        graph: &PhysicalTestGraph<E>,
    ) -> (Option<CacheEntry>, [Option<CacheEntry>; 64]) {
        let project = |entry: Option<super::super::BucketLookupCache>| {
            entry.map(|entry| {
                (
                    entry.vid,
                    entry.bucket_key,
                    entry.base_slot_start,
                    entry.degree,
                    entry.slot,
                )
            })
        };
        (
            project(graph.last_bucket_lookup.get()),
            graph
                .bucket_lookup_cache
                .each_ref()
                .map(|entry| project(entry.get())),
        )
    }

    fn warm_physical_caches<E: crate::traits::CsrEdge>(graph: &PhysicalTestGraph<E>) {
        for ordinal in 0..2 {
            let id = VertexId::from(ordinal);
            graph
                .find_bucket(id, &graph.vertices.get(id), BucketLabelKey::from_raw(2))
                .unwrap();
        }
    }

    fn take_events() -> Vec<PhysicalLayoutEvent> {
        super::PHYSICAL_LAYOUT_EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
    }

    fn assert_physical_result<E: crate::traits::CsrEdge>(
        graph: &PhysicalTestGraph<E>,
        starts: [u64; 2],
        widths: [u32; 2],
        total: i64,
    ) {
        for (ordinal, (start, width)) in starts.into_iter().zip(widths).enumerate() {
            assert_eq!(
                graph.vertices.get(VertexId::from(ordinal as u32)),
                LabeledVertex::from_parts(ordinal as u64, 1, width, 0)
            );
            let (degree, stored, words) = if ordinal == 0 {
                (3, 4, vec![7u32, 0, u32::MAX, 7])
            } else {
                (2, 3, vec![11u32, u32::MAX, 11])
            };
            assert_eq!(
                graph.buckets.read_label_bucket_slot(ordinal as u64),
                Some(LabelBucket::from_parts(
                    BucketLabelKey::from_raw(2),
                    start,
                    degree,
                    stored
                ))
            );
            let mut bytes = vec![0; stored as usize * 4];
            graph.edges.read_slots_contiguous_bytes(start, &mut bytes);
            assert_eq!(
                bytes,
                words
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            graph.edges.counts_store().get(1),
            super::SegmentEdgeCounts { actual: 5, total }
        );
        assert_eq!(graph.edges.header().num_edges, 5);
    }

    #[test]
    fn mode_footprint_mixed_leaf_preserves_slack_and_peer() {
        use super::super::test_support::TestEdge;
        let (graph, m) = physical_fixture::<TestEdge>();
        let peer = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .vertices
            .set(peer, &LabeledVertex::from_parts(2, 1, 8, 0));
        let label = BucketLabelKey::from_raw(2);
        graph
            .publish_bucket_descriptor(2, LabelBucket::from_parts(label, 124, 1, 1))
            .unwrap();
        graph
            .edges
            .write_slot_bytes(124, &31u32.to_le_bytes())
            .unwrap();
        graph.edges.bump_vertex_segment_counts(peer, 1, 0).unwrap();
        graph.edges.set_num_edges(6);
        let header = graph.edges.header();
        let free = graph.edges.free_byte_spans();
        let slab = m[5].bytes.borrow().clone();
        let count_writes = m[4].writes.get();
        let counts = graph.leaf_segment_counts_for_vid(peer);
        assert_eq!(
            counts,
            super::SegmentEdgeCounts {
                actual: 6,
                total: 64
            }
        );
        for order in [[0, 1], [1, 0]] {
            let peer_before = graph
                .find_bucket(peer, &graph.vertices.get(peer), label)
                .unwrap();
            assert!(matches!(
                peer_before,
                super::super::BucketSearch::Found { .. }
            ));
            for owner in order {
                graph
                    .enable_default_edge_bypass(VertexId::from(owner))
                    .unwrap();
                assert_eq!(
                    graph.leaf_segment_counts_for_vid(peer),
                    counts,
                    "a mode handoff must not subtract assigned cover"
                );
                assert_eq!(graph.edges.span_meta_store().get(0).physical_start, 64);
                assert_eq!(
                    m[4].writes.get(),
                    count_writes,
                    "unchanged assigned counts must not be rewritten"
                );
                assert_eq!(graph.edges.header(), header);
                assert_eq!(graph.edges.free_byte_spans(), free);
                assert_eq!(*m[5].bytes.borrow(), slab);
                assert_eq!(
                    graph.out_edges(peer).unwrap(),
                    vec![TestEdge { target: 31 }]
                );
            }
            for owner in order.into_iter().rev() {
                graph
                    .promote_bypass_to_bucket_mode(VertexId::from(owner))
                    .unwrap();
                assert_eq!(graph.leaf_segment_counts_for_vid(peer), counts);
                assert_eq!(
                    graph.vertices.get(VertexId::from(owner)).stored_slots,
                    if owner == 0 { 4 } else { 3 }
                );
                assert_eq!(
                    graph.out_edges(peer).unwrap(),
                    vec![TestEdge { target: 31 }]
                );
                assert!(
                    matches!(graph.find_bucket(peer, &graph.vertices.get(peer), label).unwrap(), super::super::BucketSearch::Found { bucket, .. } if bucket.edge_start() == 124)
                );
            }
            assert_eq!(
                graph.out_edges(VertexId::from(0)).unwrap(),
                vec![
                    TestEdge { target: 7 },
                    TestEdge { target: 0 },
                    TestEdge { target: 7 }
                ]
            );
            assert_eq!(
                graph.out_edges(VertexId::from(1)).unwrap(),
                vec![TestEdge { target: 11 }, TestEdge { target: 11 }]
            );
        }
    }

    #[test]
    fn mode_footprint_unassigned_counts_use_physical_prefix() {
        use super::super::test_support::TestEdge;
        use crate::{
            labeled::spill_run_store::{ArenaRows, HEADER_SIZE, SpillRunStore},
            traits::NO_SPILL_RUN,
        };
        for assigned in [false, true] {
            for shape in 0..3 {
                let (graph, m) =
                    empty_physical_fixture::<TestEdge>(InitialCapacities::uniform(256), 16);
                let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
                let prefix = if shape == 2 { 0 } else { 5 };
                let used = if shape == 0 { 64 } else { 0 };
                let live = if shape == 0 { 69 } else { 0 };
                let arena = SpillRunStore::new(m[6].clone(), 4, ArenaRows::EDGE).unwrap();
                let run = if shape == 2 {
                    NO_SPILL_RUN
                } else {
                    let run = arena.allocate(used.max(1)).unwrap();
                    if used == 0 {
                        // Seed a still-owned empty minimum-class run; the canonical layout
                        // constant belongs to the spill store, not a second test schema.
                        m[6].write(HEADER_SIZE + u64::from(run.get()) * 4, &0u32.to_le_bytes());
                    } else {
                        for slot in 0..used {
                            arena
                                .write_row(run, slot, &(prefix + slot).to_le_bytes())
                                .unwrap();
                        }
                    }
                    run.get()
                };
                if prefix != 0 {
                    let words: Vec<_> = (0..prefix)
                        .flat_map(|slot| {
                            if live == 0 {
                                u32::MAX.to_le_bytes()
                            } else {
                                slot.to_le_bytes()
                            }
                        })
                        .collect();
                    graph
                        .edges
                        .write_slots_contiguous_bytes(64, &words)
                        .unwrap();
                }
                graph
                    .vertices
                    .set(owner, &LabeledVertex::from_parts(0, 1, prefix, 0));
                let bucket = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 64, live, prefix);
                let bucket = if run == NO_SPILL_RUN {
                    bucket
                } else {
                    bucket.with_edge_spill_run(run)
                };
                graph.publish_bucket_descriptor(0, bucket).unwrap();
                if assigned {
                    graph
                        .edges
                        .set_segment_physical_start(crate::SegmentId::from(0), 64)
                        .unwrap();
                }
                graph
                    .edges
                    .bump_vertex_segment_counts(
                        owner,
                        i64::from(live),
                        if assigned { 16 } else { i64::from(prefix) },
                    )
                    .unwrap();
                graph.edges.set_num_edges(u64::from(live));
                let header = graph.edges.header();
                let free = graph.edges.free_byte_spans();
                let backing = [m[5].bytes.borrow().clone(), m[6].bytes.borrow().clone()];
                graph.enable_default_edge_bypass(owner).unwrap();
                assert_eq!(
                    graph.vertices.get(owner).bypass_spill_run(),
                    run,
                    "run ownership is independent of used length"
                );
                assert_eq!(
                    graph.leaf_segment_counts_for_vid(owner),
                    super::SegmentEdgeCounts {
                        actual: i64::from(live),
                        total: if assigned { 16 } else { 0 }
                    }
                );
                graph.promote_bypass_to_bucket_mode(owner).unwrap();
                let vertex = graph.vertices.get(owner);
                assert!(!vertex.is_default_edge_labeled());
                assert_eq!(
                    vertex.stored_slots, prefix,
                    "normal cover excludes spill and spare quota"
                );
                if shape != 2 {
                    let bucket = graph
                        .buckets
                        .read_label_bucket_slot(vertex.base_slot_start())
                        .unwrap();
                    assert_eq!(
                        (
                            bucket.edge_start(),
                            bucket.stored_slots(),
                            bucket.edge_spill_run()
                        ),
                        (64, prefix, run)
                    );
                    assert_eq!(
                        vertex.degree(),
                        1,
                        "zero-live rows retain prefix and empty-run ownership"
                    );
                } else {
                    assert_eq!(vertex.degree(), 0);
                    assert_eq!(vertex.base_slot_start(), 0);
                    assert_eq!(vertex.spill_run(), NO_SPILL_RUN);
                }
                assert_eq!(
                    graph.leaf_segment_counts_for_vid(owner),
                    super::SegmentEdgeCounts {
                        actual: i64::from(live),
                        total: if assigned { 16 } else { i64::from(prefix) }
                    }
                );
                assert_eq!(graph.edges.header(), header);
                assert_eq!(graph.edges.free_byte_spans(), free);
                assert_eq!(*m[5].bytes.borrow(), backing[0]);
                assert_eq!(*m[6].bytes.borrow(), backing[1]);
                let expected: Vec<_> = (0..live).map(|target| TestEdge { target }).collect();
                assert_eq!(graph.out_edges(owner).unwrap(), expected);
                if shape == 0 {
                    let reopened = reopen_physical::<TestEdge>(&m);
                    assert_eq!(reopened.out_edges(owner).unwrap(), expected);
                    assert_eq!(
                        reopened.leaf_segment_counts_for_vid(owner),
                        graph.leaf_segment_counts_for_vid(owner)
                    );
                }
            }
        }
    }

    #[test]
    fn mode_footprint_count_index_rejects_both_directions() {
        use super::super::test_support::TestEdge;
        let (graph, m) = physical_fixture::<TestEdge>();
        let owner = VertexId::from(0);
        warm_physical_caches(&graph);
        let count = graph.edges.counts_store().pop().unwrap();
        let before = canonical_bytes(&m);
        let cache = cache_snapshot(&graph);
        assert!(matches!(
            graph.enable_default_edge_bypass(owner),
            Err(super::LabeledOperationError::Store(
                crate::lara::operation_error::LaraOperationError::SegmentCountsTreeTooSmall
            ))
        ));
        assert_canonical_bytes(&m, &before);
        assert_eq!(cache_snapshot(&graph), cache);
        graph.edges.counts_store().push(count).unwrap();
        graph.enable_default_edge_bypass(owner).unwrap();
        let count = graph.edges.counts_store().pop().unwrap();
        let before = canonical_bytes(&m);
        let cache = cache_snapshot(&graph);
        assert!(matches!(
            graph.promote_bypass_to_bucket_mode(owner),
            Err(super::LabeledOperationError::Store(
                crate::lara::operation_error::LaraOperationError::SegmentCountsTreeTooSmall
            ))
        ));
        assert_canonical_bytes(&m, &before);
        assert_eq!(cache_snapshot(&graph), cache);
        graph.edges.counts_store().push(count).unwrap();
        let reopened = reopen_physical::<TestEdge>(&m);
        assert_eq!(reopened.vertices.get(owner), graph.vertices.get(owner));
        assert_eq!(
            reopened.out_edges(owner).unwrap(),
            graph.out_edges(owner).unwrap()
        );
    }

    #[test]
    fn mode_footprint_source_witness_rejects_stale_assignment() {
        use super::super::test_support::TestEdge;
        let (graph, m) = physical_fixture::<TestEdge>();
        warm_physical_caches(&graph);
        let owner = VertexId::from(0);
        for field in 0..4 {
            let mut preparation = graph
                .prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite {
                    src: owner,
                    placement: VertexPhysicalPlacement::new(100, 8, None, 0),
                })
                .unwrap();
            let prior = canonical_bytes(&m);
            match field {
                0 => graph
                    .edges
                    .set_segment_physical_start(crate::SegmentId::from(0), 65)
                    .unwrap(),
                1 => graph.edges.bump_vertex_segment_counts(owner, 1, 0).unwrap(),
                2 => {
                    graph.edges.counts_store().pop().unwrap();
                }
                3 => {
                    m[5].write(16, &17u32.to_le_bytes());
                }
                _ => unreachable!(),
            }
            let before = canonical_bytes(&m);
            let cache = cache_snapshot(&graph);
            take_events();
            assert!(matches!(
                preparation.ready(),
                Err(super::LabeledOperationError::Store(
                    crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow
                ))
            ));
            assert_canonical_bytes(&m, &before);
            assert_eq!(cache_snapshot(&graph), cache);
            assert!(!take_events().contains(&PhysicalLayoutEvent::Publish));
            drop(preparation);
            match field {
                0 => graph
                    .edges
                    .set_segment_physical_start(crate::SegmentId::from(0), 64)
                    .unwrap(),
                1 => graph
                    .edges
                    .bump_vertex_segment_counts(owner, -1, 0)
                    .unwrap(),
                2 => graph
                    .edges
                    .counts_store()
                    .push(super::SegmentEdgeCounts {
                        actual: 5,
                        total: 64,
                    })
                    .unwrap(),
                3 => {
                    m[5].write(16, &16u32.to_le_bytes());
                }
                _ => unreachable!(),
            }
            assert_canonical_bytes(&m, &prior);
        }
    }

    #[test]
    fn mode_footprint_ineligible_sources_reject_before_write() {
        use super::super::test_support::TestEdge;
        let (graph, m) = physical_fixture::<TestEdge>();
        let owner = VertexId::from(0);
        let slab = graph.buckets.read_label_bucket_slot(0).unwrap();
        let tiny = slab
            .try_enable_tiny_mode(4)
            .unwrap()
            .with_tiny_target(0, 7)
            .with_tiny_target(1, 0)
            .with_tiny_target(2, u32::MAX)
            .with_tiny_target(3, 7)
            .with_degree_field(3)
            .with_tiny_used_width(4);
        let tree = slab.with_tree_mode(true);
        let property = LabelBucket::from_parts_with_inline_property(
            slab.bucket_label_key(),
            slab.edge_start(),
            slab.degree(),
            slab.stored_slots(),
            1,
            0,
            3,
            -1,
            0,
        );
        for bucket in [tiny, tree, property] {
            graph.publish_bucket_descriptor(0, bucket).unwrap();
            warm_physical_caches(&graph);
            let before = canonical_bytes(&m);
            let cache = cache_snapshot(&graph);
            assert!(matches!(
                graph.enable_default_edge_bypass(owner),
                Err(super::LabeledOperationError::InvalidDefaultBypass)
            ));
            assert_canonical_bytes(&m, &before);
            assert_eq!(cache_snapshot(&graph), cache);
        }
        graph.publish_bucket_descriptor(0, slab).unwrap();
        graph.enable_default_edge_bypass(owner).unwrap();
        let before = canonical_bytes(&m);
        graph.enable_default_edge_bypass(owner).unwrap();
        assert_canonical_bytes(&m, &before);
    }

    // Raw physical images must not require the logical tombstone trait.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct CopyOnlyEdge(u32);

    impl crate::traits::CsrEdge for CopyOnlyEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self(u32::from_le_bytes(bytes.try_into().unwrap()))
        }

        fn write_to(&self, bytes: &mut [u8]) {
            bytes.copy_from_slice(&self.0.to_le_bytes());
        }

        fn neighbor_vid(&self) -> crate::VertexId {
            crate::VertexId::from(self.0)
        }

        fn with_neighbor_vid(&self, vid: crate::VertexId) -> Self {
            Self(u32::from(vid))
        }
    }

    #[derive(Clone, Default)]
    struct CopyAdmissionMemory {
        bytes: VectorMemory,
        reject_grow: std::rc::Rc<std::cell::Cell<bool>>,
        grows: std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
        writes: std::rc::Rc<std::cell::Cell<u64>>,
        write_bytes: std::rc::Rc<std::cell::Cell<u64>>,
        reads: std::rc::Rc<std::cell::Cell<u64>>,
        read_bytes: std::rc::Rc<std::cell::Cell<u64>>,
        header_writes: std::rc::Rc<std::cell::Cell<u64>>,
        write_hook: std::rc::Rc<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>,
        size_hook: std::rc::Rc<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>,
    }

    impl Memory for CopyAdmissionMemory {
        fn size(&self) -> u64 {
            let hook = self.size_hook.borrow_mut().take();
            if let Some(hook) = hook {
                hook();
            }
            self.bytes.size()
        }

        fn grow(&self, pages: u64) -> i64 {
            self.grows.borrow_mut().push(pages);
            if self.reject_grow.replace(false) {
                -1
            } else {
                self.bytes.grow(pages)
            }
        }

        fn read(&self, offset: u64, bytes: &mut [u8]) {
            self.reads.set(self.reads.get() + 1);
            self.read_bytes
                .set(self.read_bytes.get() + bytes.len() as u64);
            self.bytes.read(offset, bytes);
        }

        fn write(&self, offset: u64, bytes: &[u8]) {
            self.writes.set(self.writes.get() + 1);
            self.write_bytes
                .set(self.write_bytes.get() + bytes.len() as u64);
            if offset == 0 && bytes.len() == 64 {
                self.header_writes.set(self.header_writes.get() + 1);
            }
            self.bytes.write(offset, bytes);
            let hook = self.write_hook.borrow_mut().take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    #[test]
    fn mode_footprint_tree_append_preserves_pin_and_retirement() {
        use super::super::test_support::TestEdge;
        use crate::{
            labeled::{
                ltb_raw_block_store::{BlockHeader, BlockKind},
                spill_run_store::{ArenaRows, MAX_ROWS, SpillRunStore},
            },
            traits::CsrEdge,
        };
        use std::{cell::RefCell, ops::ControlFlow, rc::Rc};
        for fault in [0, 2, 1, 3, 4, 5] {
            let (graph, m) =
                empty_physical_fixture::<TestEdge>(InitialCapacities::uniform(256), 16);
            let graph = Rc::new(graph);
            let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
            let peer = graph.push_vertex(LabeledVertex::default()).unwrap();
            let arena = SpillRunStore::new(m[6].clone(), 4, ArenaRows::EDGE).unwrap();
            let run = arena.allocate(MAX_ROWS).unwrap();
            let extent = 5 + MAX_ROWS;
            let holes = [1, 7, extent - 1];
            for slot in 0..extent {
                let target = if holes.contains(&slot) {
                    u32::MAX
                } else {
                    slot
                };
                if slot < 5 {
                    graph
                        .edges
                        .write_slot_bytes(64 + u64::from(slot), &target.to_le_bytes())
                        .unwrap();
                } else {
                    arena
                        .write_row(run, slot - 5, &target.to_le_bytes())
                        .unwrap();
                }
            }
            let live = extent - holes.len() as u32;
            graph.vertices.set(
                owner,
                &LabeledVertex::from_parts(64, live, extent, 0)
                    .with_default_edge_labeled(true)
                    .with_degree(live)
                    .with_stored_slots(extent)
                    .with_bypass_spill_run(run.get()),
            );
            let peer_label = BucketLabelKey::from_raw(2);
            graph
                .vertices
                .set(peer, &LabeledVertex::from_parts(0, 1, 1, 0));
            let source_free_head = graph.ltb.free_head();
            assert_eq!(graph.ltb.free_count(), 0);
            let source_id = graph.ltb.mint().unwrap();
            assert_eq!(source_id, 0);
            let source_ltb_header = BlockHeader {
                kind: BlockKind::Edge,
                bucket_label_key_wire: peer_label.raw(),
                owner_or_next_free: u32::from(peer),
                ordinal: 0,
                level: 0,
                tombstone_count: 0,
                reserved: [0],
            };
            graph.ltb.write_block_header(source_id, &source_ltb_header);
            let source_payload = 9999u32.to_le_bytes();
            graph
                .ltb
                .write_payload_partial(source_id, 0, &source_payload)
                .unwrap();
            graph
                .publish_bucket_descriptor(
                    0,
                    LabelBucket::from_parts(peer_label, 75, 1, 1).with_tree_mode(true),
                )
                .unwrap();
            graph
                .edges
                .write_slot_bytes(75, &source_id.to_le_bytes())
                .unwrap();
            graph
                .edges
                .set_segment_physical_start(crate::SegmentId::from(0), 64)
                .unwrap();
            graph
                .edges
                .bump_vertex_segment_counts(owner, i64::from(live), 16)
                .unwrap();
            graph.edges.set_num_edges(u64::from(live + 1));
            assert!(matches!(
                graph
                    .find_bucket(peer, &graph.vertices.get(peer), peer_label)
                    .unwrap(),
                super::super::BucketSearch::Found { .. }
            ));
            let source_vertex = graph.vertices.get(owner);
            let source_peer = graph.vertices.get(peer);
            let source_bucket = graph.buckets.read_label_bucket_slot(0).unwrap();
            let source_counts = graph.leaf_segment_counts_for_vid(owner);
            let source_header = graph.edges.header();
            let source_backing = canonical_bytes(&m);
            let snapshot = Rc::new(RefCell::new(None));
            if fault != 0 {
                let g = graph.clone();
                let memories = m.clone();
                let saved = snapshot.clone();
                let inject = move || {
                    match fault {
                        1 => g
                            .edges
                            .set_segment_physical_start(crate::SegmentId::from(0), 65)
                            .unwrap(),
                        2 => g.edges.bump_vertex_segment_counts(owner, 1, 0).unwrap(),
                        3 => {
                            g.edges.counts_store().pop().unwrap();
                        }
                        4 => {
                            memories[5].write(16, &17u32.to_le_bytes());
                        }
                        5 => g.edges.set_num_edges(source_header.num_edges + 10),
                        _ => unreachable!(),
                    }
                    *saved.borrow_mut() = Some((
                        canonical_bytes(&memories),
                        cache_snapshot(&g),
                        g.edges.header(),
                    ));
                };
                if fault == 2 {
                    // Arm only after target allocation. The descriptor backing check in
                    // SegmentRewritePlan::reserve is later than the old tree witness check.
                    let descriptor = m[1].clone();
                    *m[15].write_hook.borrow_mut() = Some(Box::new(move || {
                        *descriptor.size_hook.borrow_mut() = Some(Box::new(inject));
                    }));
                } else {
                    *m[15].write_hook.borrow_mut() = Some(Box::new(inject));
                }
            }
            let result = super::super::promote::promote_bypass_with_edge(
                &graph,
                owner,
                &TestEdge { target: 4242 },
            );
            // Promotion is a shrink: the two-slot root reuses the bypass row's
            // own span, so no capacity is allocated.
            assert_eq!(
                graph.edges.header().elem_capacity,
                source_header.elem_capacity,
                "in-place promotion allocates nothing"
            );
            if fault == 0 {
                result.unwrap();
                let vertex = graph.vertices.get(owner);
                assert!(!vertex.is_default_edge_labeled());
                assert_eq!(
                    vertex.stored_slots, 2,
                    "normal tree cover is its physical root, not logical extent"
                );
                let bucket = graph
                    .buckets
                    .read_label_bucket_slot(vertex.base_slot_start())
                    .unwrap();
                assert!(bucket.is_tree_mode());
                assert_eq!(
                    (
                        bucket.edge_start(),
                        bucket.stored_slots(),
                        bucket.degree(),
                        bucket.edge_spill_run()
                    ),
                    (
                        // In-place: the two-slot root reuses the bypass row's
                        // own span (base 64), it is not allocated past capacity.
                        64,
                        extent + 1,
                        live + 1,
                        crate::traits::NO_SPILL_RUN
                    )
                );
                assert_eq!(
                    graph.leaf_segment_counts_for_vid(owner),
                    super::SegmentEdgeCounts {
                        actual: 0,
                        total: 16
                    }
                );
                assert_eq!(graph.edges.span_meta_store().get(0).physical_start, 64);
                let expected: Vec<_> = (0..=extent)
                    .map(|slot| {
                        (
                            slot,
                            if slot == extent {
                                4242
                            } else if holes.contains(&slot) {
                                u32::MAX
                            } else {
                                slot
                            },
                        )
                    })
                    .collect();
                for order in [
                    super::super::OutEdgeOrder::Ascending,
                    super::super::OutEdgeOrder::Descending,
                ] {
                    let mut rows = Vec::new();
                    let flow = graph
                        .visit_edges_with_inline_property(
                            owner,
                            graph.default_label,
                            order,
                            |slot, item| {
                                rows.push((slot.raw(), u32::from(item.edge.neighbor_vid())));
                                ControlFlow::<()>::Continue(())
                            },
                        )
                        .unwrap();
                    assert!(matches!(flow, ControlFlow::Continue(())));
                    assert_eq!(
                        rows,
                        if matches!(order, super::super::OutEdgeOrder::Ascending) {
                            expected.clone()
                        } else {
                            expected.iter().copied().rev().collect()
                        }
                    );
                }
                assert_eq!(
                    graph.edges.spill_used_len(run),
                    crate::traits::NO_SPILL_RUN,
                    "source run is retired"
                );
                assert!(
                    graph
                        .edges
                        .free_byte_spans()
                        .iter()
                        .all(|span| span.start_slot + span.len <= 64 || span.start_slot >= 80)
                );
                let reopened = reopen_physical::<TestEdge>(&m);
                assert_eq!(
                    reopened.leaf_segment_counts_for_vid(owner),
                    super::SegmentEdgeCounts {
                        actual: 0,
                        total: 16
                    }
                );
                assert_eq!(reopened.vertices.get(owner), vertex);
            } else {
                assert!(matches!(
                    result,
                    Err(super::LabeledOperationError::Store(
                        crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow
                    ))
                ));
                let snapshot = snapshot.borrow();
                let (bytes, cache, header) = snapshot
                    .as_ref()
                    .expect("fault must reach target preparation after allocation");
                assert_eq!(cache_snapshot(&graph), *cache);
                assert_eq!(graph.edges.header(), *header);
                for role in [0, 1, 4, 6, 7, 10, 13, 14] {
                    assert_eq!(*m[role].bytes.borrow(), bytes[role], "source role {role}");
                }
                assert_eq!(graph.vertices.get(owner), source_vertex);
                assert_eq!(graph.vertices.get(peer), source_peer);
                assert_eq!(graph.buckets.read_label_bucket_slot(0), Some(source_bucket));
                assert_eq!(graph.edges.spill_used_len(run), MAX_ROWS);
                assert_eq!(&m[5].bytes.borrow()[320..340], &source_backing[5][320..340]);
                assert_eq!(&m[5].bytes.borrow()[364..368], &source_backing[5][364..368]);
                assert_eq!(graph.ltb.tail_next(), 3);
                assert_eq!(
                    graph.ltb.free_count(),
                    2,
                    "every target block must be released exactly once"
                );
                let mut free_ids = Vec::new();
                let mut next = graph.ltb.free_head();
                for _ in 0..graph.ltb.free_count() {
                    assert!(next < graph.ltb.tail_next());
                    assert!(!free_ids.contains(&next), "LTB free list must not cycle");
                    let header = graph.ltb.read_block_header(next);
                    assert_eq!(header.kind, BlockKind::Free);
                    free_ids.push(next);
                    next = header.owner_or_next_free;
                }
                assert_eq!(next, source_free_head, "exact free-list terminator");
                free_ids.sort_unstable();
                assert_eq!(free_ids, vec![1, 2], "exact attempt-owned target IDs");
                assert_eq!(free_ids.len() as u32, graph.ltb.free_count());
                // In-place promotion allocates no root span, so a rollback has
                // none to release: the free-span store is unchanged.
                assert!(
                    graph.edges.free_byte_spans().is_empty(),
                    "in-place promotion frees no span: {:?}",
                    graph.edges.free_byte_spans()
                );
                if fault == 1 {
                    graph
                        .edges
                        .set_segment_physical_start(crate::SegmentId::from(0), 64)
                        .unwrap();
                    let reopened = reopen_physical::<TestEdge>(&m);
                    assert_eq!(reopened.vertices.get(owner), source_vertex);
                    assert_eq!(reopened.leaf_segment_counts_for_vid(owner), source_counts);
                    assert_eq!(reopened.edges.spill_used_len(run), MAX_ROWS);
                    for order in [
                        super::super::OutEdgeOrder::Ascending,
                        super::super::OutEdgeOrder::Descending,
                    ] {
                        let mut rows = Vec::new();
                        let flow = reopened
                            .visit_edges_with_inline_property(
                                owner,
                                reopened.default_label,
                                order,
                                |slot, item| {
                                    rows.push((slot.raw(), u32::from(item.edge.neighbor_vid())));
                                    ControlFlow::<()>::Continue(())
                                },
                            )
                            .unwrap();
                        assert!(matches!(flow, ControlFlow::Continue(())));
                        let expected: Vec<_> = (0..extent)
                            .filter(|slot| !holes.contains(slot))
                            .map(|slot| (slot, slot))
                            .collect();
                        assert_eq!(
                            rows,
                            if matches!(order, super::super::OutEdgeOrder::Ascending) {
                                expected
                            } else {
                                expected.into_iter().rev().collect()
                            }
                        );
                    }
                    crate::labeled::invariants::assert_labeled_layout_invariants(
                        reopened.vertices(),
                        reopened.buckets(),
                        reopened.edges(),
                    );
                    reopened.edges.free_span_store().validate().unwrap();
                    assert_eq!(reopened.ltb.free_head(), graph.ltb.free_head());
                    assert_eq!(reopened.ltb.free_count(), 2);
                    assert_eq!(
                        reopened.out_edges(peer).unwrap(),
                        vec![TestEdge { target: 9999 }]
                    );
                }
            }
            assert_eq!(graph.ltb.read_block_header(source_id), source_ltb_header);
            let mut payload = [0; 4];
            graph
                .ltb
                .read_payload_partial(source_id, 0, &mut payload)
                .unwrap();
            assert_eq!(payload, source_payload, "old source LTB remains owned");
            assert_eq!(
                graph.out_edges(peer).unwrap(),
                vec![TestEdge { target: 9999 }]
            );
        }
    }

    #[test]
    fn rebuild_packs_a_tombstoned_spill_and_reports_the_moves() {
        use super::super::test_support::TestEdge;
        use crate::traits::CsrEdgeTombstone;
        let (graph, _m) = physical_fixture::<TestEdge>();
        let source = append_physical_source_run(&graph);
        let run = graph.edges.spill_mint(source.edge_spill_run()).unwrap();
        let prefix = super::bucket_content_slots(&source);
        for target in [7u32, 8, 9] {
            assert!(
                graph
                    .edges
                    .spill_append(run, &target.to_le_bytes())
                    .unwrap()
            );
        }
        graph.edges.spill_tombstone_row(run, 0).unwrap();
        let used = graph.edges.spill_used_len(run);
        let live = (0..used)
            .filter(|index| {
                !graph
                    .edges
                    .spill_edge_at(run, *index)
                    .expect("spill row")
                    .is_tombstone_edge()
            })
            .count() as u32;
        assert!(live < used, "the rebuild must have holes to pack");

        let moves = graph.rebuild_bucket_edge_spill(0, &source).unwrap();

        let after = graph.buckets.read_label_bucket_slot(0).unwrap();
        let fresh = graph.edges.spill_mint(after.edge_spill_run()).unwrap();
        assert_ne!(fresh, run, "the rebuild published a new run");
        assert_eq!(
            graph.edges.spill_used_len(fresh),
            live,
            "the run holds exactly the live rows"
        );
        assert_eq!(
            moves.len(),
            live as usize,
            "every live row after the hole moved"
        );
        assert_eq!(moves[0].old_slot_index, prefix + 1);
        assert_eq!(moves[0].new_slot_index, prefix);
        assert_eq!(moves[0].label_id, source.bucket_label_key());
        // The rows kept their order: spill row 1 is now the run's first row.
        let first = graph.edges.spill_edge_at(fresh, 0).unwrap();
        assert_eq!(first, graph.edges.spill_edge_at(run, 1).unwrap_or(first));
    }

    #[test]
    fn non_forcing_compaction_rebuilds_a_dense_spill_run() {
        use super::super::test_support::TestEdge;
        use crate::labeled::EdgePlacementPolicy;
        use crate::labeled::graph::{SpanResolutionPolicy, VertexEdgeSpanCompactOneStep};
        let (graph, _m) = physical_fixture::<TestEdge>();
        let source = append_physical_source_run(&graph);
        let run = graph.edges.spill_mint(source.edge_spill_run()).unwrap();
        // Row 2 is already a tombstone; two more make the dead rows dominate the run, which is the
        // hysteresis a rebuild needs (a repack must reclaim more than it copies).
        for index in [0u32, 1] {
            graph.edges.spill_tombstone_row(run, index).unwrap();
        }
        assert_eq!(graph.edges.spill_used_len(run), 4);
        assert_eq!(graph.bucket_edge_spill_dead_rows(&source).unwrap(), 3);
        // The vertex's whole span is the bucket's four slots, so the fold has no room for the
        // run's live row and fails - the condition this arm exists for.
        graph
            .vertices
            .set(VertexId::from(0), &LabeledVertex::from_parts(0, 1, 4, 0));

        // A delete-driven pass may not relocate the leaf for rows that already live in the run, so
        // the rebuild is the only reclaimer left.
        let outcome = graph
            .compact_vertex_edge_span_one_step_with_span_policy(
                VertexId::from(0),
                0,
                0,
                &|_| EdgePlacementPolicy::Insertion,
                SpanResolutionPolicy::SlackMayBeDropped,
            )
            .unwrap();

        let moves = match outcome {
            VertexEdgeSpanCompactOneStep::OverflowRewrite(moves) => moves,
            other => panic!("expected the rebuild's batch, got {other:?}"),
        };
        let prefix = super::bucket_content_slots(&source);
        assert_eq!(moves.len(), 1, "only the run's last row was live");
        assert_eq!(moves[0].label_id, source.bucket_label_key());
        assert_eq!(moves[0].old_slot_index, prefix + 3);
        assert_eq!(moves[0].new_slot_index, prefix);

        let after = graph.buckets.read_label_bucket_slot(0).unwrap();
        let fresh = graph.edges.spill_mint(after.edge_spill_run()).unwrap();
        assert_ne!(fresh, run, "the rebuild published a new run");
        assert_eq!(
            graph.edges.spill_used_len(fresh),
            1,
            "the run holds only the live row"
        );
    }

    #[test]
    fn non_forcing_compaction_keeps_a_spill_run_below_the_rebuild_hysteresis() {
        use super::super::test_support::TestEdge;
        use crate::labeled::EdgePlacementPolicy;
        use crate::labeled::graph::{SpanResolutionPolicy, VertexEdgeSpanCompactOneStep};
        let (graph, _m) = physical_fixture::<TestEdge>();
        let source = append_physical_source_run(&graph);
        let run = graph.edges.spill_mint(source.edge_spill_run()).unwrap();
        // Two dead rows in four is exactly the threshold, which a rebuild must not cross: the
        // repack copies three live rows to reclaim two, so the pass would pay more than it frees.
        graph.edges.spill_tombstone_row(run, 0).unwrap();
        assert_eq!(graph.bucket_edge_spill_dead_rows(&source).unwrap(), 2);
        graph
            .vertices
            .set(VertexId::from(0), &LabeledVertex::from_parts(0, 1, 4, 0));

        let outcome = graph
            .compact_vertex_edge_span_one_step_with_span_policy(
                VertexId::from(0),
                0,
                0,
                &|_| EdgePlacementPolicy::Insertion,
                SpanResolutionPolicy::SlackMayBeDropped,
            )
            .unwrap();

        assert!(
            !matches!(outcome, VertexEdgeSpanCompactOneStep::OverflowRewrite(_)),
            "the run must keep its ordinals below the hysteresis, got {outcome:?}"
        );
        let after = graph.buckets.read_label_bucket_slot(0).unwrap();
        assert_eq!(after.edge_spill_run(), source.edge_spill_run());
        assert_eq!(
            graph.edges.spill_used_len(run),
            4,
            "the run was not repacked"
        );
    }

    #[test]
    fn non_forcing_compaction_repacks_a_tail_tombstoned_run_without_reporting_moves() {
        use super::super::test_support::TestEdge;
        use crate::labeled::EdgePlacementPolicy;
        use crate::labeled::graph::{SpanResolutionPolicy, VertexEdgeSpanCompactOneStep};
        let (graph, _m) = physical_fixture::<TestEdge>();
        let source = append_physical_source_run(&graph);
        let run = graph.edges.spill_mint(source.edge_spill_run()).unwrap();
        for target in [21u32, 22] {
            assert!(
                graph
                    .edges
                    .spill_append(run, &target.to_le_bytes())
                    .unwrap()
            );
        }
        // Every hole sits at the tail, so packing them leaves the live prefix in place and the
        // rebuild reports nothing. Returning that empty batch would reset the pass to bucket 0
        // and spin, so the arm must fall through instead.
        for index in [2u32, 3, 4, 5] {
            graph.edges.spill_tombstone_row(run, index).unwrap();
        }
        assert_eq!(graph.bucket_edge_spill_dead_rows(&source).unwrap(), 4);
        graph
            .vertices
            .set(VertexId::from(0), &LabeledVertex::from_parts(0, 1, 4, 0));

        let outcome = graph
            .compact_vertex_edge_span_one_step_with_span_policy(
                VertexId::from(0),
                0,
                0,
                &|_| EdgePlacementPolicy::Insertion,
                SpanResolutionPolicy::SlackMayBeDropped,
            )
            .unwrap();

        assert!(
            !matches!(outcome, VertexEdgeSpanCompactOneStep::OverflowRewrite(_)),
            "a move-free repack must not claim a batch, got {outcome:?}"
        );
        let after = graph.buckets.read_label_bucket_slot(0).unwrap();
        let fresh = graph.edges.spill_mint(after.edge_spill_run()).unwrap();
        assert_ne!(fresh, run, "the tail holes were still packed away");
        assert_eq!(graph.edges.spill_used_len(fresh), 2);
    }

    #[test]
    fn next_leaf_mate_edge_start_reports_the_mates_position() {
        use super::super::test_support::TestEdge;
        let (graph, _m) = physical_fixture::<TestEdge>();
        let source = graph.buckets.read_label_bucket_slot(0).unwrap();

        // The fixture's two buckets sit at 100 and 68, so the mate's position is the lower one. The
        // fold's room is measured against this, not against the region end, which is derived from
        // the regions that exist and stretches over a mate that owns none (ADR 0098).
        assert_eq!(
            graph
                .next_leaf_vertex_edge_start(VertexId::from(0))
                .unwrap(),
            Some(68)
        );

        // Moving the mate past this bucket's content moves the reported position with it, which is
        // what bounds the fold.
        let content_end = source.edge_start() + u64::from(source.stored_slots());
        graph
            .publish_bucket_descriptor(
                1,
                LabelBucket::from_parts(
                    source.bucket_label_key(),
                    content_end + 1,
                    source.degree(),
                    1,
                ),
            )
            .unwrap();
        assert_eq!(
            graph
                .next_leaf_vertex_edge_start(VertexId::from(0))
                .unwrap(),
            Some(content_end + 1)
        );

        // A row that was never written is not a mate: `vertices.len()` counts capacity, and reading
        // its buckets would be reading uninitialised slots.
        graph.push_vertex(LabeledVertex::default()).unwrap();
        assert_eq!(
            graph
                .next_leaf_vertex_edge_start(VertexId::from(0))
                .unwrap(),
            Some(content_end + 1),
            "an unwritten row contributes no edge position"
        );
    }

    #[test]
    fn fold_moves_the_spill_head_into_the_span_without_moving_ordinals() {
        use super::super::test_support::TestEdge;
        let (graph, _m) = physical_fixture::<TestEdge>();
        let source = append_physical_source_run(&graph);
        let run = graph.edges.spill_mint(source.edge_spill_run()).unwrap();
        // A head drop is legal only above the run's class floor (`used - class/2`), so the
        // fixture pushes the run past the 4-row floor of its 8-row class.
        for target in [7u32, 8, 9] {
            let bytes = target.to_le_bytes();
            assert!(graph.edges.spill_append(run, &bytes).unwrap());
        }
        let used = graph.edges.spill_used_len(run);
        assert!(used > 4, "the run must sit above its class floor");
        let head: Vec<TestEdge> = (0..2)
            .map(|index| graph.edges.spill_edge_at(run, index).unwrap())
            .collect();

        let folded = graph
            .fold_bucket_edge_spill_head_into_span(VertexId::from(0), 0, &source, 2)
            .unwrap();

        assert_eq!(folded, 2, "two spill rows move into the span");
        let after = graph.buckets.read_label_bucket_slot(0).unwrap();
        assert_eq!(
            after.stored_slots(),
            source.stored_slots() + 2,
            "the span grew by exactly the rows it absorbed"
        );
        assert_eq!(
            graph.edges.spill_used_len(run),
            used - 2,
            "and the run lost exactly those rows from its head"
        );
        // The absorbed rows sit at the end of the span, in the order they had in the spill.
        let start = super::bucket_content_region(&after).unwrap().0;
        for (index, expected) in head.iter().enumerate() {
            let slot = start + u64::from(after.stored_slots()) - 2 + index as u64;
            assert_eq!(graph.edges.read_slot(slot), *expected);
        }
    }

    #[test]
    fn physical_layout_overlapping_copy_keeps_source_ordinals() {
        use super::super::test_support::TestEdge;
        for destination in [98u64, 102] {
            let (graph, m) = physical_fixture::<TestEdge>();
            warm_physical_caches(&graph);
            let caches = cache_snapshot(&graph);
            let before = canonical_bytes(&m);
            take_events();
            let mut preparation = graph
                .prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite {
                    src: VertexId::from(0),
                    placement: VertexPhysicalPlacement::new(destination, 8, None, 0),
                })
                .unwrap();
            assert_canonical_bytes(&m, &before);
            assert_eq!(cache_snapshot(&graph), caches);
            preparation.ready().unwrap().publish();
            drop(preparation);
            assert_physical_result(&graph, [destination, 68], [8, 8], 64);
            assert_eq!(cache_snapshot(&graph), caches);
            assert!(graph.edges.free_byte_spans().is_empty());
            let events = take_events();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == PhysicalLayoutEvent::Publish)
                    .count(),
                1
            );
            assert!(!events.contains(&PhysicalLayoutEvent::Count));
            drop(graph);
            let graph = reopen_physical::<TestEdge>(&m);
            assert_physical_result(&graph, [destination, 68], [8, 8], 64);
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(2),
                    TestEdge { target: 17 },
                    super::super::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            let targets: Vec<_> = graph
                .iter_edges_for_label(VertexId::from(0), BucketLabelKey::from_raw(2))
                .unwrap()
                .into_iter()
                .map(|edge| edge.target)
                .collect();
            assert_eq!(targets, [17, 7, 0, 7]);
            let mut raw = [0u8; 20];
            graph
                .edges
                .read_slots_contiguous_bytes(destination, &mut raw);
            assert_eq!(
                raw,
                [7u32, 0, u32::MAX, 7, 17]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
                    .as_slice()
            );

            // A second overlapping copy retains a real run, not just NO_SPILL_RUN.
            // The original no-run next-insert case above remains independently fixed.
            let source = append_physical_source_run(&graph);
            assert_eq!(source.degree(), 7);
            assert_eq!(source.stored_slots(), 5);
            warm_physical_caches(&graph);
            let caches = cache_snapshot(&graph);
            let spill_before = m[6].bytes.borrow().clone();
            let untouched_before = canonical_bytes(&m)[10..16].to_vec();
            take_events();
            let mut preparation = graph
                .prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite {
                    src: VertexId::from(0),
                    placement: VertexPhysicalPlacement::new(100, 8, None, 0),
                })
                .unwrap();
            preparation.ready().unwrap().publish();
            drop(preparation);
            assert_eq!(cache_snapshot(&graph), caches);
            assert_eq!(*m[6].bytes.borrow(), spill_before);
            assert_eq!(&canonical_bytes(&m)[10..16], untouched_before);
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(source.with_edge_range(100, 5))
            );
            assert_eq!(
                graph.buckets.read_label_bucket_slot(1),
                Some(LabelBucket::from_parts(
                    BucketLabelKey::from_raw(2),
                    68,
                    2,
                    3
                ))
            );
            assert_eq!(
                graph.edges.counts_store().get(1),
                super::SegmentEdgeCounts {
                    actual: 9,
                    total: 64
                }
            );
            assert_eq!(graph.edges.header().num_edges, 9);
            graph.edges.read_slots_contiguous_bytes(100, &mut raw);
            assert_eq!(
                raw.as_slice(),
                [7u32, 0, u32::MAX, 7, 17]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            for ordinal in 0..2u32 {
                assert_eq!(
                    graph.vertices.get(VertexId::from(ordinal)),
                    LabeledVertex::from_parts(u64::from(ordinal), 1, 8, 0)
                );
            }
            let mut peer_raw = [0u8; 12];
            graph.edges.read_slots_contiguous_bytes(68, &mut peer_raw);
            assert_eq!(
                peer_raw.as_slice(),
                [11u32, u32::MAX, 11]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            assert!(!take_events().contains(&PhysicalLayoutEvent::Count));
            drop(graph);
            let graph = reopen_physical::<TestEdge>(&m);
            let run = graph.edges.spill_mint(source.edge_spill_run()).unwrap();
            assert_eq!(graph.edges.spill_used_len(run), 4);
            for (ordinal, target) in [31u32, 0, u32::MAX, 31].into_iter().enumerate() {
                assert_eq!(
                    graph
                        .edges
                        .spill_edge_at(run, ordinal as u32)
                        .unwrap()
                        .target,
                    target
                );
            }
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(source.with_edge_range(100, 5))
            );
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(2),
                    TestEdge { target: 19 },
                    super::super::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            assert_eq!(graph.edges.spill_used_len(run), 5);
            assert_eq!(graph.edges.spill_edge_at(run, 4).unwrap().target, 19);
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(source.with_edge_range(100, 5).with_degree_field(8))
            );
            let targets: Vec<_> = graph
                .iter_edges_for_label(VertexId::from(0), BucketLabelKey::from_raw(2))
                .unwrap()
                .into_iter()
                .map(|edge| edge.target)
                .collect();
            assert_eq!(targets, [19, 31, 0, 31, 17, 7, 0, 7]);

            // Outside a pin, overlapping rewrites retire only old slots outside
            // the whole new cover, including its uninitialized reserved slack.
            let src = VertexId::from(0);
            graph
                .edges
                .set_segment_physical_start(
                    crate::SegmentId::from(0),
                    crate::lara::edge::span_meta::SPAN_PHYSICAL_UNASSIGNED,
                )
                .unwrap();
            graph.edges.counts_store().set(
                1,
                &super::SegmentEdgeCounts {
                    actual: 10,
                    total: 16,
                },
            );
            warm_physical_caches(&graph);
            let caches = cache_snapshot(&graph);
            let spill_before = m[6].bytes.borrow().clone();
            let publications = graph.edges.free_span_store().publication_count();
            reset_free_span_release_calls();
            take_events();
            graph
                .rewrite_vertex_edge_span(src, None, 0, false, Some((destination, 8)))
                .unwrap();
            let retired = FreeSpan {
                start_slot: if destination == 98 { 106 } else { 100 },
                len: 2,
            };
            assert_eq!(graph.edges.free_byte_spans(), vec![retired]);
            assert_eq!(
                graph.edges.allocator_stats(),
                FreeSpanAllocatorStats {
                    free_bytes: 2,
                    largest_free_span: 2,
                    free_span_count: 1,
                }
            );
            assert_eq!(
                graph.edges.free_span_store().publication_count() - publications,
                1
            );
            assert_eq!(free_span_release_calls(), 0);
            assert_eq!(cache_snapshot(&graph), caches);
            assert_eq!(*m[6].bytes.borrow(), spill_before);
            let events = take_events();
            for event in [
                PhysicalLayoutEvent::Prepare(src),
                PhysicalLayoutEvent::Ready,
                PhysicalLayoutEvent::Publish,
                PhysicalLayoutEvent::Header,
                PhysicalLayoutEvent::Vertex(src),
                PhysicalLayoutEvent::Count,
                PhysicalLayoutEvent::Retire,
            ] {
                assert_eq!(events.iter().filter(|actual| **actual == event).count(), 1);
            }
            assert!(!events.contains(&PhysicalLayoutEvent::Pin));
            assert_eq!(graph.labeled_leaf_physical_range(src).unwrap(), None);
            let expected_bucket = source.with_edge_range(destination, 5).with_degree_field(8);
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(expected_bucket)
            );
            graph
                .edges
                .read_slots_contiguous_bytes(destination, &mut raw);
            assert_eq!(
                raw.as_slice(),
                [7u32, 0, u32::MAX, 7, 17]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                graph.edges.counts_store().get(1),
                super::SegmentEdgeCounts {
                    actual: 10,
                    total: 16
                }
            );
            assert_eq!(graph.edges.header().num_edges, 10);
            drop(graph);
            let graph = reopen_physical::<TestEdge>(&m);
            assert_eq!(graph.edges.free_byte_spans(), vec![retired]);
            assert_eq!(
                graph.edges.allocator_stats(),
                FreeSpanAllocatorStats {
                    free_bytes: 2,
                    largest_free_span: 2,
                    free_span_count: 1,
                }
            );
            assert_eq!(graph.labeled_leaf_physical_range(src).unwrap(), None);
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(expected_bucket)
            );
            assert_eq!(
                graph.buckets.read_label_bucket_slot(1),
                Some(LabelBucket::from_parts(
                    BucketLabelKey::from_raw(2),
                    68,
                    2,
                    3,
                ))
            );
            for ordinal in 0..2u32 {
                assert_eq!(
                    graph.vertices.get(VertexId::from(ordinal)),
                    LabeledVertex::from_parts(u64::from(ordinal), 1, 8, 0)
                );
            }
            assert_eq!(
                graph.edges.counts_store().get(1),
                super::SegmentEdgeCounts {
                    actual: 10,
                    total: 16
                }
            );
            assert_eq!(graph.edges.header().num_edges, 10);
            assert_eq!(*m[6].bytes.borrow(), spill_before);
            let run = graph
                .edges
                .spill_mint(expected_bucket.edge_spill_run())
                .unwrap();
            assert_eq!(graph.edges.spill_used_len(run), 5);
            for (ordinal, target) in [31u32, 0, u32::MAX, 31, 19].into_iter().enumerate() {
                assert_eq!(
                    graph
                        .edges
                        .spill_edge_at(run, ordinal as u32)
                        .unwrap()
                        .target,
                    target
                );
            }
            // Reusing and overwriting the freed range must not touch any new
            // resident or peer; allocator validity alone cannot prove that.
            assert_eq!(
                graph.edges.free_span_store().take_best_fit(2).unwrap(),
                Some(retired)
            );
            graph
                .edges
                .write_slots_contiguous_bytes(
                    retired.start_slot,
                    &[29u32, 37]
                        .into_iter()
                        .flat_map(u32::to_le_bytes)
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            assert!(graph.edges.free_byte_spans().is_empty());
            graph.edges.free_span_store().validate().unwrap();
            graph
                .edges
                .read_slots_contiguous_bytes(destination, &mut raw);
            assert_eq!(
                raw.as_slice(),
                [7u32, 0, u32::MAX, 7, 17]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            graph.edges.read_slots_contiguous_bytes(68, &mut peer_raw);
            assert_eq!(
                peer_raw.as_slice(),
                [11u32, u32::MAX, 11]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(expected_bucket)
            );
            assert_eq!(*m[6].bytes.borrow(), spill_before);
        }
    }

    #[test]
    fn physical_layout_ready_rejects_changed_source() {
        for changed in 0..5 {
            let (graph, m) = physical_fixture::<CopyOnlyEdge>();
            warm_physical_caches(&graph);
            let caches = cache_snapshot(&graph);
            take_events();
            let mut preparation = graph
                .prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite {
                    src: VertexId::from(0),
                    placement: VertexPhysicalPlacement::new(98, 8, None, 0),
                })
                .unwrap();
            match changed {
                0 => graph
                    .vertices
                    .set(VertexId::from(0), &LabeledVertex::from_parts(0, 1, 9, 0)),
                1 => graph
                    .publish_bucket_descriptor(
                        0,
                        LabelBucket::from_parts(BucketLabelKey::from_raw(2), 100, 2, 4),
                    )
                    .unwrap(),
                2 => graph.edges.set_num_edges(6),
                3 => graph
                    .edges
                    .set_segment_physical_start(crate::SegmentId::from(0), 65)
                    .unwrap(),
                4 => graph.edges.counts_store().set(
                    1,
                    &super::SegmentEdgeCounts {
                        actual: 6,
                        total: 64,
                    },
                ),
                _ => unreachable!(),
            }
            let deliberate = canonical_bytes(&m);
            assert!(matches!(
                preparation.ready(),
                Err(super::LabeledOperationError::Store(
                    super::LaraOperationError::CollectAllocationOverflow
                ))
            ));
            drop(preparation);
            assert_eq!(cache_snapshot(&graph), caches);
            assert_canonical_bytes(&m, &deliberate);
            assert!(!take_events().contains(&PhysicalLayoutEvent::Publish));
        }
    }

    #[test]
    fn physical_layout_mixed_residency_keeps_tree_and_values() {
        for destination in [80u64, 104] {
            let (graph, m) = physical_fixture::<CopyOnlyEdge>();
            let [slab, tree, tiny] = seed_mixed_physical_residents(&graph);
            assert_eq!(slab.degree(), 6);
            assert_eq!(slab.stored_slots(), 4);
            let peer = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 68, 2, 3);
            warm_physical_caches(&graph);
            let caches = cache_snapshot(&graph);
            let spill_before = m[6].bytes.borrow().clone();
            let ltb_before = m[15].bytes.borrow().clone();
            let value_before = canonical_bytes(&m)[10..15].to_vec();
            let mut preparation = graph
                .prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite {
                    src: VertexId::from(0),
                    placement: VertexPhysicalPlacement::new(destination, 16, None, 0),
                })
                .unwrap();
            preparation.ready().unwrap().publish();
            drop(preparation);
            assert_eq!(cache_snapshot(&graph), caches);
            assert_eq!(*m[6].bytes.borrow(), spill_before);
            assert_eq!(*m[15].bytes.borrow(), ltb_before);
            assert_eq!(&canonical_bytes(&m)[10..15], value_before);
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(slab.with_edge_range(destination, 4))
            );
            assert_eq!(
                graph.buckets.read_label_bucket_slot(1),
                Some(tree.with_edge_range(destination + 4, 512))
            );
            assert_eq!(
                graph.buckets.read_label_bucket_slot(2),
                Some(tiny.with_edge_range(destination + 15, 0))
            );
            assert_eq!(graph.buckets.read_label_bucket_slot(3), Some(peer));
            let mut actual = [0u8; 24];
            graph
                .edges
                .read_slots_contiguous_bytes(destination, &mut actual);
            assert_eq!(
                actual,
                [7u32, 0, u32::MAX, 7, 0, 1]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
                    .as_slice()
            );
            assert_eq!(
                graph.edges.counts_store().get(1),
                super::SegmentEdgeCounts {
                    actual: 8,
                    total: 64
                }
            );
            assert_eq!(graph.edges.header().num_edges, 521);
            assert_eq!(
                graph.vertices.get(VertexId::from(0)),
                LabeledVertex::from_parts(0, 3, 16, 0)
            );
            assert_eq!(
                graph.vertices.get(VertexId::from(1)),
                LabeledVertex::from_parts(3, 1, 8, 0)
            );
            let mut peer_raw = [0u8; 12];
            graph.edges.read_slots_contiguous_bytes(68, &mut peer_raw);
            assert_eq!(
                peer_raw.as_slice(),
                [11u32, u32::MAX, 11]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            drop(graph);
            let graph = reopen_physical::<CopyOnlyEdge>(&m);
            assert_eq!(
                graph.buckets.read_label_bucket_slot(0),
                Some(slab.with_edge_range(destination, 4))
            );
            assert_eq!(
                graph.buckets.read_label_bucket_slot(1),
                Some(tree.with_edge_range(destination + 4, 512))
            );
            assert_eq!(*m[6].bytes.borrow(), spill_before);
            assert_eq!(*m[15].bytes.borrow(), ltb_before);
        }
    }

    fn physical_copy_64_metrics() {
        use crate::labeled::ltb_raw_block_store::{BlockHeader, BlockKind};
        for tree in [false, true] {
            let (graph, m) =
                empty_physical_fixture::<CopyOnlyEdge>(InitialCapacities::uniform(4096), 64);
            for ordinal in 0..64u32 {
                let vid = VertexId::from(ordinal);
                graph.push_vertex(LabeledVertex::default()).unwrap();
                let origin = 128 + u64::from(ordinal) * 16 + 5;
                let (bucket, width, raw) = if tree {
                    let mut raw = Vec::new();
                    for index in 0..8u32 {
                        let id = graph.ltb.mint().unwrap();
                        assert_eq!(id, ordinal * 8 + index);
                        graph.ltb.write_block_header(
                            id,
                            &BlockHeader {
                                kind: if index < 4 {
                                    BlockKind::Edge
                                } else {
                                    BlockKind::InlineProperty
                                },
                                bucket_label_key_wire: 2,
                                owner_or_next_free: ordinal,
                                ordinal: index % 4,
                                level: 0,
                                tombstone_count: 0,
                                reserved: [0],
                            },
                        );
                        let mut payload = [0u8; 4096];
                        for (slot, word) in payload.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                            let value = if index < 4 {
                                ordinal * 4096 + index * 1024 + slot as u32
                            } else {
                                0x4000_0000 + ordinal * 4096 + (index - 4) * 1024 + slot as u32
                            };
                            word.copy_from_slice(&value.to_le_bytes());
                        }
                        graph.ltb.write_payload(id, &payload).unwrap();
                        raw.extend(id.to_le_bytes());
                    }
                    (
                        LabelBucket::from_parts_with_inline_property(
                            BucketLabelKey::from_raw(2),
                            origin,
                            4096,
                            4096,
                            4,
                            0,
                            0,
                            -1,
                            0,
                        )
                        .with_tree_mode(true),
                        8,
                        raw,
                    )
                } else {
                    (
                        LabelBucket::from_parts(BucketLabelKey::from_raw(2), origin, 4, 6),
                        6,
                        [
                            ordinal * 16,
                            u32::MAX,
                            ordinal * 16 + 2,
                            ordinal * 16 + 2,
                            u32::MAX,
                            ordinal * 16 + 5,
                        ]
                        .into_iter()
                        .flat_map(u32::to_le_bytes)
                        .collect(),
                    )
                };
                graph
                    .edges
                    .write_slots_contiguous_bytes(origin, &raw)
                    .unwrap();
                graph
                    .publish_bucket_descriptor(u64::from(ordinal), bucket)
                    .unwrap();
                graph.vertices.set(
                    vid,
                    &LabeledVertex::from_parts(u64::from(ordinal), 1, width, 0),
                );
            }
            graph
                .edges
                .set_segment_physical_start(crate::SegmentId::from(0), 128)
                .unwrap();
            graph
                .edges
                .bump_vertex_segment_counts(VertexId::from(0), if tree { 0 } else { 256 }, 1024)
                .unwrap();
            graph
                .edges
                .set_num_edges(if tree { 64 * 4096 } else { 256 });
            let before: Vec<_> = m
                .iter()
                .map(|memory| {
                    (
                        memory.reads.get(),
                        memory.read_bytes.get(),
                        memory.writes.get(),
                        memory.write_bytes.get(),
                        memory.size(),
                    )
                })
                .collect();
            let mut preparation = graph
                .prepare_physical_layout(PhysicalLayoutOperation::LeafSlide {
                    src: VertexId::from(0),
                    destination: LeafPhysicalDestination::Current(FreeSpan {
                        start_slot: 128,
                        len: 1024,
                    }),
                })
                .unwrap();
            let (retained, scratch) = preparation.slab.copy_buffer_metrics();
            assert_eq!(retained, if tree { 2048 } else { 1536 });
            assert_eq!(scratch, 0);
            preparation.ready().unwrap().publish();
            drop(preparation);
            let after: Vec<_> = m
                .iter()
                .zip(&before)
                .map(|(memory, before)| {
                    (
                        memory.reads.get() - before.0,
                        memory.read_bytes.get() - before.1,
                        memory.writes.get() - before.2,
                        memory.write_bytes.get() - before.3,
                        memory.size() - before.4,
                    )
                })
                .collect();
            assert_eq!(after[5].2, 65);
            assert_eq!(after[5].3, retained as u64 + 64);
            assert_eq!(after[1].2, 64);
            assert_eq!(after[1].3, 64 * 34);
            assert_eq!(after[0].2, 64);
            assert_eq!(after[0].3, 64 * 25);
            assert_eq!(after[4].2, 1);
            assert!(after.iter().all(|row| row.4 == 0));
            assert_eq!(after[15].0, 0);
            assert_eq!(after[15].2, 0);
            println!(
                "physical_copy_64_metrics shape={} retained={} streaming_scratch={} validation_stack=128 regions={:?}",
                if tree { "tree" } else { "slab" },
                retained,
                scratch,
                after
            );
            for ordinal in 0..64u32 {
                let origin = if tree {
                    128 + u64::from(ordinal) * 16
                } else {
                    128 + u64::from(ordinal) * 15
                };
                let width = if tree {
                    16
                } else if ordinal == 63 {
                    79
                } else {
                    15
                };
                assert_eq!(
                    graph.vertices.get(VertexId::from(ordinal)),
                    LabeledVertex::from_parts(u64::from(ordinal), 1, width, 0)
                );
                assert_eq!(
                    graph
                        .buckets
                        .read_label_bucket_slot(u64::from(ordinal))
                        .unwrap()
                        .edge_start(),
                    origin
                );
            }
        }
    }

    #[test]
    fn physical_layout_commit_paths_share_admission() {
        use super::super::test_support::TestEdge;
        for branch in 0..3 {
            let (graph, m) = physical_fixture::<TestEdge>();
            let src = VertexId::from(0);
            let original = graph.vertices.get(src);
            let bucket = graph.buckets.read_label_bucket_slot(0).unwrap();
            graph.publish_bucket_descriptor(255, bucket).unwrap();
            graph.vertices.set(
                src,
                &original.try_with_bucket_row_and_slack(255, 1, 1).unwrap(),
            );
            warm_physical_caches(&graph);
            let caches = cache_snapshot(&graph);
            let before = canonical_bytes(&m);
            let publications = graph.edges.free_span_store().publication_count();
            reset_free_span_release_calls();
            let consume = || match branch {
                0 => graph.rewrite_vertex_edge_span(src, None, 0, false, Some((98, 8))),
                1 => graph.relocate_labeled_leaf_physical_block(src),
                2 => graph.rebalance_labeled_leaf_weighted_slide(src),
                _ => unreachable!(),
            };
            take_events();
            assert!(matches!(
                consume(),
                Err(super::LabeledOperationError::Store(
                    super::LaraOperationError::CollectAllocationOverflow
                ))
            ));
            assert_canonical_bytes(&m, &before);
            assert_eq!(cache_snapshot(&graph), caches);
            assert_eq!(
                graph.edges.free_span_store().publication_count(),
                publications
            );
            assert_eq!(free_span_release_calls(), 0);
            let events = take_events();
            assert!(!events.contains(&PhysicalLayoutEvent::Publish));
            assert_eq!(
                events.last(),
                Some(&PhysicalLayoutEvent::AdmitDescriptor(src))
            );
            if branch != 0 {
                assert!(events.contains(&PhysicalLayoutEvent::AdmitDescriptor(VertexId::from(1))));
            }
            graph.vertices.set(src, &original);
            take_events();
            consume().unwrap();
            assert_eq!(
                graph.edges.free_span_store().publication_count() - publications,
                1
            );
            assert_eq!(free_span_release_calls(), 0);
            assert_eq!(
                graph.edges.free_byte_spans(),
                if branch == 1 {
                    vec![FreeSpan {
                        start_slot: 64,
                        len: 64,
                    }]
                } else {
                    vec![]
                }
            );
            let events = take_events();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == PhysicalLayoutEvent::Pin)
                    .count(),
                usize::from(branch == 1)
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == PhysicalLayoutEvent::Prepare(src))
                    .count(),
                1
            );
            for event in [
                PhysicalLayoutEvent::Ready,
                PhysicalLayoutEvent::Publish,
                PhysicalLayoutEvent::Header,
                PhysicalLayoutEvent::Retire,
            ] {
                assert_eq!(events.iter().filter(|actual| **actual == event).count(), 1);
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == PhysicalLayoutEvent::Count)
                    .count(),
                usize::from(branch != 0)
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == PhysicalLayoutEvent::Vertex(src))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == PhysicalLayoutEvent::Vertex(VertexId::from(1)))
                    .count(),
                usize::from(branch != 0)
            );
            match branch {
                0 => assert_physical_result(&graph, [98, 68], [8, 8], 64),
                1 => {
                    assert!(events.contains(&PhysicalLayoutEvent::Streamed));
                    assert_physical_result(&graph, [256, 301], [45, 35], 80);
                    assert_eq!(graph.edges.span_meta_store().get(0).physical_start, 256);
                    assert_eq!(graph.edges.header().elem_capacity, 336);
                }
                2 => {
                    assert!(events.contains(&PhysicalLayoutEvent::Buffered));
                    assert_physical_result(&graph, [64, 100], [36, 28], 64);
                }
                _ => unreachable!(),
            }
            if branch == 0 {
                // The pinned case above must not stand in for ordinary unpinned
                // count admission. Reuse its graph; only the fixture pin/count changes.
                graph
                    .edges
                    .set_segment_physical_start(
                        crate::SegmentId::from(0),
                        crate::lara::edge::span_meta::SPAN_PHYSICAL_UNASSIGNED,
                    )
                    .unwrap();
                graph.edges.counts_store().set(
                    1,
                    &super::SegmentEdgeCounts {
                        actual: 5,
                        total: i64::MAX,
                    },
                );
                let before = canonical_bytes(&m);
                let caches = cache_snapshot(&graph);
                let publications = graph.edges.free_span_store().publication_count();
                reset_free_span_release_calls();
                take_events();
                assert!(matches!(
                    graph.rewrite_vertex_edge_span(src, None, 0, false, Some((98, 12))),
                    Err(super::LabeledOperationError::Store(
                        super::LaraOperationError::CollectAllocationOverflow
                    ))
                ));
                assert_canonical_bytes(&m, &before);
                assert_eq!(cache_snapshot(&graph), caches);
                assert_eq!(
                    graph.edges.free_span_store().publication_count(),
                    publications
                );
                assert_eq!(free_span_release_calls(), 0);
                assert!(!take_events().contains(&PhysicalLayoutEvent::Publish));
                graph.edges.counts_store().set(
                    1,
                    &super::SegmentEdgeCounts {
                        actual: 5,
                        total: 16,
                    },
                );
                take_events();
                graph
                    .rewrite_vertex_edge_span(src, None, 0, false, Some((98, 12)))
                    .unwrap();
                assert_eq!(
                    graph.edges.free_span_store().publication_count() - publications,
                    1
                );
                assert_eq!(free_span_release_calls(), 0);
                let events = take_events();
                for event in [
                    PhysicalLayoutEvent::Prepare(src),
                    PhysicalLayoutEvent::Ready,
                    PhysicalLayoutEvent::Publish,
                    PhysicalLayoutEvent::Header,
                    PhysicalLayoutEvent::Vertex(src),
                    PhysicalLayoutEvent::Count,
                    PhysicalLayoutEvent::Retire,
                ] {
                    assert_eq!(events.iter().filter(|actual| **actual == event).count(), 1);
                }
                assert!(!events.contains(&PhysicalLayoutEvent::Pin));
                assert_physical_result(&graph, [98, 68], [12, 8], 20);
                assert_eq!(cache_snapshot(&graph), caches);
            }
            drop(graph);
            let graph = reopen_physical::<TestEdge>(&m);
            assert_eq!(
                graph.edges.free_byte_spans(),
                if branch == 1 {
                    vec![FreeSpan {
                        start_slot: 64,
                        len: 64,
                    }]
                } else {
                    vec![]
                }
            );
            assert_eq!(
                graph.edges.allocator_stats(),
                if branch == 1 {
                    FreeSpanAllocatorStats {
                        free_bytes: 64,
                        largest_free_span: 64,
                        free_span_count: 1,
                    }
                } else {
                    FreeSpanAllocatorStats {
                        free_bytes: 0,
                        largest_free_span: 0,
                        free_span_count: 0,
                    }
                }
            );
            graph.edges.free_span_store().validate().unwrap();
            match branch {
                0 => {
                    assert_physical_result(&graph, [98, 68], [12, 8], 20);
                    assert_eq!(graph.labeled_leaf_physical_range(src).unwrap(), None);
                }
                1 => {
                    assert_physical_result(&graph, [256, 301], [45, 35], 80);
                    assert_eq!(
                        graph.labeled_leaf_physical_range(src).unwrap(),
                        Some((256, 80))
                    );
                    assert_eq!(graph.edges.header().elem_capacity, 336);
                }
                2 => {
                    assert_physical_result(&graph, [64, 100], [36, 28], 64);
                    assert_eq!(
                        graph.labeled_leaf_physical_range(src).unwrap(),
                        Some((64, 64))
                    );
                }
                _ => unreachable!(),
            }
        }
        physical_copy_64_metrics();
    }

    #[test]
    fn physical_layout_backing_rejection_is_unpublished() {
        use crate::lara::{
            edge::{EdgeHeaderV1, EdgeSlabStore},
            operation_error::LaraOperationError,
        };

        let memory = CopyAdmissionMemory::default();
        let header = EdgeHeaderV1::new(16, 1, 16, 4, 0);
        let store = EdgeSlabStore::<CopyOnlyEdge, _>::new(memory.clone(), header).unwrap();
        let prefix: Vec<u8> = [7u32, 0, u32::MAX, 7]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        let roots: Vec<u8> = [101u32, 0, 103, 201]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        store.write_slots_contiguous(0, &prefix).unwrap();
        store.write_slots_contiguous(8, &roots).unwrap();
        let before = memory.bytes.borrow().clone();
        let writes_before = memory.writes.get();
        let headers_before = memory.header_writes.get();
        assert_eq!(memory.size(), 5);
        memory.grows.borrow_mut().clear();

        let grown_header = EdgeHeaderV1 {
            elem_capacity: 100_000,
            ..header
        };
        memory.reject_grow.set(true);
        let error = store
            .prepare_physical_image(
                grown_header,
                vec![(0, roots.clone()), (90_000, prefix.clone())],
            )
            .map_err(LaraOperationError::ResizeFailed)
            .err()
            .unwrap();
        assert!(matches!(
            error,
            LaraOperationError::ResizeFailed(crate::GrowFailed {
                current_size: 5,
                delta: 6,
            })
        ));
        assert_eq!(*memory.grows.borrow(), [6]);
        assert_eq!(memory.writes.get(), writes_before);
        assert_eq!(memory.header_writes.get(), headers_before);
        assert_eq!(*memory.bytes.borrow(), before);
        assert_eq!(store.header().unwrap(), header);

        // A later invalid range must be checked before even backing admission.
        memory.grows.borrow_mut().clear();
        let error = store
            .prepare_physical_image(
                grown_header,
                vec![(0, roots.clone()), (100_000, prefix.clone())],
            )
            .err()
            .unwrap();
        assert_eq!(error.current_size, 5);
        assert_eq!(error.delta, 0);
        assert!(memory.grows.borrow().is_empty());
        assert_eq!(*memory.bytes.borrow(), before);
        assert_eq!(memory.writes.get(), writes_before);
        assert_eq!(memory.header_writes.get(), headers_before);

        // Successful reserve may keep backing capacity, never a new logical header.
        let image = store
            .prepare_physical_image(
                grown_header,
                vec![(0, roots.clone()), (90_000, prefix.clone())],
            )
            .unwrap();
        assert_eq!(*memory.grows.borrow(), [6]);
        assert_eq!(memory.size(), 11);
        assert_eq!(memory.writes.get() - writes_before, 1);
        assert_eq!(memory.header_writes.get(), headers_before);
        assert_eq!(&memory.bytes.borrow()[..before.len()], before.as_slice());
        assert_eq!(store.header().unwrap(), header);
        drop(image);
        assert_eq!(&memory.bytes.borrow()[..before.len()], before.as_slice());
        assert_eq!(store.header().unwrap(), header);
        memory.grows.borrow_mut().clear();
        let before = memory.bytes.borrow().clone();
        let writes_before = memory.writes.get();

        let image = store
            .prepare_physical_image(
                header,
                vec![(2, prefix.clone()), (7, roots.clone()), (16, Vec::new())],
            )
            .unwrap();
        assert_eq!(*memory.bytes.borrow(), before);
        assert_eq!(memory.writes.get(), writes_before);
        drop(image);
        assert_eq!(*memory.bytes.borrow(), before);
        assert_eq!(memory.header_writes.get(), headers_before);

        // Both buffers are retained before their destinations overwrite old sources.
        // This proves only the slab owner; enclosing graph readiness is tested later.
        let image = store
            .prepare_physical_image(header, vec![(2, prefix), (7, roots)])
            .unwrap();
        image.write();
        assert_eq!(memory.writes.get() - writes_before, 3);
        assert_eq!(memory.header_writes.get() - headers_before, 1);
        let mut actual = [0u8; 16];
        store.read_slots_contiguous(2, &mut actual);
        assert_eq!(
            actual,
            [7u32, 0, u32::MAX, 7]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>()
                .as_slice()
        );
        store.read_slots_contiguous(7, &mut actual);
        assert_eq!(
            actual,
            [101u32, 0, 103, 201]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(store.header().unwrap(), header);
        assert!(memory.grows.borrow().is_empty());

        let (graph, m) = physical_fixture::<CopyOnlyEdge>();
        let [original, tree, tiny] = seed_mixed_physical_residents(&graph);
        let src = VertexId::from(0);
        graph.edges.release_span(88, 4).unwrap();
        warm_physical_caches(&graph);
        graph
            .find_bucket(src, &graph.vertices.get(src), BucketLabelKey::from_raw(3))
            .unwrap();
        let caches = cache_snapshot(&graph);
        let allocator_cache = graph.edges.free_span_store().physical_cache_snapshot();
        let before = canonical_bytes(&m);
        m[5].grows.borrow_mut().clear();
        m[5].reject_grow.set(true);
        take_events();
        let error = graph
            .prepare_physical_layout(PhysicalLayoutOperation::LeafSlide {
                src,
                destination: LeafPhysicalDestination::Relocate(100_000),
            })
            .err()
            .unwrap();
        assert!(matches!(
            error,
            super::LabeledOperationError::Store(LaraOperationError::ResizeFailed(
                crate::GrowFailed {
                    current_size: 5,
                    delta: 6
                }
            ))
        ));
        assert_eq!(*m[5].grows.borrow(), [6]);
        assert_eq!(cache_snapshot(&graph), caches);
        assert_eq!(
            graph.edges.free_span_store().physical_cache_snapshot(),
            allocator_cache
        );
        assert_canonical_bytes(&m, &before);
        assert!(!take_events().contains(&PhysicalLayoutEvent::Publish));

        // A malformed desired descriptor reaches the shared round-trip owner,
        // independently of the backing failure and without requiring any grow.
        // This independently reached encode failure retains the same live roots,
        // run and peer; only the unpersisted desired tiny descriptor is malformed.
        warm_physical_caches(&graph);
        graph
            .find_bucket(src, &graph.vertices.get(src), BucketLabelKey::from_raw(3))
            .unwrap();
        let caches = cache_snapshot(&graph);
        let allocator_cache = graph.edges.free_span_store().physical_cache_snapshot();
        let before = canonical_bytes(&m);
        let grows: Vec<_> = m.iter().map(|memory| memory.grows.borrow().len()).collect();
        let mut malformed = tiny;
        malformed.degree = 5;
        take_events();
        let error = super::PreparedVertexPhysicalCopy::reserve_desired(
            &graph,
            src,
            (vec![original, tree, malformed], vec![None, None, None]),
            VertexPhysicalPlacement::new(98, 16, None, 0),
        )
        .err()
        .unwrap();
        assert!(matches!(
            error,
            super::LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
        assert_eq!(take_events(), [PhysicalLayoutEvent::AdmitDescriptor(src)]);
        assert_eq!(
            m.iter()
                .map(|memory| memory.grows.borrow().len())
                .collect::<Vec<_>>(),
            grows
        );
        assert_eq!(cache_snapshot(&graph), caches);
        assert_eq!(
            graph.edges.free_span_store().physical_cache_snapshot(),
            allocator_cache
        );
        assert_canonical_bytes(&m, &before);
    }

    #[test]
    fn physical_layout_earlier_shape_admits_without_publication() {
        use super::{PhysicalLayoutOperation, PreparedVertexPhysicalCopy, VertexPhysicalPlacement};
        use crate::{
            SegmentId, VertexId,
            labeled::{BucketLabelKey, InitialCapacities, LabeledVertex, record::LabelBucket},
            traits::CsrVertex,
        };

        for order in [[0usize, 3], [3, 0]] {
            let memories: [VectorMemory; 16] = std::array::from_fn(|_| VectorMemory::default());
            let graph = super::LabeledLaraGraph::<CopyOnlyEdge, _>::new_with_segment_size(
                memories[0].clone(),
                memories[1].clone(),
                memories[2].clone(),
                memories[3].clone(),
                memories[4].clone(),
                memories[5].clone(),
                memories[6].clone(),
                memories[7].clone(),
                memories[8].clone(),
                memories[9].clone(),
                memories[10].clone(),
                memories[11].clone(),
                memories[12].clone(),
                memories[13].clone(),
                memories[14].clone(),
                memories[15].clone(),
                InitialCapacities::uniform(256),
                BucketLabelKey::from_raw(9),
                16,
            )
            .unwrap();
            let src = VertexId::from(0);
            graph.push_vertex(LabeledVertex::default()).unwrap();
            graph.push_vertex(LabeledVertex::default()).unwrap();
            let early = LabelBucket::from_parts(BucketLabelKey::from_raw(1), 80, 0, 0)
                .try_enable_tiny_mode(4)
                .unwrap()
                .with_tiny_target(0, 41)
                .with_tiny_target(1, 0)
                .with_tiny_target(2, u32::MAX)
                .with_tiny_target(3, 41)
                .with_degree_field(3)
                .with_tiny_used_width(4);
            let slab = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 80, 3, 4);
            let tree = LabelBucket::from_parts_with_inline_property(
                BucketLabelKey::from_raw(3),
                88,
                512,
                512,
                4,
                0,
                0,
                -1,
                0,
            )
            .with_tree_mode(true);
            let late = LabelBucket::from_parts(BucketLabelKey::from_raw(4), 90, 0, 0)
                .try_enable_tiny_mode(4)
                .unwrap()
                .with_tiny_target(0, 51)
                .with_degree_field(1)
                .with_tiny_used_width(1);
            let source = vec![early, slab, tree, late];
            graph
                .buckets
                .write_label_bucket_slots_contiguous(0, &source)
                .unwrap();
            graph
                .vertices
                .set(src, &LabeledVertex::from_parts(0, 4, 32, 0));
            let peer = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 192, 1, 2);
            graph.publish_bucket_descriptor(4, peer).unwrap();
            graph
                .vertices
                .set(VertexId::from(1), &LabeledVertex::from_parts(4, 1, 16, 0));
            let words = |values: &[u32]| {
                values
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>()
            };
            graph
                .edges
                .write_slots_contiguous_bytes(80, &words(&[21, 0, u32::MAX, 21]))
                .unwrap();
            graph
                .edges
                .write_slots_contiguous_bytes(88, &words(&[101, 201]))
                .unwrap();
            graph
                .edges
                .write_slots_contiguous_bytes(192, &words(&[61, u32::MAX]))
                .unwrap();
            graph
                .edges
                .set_segment_physical_start(SegmentId::from(0), 64)
                .unwrap();
            graph.edges.bump_vertex_segment_counts(src, 4, 144).unwrap();
            graph.edges.set_num_edges(520);

            // Desired logical transcription is supplied by the geometry caller;
            // neither requested promotion order can influence final label order.
            let mut desired = source.clone();
            let mut transcribed = vec![None, None, None, None];
            for index in order {
                desired[index] = LabelBucket::from_parts(
                    source[index].bucket_label_key(),
                    source[index].edge_start(),
                    source[index].degree(),
                    source[index].tiny_used_width(),
                );
                transcribed[index] = Some(if index == 0 {
                    words(&[41, 0, u32::MAX, 41])
                } else {
                    words(&[51])
                });
            }
            let before: Vec<_> = memories.iter().map(|m| m.borrow().clone()).collect();
            graph
                .find_bucket(src, &graph.vertices.get(src), BucketLabelKey::from_raw(2))
                .unwrap();
            graph
                .find_bucket(
                    VertexId::from(1),
                    &graph.vertices.get(VertexId::from(1)),
                    BucketLabelKey::from_raw(2),
                )
                .unwrap();
            let cache_value = |entry: Option<super::super::BucketLookupCache>| {
                entry.map(|entry| {
                    (
                        entry.vid,
                        entry.bucket_key,
                        entry.base_slot_start,
                        entry.degree,
                        entry.slot,
                    )
                })
            };
            let cache_snapshot = || {
                (
                    cache_value(graph.last_bucket_lookup.get()),
                    graph
                        .bucket_lookup_cache
                        .each_ref()
                        .map(|entry| cache_value(entry.get())),
                )
            };
            let caches = cache_snapshot();
            let placement = VertexPhysicalPlacement::new(64, 16, None, 0);
            let component = PreparedVertexPhysicalCopy::reserve_desired(
                &graph,
                src,
                (desired, transcribed),
                placement,
            )
            .unwrap();
            assert_eq!(
                component
                    .final_buckets
                    .iter()
                    .copied()
                    .map(LabelBucket::edge_start)
                    .collect::<Vec<_>>(),
                [64, 68, 72, 78]
            );
            assert_eq!(
                component.final_vertex,
                LabeledVertex::from_parts(0, 4, 16, 0)
            );
            assert_eq!(component.retained_source_bytes(), 24);
            assert_eq!(component.transcribed_bytes(), 20);
            component.validate_source().unwrap();
            drop(component);
            assert_eq!(cache_snapshot(), caches);
            for (memory, expected) in memories.iter().zip(&before) {
                assert_eq!(&memory.borrow()[..expected.len()], expected);
            }

            // Only canonical descriptors enter the physical-only ready factory.
            // This subcase uses CsrEdge with no CsrEdgeTombstone implementation.
            let mut preparation = graph
                .prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite {
                    src,
                    placement: VertexPhysicalPlacement::new(80, 16, None, 0),
                })
                .unwrap();
            preparation.ready().unwrap().publish();
            assert_eq!(
                graph.vertices.get(src),
                LabeledVertex::from_parts(0, 4, 16, 0)
            );
            let final_buckets = graph
                .read_vertex_label_buckets(&graph.vertices.get(src))
                .unwrap();
            assert_eq!(final_buckets[0], early.with_edge_range(80, 41));
            assert_eq!(final_buckets[1], slab.with_edge_range(80, 4));
            assert_eq!(final_buckets[2], tree.with_edge_range(84, 512));
            assert_eq!(final_buckets[3], late.with_edge_range(95, 0));
            assert_eq!(graph.edges.counts_store().get(1).actual, 4);
            assert_eq!(graph.edges.counts_store().get(1).total, 144);
            assert_eq!(graph.edges.header().num_edges, 520);
            assert_eq!(graph.buckets.read_label_bucket_slot(4), Some(peer));
            let mut raw = [0u8; 16];
            graph.edges.read_slots_contiguous_bytes(80, &mut raw);
            assert_eq!(raw.as_slice(), words(&[21, 0, u32::MAX, 21]));
            graph.edges.read_slots_contiguous_bytes(84, &mut raw[..8]);
            assert_eq!(&raw[..8], words(&[101, 201]));
        }
    }

    #[test]
    fn physical_layout_abort_preserves_owners_and_caches() {
        for choice in 0..3 {
            let expected_free: Vec<_> = match choice {
                0 => vec![(48, 8), (88, 4), (160, 32), (208, 32)],
                1 => vec![(48, 8), (64, 64)],
                2 => vec![(48, 8), (64, 64), (160, 16)],
                _ => unreachable!(),
            }
            .into_iter()
            .map(|(start_slot, len)| FreeSpan { start_slot, len })
            .collect();
            let expected_stats = match choice {
                0 => FreeSpanAllocatorStats {
                    free_bytes: 76,
                    largest_free_span: 32,
                    free_span_count: 4,
                },
                1 => FreeSpanAllocatorStats {
                    free_bytes: 72,
                    largest_free_span: 64,
                    free_span_count: 2,
                },
                2 => FreeSpanAllocatorStats {
                    free_bytes: 88,
                    largest_free_span: 64,
                    free_span_count: 3,
                },
                _ => unreachable!(),
            };
            for consume in [false, true] {
                let (graph, m) = physical_fixture::<CopyOnlyEdge>();
                let src = VertexId::from(0);
                for (start, len) in [(48, 8), (88, 4)] {
                    graph.edges.release_span(start, len).unwrap();
                }
                match choice {
                    0 => {
                        graph.edges.release_span(128, 64).unwrap();
                        graph.edges.release_span(208, 32).unwrap();
                    }
                    1 => graph.edges.release_span(160, 96).unwrap(),
                    2 => graph.edges.release_span(160, 16).unwrap(),
                    _ => unreachable!(),
                }
                warm_physical_caches(&graph);
                let caches = cache_snapshot(&graph);
                let allocator_cache = graph.edges.free_span_store().physical_cache_snapshot();
                let before = canonical_bytes(&m);
                let publications = graph.edges.free_span_store().publication_count();
                reset_free_span_release_calls();
                take_events();
                let mut preparation = graph
                    .prepare_physical_layout(PhysicalLayoutOperation::LeafSlide {
                        src,
                        destination: LeafPhysicalDestination::Relocate(96),
                    })
                    .unwrap();
                assert_eq!(
                    preparation.allocator.selected_span(),
                    match choice {
                        0 => Some(FreeSpan {
                            start_slot: 128,
                            len: 64
                        }),
                        1 => Some(FreeSpan {
                            start_slot: 160,
                            len: 96
                        }),
                        2 => None,
                        _ => unreachable!(),
                    }
                );
                assert_eq!(
                    preparation.allocator.retired_intervals(),
                    if choice == 0 {
                        &[][..]
                    } else {
                        &[
                            FreeSpan {
                                start_slot: 64,
                                len: 24,
                            },
                            FreeSpan {
                                start_slot: 92,
                                len: 36,
                            },
                        ][..]
                    }
                );
                assert_canonical_bytes(&m, &before);
                assert_eq!(cache_snapshot(&graph), caches);
                if choice == 1 && !consume {
                    preparation.allocator.change_source_span_len_for_test(88, 6);
                    let deliberate = canonical_bytes(&m);
                    assert!(matches!(
                        preparation.ready(),
                        Err(super::LabeledOperationError::Store(
                            super::LaraOperationError::RebalanceFailed(crate::GrowFailed {
                                current_size: 0,
                                delta: 0
                            })
                        ))
                    ));
                    drop(preparation);
                    assert_canonical_bytes(&m, &deliberate);
                } else {
                    let ready = preparation.ready().unwrap();
                    if consume {
                        ready.publish();
                    } else {
                        drop(ready);
                    }
                    drop(preparation);
                    if consume {
                        let (start, widths) = match choice {
                            0 => (64, [54, 42]),
                            1 => (160, [54, 42]),
                            2 => (256, [54, 42]),
                            _ => unreachable!(),
                        };
                        assert_physical_result(&graph, [start, start + 54], widths, 96);
                        assert_eq!(graph.edges.free_byte_spans(), expected_free);
                        assert_eq!(graph.edges.allocator_stats(), expected_stats);
                        graph.edges.free_span_store().validate().unwrap();
                    } else {
                        assert_canonical_bytes(&m, &before);
                    }
                }
                assert_eq!(
                    graph.edges.free_span_store().publication_count() - publications,
                    u64::from(consume)
                );
                assert_eq!(free_span_release_calls(), 0);
                assert_eq!(cache_snapshot(&graph), caches);
                if !consume {
                    assert_eq!(
                        graph.edges.free_span_store().physical_cache_snapshot(),
                        allocator_cache
                    );
                    assert!(!take_events().contains(&PhysicalLayoutEvent::Publish));
                } else {
                    drop(graph);
                    let graph = reopen_physical::<CopyOnlyEdge>(&m);
                    let start = match choice {
                        0 => 64,
                        1 => 160,
                        2 => 256,
                        _ => unreachable!(),
                    };
                    assert_physical_result(&graph, [start, start + 54], [54, 42], 96);
                    assert_eq!(
                        graph.labeled_leaf_physical_range(src).unwrap(),
                        Some((start, 96))
                    );
                    assert_eq!(graph.edges.free_byte_spans(), expected_free);
                    assert_eq!(graph.edges.allocator_stats(), expected_stats);
                    graph.edges.free_span_store().validate().unwrap();
                }
            }
        }

        // One source-aware loan must subtract canonical holes before taking a
        // prefix, including when the allocation is inside a candidate cover.
        for consume in [false, true] {
            let records = VectorMemory::default();
            let index = VectorMemory::default();
            let store = FreeSpanStore::new(records.clone(), index.clone()).unwrap();
            store.release_span(30, 10).unwrap();
            let before = store.header();
            let cache_before = store.physical_cache_snapshot();
            let records_before = records.borrow().clone();
            let index_before = index.borrow().clone();
            let publications_before = store.publication_count();
            reset_free_span_release_calls();
            let mut preparation = store
                .prepare_physical_replacement(
                    PhysicalSpanSelection::BestFit(4),
                    &[FreeSpan {
                        start_slot: 20,
                        len: 20,
                    }],
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                preparation.selected_span(),
                Some(FreeSpan {
                    start_slot: 30,
                    len: 10
                })
            );
            assert_eq!(
                preparation.taken_span(),
                Some(FreeSpan {
                    start_slot: 30,
                    len: 4
                })
            );
            assert_eq!(
                preparation.retired_intervals(),
                &[FreeSpan {
                    start_slot: 20,
                    len: 10
                }]
            );
            assert_eq!(store.header(), before);
            assert_eq!(store.physical_cache_snapshot(), cache_before);
            assert_eq!(
                &records.borrow()[..records_before.len()],
                records_before.as_slice()
            );
            assert_eq!(
                &index.borrow()[..index_before.len()],
                index_before.as_slice()
            );
            let ready = preparation.ready().unwrap();
            if consume {
                ready.publish();
            } else {
                drop(ready);
            }
            drop(preparation);
            assert_eq!(
                store.publication_count() - publications_before,
                u64::from(consume)
            );
            assert_eq!(free_span_release_calls(), 0);
            if !consume {
                assert_eq!(store.physical_cache_snapshot(), cache_before);
                assert_eq!(store.header(), before);
                assert_eq!(
                    &records.borrow()[..records_before.len()],
                    records_before.as_slice()
                );
                assert_eq!(
                    &index.borrow()[..index_before.len()],
                    index_before.as_slice()
                );
            }
            assert_eq!(
                store.spans(),
                if consume {
                    vec![
                        FreeSpan {
                            start_slot: 20,
                            len: 10,
                        },
                        FreeSpan {
                            start_slot: 34,
                            len: 6,
                        },
                    ]
                } else {
                    vec![FreeSpan {
                        start_slot: 30,
                        len: 10,
                    }]
                }
            );
            assert_eq!(
                store.allocator_stats(),
                if consume {
                    FreeSpanAllocatorStats {
                        free_bytes: 16,
                        largest_free_span: 10,
                        free_span_count: 2,
                    }
                } else {
                    FreeSpanAllocatorStats {
                        free_bytes: 10,
                        largest_free_span: 10,
                        free_span_count: 1,
                    }
                }
            );
            store.validate().unwrap();
        }
        let store = FreeSpanStore::new(VectorMemory::default(), VectorMemory::default()).unwrap();
        for (start, len) in [(10, 15), (30, 5), (45, 15)] {
            store.release_span(start, len).unwrap();
        }
        let publications_before = store.publication_count();
        reset_free_span_release_calls();
        let mut preparation = store
            .prepare_physical_replacement(
                PhysicalSpanSelection::BestFit(32),
                &[FreeSpan {
                    start_slot: 20,
                    len: 30,
                }],
            )
            .unwrap()
            .unwrap();
        assert_eq!(preparation.selected_span(), None);
        assert_eq!(preparation.taken_span(), None);
        assert_eq!(
            preparation.retired_intervals(),
            &[
                FreeSpan {
                    start_slot: 25,
                    len: 5
                },
                FreeSpan {
                    start_slot: 35,
                    len: 10
                },
            ]
        );
        preparation.ready().unwrap().publish();
        drop(preparation);
        assert_eq!(store.publication_count() - publications_before, 1);
        assert_eq!(free_span_release_calls(), 0);
        assert_eq!(
            store.allocator_stats(),
            FreeSpanAllocatorStats {
                free_bytes: 50,
                largest_free_span: 50,
                free_span_count: 1
            }
        );
        assert_eq!(
            store.spans(),
            vec![FreeSpan {
                start_slot: 10,
                len: 50
            }]
        );
        assert_eq!(
            store
                .prepare_replacement(
                    0,
                    &[FreeSpan {
                        start_slot: 30,
                        len: 5
                    }]
                )
                .err(),
            Some(FreeSpanError::OverlapPrevious {
                previous: FreeSpan {
                    start_slot: 10,
                    len: 50
                },
                inserted: FreeSpan {
                    start_slot: 30,
                    len: 5
                },
            })
        );

        let store = FreeSpanStore::new(VectorMemory::default(), VectorMemory::default()).unwrap();
        store.release_span(40, 10).unwrap();
        store.release_span(90, 6).unwrap();
        assert_eq!(
            store.peek_best_fit(4),
            Some(FreeSpan {
                start_slot: 90,
                len: 6
            })
        );
        let before = store.header();
        let publications_before = store.publication_count();
        reset_free_span_release_calls();
        assert!(
            store
                .prepare_physical_replacement(PhysicalSpanSelection::ExactPrefixAt(41, 4), &[],)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.header(), before);
        let mut preparation = store
            .prepare_physical_replacement(PhysicalSpanSelection::ExactPrefixAt(40, 4), &[])
            .unwrap()
            .unwrap();
        assert_eq!(
            preparation.selected_span(),
            Some(FreeSpan {
                start_slot: 40,
                len: 10
            })
        );
        assert_eq!(
            preparation.taken_span(),
            Some(FreeSpan {
                start_slot: 40,
                len: 4
            })
        );
        assert!(preparation.retired_intervals().is_empty());
        preparation.ready().unwrap().publish();
        drop(preparation);
        assert_eq!(store.publication_count() - publications_before, 1);
        assert_eq!(free_span_release_calls(), 0);
        assert_eq!(
            store.allocator_stats(),
            FreeSpanAllocatorStats {
                free_bytes: 12,
                largest_free_span: 6,
                free_span_count: 2
            }
        );
        assert_eq!(
            store.spans(),
            vec![
                FreeSpan {
                    start_slot: 44,
                    len: 6
                },
                FreeSpan {
                    start_slot: 90,
                    len: 6
                }
            ]
        );
        store.validate().unwrap();

        let store = FreeSpanStore::new(VectorMemory::default(), VectorMemory::default()).unwrap();
        store.release_span(12, 2).unwrap();
        let publications_before = store.publication_count();
        assert_eq!(
            store
                .prepare_replacement(
                    0,
                    &[FreeSpan {
                        start_slot: 8,
                        len: 4
                    }; 2]
                )
                .err(),
            Some(FreeSpanError::DuplicateStart { start_slot: 8 })
        );
        assert_eq!(store.publication_count(), publications_before);
        assert_eq!(
            store.spans(),
            vec![FreeSpan {
                start_slot: 12,
                len: 2
            }]
        );
        let mut preparation = store
            .prepare_physical_replacement(
                PhysicalSpanSelection::NoAllocation,
                &[
                    FreeSpan {
                        start_slot: 8,
                        len: 10,
                    },
                    FreeSpan {
                        start_slot: 10,
                        len: 10,
                    },
                ],
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            preparation.retired_intervals(),
            &[
                FreeSpan {
                    start_slot: 8,
                    len: 4
                },
                FreeSpan {
                    start_slot: 14,
                    len: 6
                },
            ]
        );
        preparation.ready().unwrap().publish();
        drop(preparation);
        assert_eq!(
            store.spans(),
            vec![FreeSpan {
                start_slot: 8,
                len: 12
            }]
        );
        store.validate().unwrap();

        // Kept interior hole and right boundary neighbor are not image edits,
        // but both are source facts. Rejection preserves the deliberate change,
        // the other memory, and cache contents BEFORE any lookup can refill.
        for changed_start in [22u64, 40] {
            let records = VectorMemory::default();
            let index = VectorMemory::default();
            let store = FreeSpanStore::new(records.clone(), index.clone()).unwrap();
            for (start, len) in [(10, 4), (22, 2), (40, 4)] {
                store.release_span(start, len).unwrap();
            }
            let cache_before = store.physical_cache_snapshot();
            let header_before = store.header();
            let publications_before = store.publication_count();
            reset_free_span_release_calls();
            let mut preparation = store
                .prepare_physical_replacement(
                    PhysicalSpanSelection::NoAllocation,
                    &[FreeSpan {
                        start_slot: 20,
                        len: 10,
                    }],
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                preparation.retired_intervals(),
                &[
                    FreeSpan {
                        start_slot: 20,
                        len: 2
                    },
                    FreeSpan {
                        start_slot: 24,
                        len: 6
                    },
                ]
            );
            preparation.change_source_span_len_for_test(changed_start, 9);
            let deliberate_records = records.borrow().clone();
            let deliberate_index = index.borrow().clone();
            assert!(matches!(
                preparation.ready(),
                Err(FreeSpanError::BinInvariant)
            ));
            drop(preparation);
            assert_eq!(store.publication_count(), publications_before);
            assert_eq!(free_span_release_calls(), 0);
            assert_eq!(store.physical_cache_snapshot(), cache_before);
            assert_eq!(store.header(), header_before);
            assert_eq!(*records.borrow(), deliberate_records);
            assert_eq!(*index.borrow(), deliberate_index);
        }
    }
}

use crate::{
    SegmentId, VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
        slot_index::checked_add_slot_index,
    },
    lara::{
        edge::{
            BackedPhysicalEdgeImage, BackedSlabImage, EdgeHeaderV1, EdgeStore, PhysicalSlabPayload,
            counts::{PhysicalCountProjection, PreparedPhysicalCounts, SegmentEdgeCounts},
            free_span::{FreeSpan, FreeSpanPreparation, PhysicalSpanSelection, PreparedFreeSpans},
            span_meta::{PreparedSegmentPhysicalStart, SegmentSpanMeta},
        },
        operation_error::LaraOperationError,
        vertex::PreparedVertexWrite,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{
    BucketMode, BypassRowCompactOutcome, EdgePlacementPolicy, EdgeSlotMove, LabeledLaraGraph,
    VertexEdgeSpanCompactOneStep,
};

/// One swap-compaction move for an `Unordered` bucket (ADR 0052 §7): the last
/// live edge moves into the first interior tombstone. Edge-move coordinates are
/// bucket-local physical slots; `hole_ordinal` is the live ordinal of the hole
/// (the value span is dense and degree-sized, keyed by live ordinal — ADR §9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EdgeSlotSwapMove {
    /// Label row whose local slot changes.
    pub label_id: BucketLabelKey,
    /// Physical bucket-local slot of the first interior tombstone (destination).
    pub hole_slot: u32,
    /// Physical bucket-local slot of the last live edge (source; becomes a trailing tombstone).
    pub vacated_slot: u32,
    /// Live ordinal of the hole slot; the moved edge's value block lands here.
    pub hole_ordinal: u32,
}

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use std::cell::Cell;

thread_local! {
    static LABELED_LEAF_RELOCATE_IN_PROGRESS: Cell<bool> = const { Cell::new(false) };
    static LABELED_REBALANCE_RESOLVE_IN_PROGRESS: Cell<bool> = const { Cell::new(false) };
    static LABELED_REBALANCE_LEAF_RELOCATED: Cell<bool> = const { Cell::new(false) };
}

struct LabeledLeafRelocateGuard;

impl LabeledLeafRelocateGuard {
    fn new() -> Self {
        LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for LabeledLeafRelocateGuard {
    fn drop(&mut self) {
        LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.set(false));
    }
}

struct LabeledRebalanceResolveGuard;

impl LabeledRebalanceResolveGuard {
    fn new() -> Self {
        LABELED_REBALANCE_RESOLVE_IN_PROGRESS.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for LabeledRebalanceResolveGuard {
    fn drop(&mut self) {
        LABELED_REBALANCE_RESOLVE_IN_PROGRESS.with(|flag| flag.set(false));
    }
}

/// Physical tiling, not a desired logical row or an allocation request.
#[derive(Clone, Copy)]
struct VertexPhysicalPlacement {
    start: u64,
    slots: u32,
    preferred: Option<usize>,
    extra: u32,
}

impl VertexPhysicalPlacement {
    fn new(start: u64, slots: u32, preferred: Option<usize>, extra: u32) -> Self {
        Self {
            start,
            slots,
            preferred,
            extra,
        }
    }
}

enum LeafPhysicalDestination {
    Current(FreeSpan),
    Relocate(u64),
}

enum PhysicalLayoutOperation {
    VertexRewrite {
        src: VertexId,
        placement: VertexPhysicalPlacement,
    },
    LeafSlide {
        src: VertexId,
        destination: LeafPhysicalDestination,
    },
}

pub(super) struct PhysicalLayoutSource<'a, E: CsrEdge, M: Memory> {
    owner: &'a LabeledLaraGraph<E, M>,
    header: EdgeHeaderV1,
    bucket_header: EdgeHeaderV1,
    vertex_len: u32,
    count_index: u64,
    count_len: u64,
    counts: SegmentEdgeCounts,
    leaf: u32,
    pin_len: u64,
    pin: SegmentSpanMeta,
}

impl<E: CsrEdge, M: Memory> Copy for PhysicalLayoutSource<'_, E, M> {}
impl<E: CsrEdge, M: Memory> Clone for PhysicalLayoutSource<'_, E, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, E: CsrEdge, M: Memory> PhysicalLayoutSource<'a, E, M> {
    pub(super) fn capture(
        owner: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
    ) -> Result<Self, LabeledOperationError> {
        if u32::from(src) >= owner.vertices.len() {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let header = owner.edges.header();
        let leaf = LabeledLaraGraph::<E, M>::leaf_index_for_vid(src, header.segment_size.max(1));
        let count_index = owner.edges.vertex_segment_counts_index(src)?;
        if u64::from(leaf) >= owner.edges.span_meta_store().len() {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let source = Self {
            owner,
            header,
            bucket_header: owner.buckets.header(),
            vertex_len: owner.vertices.len(),
            count_index,
            count_len: owner.edges.counts_store().len(),
            counts: owner.edges.counts_store().checked_get(count_index)?,
            leaf,
            pin_len: owner.edges.span_meta_store().len(),
            pin: owner.edges.span_meta_store().get(u64::from(leaf)),
        };
        source.pinned_range()?;
        Ok(source)
    }

    pub(super) fn bind_allocated_header(
        self,
        header: EdgeHeaderV1,
    ) -> Result<Self, LabeledOperationError> {
        let mut configuration = header;
        configuration.elem_capacity = self.header.elem_capacity;
        if header.elem_capacity < self.header.elem_capacity || configuration != self.header {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let source = Self { header, ..self };
        source.validate()?;
        Ok(source)
    }

    pub(super) fn validate(&self) -> Result<(), LabeledOperationError> {
        if !self.owner.edges.physical_header_matches(self.header)
            || self.owner.buckets.header() != self.bucket_header
            || self.owner.vertices.len() != self.vertex_len
            || !self.owner.edges.counts_store().physical_source_matches(
                self.count_index,
                self.count_len,
                self.counts,
            )
            || !self.owner.edges.span_meta_store().physical_source_matches(
                u64::from(self.leaf),
                self.pin_len,
                self.pin,
            )
        {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        Ok(())
    }

    fn pinned_range(&self) -> Result<Option<FreeSpan>, LabeledOperationError> {
        EdgeStore::<E, M>::assigned_range_from_source(
            self.pin,
            self.counts,
            self.header.elem_capacity,
        )
        .map_err(LabeledOperationError::from)
    }
}

pub(super) struct CanonicalPhysicalVertex {
    src: VertexId,
    pub(super) vertex: LabeledVertex,
    buckets: Vec<LabelBucket>,
}

impl CanonicalPhysicalVertex {
    fn copy_source(&self) -> Result<Self, LabeledOperationError> {
        let mut buckets = Vec::new();
        buckets
            .try_reserve_exact(self.buckets.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        buckets.extend_from_slice(&self.buckets);
        Ok(Self {
            src: self.src,
            vertex: self.vertex,
            buckets,
        })
    }

    pub(super) fn capture<E: CsrEdge, M: Memory>(
        owner: &LabeledLaraGraph<E, M>,
        src: VertexId,
    ) -> Result<Self, LabeledOperationError> {
        let vertex = owner.vertices.get(src);
        Ok(Self {
            src,
            vertex,
            buckets: owner.leaf_span_buckets(&vertex)?,
        })
    }

    pub(super) fn validate<E: CsrEdge, M: Memory>(
        &self,
        owner: &LabeledLaraGraph<E, M>,
    ) -> Result<(), LabeledOperationError> {
        if owner.vertices.get(self.src) != self.vertex
            || owner.leaf_span_buckets(&self.vertex)? != self.buckets
        {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        Ok(())
    }
}

enum PhysicalCopyInput {
    Buffered(Vec<Option<Vec<u8>>>),
    Disjoint,
}

/// A desired shape can reserve and revalidate, never manufacture a ready publisher.
/// Its opaque store writers are consumed only inside the canonical factory below.
struct PreparedVertexPhysicalCopy<'a, E: CsrEdge, M: Memory> {
    source: PhysicalLayoutSource<'a, E, M>,
    canonical: CanonicalPhysicalVertex,
    final_buckets: Vec<LabelBucket>,
    final_vertex: LabeledVertex,
    payload: BackedSlabImage<'a, E, M>,
    writers: PhysicalVertexWriters<'a, M>,
}

impl<'a, E: CsrEdge, M: Memory> PreparedVertexPhysicalCopy<'a, E, M> {
    fn reserve_desired(
        owner: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        shape: (Vec<LabelBucket>, Vec<Option<Vec<u8>>>),
        placement: VertexPhysicalPlacement,
    ) -> Result<Self, LabeledOperationError> {
        let source = PhysicalLayoutSource::capture(owner, src)?;
        let canonical = CanonicalPhysicalVertex::capture(owner, src)?;
        let header = source.header;
        let component = Self::reserve(
            source,
            canonical,
            shape.0,
            (placement, header),
            PhysicalCopyInput::Buffered(shape.1),
        )?;
        component.validate_source()?;
        Ok(component)
    }

    fn reserve(
        source: PhysicalLayoutSource<'a, E, M>,
        canonical: CanonicalPhysicalVertex,
        desired: Vec<LabelBucket>,
        destination: (VertexPhysicalPlacement, EdgeHeaderV1),
        input: PhysicalCopyInput,
    ) -> Result<Self, LabeledOperationError> {
        let owner = source.owner;
        let (placement, header) = destination;
        if desired.len() != canonical.buckets.len()
            || desired
                .windows(2)
                .any(|pair| pair[0].bucket_label_key() >= pair[1].bucket_label_key())
            || desired
                .iter()
                .zip(&canonical.buckets)
                .any(|(new, old)| new.bucket_label_key() != old.bucket_label_key())
        {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let end = crate::slab_index::checked_add_slot_exclusive_end(
            placement.start,
            u64::from(placement.slots),
        )
        .filter(|end| *end <= header.elem_capacity)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let positions = if canonical.vertex.is_default_edge_labeled() {
            let mut positions = Vec::new();
            positions
                .try_reserve_exact(1)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            positions.push(placement.start);
            positions
        } else {
            LabeledLaraGraph::<E, M>::calculate_label_edge_span_positions_by_resident_slots(
                placement.start,
                placement.slots,
                &desired,
                placement.preferred,
                placement.extra,
            )?
        };
        let transcribed = match input {
            PhysicalCopyInput::Buffered(bytes) => {
                if bytes.len() != desired.len() {
                    return Err(LaraOperationError::CollectAllocationOverflow.into());
                }
                Some(bytes)
            }
            PhysicalCopyInput::Disjoint => None,
        };
        let mut final_buckets = Vec::new();
        final_buckets
            .try_reserve_exact(desired.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let mut payloads = Vec::new();
        payloads
            .try_reserve_exact(desired.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let mut supplied = transcribed.map(Vec::into_iter);
        for ((old, new), position) in canonical.buckets.iter().zip(desired).zip(positions) {
            let region = bucket_content_region(&new);
            let bytes = supplied.as_mut().and_then(Iterator::next).flatten();
            if let Some((_, slots)) = region {
                if crate::slab_index::checked_add_slot_exclusive_end(position, u64::from(slots))
                    .is_none_or(|bound| bound > end)
                    || position < placement.start
                {
                    return Err(LaraOperationError::CollectAllocationOverflow.into());
                }
                let old_region = bucket_content_region(old);
                if let Some(bytes) = bytes {
                    if bytes.len() != LabeledLaraGraph::<E, M>::edge_bytes_for_len(slots as usize)?
                    {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    payloads.push(PhysicalSlabPayload::Buffered {
                        source: None,
                        destination: position,
                        bytes,
                    });
                } else {
                    let (start, old_slots) =
                        old_region.ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if old_slots != slots {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    if supplied.is_some() {
                        let len = LabeledLaraGraph::<E, M>::edge_bytes_for_len(slots as usize)?;
                        let mut bytes = Vec::new();
                        bytes
                            .try_reserve_exact(len)
                            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                        bytes.resize(len, 0);
                        owner.edges.read_slots_contiguous(start, &mut bytes);
                        payloads.push(PhysicalSlabPayload::Buffered {
                            source: Some(start),
                            destination: position,
                            bytes,
                        });
                    } else {
                        payloads.push(PhysicalSlabPayload::Disjoint {
                            source: start,
                            destination: position,
                            slots: u64::from(slots),
                        });
                    }
                }
            } else if bytes.is_some() {
                return Err(LaraOperationError::CollectAllocationOverflow.into());
            }
            final_buckets.push(new.with_edge_range(position, new.stored_slots_raw()));
        }
        // The final first resident, not the source anchor, owns the whole cover.
        if final_buckets
            .iter()
            .find(|bucket| !bucket.is_tiny_mode())
            .is_some_and(|bucket| bucket.edge_start() != placement.start)
        {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let final_vertex = if canonical.vertex.is_default_edge_labeled() {
            canonical.vertex.with_base_slot_start(placement.start)
        } else {
            canonical.vertex.with_stored_slots(placement.slots)
        };
        let descriptors = if canonical.vertex.is_default_edge_labeled() {
            None
        } else {
            #[cfg(test)]
            physical_layout_event(PhysicalLayoutEvent::AdmitDescriptor(canonical.src));
            Some(
                owner
                    .buckets
                    .prepare_existing_row_image(canonical.vertex, &final_buckets)?,
            )
        };
        let vertex = owner.vertices.prepare_set(canonical.src, &final_vertex)?;
        let payload = owner.edges.prepare_physical_copy_image(header, payloads)?;
        let writers = PhysicalVertexWriters {
            vertex,
            descriptors,
            #[cfg(test)]
            id: canonical.src,
        };
        Ok(Self {
            source,
            canonical,
            final_buckets,
            final_vertex,
            payload,
            writers,
        })
    }

    fn validate_source(&self) -> Result<(), LabeledOperationError> {
        self.source.validate()?;
        self.canonical.validate(self.source.owner)?;
        if !self.payload.validate_source() {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        Ok(())
    }

    #[cfg(test)]
    fn retained_source_bytes(&self) -> usize {
        self.payload.retained_source_bytes()
    }
    #[cfg(test)]
    fn transcribed_bytes(&self) -> usize {
        self.payload.transcribed_bytes()
    }
}

struct PhysicalVertexWriters<'a, M: Memory> {
    vertex: PreparedVertexWrite<'a, LabeledVertex>,
    descriptors: Option<crate::labeled::bucket_store::PreparedExistingBucketRow<'a, M>>,
    #[cfg(test)]
    id: VertexId,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum PhysicalLayoutEvent {
    Prepare(VertexId),
    Buffered,
    Streamed,
    Ready,
    Publish,
    Header,
    AdmitDescriptor(VertexId),
    Descriptor(VertexId),
    Vertex(VertexId),
    Pin,
    Count,
    Retire,
}

#[cfg(test)]
thread_local! {
    static PHYSICAL_LAYOUT_EVENTS: std::cell::RefCell<Vec<PhysicalLayoutEvent>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn physical_layout_event(event: PhysicalLayoutEvent) {
    PHYSICAL_LAYOUT_EVENTS.with(|events| events.borrow_mut().push(event));
}

struct PhysicalLayoutPreparation<'a, E: CsrEdge, M: Memory> {
    source: PhysicalLayoutSource<'a, E, M>,
    canonical: Vec<CanonicalPhysicalVertex>,
    rows: Vec<PhysicalVertexWriters<'a, M>>,
    slab: BackedPhysicalEdgeImage<'a, E, M>,
    counts: Option<PreparedPhysicalCounts<'a, E, M>>,
    pin: Option<PreparedSegmentPhysicalStart<'a, M>>,
    allocator: FreeSpanPreparation<'a, M>,
    #[cfg(test)]
    retired_leaf: Option<FreeSpan>,
}

struct ReadyPhysicalLayout<'a, E: CsrEdge, M: Memory> {
    rows: &'a [PhysicalVertexWriters<'a, M>],
    slab: &'a BackedPhysicalEdgeImage<'a, E, M>,
    counts: Option<&'a PreparedPhysicalCounts<'a, E, M>>,
    pin: Option<&'a PreparedSegmentPhysicalStart<'a, M>>,
    allocator: PreparedFreeSpans<'a, M>,
    #[cfg(test)]
    retired_leaf: Option<FreeSpan>,
}

impl<E: CsrEdge, M: Memory> PhysicalLayoutPreparation<'_, E, M> {
    fn ready(&mut self) -> Result<ReadyPhysicalLayout<'_, E, M>, LabeledOperationError> {
        self.source.validate()?;
        for canonical in &self.canonical {
            canonical.validate(self.source.owner)?;
        }
        if !self.slab.validate_source()
            || self
                .counts
                .as_ref()
                .is_some_and(|image| !image.validate_source())
            || self
                .pin
                .as_ref()
                .is_some_and(|image| !image.validate_source())
        {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let allocator = self.allocator.ready().map_err(|_| {
            LaraOperationError::RebalanceFailed(crate::GrowFailed {
                current_size: 0,
                delta: 0,
            })
        })?;
        #[cfg(test)]
        physical_layout_event(PhysicalLayoutEvent::Ready);
        Ok(ReadyPhysicalLayout {
            rows: &self.rows,
            slab: &self.slab,
            counts: self.counts.as_ref(),
            pin: self.pin.as_ref(),
            allocator,
            #[cfg(test)]
            retired_leaf: self.retired_leaf,
        })
    }
}

impl<E: CsrEdge, M: Memory> ReadyPhysicalLayout<'_, E, M> {
    fn publish(self) {
        #[cfg(test)]
        physical_layout_event(PhysicalLayoutEvent::Publish);
        self.slab.write();
        #[cfg(test)]
        physical_layout_event(PhysicalLayoutEvent::Header);
        for row in self.rows {
            if let Some(descriptors) = &row.descriptors {
                descriptors.write();
                #[cfg(test)]
                physical_layout_event(PhysicalLayoutEvent::Descriptor(row.id));
            }
            row.vertex.write();
            #[cfg(test)]
            physical_layout_event(PhysicalLayoutEvent::Vertex(row.id));
        }
        if let Some(pin) = self.pin {
            pin.write();
            #[cfg(test)]
            physical_layout_event(PhysicalLayoutEvent::Pin);
        }
        if let Some(counts) = self.counts
            && counts.write()
        {
            #[cfg(test)]
            physical_layout_event(PhysicalLayoutEvent::Count);
        }
        self.allocator.publish();
        #[cfg(test)]
        {
            physical_layout_event(PhysicalLayoutEvent::Retire);
            if self.retired_leaf.is_some() {
                record_labeled_leaf_physical_release();
            }
        }
    }
}

/// Storage-owned target geometry for relocating one labeled PMA leaf.
///
/// Batch reservation may use this plan to reserve a destination without
/// duplicating the leaf's resident/log/slack projection. The plan contains no
/// allocator mutation; allocation and publication remain owned by the commit
/// path that consumes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LabeledLeafRelocationTarget {
    pub(crate) leaf: u32,
    pub(crate) old_start: u64,
    pub(crate) old_len: u64,
    pub(crate) new_len: u64,
}

// Test-only leaf/footprint release metrics. These are thread-local rather than process-global so
// the `reset` + delta assertions in parallel tests cannot contaminate one another: every release is
// recorded synchronously on the same thread that runs the graph operation under test, so a release
// triggered by a concurrent test on another thread is never observed here.
#[cfg(test)]
thread_local! {
    static LABELED_LEAF_PHYSICAL_RELEASE_CALLS: Cell<u32> = const { Cell::new(0) };
    static LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS: Cell<u32> = const { Cell::new(0) };
    static REWRITE_VERTEX_EDGE_SPAN_CALLS: Cell<u32> = const { Cell::new(0) };
    // Fault-injection latch: when set, the next `compact_vertex_edge_span_one_step`
    // returns an error before mutating any state, so maintenance-loop tests can assert
    // a failed step is requeued for retry rather than silently marked complete.
    static FORCE_COMPACT_VERTEX_EDGE_SPAN_STEP_ERROR: Cell<bool> = const { Cell::new(false) };
}

/// Arms a one-shot error from the next `compact_vertex_edge_span_one_step` call on
/// this thread. Consumed by the first call. Test-only.
#[cfg(test)]
pub(crate) fn force_next_compact_vertex_edge_span_step_error() {
    FORCE_COMPACT_VERTEX_EDGE_SPAN_STEP_ERROR.with(|c| c.set(true));
}

#[cfg(test)]
fn take_forced_compact_vertex_edge_span_step_error() -> bool {
    FORCE_COMPACT_VERTEX_EDGE_SPAN_STEP_ERROR.with(|c| c.replace(false))
}

#[cfg(test)]
pub(crate) fn labeled_leaf_physical_release_calls() -> u32 {
    LABELED_LEAF_PHYSICAL_RELEASE_CALLS.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn labeled_vertex_footprint_release_calls() -> u32 {
    LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn rewrite_vertex_edge_span_calls() -> u32 {
    REWRITE_VERTEX_EDGE_SPAN_CALLS.with(|c| c.get())
}

#[cfg(test)]
fn record_labeled_leaf_physical_release() {
    LABELED_LEAF_PHYSICAL_RELEASE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
fn record_labeled_vertex_footprint_release() {
    LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
fn record_rewrite_vertex_edge_span() {
    REWRITE_VERTEX_EDGE_SPAN_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn reset_labeled_leaf_release_test_metrics() {
    LABELED_LEAF_PHYSICAL_RELEASE_CALLS.with(|c| c.set(0));
    LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn reset_rewrite_vertex_edge_span_test_metrics() {
    REWRITE_VERTEX_EDGE_SPAN_CALLS.with(|c| c.set(0));
}

#[cfg(target_family = "wasm")]
fn log_collect_overflow(message: &str) {
    ic_cdk::println!("LARA CollectAllocationOverflow: {}", message);
}

#[cfg(not(target_family = "wasm"))]
fn log_collect_overflow(_message: &str) {}

/// Returns the next per-vertex edge-span allocation after a fold or rewrite.
///
/// The initial leaf quota is deliberately not used as a growth factor. Growth
/// keeps a segment-sized minimum quantum and otherwise adds configured
/// geometric headroom to the existing allocation.
fn next_vertex_edge_span_allocation(
    old_alloc: u32,
    min_required: u32,
    segment_size: u32,
) -> Result<u32, LaraOperationError> {
    let segment_size = segment_size.max(1);
    if old_alloc == 0 {
        return Ok(min_required.max(segment_size));
    }
    let geometric_headroom = old_alloc
        .checked_mul(5)
        .and_then(|value| value.checked_add(3))
        .map(|value| value / 4)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    let growth = segment_size.max(geometric_headroom);
    old_alloc
        .checked_add(growth)
        .map(|grown| grown.max(min_required).max(segment_size))
        .ok_or(LaraOperationError::CollectAllocationOverflow)
}

#[allow(clippy::needless_return)]
/// Plan 0319 §Step 3: returns the LEG span region length for a label
/// bucket. For slab-mode buckets, this is `stored_slots` (the
/// contiguous edge-slab width). For tree-mode buckets, this is the
/// **physical root region length** (a `u32` block_id array of
/// length `ceil(stored_slots / B^depth)`), NOT `stored_slots`.
///
/// **Why this matters**: the vertex edge-span rewrite path
/// (`vertex_edge_span_retire_intervals`, `compact_vertex_edge_span`,
/// `rewrite_vertex_edge_span`) reads each bucket's region as
/// `[edge_start, edge_start + region_len)`. For a slab bucket,
/// `region_len = stored_slots` correctly describes the contiguous
/// edge slots. For a tree bucket, `region_len = stored_slots` is
/// WRONG — the actual LEG region is only the root region
/// (`physical_root_len` slots), and `stored_slots` is the count of
/// live + tombstoned edge entries (a logical number, not a
/// physical LEG span). Using `stored_slots` for a tree bucket would
/// cause the rewrite to read past the root region into neighboring
/// buckets and misread block ids as edge targets (corruption).
/// One tree bucket's physical spans in the edge slab: the single owner of "where does the
/// property root live".
///
/// The reader used to derive `edge_start + bucket_span_region_len` while writers recorded
/// `inline_property_bytes_offset`; the two agreed only by allocator luck (see the round-trip test
/// comment in `tree_read.rs`). Every tree reader and writer now takes the address from here, and
/// `validate` states the invariant the two conventions must satisfy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TreeSpans {
    edge_start: u64,
    edge_root_len: u32,
    property_root_len: u32,
}

impl TreeSpans {
    /// `None` for non-tree buckets: they have no root regions.
    pub(crate) fn of(bucket: &LabelBucket) -> Option<Self> {
        if !bucket.is_tree_mode() {
            return None;
        }
        Some(Self {
            edge_start: bucket.edge_start(),
            edge_root_len: bucket_span_region_len(bucket),
            property_root_len: property_root_region_len(bucket),
        })
    }

    pub(crate) fn edge_root(&self) -> (u64, u32) {
        (self.edge_start, self.edge_root_len)
    }

    /// `(start, len)` of the property root. This is the only place the address is computed.
    pub(crate) fn property_root(&self) -> (u64, u32) {
        (
            self.edge_start
                .saturating_add(u64::from(self.edge_root_len)),
            self.property_root_len,
        )
    }

    /// Address of property-root entry `index` as `(slot, byte offset past that slot)`.
    ///
    /// The property root starts slot-aligned (it follows the edge root), but its block
    /// ids are packed at [`PROPERTY_ID_BYTES`](crate::labeled::tree_csr::PROPERTY_ID_BYTES)
    /// inside it, so entries are addressed in bytes rather than by slot index.
    pub(crate) fn property_root_entry(&self, index: u32) -> (u64, u64) {
        (
            self.edge_start
                .saturating_add(u64::from(self.edge_root_len)),
            u64::from(index) * 4,
        )
    }

    /// `(start, len)` covering both roots (gap 0).
    pub(crate) fn combined(&self) -> (u64, u32) {
        (
            self.edge_start,
            self.edge_root_len.saturating_add(self.property_root_len),
        )
    }

    /// The descriptor's recorded offset must agree with the derived address. A non-zero offset is
    /// accepted only when it equals the derived one; anything else is the split-brain this type
    /// exists to prevent.
    pub(crate) fn validate(&self, bucket: &LabelBucket) -> Result<(), &'static str> {
        let recorded = bucket.inline_property_bytes_offset();
        let (derived, _) = self.property_root();
        if recorded != 0 && recorded != derived {
            return Err("property root offset disagrees with the derived address");
        }
        let (_, combined_len) = self.combined();
        // Edge root and property root are each bounded by R_MAX (1024), so the combined region
        // can never exceed 2048 slots.
        if combined_len > 2048 {
            return Err("combined root length out of range");
        }
        Ok(())
    }
}

pub(super) fn bucket_span_region_len(bucket: &LabelBucket) -> u32 {
    // ADR 0096 §5 + §7: match-first — tiny buckets hold zero slab slots
    // (inline targets only); tree/slab keep their region rules.
    match BucketMode::from_bucket(bucket) {
        BucketMode::Tiny => 0,
        BucketMode::Slab => bucket.stored_slots(),
        BucketMode::Tree => {
            // Tree mode: edge root region length + property root region length
            // (Plan 0326 LPB-in-tree: the vertex span in tree mode is
            // `[edge root | property root]`, gap 0 — but the LEG allocator
            // does not guarantee contiguity, so the property root may live
            // at an arbitrary LEG offset stored in
            // `inline_property_bytes_offset`. The edge root is contiguous
            // at `edge_start`; the property root is at a separate offset,
            // and `vertex_label_edge_span_end_exclusive` (and friends) sum
            // the two regions for the compaction rewrite path).
            let physical_depth = bucket.tree_mode_physical_depth();
            let stored = bucket.stored_slots();
            let leaf_count =
                u32::try_from((u64::from(stored)).div_ceil(crate::labeled::tree_csr::B as u64))
                    .expect("leaf_count fits u32 for MAX_DEPTH=3");
            let k = u32::try_from(crate::labeled::tree_csr::R_MAX).expect("R_MAX fits u32");
            let edge_root_len = match physical_depth {
                1 => leaf_count,
                2 => leaf_count.div_ceil(k),
                3 => leaf_count.div_ceil(k).div_ceil(k),
                _ => unreachable!("tree_mode_physical_depth out of range"),
            };
            // Plan 0326: property root length (separate LEG span; the
            // compact span logic adds the property root via
            // `property_root_region_len` below, so `bucket_span_region_len`
            // returns the EDGE root only — the property root is appended
            // explicitly by the compaction rewrite path).
            edge_root_len
        }
    }
}

/// Plan 0326 LPB-in-tree: property root region length for a
/// tree-mode bucket. Returns 0 for `w == 0` buckets (no property
/// stream). The property root is contiguous at
/// `edge_start + bucket_span_region_len(bucket)` (per ADR 0088
/// §2: `[edge root | property root]`, gap 0); its length is
/// `ceil(L_p / B^(d'-1))` where `L_p = ceil(S / K)`,
/// `K = floor(4096 / w)`, `S = stored_slots`, and `d' =
/// bucket.tree_mode_property_depth()` is the property-tree physical
/// depth (Plan 0327). At `d' == 1` this reduces to `ceil(S / K)`.
/// No tail buffer (the ADR §2 wire truth is the exact coverage).
pub(super) fn property_root_entry_count(bucket: &LabelBucket) -> u32 {
    if !bucket.is_tree_mode() {
        return 0;
    }
    let w = bucket.inline_property_byte_width();
    if w == 0 {
        return 0;
    }
    let k = match crate::labeled::tree_csr::property_slots_per_block(w) {
        Some(k) if k >= 1 => k,
        _ => return 0,
    };
    let l_p = u64::from(bucket.stored_slots().div_ceil(k));
    let b = crate::labeled::tree_csr::B as u64;
    let mut r = l_p;
    for _ in 1..bucket.tree_mode_property_depth() {
        r = r.div_ceil(b);
    }
    u32::try_from(r).expect("property root entry count fits u32 (bounded by R_MAX)")
}

/// Edge slots the property root reserves inside the combined region.
///
/// The root holds [`property_root_entry_count`] u32 ids packed at
/// [`PROPERTY_ID_BYTES`](crate::labeled::tree_csr::PROPERTY_ID_BYTES), so the entries
/// occupy `entries * 4` bytes at the start of the region. The region itself still
/// reserves one edge slot per entry: `entries * 4 <= entries * E::BYTES` for every
/// edge width, so the packed ids always fit, and at the 4-byte tree-mode edge the two
/// lengths coincide exactly.
pub(super) fn property_root_region_len(bucket: &LabelBucket) -> u32 {
    property_root_entry_count(bucket)
}

#[allow(clippy::needless_return)]
/// Plan 0326 LPB-in-tree: combined LEG span length for a tree-mode
/// bucket. The span holds `[edge root | property root]` (gap 0);
/// the read path derives the property root start as
/// `edge_start + edge_root_len`. The compaction rewrite path uses
/// this combined length to compute per-vertex span intervals so
/// the rewrite does not reallocate the property root.
pub(super) fn combined_span_region_len(bucket: &LabelBucket) -> u32 {
    // ADR 0096 §5 + §7: match-first — tiny buckets hold zero slab slots.
    match BucketMode::from_bucket(bucket) {
        BucketMode::Tiny => 0,
        BucketMode::Slab => bucket.stored_slots(),
        BucketMode::Tree => {
            let edge = bucket_span_region_len(bucket);
            let prop = property_root_region_len(bucket);
            edge.checked_add(prop).expect(
                "combined_span_region_len: edge_root_len + property_root_len overflows u32 (impossible: \
                 both bounded by R_MAX = 1024, so total <= 2048)",
            )
        }
    }
}

/// Physical LEG slots one bucket occupies inside a leaf block.
///
/// Single source of truth for every resident-geometry computation
/// (relocation sizing, slide tiling, rebalance planning, fold planning,
/// snapshot reads, release intervals): tiny buckets hold zero slab slots
/// (inline targets only), slab buckets hold `stored_slots`, tree buckets hold
/// only their root region (`combined_span_region_len` — the logical
/// `stored_slots` edge count is NOT a physical width). Log-chain slots are
/// added by the caller when the bucket carries an overflow log.
/// GAP-2026-09-17-001: sizing any of these paths on the logical width
/// over-allocates the leaf, misreads live ranges on relocate, and feeds
/// unowned ranges to the free store on release.
pub(crate) fn bucket_content_slots(bucket: &LabelBucket) -> u32 {
    bucket_content_region(bucket).map_or(0, |(_, len)| len)
}

/// The edge-slab region a bucket physically owns, as `(start_slot, len)`, or
/// `None` when it owns none (tiny). The mode-blind geometry layer reads sizes
/// only through this SSOT: slab is its `stored_slots` prefix, tree its LEG root
/// array (its `stored_slots` is a logical count), tiny nothing.
#[inline]
pub(crate) fn bucket_content_region(bucket: &LabelBucket) -> Option<(u64, u32)> {
    if bucket.is_tiny_mode() {
        return None;
    }
    let len = if bucket.is_tree_mode() {
        combined_span_region_len(bucket)
    } else {
        bucket.stored_slots_raw()
    };
    (len > 0).then_some((bucket.edge_start(), len))
}

/// Which outcome a caller accepts when the requested span cannot be placed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanResolutionPolicy {
    /// The span must exist: fail when nothing can back it — the insert path, which has
    /// to grow for real.
    Required,
    /// The span was requested for spare room: keep the span the vertex already holds
    /// when it cannot be placed — the maintenance path, which must not fail a healthy
    /// graph for slack.
    SlackMayBeDropped,
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The sole ready constructor takes an operation, never a desired logical copy.
    fn prepare_physical_layout(
        &self,
        operation: PhysicalLayoutOperation,
    ) -> Result<PhysicalLayoutPreparation<'_, E, M>, LabeledOperationError> {
        let src = match &operation {
            PhysicalLayoutOperation::VertexRewrite { src, .. }
            | PhysicalLayoutOperation::LeafSlide { src, .. } => *src,
        };
        #[cfg(test)]
        physical_layout_event(PhysicalLayoutEvent::Prepare(src));
        let (source, canonical) = self.capture_physical_layout_source(src)?;
        self.prepare_physical_layout_from(src, source, canonical, operation)
    }

    /// Capture one leaf's mutable sources once: the owner snapshot and every canonical row.
    /// A caller that already captured (relocation sizing) hands the same rows to the preparation.
    fn capture_physical_layout_source(
        &self,
        src: VertexId,
    ) -> Result<(PhysicalLayoutSource<'_, E, M>, Vec<CanonicalPhysicalVertex>), LabeledOperationError>
    {
        let source = PhysicalLayoutSource::capture(self, src)?;
        let seg = source.header.segment_size.max(1);
        let first = source
            .leaf
            .checked_mul(seg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let last = first
            .checked_add(seg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?
            .min(source.vertex_len);
        let mut canonical = Vec::new();
        canonical
            .try_reserve_exact((last - first) as usize)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        for ordinal in first..last {
            canonical.push(CanonicalPhysicalVertex::capture(
                self,
                VertexId::from(ordinal),
            )?);
        }
        Ok((source, canonical))
    }

    fn prepare_physical_layout_from<'a>(
        &'a self,
        src: VertexId,
        source: PhysicalLayoutSource<'a, E, M>,
        canonical: Vec<CanonicalPhysicalVertex>,
        operation: PhysicalLayoutOperation,
    ) -> Result<PhysicalLayoutPreparation<'a, E, M>, LabeledOperationError> {
        let pinned = source.pinned_range()?;
        let mut header = source.header;
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(canonical.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let (allocator, block, count_projection) = match &operation {
            PhysicalLayoutOperation::VertexRewrite { placement, .. } => {
                let row = canonical
                    .iter()
                    .find(|row| row.src == src)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if row.vertex.stored_slots > 0
                    && let Some(bucket) = row.buckets.iter().find(|bucket| !bucket.is_tiny_mode())
                {
                    let old_base = bucket.edge_start();
                    if placement.start != old_base
                        && !self.labeled_edge_footprint_in_current_leaf_pin(
                            src,
                            old_base,
                            row.vertex.stored_slots,
                        )?
                    {
                        let live_end = crate::slab_index::checked_add_slot_exclusive_end(
                            placement.start,
                            u64::from(placement.slots),
                        )
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        for (start_slot, len) in Self::vertex_edge_span_retire_intervals(
                            old_base,
                            row.vertex.stored_slots,
                            &row.buckets,
                        )? {
                            let old_end = checked_add_slot_index(start_slot, len)
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            // The retained destination owns its entire cover, not just
                            // the copied prefix. Free holes remain the allocator's rule.
                            for (start, end) in [
                                (start_slot, old_end.min(placement.start)),
                                (start_slot.max(live_end), old_end),
                            ] {
                                if start < end {
                                    if matches!(
                                        super::leaf_arena::LeafArena::capture(self, src),
                                        Ok(Some(pin)) if pin.extent_contains(start, end - start)
                                    ) {
                                        continue;
                                    }
                                    retired.try_reserve(1).map_err(|_| {
                                        LaraOperationError::CollectAllocationOverflow
                                    })?;
                                    retired.push(FreeSpan {
                                        start_slot: start,
                                        len: end - start,
                                    });
                                }
                            }
                        }
                    }
                }
                let allocator = self
                    .edges
                    .prepare_physical_span_edit(PhysicalSpanSelection::NoAllocation, &retired)?
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let projection = Some(PhysicalCountProjection::VertexSpan {
                    old: row.vertex.stored_slots,
                    new: placement.slots,
                    actual: 0,
                });
                (allocator, pinned, projection)
            }
            PhysicalLayoutOperation::LeafSlide { destination, .. } => match destination {
                LeafPhysicalDestination::Current(range) => {
                    if Some(*range) != pinned {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    let allocator = self
                        .edges
                        .prepare_physical_span_edit(PhysicalSpanSelection::NoAllocation, &[])?
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    (
                        allocator,
                        Some(*range),
                        Some(PhysicalCountProjection::LeafBlock(range.len)),
                    )
                }
                LeafPhysicalDestination::Relocate(len) => {
                    let adjacent = if let Some(old) = pinned.filter(|old| *len > old.len) {
                        let start = crate::slab_index::checked_add_slot_exclusive_end(
                            old.start_slot,
                            old.len,
                        )
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges.prepare_physical_span_edit(
                            PhysicalSpanSelection::ExactPrefixAt(start, *len - old.len),
                            &[],
                        )?
                    } else {
                        None
                    };
                    if let Some(allocator) = adjacent {
                        let range = FreeSpan {
                            start_slot: pinned.expect("adjacent source pin").start_slot,
                            len: *len,
                        };
                        let end = crate::slab_index::checked_add_slot_exclusive_end(
                            range.start_slot,
                            range.len,
                        )
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        header.elem_capacity = header.elem_capacity.max(end);
                        (
                            allocator,
                            Some(range),
                            Some(PhysicalCountProjection::LeafBlock(*len)),
                        )
                    } else {
                        if let Some(old) = pinned {
                            retired.push(old);
                        } else {
                            for row in &canonical {
                                if let Some(bucket) =
                                    row.buckets.iter().find(|bucket| !bucket.is_tiny_mode())
                                {
                                    for (start_slot, len) in
                                        Self::vertex_edge_span_retire_intervals(
                                            bucket.edge_start(),
                                            row.vertex.stored_slots,
                                            &row.buckets,
                                        )?
                                    {
                                        retired.try_reserve(1).map_err(|_| {
                                            LaraOperationError::CollectAllocationOverflow
                                        })?;
                                        retired.push(FreeSpan { start_slot, len });
                                    }
                                }
                            }
                        }
                        let allocator = self
                            .edges
                            .prepare_physical_span_edit(
                                PhysicalSpanSelection::BestFit(*len),
                                &retired,
                            )?
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        if allocator.selected_span().is_some_and(|whole| {
                            whole
                                .start_slot
                                .checked_add(whole.len)
                                .is_none_or(|end| end > header.elem_capacity)
                        }) {
                            return Err(LaraOperationError::CollectAllocationOverflow.into());
                        }
                        let start_slot = allocator
                            .taken_span()
                            .map_or(header.elem_capacity, |span| span.start_slot);
                        let end =
                            crate::slab_index::checked_add_slot_exclusive_end(start_slot, *len)
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        header.elem_capacity = header.elem_capacity.max(end);
                        (
                            allocator,
                            Some(FreeSpan {
                                start_slot,
                                len: *len,
                            }),
                            Some(PhysicalCountProjection::LeafBlock(*len)),
                        )
                    }
                }
            },
        };
        let mut positioned = Vec::new();
        positioned
            .try_reserve_exact(canonical.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        match &operation {
            PhysicalLayoutOperation::VertexRewrite { placement, .. } => {
                if placement.slots > 0 {
                    let end = crate::slab_index::checked_add_slot_exclusive_end(
                        placement.start,
                        u64::from(placement.slots),
                    )
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if let Some(pin) = pinned
                        && (placement.start < pin.start_slot || end > pin.start_slot + pin.len)
                    {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    for mate in canonical
                        .iter()
                        .filter(|row| row.src != src && row.vertex.stored_slots > 0)
                    {
                        if let Some(bucket) =
                            mate.buckets.iter().find(|bucket| !bucket.is_tiny_mode())
                        {
                            let mate_end = crate::slab_index::checked_add_slot_exclusive_end(
                                bucket.edge_start(),
                                u64::from(mate.vertex.stored_slots),
                            )
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            if placement.start < mate_end && bucket.edge_start() < end {
                                return Err(LaraOperationError::CollectAllocationOverflow.into());
                            }
                        }
                    }
                }
                let index = canonical
                    .iter()
                    .position(|row| row.src == src)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                positioned.push((index, *placement));
            }
            PhysicalLayoutOperation::LeafSlide { .. } => {
                let range = block.ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let leaf_end =
                    crate::slab_index::checked_add_slot_exclusive_end(range.start_slot, range.len)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let mut residents = Vec::new();
                residents
                    .try_reserve_exact(canonical.len())
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                let mut indexes = Vec::new();
                indexes
                    .try_reserve_exact(canonical.len())
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                let mut total = 0u64;
                for (index, row) in canonical.iter().enumerate() {
                    if (row.vertex.degree() == 0 && row.vertex.stored_slots == 0)
                        || (!row.buckets.is_empty()
                            && row.buckets.iter().all(|bucket| bucket.is_tiny_mode()))
                    {
                        continue;
                    }
                    let resident = row.buckets.iter().try_fold(0u32, |n, bucket| {
                        n.checked_add(self.bucket_resident_rows(bucket)?)
                            .ok_or(LaraOperationError::RowDegreeOverflow)
                            .map_err(LabeledOperationError::from)
                    })?;
                    total = total
                        .checked_add(u64::from(resident))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    residents.push(resident);
                    indexes.push(index);
                }
                let gaps = range
                    .len
                    .checked_sub(total)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let starts = Self::calculate_labeled_leaf_vertex_positions(
                    range.start_slot,
                    &residents,
                    gaps,
                )?;
                for (i, index) in indexes.into_iter().enumerate() {
                    let end = if residents[i] == 0 {
                        starts[i]
                    } else {
                        starts.get(i + 1).copied().unwrap_or(leaf_end)
                    };
                    let width = u32::try_from(
                        end.checked_sub(starts[i])
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                    )
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                    positioned.push((
                        index,
                        VertexPhysicalPlacement::new(starts[i], width, None, 0),
                    ));
                }
                positioned.sort_unstable_by_key(|(index, placement)| {
                    (std::cmp::Reverse(placement.start), *index)
                });
            }
        }
        // Streaming requires complete block disjointness AND containment of every
        // actual resident source, never just disjoint descriptor anchors.
        let stream = matches!(operation, PhysicalLayoutOperation::LeafSlide { .. })
            && pinned.zip(block).is_some_and(|(old, new)| {
                let old_end = old.start_slot + old.len;
                let new_end = new.start_slot + new.len;
                (old_end <= new.start_slot || new_end <= old.start_slot)
                    && canonical
                        .iter()
                        .flat_map(|row| &row.buckets)
                        .filter_map(bucket_content_region)
                        .all(|(start, len)| {
                            start >= old.start_slot
                                && start
                                    .checked_add(u64::from(len))
                                    .is_some_and(|end| end <= old_end)
                        })
            });
        #[cfg(test)]
        physical_layout_event(if stream {
            PhysicalLayoutEvent::Streamed
        } else {
            PhysicalLayoutEvent::Buffered
        });
        let mut rows = Vec::new();
        rows.try_reserve_exact(positioned.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let mut images = Vec::new();
        images
            .try_reserve_exact(positioned.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        for (index, placement) in positioned {
            let row = &canonical[index];
            let mut desired = Vec::new();
            desired
                .try_reserve_exact(row.buckets.len())
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            desired.extend_from_slice(&row.buckets);
            let mut transcribed = Vec::new();
            transcribed
                .try_reserve_exact(row.buckets.len())
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            transcribed.resize_with(row.buckets.len(), || None);
            let component = if matches!(operation, PhysicalLayoutOperation::VertexRewrite { .. }) {
                PreparedVertexPhysicalCopy::reserve_desired(
                    self,
                    row.src,
                    (desired, transcribed),
                    placement,
                )?
            } else {
                PreparedVertexPhysicalCopy::reserve(
                    source,
                    row.copy_source()?,
                    desired,
                    (placement, header),
                    if stream {
                        PhysicalCopyInput::Disjoint
                    } else {
                        PhysicalCopyInput::Buffered(transcribed)
                    },
                )?
            };
            if component.canonical.vertex != row.vertex
                || component.canonical.buckets != row.buckets
                || component
                    .final_buckets
                    .iter()
                    .zip(&row.buckets)
                    .any(|(new, old)| {
                        new.with_edge_range(old.edge_start(), old.stored_slots_raw()) != *old
                    })
                || component.final_vertex
                    != if row.vertex.is_default_edge_labeled() {
                        row.vertex.with_base_slot_start(placement.start)
                    } else {
                        row.vertex.with_stored_slots(placement.slots)
                    }
            {
                return Err(LaraOperationError::CollectAllocationOverflow.into());
            }
            rows.push(component.writers);
            images.push(component.payload);
        }
        let counts = count_projection
            .map(|projection| self.edges.prepare_physical_counts(src, projection))
            .transpose()?;
        let pin = if matches!(operation, PhysicalLayoutOperation::LeafSlide { .. })
            && block.is_some_and(|range| source.pin.physical_start != range.start_slot)
        {
            Some(self.edges.prepare_segment_physical_start(
                SegmentId::from(source.leaf),
                block.expect("slide destination").start_slot,
            )?)
        } else {
            None
        };
        let slab = self.edges.prepare_physical_image(header, images)?;
        if !slab.validate_source() {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        source.validate()?;
        for row in &canonical {
            row.validate(self)?;
        }
        Ok(PhysicalLayoutPreparation {
            source,
            canonical,
            rows,
            slab,
            counts,
            pin,
            allocator,
            #[cfg(test)]
            retired_leaf: if matches!(operation, PhysicalLayoutOperation::LeafSlide { .. })
                && pinned != block
            {
                pinned
            } else {
                None
            },
        })
    }

    pub(super) fn vertex_label_edge_span_end_exclusive(
        vertex: &LabeledVertex,
        first_bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        checked_add_slot_index(first_bucket.edge_start(), u64::from(vertex.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(crate) fn release_vertex_edge_span_slab(
        &self,
        base: u64,
        len: u64,
    ) -> Result<(), LabeledOperationError> {
        let mut cur_base = base;
        let mut cur_len = len;
        while cur_len > 0 {
            if self.edges.release_span(cur_base, cur_len).is_ok() {
                return Ok(());
            }
            if let Some(free_at_base) = self.edges.free_span_store().free_span_starting_at(cur_base)
            {
                let skip = free_at_base.len.min(cur_len);
                cur_base = cur_base
                    .checked_add(skip)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                cur_len = cur_len
                    .checked_sub(skip)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                continue;
            }
            let mut lo = 1u64;
            let mut hi = cur_len;
            let mut best = 0u64;
            while lo <= hi {
                let mid = lo + (hi - lo) / 2;
                if self.edges.release_span(cur_base, mid).is_ok() {
                    best = mid;
                    lo = mid
                        .checked_add(1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                } else if mid == 0 {
                    break;
                } else {
                    hi = mid
                        .checked_sub(1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                }
            }
            if best > 0 {
                cur_base = cur_base
                    .checked_add(best)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                cur_len = cur_len
                    .checked_sub(best)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            } else {
                cur_base = cur_base
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                cur_len = cur_len
                    .checked_sub(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
        }
        Ok(())
    }

    /// Returns contiguous physical intervals whose union is the retired VertexEdgeSpan
    /// `[span_start, span_start + span_len)`, including proportional interior slack.
    pub(super) fn vertex_edge_span_retire_intervals(
        span_start: u64,
        span_len: u32,
        buckets: &[LabelBucket],
    ) -> Result<Vec<(u64, u64)>, LabeledOperationError> {
        if span_len == 0 {
            return Ok(Vec::new());
        }
        let span_end = checked_add_slot_index(span_start, u64::from(span_len))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        // Plan 0319 §Step 3: for tree-mode buckets, the LEG span is
        // the **physical root region** (a u32 block_id array), NOT
        // `stored_slots` edge slots. The previous code used
        // `bucket.stored_slots()` for every bucket, which for a tree
        // bucket would claim a region far larger than the root region
        // and cause the rewrite to read past the region into
        // neighboring buckets (corruption). The helper
        // `bucket_span_region_len` returns the correct region length
        // for both modes.
        let mut bucket_intervals: Vec<(u64, u64)> = Vec::new();
        bucket_intervals
            .try_reserve_exact(buckets.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        bucket_intervals.extend(
            buckets
                .iter()
                // ADR 0096 §5 + GAP-2026-09-17-001: tiny buckets occupy zero slab
                // slots (including them would validate phantom anchor spans
                // against real geometry); tree buckets occupy only their root
                // region (`bucket_content_slots` — the logical
                // `stored_slots` width is NOT releasable). The zero-physical
                // filter below subsumes both (tiny and empty buckets carry no
                // owned ranges).
                .filter(|bucket| bucket_content_slots(bucket) > 0)
                .map(|bucket| {
                    // Plan 0326 LPB-in-tree (REWORK): for `w > 0` tree
                    // buckets, the vertex span is `[edge root | property
                    // root]` (gap 0, ADR 0088 §2). The combined length is
                    // `edge_root_len + property_root_len` — the physical width
                    // from `bucket_content_slots` (NOT the logical
                    // `stored_slots`). The single interval covers both regions
                    // contiguously; no separate property-root interval is needed.
                    (bucket.edge_start(), u64::from(bucket_content_slots(bucket)))
                }),
        );
        bucket_intervals.sort_unstable_by_key(|(start, _)| *start);

        let mut merged_buckets: Vec<(u64, u64)> = Vec::new();
        merged_buckets
            .try_reserve_exact(bucket_intervals.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        for (start, len) in bucket_intervals {
            let end = checked_add_slot_index(start, len)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if let Some((last_start, last_len)) = merged_buckets.last_mut() {
                let last_end = checked_add_slot_index(*last_start, *last_len)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if start <= last_end {
                    if end > last_end {
                        *last_len = end
                            .checked_sub(*last_start)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    }
                    continue;
                }
            }
            merged_buckets.push((start, len));
        }

        let mut intervals: Vec<(u64, u64)> = Vec::new();
        let interval_count = merged_buckets
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        intervals
            .try_reserve_exact(interval_count)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let mut cursor = span_start;
        for (bucket_start, bucket_len) in merged_buckets {
            if cursor < bucket_start {
                let gap_len = bucket_start
                    .checked_sub(cursor)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                intervals.push((cursor, gap_len));
            }
            intervals.push((bucket_start, bucket_len));
            cursor = checked_add_slot_index(bucket_start, bucket_len)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .max(cursor);
        }
        if cursor < span_end {
            let tail_len = span_end
                .checked_sub(cursor)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            intervals.push((cursor, tail_len));
        }

        Ok(intervals)
    }

    fn tail_append_labeled_edge_base(&self, new_alloc: u32) -> Result<u64, LabeledOperationError> {
        let start = self.edges.header().elem_capacity;
        let end = crate::slab_index::checked_add_slot_exclusive_end(start, u64::from(new_alloc))
            .ok_or_else(|| {
                log_collect_overflow(&format!(
                    "tail_append_labeled_edge_base: elem_capacity={start} new_alloc={new_alloc} exceeds index space"
                ));
                LaraOperationError::CollectAllocationOverflow
            })?;
        self.edges
            .set_elem_capacity(end)
            .map_err(LabeledOperationError::from)?;
        Ok(start)
    }

    /// Resolves edge-slab base for [`rebalance_vertex_edge_span`]: in-leaf pin, one leaf
    /// relocate, or (unpinned / relocate-internal only) tail append at `elem_capacity`.
    pub(super) fn labeled_edge_base_from_first_bucket(
        &self,
        src: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(src);
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        // ADR 0096 §5: anchor at the first non-tiny bucket (tiny anchors are
        // placeholders, not span bases). Spanless vertices have no slab base.
        buckets
            .iter()
            .find(|bucket| !bucket.is_tiny_mode())
            .map(|bucket| bucket.edge_start())
            .ok_or_else(|| {
                log_collect_overflow(&format!(
                    "labeled_edge_base_from_first_bucket: src={src:?} has no buckets"
                ));
                LaraOperationError::CollectAllocationOverflow.into()
            })
    }

    pub(super) fn resolve_labeled_edge_base_for_rebalance(
        &self,
        src: VertexId,
        new_alloc: u32,
    ) -> Result<(u64, u32), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)? {
            return Ok((base, new_alloc));
        }
        if LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.get())
            || LABELED_REBALANCE_RESOLVE_IN_PROGRESS.with(|flag| flag.get())
        {
            if self.labeled_leaf_physical_range(src)?.is_some() {
                return self.finish_span_resolution(
                    src,
                    new_alloc,
                    SpanResolutionPolicy::SlackMayBeDropped,
                    true,
                );
            }
            log_collect_overflow(
                "resolve_labeled_edge_base_for_rebalance: leaf not pinned; pinning before allocating",
            );
            self.relocate_labeled_leaf_physical_block(src)?;
            if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)? {
                return Ok((base, new_alloc));
            }
            return self.finish_span_resolution(
                src,
                new_alloc,
                SpanResolutionPolicy::SlackMayBeDropped,
                false,
            );
        }
        let _resolve_guard = LabeledRebalanceResolveGuard::new();
        if self.labeled_leaf_physical_range(src)?.is_some() {
            self.relocate_labeled_leaf_physical_block(src)?;
            LABELED_REBALANCE_LEAF_RELOCATED.with(|flag| flag.set(true));
            if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)? {
                return Ok((base, new_alloc));
            }
            let base = self.labeled_edge_base_from_first_bucket(src)?;
            return Ok((base, self.vertices.get(src).stored_slots));
        }
        log_collect_overflow(
            "resolve_labeled_edge_base_for_rebalance: leaf not pinned; pinning before allocating",
        );
        self.relocate_labeled_leaf_physical_block(src)?;
        LABELED_REBALANCE_LEAF_RELOCATED.with(|flag| flag.set(true));
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)? {
            return Ok((base, new_alloc));
        }
        self.finish_span_resolution(
            src,
            new_alloc,
            SpanResolutionPolicy::SlackMayBeDropped,
            false,
        )
    }

    /// Resolves edge-slab base for [`rewrite_vertex_edge_span`]: in-leaf pin, leaf relocate
    /// (with growth retries), or (unpinned / relocate-internal only) tail append.
    /// Returns `(base, width)` — the span the resolution actually obtained, which is
    /// what the caller may publish. Invariant I1: a cover is a claim that must be
    /// backed, so publishing the *requested* width over a base that kept the old
    /// reservation is exactly the phantom cover this pair of resolvers exists to
    /// prevent (GAP-2026-09-20-005).
    /// Dispatch for callers that carry a resolution policy explicitly.
    fn resolve_labeled_edge_base_for_policy(
        &self,
        src: VertexId,
        new_alloc: u32,
        policy: SpanResolutionPolicy,
    ) -> Result<(u64, u32), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        match policy {
            SpanResolutionPolicy::Required => {
                self.resolve_labeled_edge_base_for_growth(src, new_alloc)
            }
            SpanResolutionPolicy::SlackMayBeDropped => {
                self.resolve_labeled_edge_base_for_rebalance(src, new_alloc)
            }
        }
    }

    /// The policy-dependent tail of a resolution that could not place the requested span.
    /// The control flow that reaches it stays with each caller: the two resolvers differ
    /// only in which outcome they accept.
    fn finish_span_resolution(
        &self,
        src: VertexId,
        new_alloc: u32,
        policy: SpanResolutionPolicy,
        relocation_in_progress: bool,
    ) -> Result<(u64, u32), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if policy == SpanResolutionPolicy::Required {
            if relocation_in_progress {
                // Recursion is impossible mid-relocation, so a span that must exist
                // escapes to the tail: that is a real reservation, not spare room.
                return Ok((self.tail_append_labeled_edge_base(new_alloc)?, new_alloc));
            }
            log_collect_overflow(&format!(
                "resolve_labeled_edge_base: no placement for a required span of {new_alloc} slots"
            ));
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        // Slack may be dropped: keep the span the vertex holds (invariant I1 — the
        // returned width is what the caller publishes, so it must be backed).
        let base = self.labeled_edge_base_from_first_bucket(src)?;
        Ok((base, self.vertices.get(src).stored_slots))
    }

    pub(super) fn resolve_labeled_edge_base_for_growth(
        &self,
        src: VertexId,
        new_alloc: u32,
    ) -> Result<(u64, u32), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)? {
            return Ok((base, new_alloc));
        }
        if self.labeled_leaf_physical_range(src)?.is_some() {
            if LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.get()) {
                return self.finish_span_resolution(
                    src,
                    new_alloc,
                    SpanResolutionPolicy::Required,
                    true,
                );
            }
            for _ in 0..4 {
                if let Some(base) =
                    self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)?
                {
                    return Ok((base, new_alloc));
                }
                self.relocate_labeled_leaf_physical_block(src)?;
            }
            return self.finish_span_resolution(
                src,
                new_alloc,
                SpanResolutionPolicy::Required,
                false,
            );
        }
        // Leaf is not pinned yet. Pin it with a block-aligned leaf physical block
        // instead of tail-appending, per ADR 0001 new-bucket contract.
        self.relocate_labeled_leaf_physical_block(src)?;
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)? {
            return Ok((base, new_alloc));
        }
        self.finish_span_resolution(src, new_alloc, SpanResolutionPolicy::Required, false)
    }

    /// Releases a relocated VertexEdgeSpan footprint back to the edge free-span store.
    ///
    /// The footprint is released as the mode-aware bucket regions plus the
    /// interior/tail remainder of the `[span_start, span_len)` reservation
    /// (see [`Self::vertex_edge_span_retire_intervals`]). A monolithic release of
    /// the whole cover is NOT attempted: for tree-mode buckets the cover is a
    /// logical width (`stored_slots` edge count) while the physical LEG span is
    /// only the root region (`combined_span_region_len`), so a whole-cover
    /// release would hand unowned ranges to the free store (GAP-2026-09-17-001:
    /// `OverlapPrevious` trap past ~5.7K edges). The interval path releases only
    /// owned bucket regions plus slack remainder, which is correct in every mode.
    pub(super) fn release_vertex_edge_span_footprint(
        &self,
        span_start: u64,
        span_len: u32,
        buckets: &[LabelBucket],
    ) -> Result<(), LabeledOperationError> {
        #[cfg(test)]
        record_labeled_vertex_footprint_release();
        if span_len == 0 {
            return Ok(());
        }
        // ADR 0096 §5 + GAP-2026-09-17-001: spanless vertices (all tiny or all
        // emptied) hold no slab bytes; a tree bucket with an empty root region
        // likewise owns nothing — a stale width/anchor cover would poison the
        // free store with live ranges on release. Nothing live, nothing to free.
        let has_live_slab = buckets
            .iter()
            .any(|bucket| bucket_content_slots(bucket) > 0);
        if !has_live_slab {
            return Ok(());
        }
        // GAP-2026-09-17-001 (A' fix): no monolithic whole-cover release.
        // For tree-mode buckets `span_len` is the logical `stored_slots` width
        // while the physical LEG span is only the root region, so the cover
        // contains unowned ranges that must never reach the free store.
        // The interval path below releases exactly the owned bucket regions
        // plus the interior/tail slack remainder.
        for (start, interval_len) in
            Self::vertex_edge_span_retire_intervals(span_start, span_len, buckets)?
        {
            if interval_len == 0 {
                continue;
            }
            if self.edges.release_span(start, interval_len).is_ok() {
                continue;
            }
            self.release_vertex_edge_span_slab(start, interval_len)?;
        }
        Ok(())
    }

    pub(super) fn rewrite_vertex_edge_span_read_and_plan(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        preferred_extra: u32,
        force_slack_grow: bool,
        in_window_layout: Option<(u64, u32)>,
        policy: SpanResolutionPolicy,
    ) -> Result<(Vec<LabelBucket>, u32, u64, u32, bool, u64), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_rewrite_read_and_plan");
        let segment_size = self.edges.header().segment_size.max(1);
        let buckets = self.read_vertex_label_buckets(vertex)?;
        let old_alloc = vertex.stored_slots;
        // ADR 0096 §5: anchor at the first NON-TINY bucket (tiny anchors are
        // placeholders; releasing from an anchor poisons the free store with
        // live ranges). Spanless vertices resolve to 0 (callers skip release
        // on empty spans).
        let old_base = buckets
            .iter()
            .find(|bucket| !bucket.is_tiny_mode())
            .map(|bucket| bucket.edge_start())
            .unwrap_or(0);
        let mut total_live = 0u32;
        for bucket in &buckets {
            // ADR 0096 §5: tiny edges need no slab space in rewrite sizing.
            if bucket.is_tiny_mode() {
                continue;
            }
            // Physical publication preserves initialized regions and run ordinals.
            // Logical reservation/headroom policy remains separate.
            let resident = bucket_content_slots(bucket);
            total_live = total_live
                .checked_add(resident)
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
        }

        let min_required = total_live
            .checked_add(preferred_extra)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        if let Some((new_base, new_alloc)) = in_window_layout {
            if new_alloc < min_required {
                return Err(LaraOperationError::CollectAllocationOverflow.into());
            }
            let old_alloc = vertex.stored_slots;
            let old_base = buckets
                .first()
                .map(|bucket| bucket.edge_start())
                .unwrap_or(0);
            let moved = old_alloc != new_alloc || old_base != new_base;
            return Ok((buckets, old_alloc, old_base, new_alloc, moved, new_base));
        }
        let new_alloc = if total_live == 0 && preferred_extra == 0 {
            // A spanless vertex (all buckets tiny, no room requested) owns zero slab slots by
            // ADR 0096 §5, so it must not request slack: the request would be a real span
            // (`next_vertex_edge_span_allocation` returns at least `segment_size`), and when
            // it cannot be placed the slack tail has no base to report
            // (`labeled_edge_base_from_first_bucket` needs a non-tiny bucket), which failed
            // the whole leaf-wide log recovery (class C).
            //
            // `preferred_extra` is the caller's real room request, so it vetoes the shortcut: the
            // promotion's Phase 0a recovery grows the span for a bucket whose rows sit in its run
            // (`total_live` counts the prefix alone, ADR 0097 §7) and must get a span that can hold
            // the compaction target it is about to write.
            0
        } else if !force_slack_grow && old_alloc >= min_required && old_alloc > 0 {
            old_alloc
        } else {
            next_vertex_edge_span_allocation(old_alloc, min_required, segment_size)?
        };

        let requested_alloc = new_alloc;
        let (new_base, new_alloc) = if new_alloc == 0 {
            (0, 0)
        } else if old_alloc > 0 && old_alloc >= min_required && new_alloc > old_alloc {
            // Content already fits and the only reason to widen is spare capacity. One
            // placement attempt in the current window *is* the answer when it succeeds (no
            // second attempt through the resolver), and when it fails the span stays as it is:
            // slack never moves a leaf and never fails an insert. Only content that does not
            // fit may trigger a relocation.
            match self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)? {
                Some(base) => (base, new_alloc),
                None => (old_base, old_alloc),
            }
        } else if old_alloc == 0 || new_alloc > old_alloc {
            self.resolve_labeled_edge_base_for_policy(src, requested_alloc, policy)?
        } else {
            (old_base, new_alloc)
        };
        // A widened span may not run past the next leaf-mate's edge start: that is the space its
        // rows must come back to, and borrowing it is how a prefix grew over a sibling (ADR 0098).
        // Dropping the growth keeps the span the vertex already holds, which is what the
        // `SlackMayBeDropped` callers accept.
        let (new_base, new_alloc) = match self.next_leaf_vertex_edge_start(src)? {
            Some(next)
                if new_alloc > old_alloc
                    && new_base.saturating_add(u64::from(new_alloc)) > next =>
            {
                (old_base, old_alloc)
            }
            _ => (new_base, new_alloc),
        };
        // I1: the plan reports the width the resolution obtained, so the commit never
        // publishes a cover wider than the span that backs it.
        let moved = old_alloc == 0 || new_alloc > old_alloc;

        // Positions are the shared commit's decision: it recomputes them from the
        // materialized runs and the caller's placement preference, so the plan only
        // reports what the commit cannot know (span width and base).
        Ok((buckets, old_alloc, old_base, new_alloc, moved, new_base))
    }

    pub(super) fn edge_bytes_for_len(edge_count: usize) -> Result<usize, LabeledOperationError> {
        edge_count
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn rewrite_vertex_edge_span(
        &self,
        src: VertexId,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        force_slack_grow: bool,
        in_window_layout: Option<(u64, u32)>,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.rewrite_vertex_edge_span_with_policy(
            src,
            preferred_bucket,
            preferred_extra,
            force_slack_grow,
            in_window_layout,
            SpanResolutionPolicy::Required,
        )
    }

    /// Rewrite that also names which base-resolution policy its caller accepts.
    pub(super) fn rewrite_vertex_edge_span_with_policy(
        &self,
        src: VertexId,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        force_slack_grow: bool,
        in_window_layout: Option<(u64, u32)>,
        policy: SpanResolutionPolicy,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        #[cfg(test)]
        record_rewrite_vertex_edge_span();
        // Rebuild the VertexEdgeSpan from initialized slab prefixes and tree roots.
        //
        // The bucket layer is exact-fit, so a new label can appear anywhere in
        // BucketLabelKey order without reserving space in the bucket descriptor. Edge
        // slack is instead distributed inside this VertexEdgeSpan. As in the
        // regular LARA PMA rebalance, row gaps are weighted by `degree + 1`, so a
        // high-degree label receives more spare room than a cold label. The
        // preferred bucket receives `preferred_extra` before proportional gap
        // placement because the caller is about to insert there.
        //
        // Capture each physical region before publishing any destination: in-place
        // layouts may overlap. Slab prefixes include holes; spill runs keep their
        // owners and ordinals and are not decoded or folded by this publisher.
        let planned_vertex = self.vertices.get(src);
        if planned_vertex.is_default_edge_labeled() || planned_vertex.degree() == 0 {
            return Ok(());
        }

        let (buckets, _, _, new_alloc, _, new_base) = self.rewrite_vertex_edge_span_read_and_plan(
            src,
            &planned_vertex,
            preferred_extra,
            force_slack_grow,
            in_window_layout,
            policy,
        )?;

        // Resolving a growth base may relocate the leaf and update bucket origins.
        // Re-read the edge buckets rather than publishing positions from a stale
        // pre-relocation snapshot.
        let vertex = self.vertices.get(src);
        let current_buckets = self.read_vertex_label_buckets(&vertex)?;
        if vertex != planned_vertex || current_buckets != buckets {
            // Base resolution may relocate the whole pinned leaf. That updates all
            // vertex spans and bucket starts, invalidating this vertex's pre-relocation
            // plan. Restart from the published layout instead of committing stale
            // positions over the relocation result.
            // Escalation guard: planning just made room by relocation, so the
            // restart must fit into it, not re-grow from the fresh (tile-inflated)
            // span. Re-growing would demand a larger quantum than the relocate
            // sized for, exhaust its retry loop, and fail a healthy span —
            // force_slack drops on restart (proactive slack accrues later).
            return self.rewrite_vertex_edge_span_with_policy(
                src,
                preferred_bucket,
                preferred_extra,
                false,
                in_window_layout,
                policy,
            );
        }

        // Destination policy and any earlier relocation have completed. This is
        // the physical unit's freeze point, not rollback of the outer resolver.
        let mut preparation =
            self.prepare_physical_layout(PhysicalLayoutOperation::VertexRewrite {
                src,
                placement: VertexPhysicalPlacement::new(
                    new_base,
                    new_alloc,
                    preferred_bucket.map(|index| index as usize),
                    preferred_extra,
                ),
            })?;
        preparation.ready()?.publish();
        Ok(())
    }

    fn calculate_labeled_leaf_vertex_positions(
        leaf_start: u64,
        resident_slots: &[u32],
        gaps: u64,
    ) -> Result<Vec<u64>, LabeledOperationError> {
        let mut total_weight = 0u64;
        for resident in resident_slots {
            total_weight = total_weight
                .checked_add(u64::from(*resident))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }

        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .checked_div(tw)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
        };

        let mut cursor_fp = u128::from(leaf_start)
            .checked_mul(P)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let mut out = Vec::new();
        out.try_reserve_exact(resident_slots.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        for resident in resident_slots {
            let start = u64::try_from(cursor_fp / P)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            out.push(start);
            let resident = u128::from(*resident);
            let start_fp = u128::from(start)
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            cursor_fp = start_fp
                .checked_add(
                    resident
                        .checked_mul(P)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                )
                .and_then(|cursor| cursor.checked_add(step_fp.checked_mul(resident)?))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(out)
    }

    /// Weighted in-window slide for all labeled vertices sharing one pinned PMA leaf block.
    pub(crate) fn rebalance_labeled_leaf_weighted_slide(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let (leaf_start, leaf_len) = match self.labeled_leaf_physical_range(src)? {
            Some(range) => range,
            None => return Ok(()),
        };
        let mut preparation = self.prepare_physical_layout(PhysicalLayoutOperation::LeafSlide {
            src,
            destination: LeafPhysicalDestination::Current(FreeSpan {
                start_slot: leaf_start,
                len: leaf_len,
            }),
        })?;
        preparation.ready()?.publish();
        Ok(())
    }

    /// Relocates the leaf's block with `extra_slots` of room beyond its residents, so a
    /// caller that is about to place a span there does not have to relocate twice.
    pub(crate) fn relocate_labeled_leaf_physical_block_with_room(
        &self,
        src: VertexId,
        extra_slots: u64,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let _relocate_guard = LabeledLeafRelocateGuard::new();
        #[cfg(test)]
        physical_layout_event(PhysicalLayoutEvent::Prepare(src));
        let (source, canonical) = self.capture_physical_layout_source(src)?;
        let target = self.plan_labeled_leaf_relocation_from(src, &source, &canonical)?;
        let new_len = target
            .new_len
            .checked_add(extra_slots)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let mut preparation = self.prepare_physical_layout_from(
            src,
            source,
            canonical,
            PhysicalLayoutOperation::LeafSlide {
                src,
                destination: LeafPhysicalDestination::Relocate(new_len),
            },
        )?;
        preparation.ready()?.publish();
        Ok(())
    }

    pub(crate) fn relocate_labeled_leaf_physical_block(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let _relocate_guard = LabeledLeafRelocateGuard::new();
        // One capture feeds both the target sizing and the preparation: the leaf's rows are read
        // once, and every later stage works from this snapshot.
        #[cfg(test)]
        physical_layout_event(PhysicalLayoutEvent::Prepare(src));
        let (source, canonical) = self.capture_physical_layout_source(src)?;
        let target = self.plan_labeled_leaf_relocation_from(src, &source, &canonical)?;
        // Destination acquisition, pin/count and retirement now join one loan.
        // No allocator/header/pin mutation occurs between this freeze and ready.
        let mut preparation = self.prepare_physical_layout_from(
            src,
            source,
            canonical,
            PhysicalLayoutOperation::LeafSlide {
                src,
                destination: LeafPhysicalDestination::Relocate(target.new_len),
            },
        )?;
        preparation.ready()?.publish();
        // The slide re-tiles the whole leaf, so it is no longer in the fixed-quota layout the fast
        // placement path assumes (ADR 0098). Recorded after the publish, next to the other writer of
        // this fact - the fold, which grows a prefix without a rewrite.
        self.clear_leaf_fixed_quota_intact(src);
        Ok(())
    }

    /// Include bypass-owned prefixes in the same leaf snapshot as bucket-owned spans.
    fn leaf_span_buckets(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            Ok(vec![self.bypass_bucket_view(vertex)?])
        } else {
            self.read_vertex_label_buckets(vertex)
        }
    }

    /// Sizes one relocation from the rows a caller already captured, so a relocation does not
    /// read the leaf twice. Same arithmetic as [`Self::plan_labeled_leaf_relocation`].
    fn plan_labeled_leaf_relocation_from(
        &self,
        src: VertexId,
        source: &PhysicalLayoutSource<'_, E, M>,
        canonical: &[CanonicalPhysicalVertex],
    ) -> Result<LabeledLeafRelocationTarget, LabeledOperationError> {
        let _ = src;
        let (old_start, old_len) = source
            .pinned_range()?
            .map_or((0, 0), |pin| (pin.start_slot, pin.len));
        let leaf = source.leaf;
        let counts = source.counts;
        let used = counts.actual.max(0) as u64;
        let active_vertices = canonical
            .iter()
            .filter(|row| row.vertex.degree() > 0 || row.vertex.stored_slots > 0)
            .count() as u64;
        let mut resident_geometry = 0u64;
        for row in canonical {
            if row.vertex.degree() == 0 && row.vertex.stored_slots == 0 {
                continue;
            }
            let mut resident_slots = 0u32;
            for bucket in &row.buckets {
                if bucket.is_tiny_mode() {
                    continue;
                }
                resident_slots = resident_slots
                    .checked_add(self.bucket_resident_rows(bucket)?)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?;
            }
            resident_geometry = resident_geometry.saturating_add(u64::from(resident_slots));
        }
        let seg = source.header.segment_size.max(1);
        let block_len = super::leaf_arena::labeled_leaf_physical_block_len(seg);
        let geometric_slack = resident_geometry.div_ceil(8).max(u64::from(seg));
        let raw_len = resident_geometry
            .saturating_add(active_vertices)
            .saturating_add(geometric_slack)
            .max(used.saturating_add(active_vertices))
            .max(old_len.saturating_add(1));
        let new_len = raw_len
            .div_ceil(block_len)
            .saturating_mul(block_len)
            .max(block_len);
        Ok(LabeledLeafRelocationTarget {
            leaf,
            old_start,
            old_len,
            new_len,
        })
    }

    /// Computes one relocation target from the leaf's current resident geometry.
    /// This is mutation-free: callers can reserve before publishing the new segment start.
    pub(crate) fn plan_labeled_leaf_relocation(
        &self,
        src: VertexId,
    ) -> Result<LabeledLeafRelocationTarget, LabeledOperationError> {
        let pinned_range = self.labeled_leaf_physical_range(src)?;
        let (old_start, old_len) = pinned_range.unwrap_or((0, 0));
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(src, header.segment_size);
        let counts = self.leaf_segment_counts_for_vid(src);
        let used = counts.actual.max(0) as u64;
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let active_vertices = (start_vid..end_vid)
            .filter(|vid_u| {
                let vertex = self.vertices.get(VertexId::from(*vid_u));
                vertex.degree() > 0 || vertex.stored_slots > 0
            })
            .count() as u64;
        let mut resident_geometry = 0u64;
        for vid_u in start_vid..end_vid {
            let vertex = self.vertices.get(VertexId::from(vid_u));
            if vertex.degree() == 0 && vertex.stored_slots == 0 {
                continue;
            }
            let buckets = self.leaf_span_buckets(&vertex)?;
            let mut resident_slots = 0u32;
            for bucket in &buckets {
                // ADR 0096 §5: tiny buckets occupy zero leaf slots (relocation
                // sizing covers physical spans only).
                if bucket.is_tiny_mode() {
                    continue;
                }
                // GAP-2026-09-17-001: tree buckets occupy only their root
                // region (`combined_span_region_len`), not `stored_slots` edge
                // slots. Sizing the leaf on the logical width over-allocates
                // the block and — worse — makes the whole-cover release hand
                // unowned ranges to the free store. Single source of truth:
                // `combined_span_region_len` (same helper the retire-interval
                // path uses).
                resident_slots = resident_slots
                    .checked_add(self.bucket_resident_rows(bucket)?)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?;
            }
            resident_geometry = resident_geometry.saturating_add(u64::from(resident_slots));
        }
        let block_len = super::leaf_arena::labeled_leaf_physical_block_len(seg);
        let geometric_slack = resident_geometry.div_ceil(8).max(u64::from(seg));
        let raw_len = resident_geometry
            .saturating_add(active_vertices)
            .saturating_add(geometric_slack)
            .max(used.saturating_add(active_vertices))
            .max(old_len.saturating_add(1));
        let new_len = raw_len
            .div_ceil(block_len)
            .saturating_mul(block_len)
            .max(block_len);
        Ok(LabeledLeafRelocationTarget {
            leaf,
            old_start,
            old_len,
            new_len,
        })
    }

    /// Reservation width: physical region plus used spill rows. For slab buckets
    /// this is the tombstone-inclusive logical extent used for promotion admission.
    /// It is not the number of prefix rows copied by a run-retaining relocation.
    pub(super) fn bucket_resident_rows(
        &self,
        bucket: &LabelBucket,
    ) -> Result<u32, LabeledOperationError> {
        let spill_rows = match self.edges.spill_mint(bucket.edge_spill_run()) {
            Ok(run) => self.edges.spill_used_len(run),
            // Unset (NO_SPILL_RUN) or outside the arena: no rows beyond the prefix.
            Err(_) => 0,
        };
        bucket_content_slots(bucket)
            .checked_add(spill_rows)
            .ok_or(LaraOperationError::RowDegreeOverflow)
            .map_err(LabeledOperationError::from)
    }

    /// Remove physical inter-bucket slack without changing any logical ordinal.
    /// Used by completion and peer cascades, neither of which can report row moves.
    pub(super) fn pack_vertex_edge_span_layout(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let physical = buckets.iter().try_fold(0u32, |sum, bucket| {
            sum.checked_add(bucket_content_slots(bucket))
                .ok_or(LaraOperationError::RowDegreeOverflow)
        })?;
        if vertex.stored_slots > physical {
            let base = buckets
                .iter()
                .find(|bucket| !bucket.is_tiny_mode())
                .map(|bucket| bucket.edge_start())
                .unwrap_or(0);
            self.rewrite_vertex_edge_span(vid, None, 0, false, Some((base, physical)))?;
        }
        Ok(())
    }

    pub(super) fn compact_label_bucket_overflow_to_slab(
        &self,
        src: VertexId,
        bucket_index: u32,
        bucket: LabelBucket,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.edge_run().is_none() {
            return Ok(Vec::new());
        }
        let mut moves = Vec::new();
        let run = self.edges.spill_mint(bucket.edge_spill_run())?;
        let mut live_log_edges = Vec::with_capacity(self.edges.spill_used_len(run) as usize);
        let mut live_log_sources: Vec<(u32, u32)> =
            Vec::with_capacity(self.edges.spill_used_len(run) as usize);
        for (log_offset, index) in self.edges.spill_asc_indices(run).into_iter().enumerate() {
            let log_offset = u32::try_from(log_offset)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let old_slot_index = bucket
                .stored_slots()
                .checked_add(log_offset)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.edges.spill_edge_at(run, index)?;
            if edge.is_tombstone_edge() {
                continue;
            }
            let live_log_offset = u32::try_from(live_log_edges.len())
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let new_slot_index = bucket
                .stored_slots()
                .checked_add(live_log_offset)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if old_slot_index != new_slot_index {
                moves.push(EdgeSlotMove {
                    label_id: bucket.bucket_label_key(),
                    old_slot_index,
                    new_slot_index,
                });
            }
            live_log_edges.push(edge.with_slot_index(new_slot_index));
            live_log_sources.push((index, new_slot_index));
        }

        let live_log_count = u32::try_from(live_log_edges.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let new_stored_slots = bucket
            .stored_slots()
            .checked_add(live_log_count)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        // Folding rows into the slab grows a bucket's prefix, so this leaf is no longer in the
        // fixed-quota layout the fast placement assumes (ADR 0098). Recorded here because the fold
        // is the one path that grows a prefix without a leaf rewrite.
        self.clear_leaf_fixed_quota_intact(src);
        let vertex = self.vertices.get(src);
        let successor = self.bucket_successor_start(&vertex, bucket_index)?;
        let available = successor.saturating_sub(bucket.edge_start());
        if available < u64::from(new_stored_slots) {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }

        for (log_offset, edge) in live_log_edges.into_iter().enumerate() {
            let log_offset = u64::try_from(log_offset)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let out_slot = checked_add_slot_index(
                bucket.edge_start(),
                u64::from(bucket.stored_slots())
                    .checked_add(log_offset)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?,
            )
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges
                .write_slot(out_slot, edge)
                .map_err(LabeledOperationError::from)?;
        }

        let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        // ADR 0099: the values plane mirrors the edge plane, so the rows that just moved into the
        // prefix bring their value blocks with them, and clearing the edge run clears the values run
        // in the same publication. A tombstoned run row is dropped with its value.
        let width = bucket.inline_property_byte_width();
        let compacted = if width != 0
            && let Some(values_run_id) = bucket.values_run()
        {
            let sized = self.ensure_bucket_inline_property_bytes_span(
                src,
                bucket_slot,
                bucket.with_stored_slots(new_stored_slots),
            )?;
            let mut value = vec![0u8; usize::from(width)];
            for (index, slot) in live_log_sources.iter().copied() {
                self.values
                    .spill_read(values_run_id, index, width, &mut value)
                    .map_err(LabeledOperationError::from)?;
                let offset = crate::labeled::invariants::inline_property_bytes_byte_offset_at_slot(
                    &sized, slot,
                )
                .map_err(LabeledOperationError::from)?;
                self.values
                    .write_bytes(offset, &value)
                    .map_err(LabeledOperationError::from)?;
            }
            // Both run releases go through their plane's owner, which pairs the store release with the
            // descriptor field: a released run must never be named by the published bucket.
            self.value_plane(bucket_slot, sized)
                .release_run()
                .bucket()
                .without_edge_spill_run()
        } else {
            bucket
                .with_stored_slots(new_stored_slots)
                .without_edge_spill_run()
        };
        self.publish_bucket_descriptor(bucket_slot, compacted)?;
        // The run's live rows now live in the prefix: release it (best-effort, like every other
        // span release on this path - a failed release leaks a class, never loses a row).
        let _ = self.edges.spill_release(run);
        Ok(moves)
    }

    /// Finds the first out-of-position live edge in an admitted slab bucket and returns
    /// the left-pack move filling the earliest hole. `resume_slot_index` is the compaction cursor:
    /// slots `[0, resume_slot_index)` are already front-packed (GAP-2026-07-14-001), so
    /// the scan starts there with `next_live` seeded from it instead of rescanning the
    /// packed prefix after every move. The step re-scans from zero when the cursor-based
    /// scan finds nothing, because an interleaved delete can break the packed-prefix
    /// invariant between steps.
    fn first_edge_slot_move_in_bucket(
        bucket: &LabelBucket,
        edges: &EdgeStore<E, M>,
        resume_slot_index: u32,
    ) -> Result<Option<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.degree() == 0 || bucket.edge_run().is_some() {
            return Ok(None);
        }
        let mut next_live = resume_slot_index;
        let mut bytes = [0u8; crate::lara::edge::INLINE_EDGE_BYTES];
        // Only slab descriptors expose a prefix width here. Tree stored_slots is
        // logical extent, not root-array width; batching requires that physical bound.
        let prefetch = !bucket.is_tiny_mode()
            && !bucket.is_tree_mode()
            && E::BYTES > 0
            && E::BYTES <= bytes.len();
        let (mut byte_cursor, mut buffered_bytes) = (0, 0);
        for old_slot_index in resume_slot_index..bucket.stored_slots() {
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(old_slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = if prefetch {
                if byte_cursor == buffered_bytes {
                    // Prefetch only initialized slots, also stopping at the address limit.
                    // Keep per-slot admission above: an earlier selected move must not be
                    // rejected merely because a later slot would overflow.
                    let count = u64::from(bucket.stored_slots() - old_slot_index)
                        .min((bytes.len() / E::BYTES) as u64)
                        .min(crate::slab_index::MAX_SLOT_EXCLUSIVE_END - edge_slot);
                    buffered_bytes = count as usize * E::BYTES; // bounded by bytes.len()
                    edges.read_slots_contiguous(edge_slot, &mut bytes[..buffered_bytes]);
                    byte_cursor = 0;
                }
                let edge = E::read_from(&bytes[byte_cursor..byte_cursor + E::BYTES]);
                byte_cursor += E::BYTES;
                edge
            } else {
                edges.read_slot(edge_slot)
            };
            if edge.is_tombstone_edge() {
                continue;
            }
            if old_slot_index != next_live {
                return Ok(Some(EdgeSlotMove {
                    label_id: bucket.bucket_label_key(),
                    old_slot_index,
                    new_slot_index: next_live,
                }));
            }
            next_live = next_live
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(None)
    }

    /// After slab-mode admission, permit swap-compaction only for prefix-only
    /// edges with width 0 or a dense, degree-sized value prefix. Other value
    /// layouts retain the order-preserving left-pack step (ADR 0052 §9).
    fn slab_bucket_allows_unordered_swap(bucket: &LabelBucket) -> bool {
        // ADR 0099 withdrew the value clause this used to carry. It existed because values were
        // addressed by live rank, so a swap had to shift a value window and could only do that on a
        // dense, run-free plane; with the values plane addressed by slot, a swap moves the row's
        // block into the hole and needs nothing of the kind.
        bucket.edge_run().is_none()
    }

    /// Finds the first interior tombstone (a non-live slot with a later live row) and
    /// the last live slot in one pass, computing the hole's live ordinal as a rank
    /// (never a physical slot index). Mirrors the left-pack finder's gates and treats
    /// both `is_tombstone_edge` and `is_deleted_slot` as non-live.
    ///
    /// `resume_slot_index` is the compaction cursor: under the packed-prefix invariant
    /// every slot before the cursor is live, so the first non-live slot at or after the
    /// cursor is also the bucket's first non-live slot and its live ordinal equals its
    /// position (the trailing last-live search still scans the unpacked suffix). The
    /// step re-scans from zero when this finds nothing, guarding against an interleaved
    /// delete that broke the prefix.
    fn first_edge_slot_swap_move_in_bucket(
        bucket: &LabelBucket,
        edges: &EdgeStore<E, M>,
        resume_slot_index: u32,
    ) -> Result<Option<EdgeSlotSwapMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.degree() == 0 || bucket.edge_run().is_some() {
            return Ok(None);
        }
        let mut first_hole: Option<u32> = None; // slot
        let mut last_live_slot: Option<u32> = None;
        for slot_index in resume_slot_index..bucket.stored_slots() {
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = edges.read_slot(edge_slot);
            if edge.is_tombstone_edge() || edge.is_deleted_slot() {
                if first_hole.is_none() {
                    first_hole = Some(slot_index);
                }
                continue;
            }
            last_live_slot = Some(slot_index);
        }
        match (first_hole, last_live_slot) {
            // Interior only: a trailing tombstone (no live row after it) is trimmable,
            // not a swap target. The hole's live ordinal equals its slot position because
            // every slot before the first hole is live.
            (Some(hole_slot), Some(vacated_slot)) if hole_slot < vacated_slot => {
                Ok(Some(EdgeSlotSwapMove {
                    label_id: bucket.bucket_label_key(),
                    hole_slot,
                    vacated_slot,
                    hole_ordinal: hole_slot,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Applies one swap-compaction move: value block first (ordinal space), then the
    /// edge write into the hole, then the tombstone at the vacated slot. All writes
    /// are preflighted in-bounds within the existing edge/value spans, so a recoverable
    /// error cannot occur after the first canonical byte write (the same
    /// atomicity-in-practice discipline as `commit_slab_holes_run`).
    fn apply_edge_slot_swap_move_in_bucket(
        &self,
        bucket: &LabelBucket,
        swap: &EdgeSlotSwapMove,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let hole = checked_add_slot_index(bucket.edge_start(), u64::from(swap.hole_slot))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let vacated = checked_add_slot_index(bucket.edge_start(), u64::from(swap.vacated_slot))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let width = bucket.inline_property_byte_width();
        if width > 0 {
            // ADR 0099: the moved row's value block goes where the row goes. The window shift this
            // used to perform existed because values were addressed by rank and the swap had to keep
            // the ranks in order; with one geometry it is a single block copy, and it works whichever
            // plane holds the block.
            let value = self.read_value_block_at_slot(bucket, swap.vacated_slot)?;
            self.write_value_block_at_slot(bucket, swap.hole_slot, &value)?;
        }
        let edge = self.edges.read_slot(vacated);
        self.edges
            .write_slot(hole, edge)
            .map_err(LabeledOperationError::from)?;
        self.edges
            .write_slot(vacated, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }

    fn apply_edge_slot_move_in_bucket(
        &self,
        bucket: &LabelBucket,
        moved: EdgeSlotMove,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let from = checked_add_slot_index(bucket.edge_start(), u64::from(moved.old_slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let to = checked_add_slot_index(bucket.edge_start(), u64::from(moved.new_slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let edge = self.edges.read_slot(from);
        self.edges
            .write_slot(to, edge)
            .map_err(LabeledOperationError::from)?;
        self.edges
            .write_slot(from, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        // ADR 0099: a moved row takes its value block with it. Both planes are addressed by slot, so
        // this is one block copy; before the planes were unified the values were addressed by rank
        // and did not need to move, which is exactly why the moves and the values could disagree.
        let width = bucket.inline_property_byte_width();
        if width != 0 {
            let value = self.read_value_block_at_slot(bucket, moved.old_slot_index)?;
            self.write_value_block_at_slot(bucket, moved.new_slot_index, &value)?;
        }
        Ok(())
    }

    fn finalize_bucket_slab_metadata(bucket: LabelBucket) -> LabelBucket {
        // ADR 0097 §5: a run-backed bucket's rows are addressed as `prefix_len + i`, so shrinking
        // the prefix to `degree` (and clearing the run) would shift every one of them onto the
        // wrong ordinal. Only a bucket whose rows are all in its prefix may be finalized.
        if bucket.edge_run().is_some() {
            return bucket;
        }
        bucket
            .with_stored_slots(bucket.degree())
            .without_edge_spill_run()
    }

    pub(crate) fn compact_vertex_edge_span_one_step(
        &self,
        vid: VertexId,
        resume_bucket_index: u32,
        resume_slot_index: u32,
        policy_for_label: &dyn Fn(BucketLabelKey) -> EdgePlacementPolicy,
    ) -> Result<VertexEdgeSpanCompactOneStep, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.compact_vertex_edge_span_one_step_with_span_policy(
            vid,
            resume_bucket_index,
            resume_slot_index,
            policy_for_label,
            SpanResolutionPolicy::Required,
        )
    }

    /// The same step, with the caller's span-resolution policy (ADR 0097 §7).
    ///
    /// `Required` may force room for a run-backed bucket's rows (a real relocation): that is what a
    /// compaction asked to make the span compact needs. `SlackMayBeDropped` never forces it - the
    /// rows already live in the bucket's own run, so a declined fold retains both the prefix
    /// width and run ordinals, including at Finished. Forcing it under maintenance was a spin:
    /// the policy keeps the old span, the fold fails again, and the retry repeated (~56 leaf
    /// rewrites per delete; scope calls 1 -> 56 for `labeled_leaf_try_expand_in_place`).
    pub(crate) fn compact_vertex_edge_span_one_step_with_span_policy(
        &self,
        vid: VertexId,
        resume_bucket_index: u32,
        resume_slot_index: u32,
        policy_for_label: &dyn Fn(BucketLabelKey) -> EdgePlacementPolicy,
        span_policy: SpanResolutionPolicy,
    ) -> Result<VertexEdgeSpanCompactOneStep, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        #[cfg(test)]
        if take_forced_compact_vertex_edge_span_step_error() {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        self.ensure_vertex(vid)?;
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(VertexEdgeSpanCompactOneStep::Finished);
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        // Preserve logical-count admission before any movement, even when this pass
        // may retain a run instead of materializing the complete packed span.
        buckets
            .iter()
            .filter(|bucket| !bucket.is_tiny_mode())
            .try_fold(0u32, |sum, bucket| {
                sum.checked_add(bucket.degree())
                    .ok_or(LaraOperationError::RowDegreeOverflow)
            })?;
        if resume_bucket_index == 0
            && let Some((bucket_index, bucket)) = buckets
                .iter()
                .enumerate()
                .find(|(_, bucket)| bucket.edge_run().is_some())
        {
            let bucket_index = u32::try_from(bucket_index)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            match self.compact_label_bucket_overflow_to_slab(vid, bucket_index, *bucket) {
                Ok(moves) => {
                    return Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(moves));
                }
                // Whether to force room for the run's rows is the caller's call (ADR 0097 §7 and
                // the slack rule db04af585 states): a delete-driven compaction must not relocate
                // the leaf for rows that already live in the bucket's own run, while a compaction
                // asked to make the span compact may.
                Err(LabeledOperationError::Store(
                    LaraOperationError::CollectAllocationOverflow,
                )) => {
                    if span_policy == SpanResolutionPolicy::Required {
                        let spill_rows = match self.edges.spill_mint(bucket.edge_spill_run()) {
                            Ok(run) => self.edges.spill_used_len(run),
                            Err(_) => 0,
                        };
                        self.rebalance_vertex_edge_span(vid, Some(bucket_index), spill_rows, true)?;
                        return self.compact_vertex_edge_span_one_step_with_span_policy(
                            vid,
                            0,
                            0,
                            policy_for_label,
                            span_policy,
                        );
                    }
                    // A non-forcing pass must not relocate the leaf for rows that already live in
                    // the bucket's own run, so the span-side fold can never make room here and the
                    // run would keep its tombstones for good. Rebuilding the run is the only path
                    // that reclaims them without touching the span or the leaf, and its slot
                    // rewrites are reported exactly like the fold's.
                    //
                    // Rebuild copies every live row into a fresh run, so it runs on the slab
                    // hysteresis (`dead > used / 2`): below that, one delete would repack a whole
                    // run to reclaim a row or two and the pass would cost more than it frees.
                    let used = self.bucket_edge_spill_rows(bucket);
                    let dead = self.bucket_edge_spill_dead_rows(bucket)?;
                    if used > 0 && dead > used / 2 {
                        let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                        let moves = self.rebuild_bucket_edge_spill(bucket_slot, bucket)?;
                        // An empty batch means every hole sat at the tail, so the run is already
                        // packed: returning it would reset the pass to bucket 0 forever. Fall
                        // through to the ordinal-retaining skip below instead.
                        if !moves.is_empty() {
                            return Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(moves));
                        }
                    }
                    // Run-backed buckets are skipped by the move finder and retain their
                    // prefix/run ordinals. Completion may pack physical regions, not rows.
                }
                Err(error) => return Err(error),
            }
        }
        if resume_bucket_index >= vertex.degree() {
            // All logical moves must already have been returned by the bounded steps.
            // Finished has no movement channel, even when a non-forcing fold declined.
            self.pack_vertex_edge_span_layout(vid)?;
            return Ok(VertexEdgeSpanCompactOneStep::Finished);
        }
        let bucket_index = resume_bucket_index;
        let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        let bucket = self
            .buckets
            .read_label_bucket_slot(slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        // Whole-span work can reach any sibling, regardless of the triggering label.
        // Admit slab logical compaction once, before selection, fallback and finalization:
        // tiny rows are descriptor-resident; tree stored_slots is logical LTB extent,
        // while only root ids occupy the edge slab. Both retain their native ordinals.
        match BucketMode::from_bucket(&bucket) {
            BucketMode::Tiny | BucketMode::Tree => {
                return Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(
                    bucket_index.saturating_add(1),
                ));
            }
            BucketMode::Slab => {}
        }
        let policy = policy_for_label(bucket.bucket_label_key());
        let unordered_swap = policy == EdgePlacementPolicy::Unordered
            && Self::slab_bucket_allows_unordered_swap(&bucket);
        let mut moved =
            self.first_slab_compaction_move_in_bucket(&bucket, unordered_swap, resume_slot_index)?;
        if moved.is_none() && resume_slot_index != 0 {
            // The cursor-based scan found no out-of-position live edge, but an interleaved
            // delete may have tombstoned a slot inside the already-packed prefix. Re-scan
            // from slot zero before finalizing the bucket so a stranded live edge is never
            // truncated by `stored_slots = degree`.
            moved = self.first_slab_compaction_move_in_bucket(&bucket, unordered_swap, 0)?;
        }
        if let Some(moved) = moved {
            return Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(moved));
        }
        let finalized = Self::finalize_bucket_slab_metadata(bucket);
        let finalized = if finalized != bucket {
            self.publish_bucket_descriptor(slot, finalized)?;
            finalized
        } else {
            finalized
        };
        // The packed bucket may leave room at the end of its own region; spilled rows belong back
        // in the span there (ADR 0097: the edge fold lost its implementation with the overflow log).
        if let Ok(Some(arena)) = super::leaf_arena::LeafArena::capture(self, vid) {
            let content_end = finalized
                .edge_start()
                .saturating_add(u64::from(finalized.stored_slots()));
            let region_end = arena
                .leaf_slice(vid)
                .regions()
                .into_iter()
                .find(|(start, _)| *start == finalized.edge_start())
                .map(|(_, end)| end);
            if let Some(region_end) = region_end {
                // The region may extend over the next leaf-mate's edge start, because the slice is
                // derived from the regions that exist (ADR 0098). Rows folded past that point would
                // take the space the mate's own rows must come back to, so the room is measured
                // against whichever comes first.
                let limit = self
                    .next_leaf_vertex_edge_start(vid)?
                    .map_or(region_end, |next| region_end.min(next));
                let free = u32::try_from(limit.saturating_sub(content_end)).unwrap_or(u32::MAX);
                if free > 0 {
                    self.fold_bucket_edge_spill_head_into_span(vid, slot, &finalized, free)?;
                }
            }
        }
        Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(
            bucket_index.saturating_add(1),
        ))
    }

    /// Finds and applies one move inside an admitted slab bucket: unordered swap when
    /// permitted, otherwise order-preserving left-pack, starting at the compaction cursor.
    fn first_slab_compaction_move_in_bucket(
        &self,
        bucket: &LabelBucket,
        unordered_swap: bool,
        resume_slot_index: u32,
    ) -> Result<Option<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if unordered_swap {
            if let Some(swap) =
                Self::first_edge_slot_swap_move_in_bucket(bucket, &self.edges, resume_slot_index)?
            {
                self.apply_edge_slot_swap_move_in_bucket(bucket, &swap)?;
                return Ok(Some(EdgeSlotMove {
                    label_id: swap.label_id,
                    old_slot_index: swap.vacated_slot,
                    new_slot_index: swap.hole_slot,
                }));
            }
            Ok(None)
        } else if let Some(moved) =
            Self::first_edge_slot_move_in_bucket(bucket, &self.edges, resume_slot_index)?
        {
            self.apply_edge_slot_move_in_bucket(bucket, moved)?;
            Ok(Some(moved))
        } else {
            Ok(None)
        }
    }

    #[cfg(test)]
    pub(super) fn calculate_label_edge_span_positions(
        start_slot: u64,
        span_slots: u32,
        buckets: &[LabelBucket],
        preferred: Option<usize>,
        preferred_extra: u32,
    ) -> Result<Vec<u64>, LabeledOperationError> {
        Self::calculate_label_edge_span_positions_by_resident_slots(
            start_slot,
            span_slots,
            buckets,
            preferred,
            preferred_extra,
        )
    }

    pub(super) fn calculate_label_edge_span_positions_by_resident_slots(
        start_slot: u64,
        span_slots: u32,
        buckets: &[LabelBucket],
        preferred: Option<usize>,
        preferred_extra: u32,
    ) -> Result<Vec<u64>, LabeledOperationError> {
        let mut out = Vec::new();
        out.try_reserve_exact(buckets.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        if buckets.is_empty() {
            return Ok(out);
        }

        let mut effective_live = 0u64;
        let mut total_weight = buckets.len() as u64;
        for (index, bucket) in buckets.iter().enumerate() {
            let extra = if preferred == Some(index) {
                preferred_extra
            } else {
                0
            };
            // ADR 0096 §5: tiny buckets occupy zero leaf slots — the position
            // calculator packs them at the running boundary — so they must not
            // contribute a resident term. `stored_slots()` returns `degree` for tiny
            // after K=4, and counting it made this tiling demand one slot more than
            // the planner reserved (`span_slots=80 < effective_live=81`), which the
            // rewrite tests surfaced as `CollectAllocationOverflow`
            // (GAP-2026-09-20-005). Non-tiny buckets take the resident-region SSOT:
            // slab takes its materialized width, tree its root region alone (its
            // `stored_slots` is a logical count, not a readable span).
            //
            // The slab case used to carry a `.max(degree)` floor, from the world where a
            // rewrite folded a bucket's overflow rows into its prefix: the tiling then had
            // to cover `degree`, because the fold put every live row inside the span. ADR
            // 0097 §7 gives those rows their own owner — the run, in the arena — and the
            // materialized width already is the SSOT for what the span must hold: the
            // prefix of a relocation, the packed live rows (`degree`) of a compaction.
            // Keeping the floor made a run-backed bucket demand a span its prefix did not
            // need (`span_slots=64 < effective_live=65`), which trapped the non-tail bypass
            // insert (GAP-2026-09-20-007).
            let resident = bucket_content_slots(bucket)
                .checked_add(extra)
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
            // Keep the validated physical width for the placement pass. The output
            // entries become starts below; admission and advancement cannot disagree.
            out.push(u64::from(resident));
            effective_live = effective_live
                .checked_add(u64::from(resident))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            total_weight = total_weight
                .checked_add(u64::from(bucket.degree()).saturating_add(1))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        let gaps = u64::from(span_slots)
            .checked_sub(effective_live)
            .ok_or_else(|| {
                log_collect_overflow(&format!(
                    "calculate_label_edge_span_positions_by_resident_slots: span_slots={span_slots} < effective_live={effective_live}"
                ));
                LaraOperationError::CollectAllocationOverflow
            })?;

        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .checked_div(tw)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
        };

        let mut cursor_fp = u128::from(start_slot)
            .checked_mul(P)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        // Place precisely the physical widths admitted above, not degree or run rows.
        let mut leading_tiny = true;
        for (bucket, width_or_start) in buckets.iter().zip(out.iter_mut()) {
            let resident = u128::from(*width_or_start);
            let start = u64::try_from(cursor_fp / P)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            *width_or_start = start;
            // Leading anchors own no slots or gap before the first resident origin.
            if leading_tiny && bucket.is_tiny_mode() {
                continue;
            }
            leading_tiny = false;
            let start_fp = u128::from(start)
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            cursor_fp = start_fp
                .checked_add(
                    resident
                        .checked_mul(P)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                )
                .and_then(|cursor| {
                    let weight = u128::from(bucket.degree()).checked_add(1)?;
                    let gap = step_fp.checked_mul(weight)?;
                    cursor.checked_add(gap)
                })
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(out)
    }

    pub(super) fn rebalance_vertex_edge_span(
        &self,
        src: VertexId,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        force_slack_grow: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        // The rebalance is a rewrite: it plans the span, materializes every bucket's
        // resident content and publishes it through the shared pair. It used to carry its
        // own snapshot, position, publish and vertex-cover code — the last duplicate of the
        // layout mechanics (GAP-2026-09-20-005).
        //
        // Its own policy stays explicit: physical publication preserves tombstones and
        // run ownership, the caller's release decision is
        // preserved (the rewrite's finalize block runs the same code this function had) and
        // base resolution accepts the maintenance policy (keep the current span when the
        // requested one cannot be placed).
        LABELED_REBALANCE_LEAF_RELOCATED.with(|flag| flag.set(false));
        self.rewrite_vertex_edge_span_with_policy(
            src,
            preferred_bucket,
            preferred_extra,
            force_slack_grow,
            None,
            SpanResolutionPolicy::SlackMayBeDropped,
        )
    }

    /// Compacts all edge buckets for `vid` into slab-backed spans.
    pub(crate) fn compact_vertex_edge_span(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.compact_vertex_edge_span_with_moves(vid, bucket_index)
            .map(|_| ())
    }

    /// Compacts all edge buckets for `vid` and returns the slab-slot moves performed.
    pub(crate) fn compact_vertex_edge_span_with_moves(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let _ = bucket_index;
        let mut moves = Vec::new();
        let mut resume = 0u32;
        let mut resume_slot = 0u32;
        loop {
            match self.compact_vertex_edge_span_one_step(vid, resume, resume_slot, &|_| {
                EdgePlacementPolicy::Insertion
            })? {
                VertexEdgeSpanCompactOneStep::EdgeMoved(moved) => {
                    moves.push(moved);
                    resume_slot = moved.new_slot_index.saturating_add(1);
                }
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => {
                    resume = next;
                    resume_slot = 0;
                }
                VertexEdgeSpanCompactOneStep::OverflowRewrite(batch) => {
                    moves.extend(batch);
                    resume = 0;
                    resume_slot = 0;
                }
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        Ok(moves)
    }

    /// `true` when any label bucket on `vid` has enough slab tombstones to benefit from
    /// deferred edge-span compaction.
    pub(crate) fn vertex_has_slab_tombstone_slack_pressure(
        &self,
        vid: VertexId,
    ) -> Result<bool, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        Ok(buckets.iter().any(|bucket| {
            bucket.stored_slots().saturating_sub(bucket.degree())
                >= self.edges.header().segment_size.max(1)
        }))
    }

    pub(super) fn maintain_vertex_edge_span_light(
        &self,
        vid: VertexId,
        allow_slack_grow: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        self.compact_vertex_edge_span(vid, 0)?;
        if !allow_slack_grow {
            return Ok(());
        }
        let vertex = self.vertices.get(vid);
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let total_live = buckets.iter().try_fold(0u32, |acc, bucket| {
            acc.checked_add(bucket.degree())
                .ok_or(LaraOperationError::RowDegreeOverflow)
        })?;
        if vertex.stored_slots > total_live.saturating_add(1) {
            self.rewrite_vertex_edge_span(vid, None, 0, true, None)?;
        }
        Ok(())
    }

    pub(super) fn rebalance_vertex_edge_span_light(
        &self,
        vid: VertexId,
        allow_slack_grow: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        if allow_slack_grow {
            self.rebalance_vertex_edge_span(vid, None, 0, true)
        } else {
            self.rebalance_vertex_edge_span(vid, None, 0, false)
        }
    }

    pub(super) fn fold_label_bucket_inline_property_bytes_log_to_slab(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        // ADR 0096 §4b capacity contract: value-log fold never serves tiny
        // buckets (no log, no values). R2b fold planning skips them; this pins
        // the rule for any future caller. R2b upgrades to a typed guard if a
        // dispatcher bug could route tiny here.
        debug_assert!(
            !bucket.is_tiny_mode(),
            "tiny bucket must not reach value-log fold"
        );
        if bucket.values_run().is_none() || !bucket.is_inline_property_bytes_allocated() {
            return Ok(bucket);
        }
        let run = bucket.values_run().ok_or_else(|| {
            LabeledOperationError::from(LaraOperationError::CollectAllocationOverflow)
        })?;

        let mut saved = Vec::with_capacity(bucket.degree() as usize);
        for ordinal in 0..bucket.degree() {
            saved.push(self.read_bucket_inline_property_bytes_for_slot(&bucket, ordinal, None)?);
        }
        // The rows are about to move into the slab, so the descriptor stops pointing at the run
        // *before* admission: `needed_slots` must cover every row the slab will hold.
        let mut bucket = bucket.without_values_spill_run();
        bucket = self.ensure_bucket_inline_property_bytes_span(src, bucket_slot, bucket)?;
        let width = bucket.inline_property_byte_width();
        for (slot_index, bytes) in saved.iter().enumerate() {
            let slot_index = u32::try_from(slot_index)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let offset = super::super::invariants::inline_property_bytes_byte_offset_at_slot(
                &bucket, slot_index,
            )?;
            self.values
                .write_inline_property_bytes_slot(offset, width, bytes)
                .map_err(LabeledOperationError::from)?;
        }
        // The fold copied the run's rows into the slab, so the run is released and the
        // descriptor stops pointing at it (ADR 0097 §5: compaction is the only place a run is
        // freed besides bucket death).
        self.values
            .spill_release(run)
            .map_err(LabeledOperationError::from)?;
        self.publish_bucket_descriptor(bucket_slot, bucket)?;
        Ok(bucket)
    }

    pub(super) fn rebuild_bucket_edge_spill(
        &self,
        bucket_slot: u64,
        bucket: &LabelBucket,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let Ok(run) = self.edges.spill_mint(bucket.edge_spill_run()) else {
            return Ok(Vec::new());
        };
        let used = self.edges.spill_used_len(run);
        let mut keep = Vec::with_capacity(used as usize);
        for index in 0..used {
            let edge = self
                .edges
                .spill_edge_at(run, index)
                .map_err(LabeledOperationError::from)?;
            if !edge.is_tombstone_edge() {
                keep.push(index);
            }
        }
        if keep.len() as u32 == used {
            return Ok(Vec::new());
        }
        let prefix = bucket_content_slots(bucket);
        if keep.is_empty() {
            // Nothing live: drop the run entirely and let the descriptor say so.
            self.publish_bucket_descriptor(bucket_slot, bucket.without_edge_spill_run())?;
            self.edges
                .spill_release(run)
                .map_err(LabeledOperationError::from)?;
            return Ok(Vec::new());
        }
        // ADR 0099: the values plane mirrors the edge plane, so the same `keep` repacks the value
        // run at the same indices. Both new ids are published in one descriptor write, and only
        // then are the old runs released - the descriptor never names a freed run, and never names
        // one plane's new run beside the other plane's old one.
        let fresh = self
            .edges
            .spill_repack(run, &keep)
            .map_err(LabeledOperationError::from)?;
        let fresh_values = match bucket.values_run() {
            None => None,
            Some(old) => Some((
                old,
                self.values
                    .spill_repack(old, &keep)
                    .map_err(LabeledOperationError::from)?,
            )),
        };
        let published = match &fresh_values {
            Some((_, new_values)) => bucket
                .with_edge_run(Some(fresh))
                .map_err(LabeledOperationError::from)?
                .with_values_run(Some(*new_values))
                .map_err(LabeledOperationError::from)?,
            None => bucket
                .with_edge_run(Some(fresh))
                .map_err(LabeledOperationError::from)?,
        };
        self.publish_bucket_descriptor(bucket_slot, published)?;
        self.edges
            .spill_release(run)
            .map_err(LabeledOperationError::from)?;
        if let Some((old_values, _)) = fresh_values {
            self.values
                .spill_release(old_values)
                .map_err(LabeledOperationError::from)?;
        }
        let mut moves = Vec::new();
        for (destination, source) in keep.iter().copied().enumerate() {
            if destination as u32 != source {
                moves.push(EdgeSlotMove {
                    label_id: bucket.bucket_label_key(),
                    old_slot_index: prefix + source,
                    new_slot_index: prefix + destination as u32,
                });
            }
        }
        Ok(moves)
    }

    /// Tombstones inside the bucket's edge-plane spill run.
    ///
    /// The run header counts `used` rows including tombstones, and a tombstone is only visible by
    /// reading the row, so this is one scan. It exists so a caller can decide whether a rebuild is
    /// worth its repack before paying for one: [`Self::rebuild_bucket_edge_spill`] copies every
    /// live row into a fresh run, which only pays for itself once the dead rows dominate.
    fn bucket_edge_spill_dead_rows(
        &self,
        bucket: &LabelBucket,
    ) -> Result<u32, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let Ok(run) = self.edges.spill_mint(bucket.edge_spill_run()) else {
            // Unset (NO_SPILL_RUN) or outside the arena: the bucket has no rows beyond its prefix.
            return Ok(0);
        };
        let used = self.edges.spill_used_len(run);
        let mut dead = 0u32;
        for index in 0..used {
            let edge = self
                .edges
                .spill_edge_at(run, index)
                .map_err(LabeledOperationError::from)?;
            if edge.is_tombstone_edge() {
                dead = dead.saturating_add(1);
            }
        }
        Ok(dead)
    }

    /// Moves the head of a bucket's edge spill run into its slab span.
    ///
    /// A spilled row is one the leaf could not give the span a slot for, and it stays spilled
    /// because the edge fold was deleted with the shared overflow log (ADR 0097). When the span
    /// has room again - its region's own gap after a left-pack - the lowest spill rows belong back
    /// in the span, where a read finds them without touching the run.
    ///
    /// No logical ordinal moves: logical row `i` is the span slot for `i < stored_slots` and spill
    /// row `i - stored_slots` otherwise, so growing `stored_slots` by `k` re-homes exactly the `k`
    /// rows removed from the front of the run. That is why this needs no move observer and no
    /// rekey, unlike a rebuild.
    ///
    /// Order matters: write the span, drop the run's head, then publish `stored_slots`. A failure
    /// before the publish leaves the old descriptor naming the spill, so the rows stay readable and
    /// the written span bytes are simply not visible yet. `max_rows` is the caller's capacity
    /// bound; the drop is also capped at the run's class floor, because a class change is the
    /// rebuild's job.
    pub(super) fn fold_bucket_edge_spill_head_into_span(
        &self,
        src: VertexId,
        bucket_slot: u64,
        bucket: &LabelBucket,
        max_rows: u32,
    ) -> Result<u32, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let run = match self.edges.spill_mint(bucket.edge_spill_run()) {
            Ok(run) => run,
            Err(_) => return Ok(0),
        };
        let Some((start, len)) = bucket_content_region(bucket) else {
            return Ok(0);
        };
        let fold = max_rows
            .min(self.edges.spill_used_len(run))
            .min(self.edges.spill_head_drop_capacity(run));
        if fold == 0 {
            return Ok(0);
        }
        let mut rows = vec![0u8; fold as usize * E::BYTES];
        for index in 0..fold {
            let offset = index as usize * E::BYTES;
            let mut row = vec![0u8; E::BYTES];
            self.edges
                .spill_read_row(run, index, &mut row)
                .map_err(LabeledOperationError::from)?;
            rows[offset..offset + E::BYTES].copy_from_slice(&row);
        }
        self.edges
            .write_slots_contiguous(start.saturating_add(u64::from(len)), &rows)
            .map_err(LabeledOperationError::from)?;
        if let Err(error) = self.edges.spill_drop_head(run, fold) {
            return Err(LabeledOperationError::from(error));
        }
        let grown = len
            .checked_add(fold)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let grown_bucket = bucket.with_stored_slots(grown);
        // ADR 0099: the values plane mirrors the edge plane, so the rows that just moved into the
        // span bring their value blocks with them. The move *is* the plane's operation (it grows the
        // prefix it fills), so it goes through the view rather than being spelled out here.
        if grown_bucket.inline_property_byte_width() != 0 && bucket.values_run().is_some() {
            let published = self
                .value_plane(bucket_slot, *bucket)
                .fold_head_into_span(src, len, fold)?
                .bucket();
            self.publish_bucket_descriptor(bucket_slot, published)?;
            return Ok(fold);
        }
        self.publish_bucket_descriptor(bucket_slot, grown_bucket)?;
        Ok(fold)
    }

    /// Plan 0341 (GAP-2026-09-07-001 bypass side): left-pack a tombstoned
    /// default-label bypass row in place.
    ///
    /// The bypass row is a vertex-level span `[base_slot_start, base_slot_start + stored_slots)`
    /// (bypass-origin geometry contract, design/storage/lara.md). This step shifts the live slots
    /// down to the front of the row (preserving insertion order), tombstone-fills the vacated tail,
    /// then publishes `stored_slots = degree` in ONE atomic `vertices.set`. An in-row shift never
    /// moves another row's origin (contract point (a)); shrinking `stored_slots` keeps the freed gap
    /// as PMA slack (contract point (b)); the single atomic publish satisfies the descending-scan
    /// extent contract (contract point (c)).
    ///
    /// v1 boundary: compacts the slab prefix only. Overflow-log-backed rows
    /// (`bypass_overflow_log_head >= 0`) are deferred to the existing fold mechanism and return
    /// `Deferred` (the admission trigger only enqueues slab-prefix-only rows; this is a defensive
    /// no-op).
    pub(crate) fn compact_default_bypass_row(
        &self,
        vid: VertexId,
    ) -> Result<BypassRowCompactOutcome, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(vid)?;
        let vertex = self.vertices.get(vid);
        if !vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(BypassRowCompactOutcome::Deferred);
        }
        // v1 boundary: slab-prefix-only rows. Overflow-log-backed rows defer to the fold mechanism.
        if vertex.bypass_spill_run() != crate::traits::NO_SPILL_RUN {
            return Ok(BypassRowCompactOutcome::Deferred);
        }
        let base = vertex.base_slot_start();
        let stored = vertex.stored_slots;
        let degree = vertex.degree;
        if stored <= degree {
            return Ok(BypassRowCompactOutcome::Deferred);
        }
        // Read the whole slab prefix contiguously (the row is a contiguous slab prefix by
        // construction). The buffer is `stored * E::BYTES` bytes; for the bench scale this is
        // small, and a resumable/stepped variant for very large rows is a later-slice concern.
        let mut buf = vec![0u8; (stored as usize) * E::BYTES];
        self.edges.read_slots_contiguous(base, &mut buf);
        // Left-pack: move live slots to the front, preserving insertion order.
        let mut write = 0usize;
        for read in 0..stored as usize {
            let off = read * E::BYTES;
            let edge = E::read_from(&buf[off..off + E::BYTES]);
            if edge.is_tombstone_edge() {
                continue;
            }
            if write != read {
                let woff = write * E::BYTES;
                buf.copy_within(off..off + E::BYTES, woff);
            }
            write += 1;
        }
        debug_assert_eq!(write, degree as usize, "live slot count must equal degree");
        // Tombstone-fill the vacated tail slots (beyond the new extent).
        let mut tomb_bytes = vec![0u8; E::BYTES];
        E::tombstone_edge().write_to(&mut tomb_bytes);
        for slot in write..stored as usize {
            let off = slot * E::BYTES;
            buf[off..off + E::BYTES].copy_from_slice(&tomb_bytes);
        }
        // Write the compacted buffer back, then publish the new extent atomically.
        self.edges
            .write_slots_contiguous(base, &buf)
            .map_err(LabeledOperationError::from)?;
        let new_vertex = vertex.with_stored_slots(degree);
        self.set_labeled_vertex(vid, new_vertex)?;
        Ok(BypassRowCompactOutcome::Compacted)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use super::{
        labeled_leaf_physical_release_calls, labeled_vertex_footprint_release_calls,
        reset_labeled_leaf_release_test_metrics,
    };
    use crate::VertexId;
    use std::ops::ControlFlow;

    thread_local! {
        static LEFT_PACK_DECODES: std::cell::RefCell<Vec<u32>> = const {
            std::cell::RefCell::new(Vec::new())
        };
    }

    // Only EdgeStore and the static selector use these two widths, not a new graph fixture.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct LeftPackRow<const N: usize>(u32);

    impl<const N: usize> crate::traits::CsrEdge for LeftPackRow<N> {
        const BYTES: usize = N;
        fn read_from(bytes: &[u8]) -> Self {
            let target = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            LEFT_PACK_DECODES.with(|decoded| decoded.borrow_mut().push(target));
            Self(target)
        }
        fn write_to(&self, bytes: &mut [u8]) {
            bytes.fill(0xa5);
            bytes[..4].copy_from_slice(&self.0.to_le_bytes());
        }
        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.0)
        }
        fn with_neighbor_vid(&self, vid: VertexId) -> Self {
            Self(u32::from(vid))
        }
    }

    impl<const N: usize> crate::traits::CsrEdgeTombstone for LeftPackRow<N> {
        fn tombstone_edge() -> Self {
            Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
        }
    }

    #[derive(Clone, Default)]
    struct LeftPackMemory {
        inner: crate::VectorMemory,
        reads: std::rc::Rc<std::cell::RefCell<Vec<(u64, usize)>>>,
        allowed: std::rc::Rc<std::cell::RefCell<Option<std::ops::Range<u64>>>>,
    }

    impl ic_stable_structures::Memory for LeftPackMemory {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn grow(&self, pages: u64) -> i64 {
            self.inner.grow(pages)
        }
        fn read(&self, offset: u64, bytes: &mut [u8]) {
            if let Some(range) = self.allowed.borrow().as_ref() {
                assert!(offset >= range.start, "read before cursor");
                assert!(
                    offset + bytes.len() as u64 <= range.end,
                    "read past initialized prefix"
                );
            }
            self.reads.borrow_mut().push((offset, bytes.len()));
            self.inner.read(offset, bytes);
        }
        fn write(&self, offset: u64, bytes: &[u8]) {
            self.inner.write(offset, bytes);
        }
    }

    fn left_pack_bounded_selector<const N: usize>() {
        use crate::{lara::edge::EdgeStore, traits::CsrEdgeTombstone};
        let memories: [LeftPackMemory; 6] = std::array::from_fn(|_| LeftPackMemory::default());
        let store = EdgeStore::<LeftPackRow<N>, _>::new(
            memories[0].clone(),
            memories[1].clone(),
            memories[2].clone(),
            memories[3].clone(),
            memories[4].clone(),
            memories[5].clone(),
            41,
            1,
            0,
        )
        .unwrap();
        let slab = &memories[1];
        let label = BucketLabelKey::from_raw(2);
        const BASE: u64 = 3;
        const LEN: u32 = 35;
        // Real bytes immediately before/after the declared prefix belong to neighbors.
        for slot in 0..41 {
            store
                .write_slot(slot, LeftPackRow::<N>(9000 + slot as u32))
                .unwrap();
        }
        slab.reads.borrow_mut().clear();
        store.read_slot_bytes(BASE, &mut vec![0; N]);
        let base_byte = slab.reads.borrow()[0].0;
        let cases = [
            (LEN, vec![0, 1, 32], 0, Some((2, 0)), 3),
            (LEN, vec![32], 0, Some((33, 32)), 34),
            (LEN, vec![31, 32, 33], 0, Some((34, 31)), 35),
            (LEN, (5..=32).collect(), 5, Some((33, 5)), 29),
            (LEN, vec![0], 33, None, 2),
            (LEN, vec![32, 33, 34], 0, None, 35),
            (LEN, vec![0], 35, None, 0),
            (LEN, vec![0], 36, None, 0),
            (LEN, (0..LEN).collect(), 0, None, 0),
            (0, vec![], 0, None, 0),
        ];
        for (case, (stored, holes, cursor, movement, decoded_len)) in cases.into_iter().enumerate()
        {
            let rows: Vec<_> = (0..stored)
                .map(|slot| {
                    if holes.contains(&slot) {
                        LeftPackRow::<N>::tombstone_edge()
                    } else {
                        LeftPackRow::<N>(slot)
                    }
                })
                .collect();
            for (slot, row) in rows.iter().enumerate() {
                store.write_slot(BASE + slot as u64, *row).unwrap();
            }
            let bucket = LabelBucket::from_parts(label, BASE, stored - holes.len() as u32, stored);
            let before: Vec<_> = memories.iter().map(|m| m.inner.borrow().clone()).collect();
            let mut descriptor = [0; LabelBucket::BYTES];
            bucket.write_to(&mut descriptor);
            *slab.allowed.borrow_mut() = Some(
                base_byte + u64::from(cursor.min(stored)) * N as u64
                    ..base_byte + u64::from(stored) * N as u64,
            );
            for m in &memories {
                m.reads.borrow_mut().clear();
            }
            LEFT_PACK_DECODES.with(|decoded| decoded.borrow_mut().clear());
            let found = LabeledLaraGraph::<LeftPackRow<N>, _>::first_edge_slot_move_in_bucket(
                &bucket, &store, cursor,
            )
            .unwrap();
            assert_eq!(
                found,
                movement.map(|(old_slot_index, new_slot_index)| EdgeSlotMove {
                    label_id: label,
                    old_slot_index,
                    new_slot_index
                })
            );
            let expected: Vec<_> = rows
                .iter()
                .skip(cursor as usize)
                .take(decoded_len)
                .map(|row| row.0)
                .collect();
            LEFT_PACK_DECODES
                .with(|decoded| assert_eq!(*decoded.borrow(), expected, "N={N} case={case}"));
            let reads = slab.reads.borrow();
            for &(_, bytes) in reads.iter() {
                assert_eq!(bytes % N, 0);
                assert!(bytes <= 32 * N, "read must remain bounded");
            }
            if decoded_len == 0 {
                assert!(memories.iter().all(|m| m.reads.borrow().is_empty()));
            }
            eprintln!(
                "LEFT_PACK width={N} case={case} reads={} bytes={} decodes={decoded_len} movement={movement:?}",
                reads.len(),
                reads.iter().map(|(_, len)| len).sum::<usize>()
            );
            let mut after = [0; LabelBucket::BYTES];
            bucket.write_to(&mut after);
            assert_eq!(descriptor, after);
            for (m, expected) in memories.iter().zip(&before) {
                assert_eq!(
                    &*m.inner.borrow(),
                    expected,
                    "selector must be read-only, including neighbor bytes"
                );
            }
            *slab.allowed.borrow_mut() = None;
        }
        // Invalid first address: preserve the exact error without reading any store.
        for m in &memories {
            m.reads.borrow_mut().clear();
        }
        let invalid = LabelBucket::from_parts(label, crate::slab_index::SLOT_INDEX_MASK, 1, 2);
        let before: Vec<_> = memories.iter().map(|m| m.inner.borrow().clone()).collect();
        assert!(matches!(
            LabeledLaraGraph::<LeftPackRow<N>, _>::first_edge_slot_move_in_bucket(
                &invalid, &store, 1
            ),
            Err(LabeledOperationError::Store(
                LaraOperationError::CollectAllocationOverflow
            ))
        ));
        assert!(memories.iter().all(|m| m.reads.borrow().is_empty()));
        for (m, expected) in memories.iter().zip(&before) {
            assert_eq!(&*m.inner.borrow(), expected);
        }
    }

    #[test]
    fn left_pack_bounded_four_byte_selector() {
        left_pack_bounded_selector::<4>();
    }

    #[test]
    fn left_pack_bounded_ten_byte_selector() {
        left_pack_bounded_selector::<10>();
    }

    #[test]
    fn left_pack_bounded_caller_falls_back_and_preserves_neighbor() {
        let graph = test_graph();
        let src = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        let neighbor = BucketLabelKey::from_raw(3);
        for (key, targets) in [(label, 0..8), (neighbor, 9000..9005)] {
            for target in targets {
                graph
                    .insert_edge_skip_leaf_cascade(
                        src,
                        key,
                        TestEdge { target },
                        EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots(), 8);
        assert_eq!(bucket.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        let neighbor_slot = graph.find_bucket_slot(&vertex, neighbor).unwrap().unwrap();
        let neighbor_before = graph
            .buckets()
            .read_label_bucket_slot(neighbor_slot)
            .unwrap();
        let rows_before = graph.iter_edges_for_label(src, neighbor).unwrap();
        assert_eq!(
            graph.remove_edge_at_slot(src, label, 0).unwrap(),
            Some(TestEdge { target: 0 })
        );
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(
            LabeledLaraGraph::first_edge_slot_move_in_bucket(&bucket, graph.edges(), 6).unwrap(),
            None
        );
        assert_eq!(
            graph
                .compact_vertex_edge_span_one_step(src, 0, 6, &|_| EdgePlacementPolicy::Insertion)
                .unwrap(),
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: label,
                old_slot_index: 1,
                new_slot_index: 0
            })
        );
        let mut rows = Vec::new();
        assert_eq!(
            graph
                .visit_edges(src, label, OutEdgeOrder::Ascending, |slot, edge| {
                    rows.push((slot.raw(), edge.target));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap(),
            ControlFlow::Continue(())
        );
        assert_eq!(
            rows,
            [(0, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6), (7, 7)]
        );
        assert_eq!(
            graph
                .buckets()
                .read_label_bucket_slot(neighbor_slot)
                .unwrap(),
            neighbor_before
        );
        assert_eq!(
            graph.iter_edges_for_label(src, neighbor).unwrap(),
            rows_before
        );
        assert_eq!(graph.edges().header().num_edges, 12);
    }

    #[test]
    fn leaf_vertex_positions_use_resident_edge_slots_only() {
        // Label-bucket descriptors live in a separate store.  Leaf edge placement
        // must use the physical resident edge slots, including tombstone/log space,
        // rather than descriptor count as a second capacity measure.
        let resident_slots = [0, 1];
        let starts =
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::calculate_labeled_leaf_vertex_positions(
                0,
                &resident_slots,
                3,
            )
                .expect("vertex positions should fit the leaf");

        assert_eq!(starts, vec![0, 0]);
        let leaf_end = 4u64;
        assert_eq!(starts[1].saturating_sub(starts[0]), 0);
        assert!(leaf_end.saturating_sub(starts[1]) >= u64::from(resident_slots[1]));
    }

    /// ADR 0022 Stage 1 regression: a labeled bucket whose edge bytes live past the
    /// edge-slab leaf-0 physical block and which has spilled into the per-leaf overflow
    /// log must be readable in descending order.
    ///
    /// `LabelEdgeSpanAccess` exposes the bucket as a synthetic `v_ord == 0` row, so
    /// `slab_window_exclusive_end` would read leaf-0's physical cap. When the bucket
    /// base is past that cap, `next_base.min(cap)` placed the window end *before* the
    /// base and `on_slab_edges_with_layout` underflowed into `CollectAllocationOverflow`
    /// — the wedge that broke the 256x64 friends-of-friends build during the descending
    /// edge-handle lookup `insert_directed_edge` runs.
    #[test]
    fn overflow_log_bucket_past_leaf0_cap_reads_descending() {
        let default = BucketLabelKey::directed_from_index(1);
        let graph = test_graph_with_default(default);
        let label = BucketLabelKey::directed_from_index(5);

        // Need leaves 0, 1, 2 (segment_size == 32) plus destination vertices.
        const FAR_VID: u32 = 64; // first vertex of leaf 2
        const EDGES: u32 = 48; // > per-vertex leaf quota (32) so the overflow log activates
        for _ in 1..=(FAR_VID + EDGES) {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }

        // Pin leaf 0 and leaf 1 with labeled edges so their physical blocks sit before
        // leaf 2's block; this pushes the far vertex's edge_start past leaf-0's cap.
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                label,
                TestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(32),
                label,
                TestEdge { target: 2 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        // Fill the far vertex past its leaf quota so excess edges land in the per-leaf
        // overflow log (`log_head >= 0`) without relocating the block.
        for i in 0..EDGES {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(FAR_VID),
                    label,
                    TestEdge {
                        target: FAR_VID + i,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|e| panic!("far insert {i}: {e:?}"));
        }

        let mut descending = Vec::new();
        graph
            .visit_edges(
                VertexId::from(FAR_VID),
                label,
                OutEdgeOrder::Descending,
                |_slot, edge| {
                    descending.push(edge.target);
                    ControlFlow::<()>::Continue(())
                },
            )
            .map(|_| ())
            .expect("descending read over overflow-log bucket must succeed");
        assert_eq!(
            descending.len(),
            EDGES as usize,
            "descending read must yield every inserted edge"
        );

        let ascending = graph
            .iter_edges_for_label(VertexId::from(FAR_VID), label)
            .unwrap();
        assert_eq!(ascending.len(), EDGES as usize);
    }

    #[test]
    fn homogeneous_bypass_append_rejects_degree_overflow() {
        let default = BucketLabelKey::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = VertexId::from(0);
        graph.vertices().set(
            hub,
            &LabeledVertex::default()
                .with_homogeneous_bypass_label(default)
                .with_degree(u32::MAX),
        );

        let err = graph
            .insert_edge(
                hub,
                default,
                TestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect_err("max degree must be rejected");
        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
        assert_eq!(graph.vertices().get(hub).degree(), u32::MAX);
    }

    #[test]
    fn homogeneous_bypass_appends_stay_slab_only_without_overflow_log() {
        let default = BucketLabelKey::from_raw(7);
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(256),
            default,
        )
        .unwrap();
        let hub = graph
            .push_vertex(LabeledVertex::default().with_homogeneous_bypass_label(default))
            .unwrap();

        for target in 0..8u32 {
            graph
                .insert_edge(
                    hub,
                    default,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(hub);
        assert_eq!(vertex.bypass_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_eq!(vertex.degree(), 8);
        assert_eq!(graph.iter_edges_for_label(hub, default).unwrap().len(), 8);
    }

    #[test]
    fn push_vertex_rejects_normal_row_with_label_bucket_overflow() {
        let graph = test_graph();
        let vertex = LabeledVertex::default().with_degree(MAX_VERTEX_LABEL_BUCKETS + 1);
        let err = graph
            .push_vertex(vertex)
            .expect_err("push must reject normal rows over MAX_VERTEX_LABEL_BUCKETS");
        assert!(matches!(
            err,
            LabeledOperationError::InvalidVertexRow(
                LabeledVertexFieldError::LabelBucketCountOverflow
            )
        ));
    }

    #[test]
    fn homogeneous_bypass_region_end_rejects_slot_overflow() {
        let default = BucketLabelKey::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = VertexId::from(0);
        graph.vertices().set(
            hub,
            &LabeledVertex::default()
                .with_homogeneous_bypass_label(default)
                .with_base_slot_start(crate::labeled::slot_index::SLOT_INDEX_MASK)
                .with_degree(1)
                .with_stored_slots(1),
        );

        let err = graph
            .bypass_region_end(hub)
            .expect_err("bypass end overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn bucket_vertex_prefix_end_rejects_slot_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(
                    BucketLabelKey::from_raw(42),
                    crate::labeled::slot_index::SLOT_INDEX_MASK,
                    1,
                    1,
                ),
            )
            .unwrap();
        graph
            .vertices()
            .set(hub, &graph.vertices().get(hub).with_stored_slots(1));

        let err = graph
            .vertex_prefix_end(hub)
            .expect_err("bucket prefix end overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn last_bucket_successor_rejects_span_end_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        let label = BucketLabelKey::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(label, crate::labeled::slot_index::SLOT_INDEX_MASK, 0, 0),
            )
            .unwrap();
        graph
            .vertices()
            .set(hub, &graph.vertices().get(hub).with_stored_slots(1));

        let vertex = graph.vertices().get(hub);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        let err = graph
            .bucket_successor_start_after_bucket(&vertex, 0, &bucket)
            .expect_err("last bucket successor overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn contiguous_tiled_out_edges_rejects_span_end_overflow() {
        let vertex = LabeledVertex::default()
            .with_bucket_row(0, 1)
            .with_stored_slots(1);
        let buckets = [LabelBucket::from_parts(
            BucketLabelKey::from_raw(42),
            crate::labeled::slot_index::SLOT_INDEX_MASK,
            0,
            0,
        )];

        assert_eq!(
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::try_contiguous_tiled_labeled_out_edges(
                &vertex,
                &buckets,
            ),
            None
        );
    }

    #[test]
    fn normal_label_bucket_insert_rejects_edge_len_overflow() {
        let graph = test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let hub = VertexId::from(0);
        let label = BucketLabelKey::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(label, 0, u32::MAX, u32::MAX),
            )
            .unwrap();

        let err = graph
            .insert_edge(
                hub,
                label,
                TestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect_err("max bucket edge_len must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
        let vertex = graph.vertices().get(hub);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.stored_slots(), u32::MAX);
    }

    #[test]
    fn vertex_edge_span_rewrite_rejects_total_live_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(BucketLabelKey::from_raw(10), 0, u32::MAX, u32::MAX),
            )
            .unwrap();
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(BucketLabelKey::from_raw(20), 0, 1, 1),
            )
            .unwrap();

        let err = graph
            .compact_vertex_edge_span(hub, 0)
            .expect_err("bucket edge_len sum overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
    }

    #[test]
    fn rewrite_copy_byte_len_rejects_usize_overflow() {
        let oversized = usize::MAX / TestEdge::BYTES + 1;
        let err = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::edge_bytes_for_len(oversized)
            .expect_err("edge byte length overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn two_label_hub_500_then_173_parallel_edges() {
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(1 << 20),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let a = BucketLabelKey::from_raw(10_000);
        let b = BucketLabelKey::from_raw(10_001);
        for edge_i in 0..500u32 {
            graph
                .insert_edge(
                    hub,
                    a,
                    TestEdge {
                        target: u32::from(dst),
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|e| panic!("label_a edge_i={edge_i}: {e:?}"));
        }
        for edge_i in 0..174u32 {
            graph
                .insert_edge(
                    hub,
                    b,
                    TestEdge {
                        target: u32::from(dst),
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|e| panic!("label_b edge_i={edge_i}: {e:?}"));
        }
    }

    #[test]
    fn mixed_label_hub_50_labels_1000_edges_each() {
        build_mixed_label_hub(50, 1000);
    }

    /// Streaming overflow-log fold must not materialize the full slab width (skewed-noise shape).
    #[test]
    fn single_label_parallel_insert_survives_overflow_log_fold() {
        let started = std::time::Instant::now();
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(1 << 20),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(42_000);
        const EDGE_COUNT: u32 = if cfg!(debug_assertions) { 600 } else { 5_000 };
        for edge_i in 0..EDGE_COUNT {
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    label,
                    TestEdge {
                        target: u32::from(dst),
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|e| panic!("edge_i={edge_i}: {e:?}"));
        }
        assert_eq!(
            graph.iter_edges_for_label(hub, label).unwrap().len(),
            EDGE_COUNT as usize
        );
        assert_eq!(graph.out_edges(hub).unwrap().len(), EDGE_COUNT as usize);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "single-label parallel insert took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn mixed_label_hub_20_labels_500_edges_each() {
        build_mixed_label_hub(20, 500);
    }

    #[test]
    fn vertex_edge_span_retire_intervals_cover_interior_gaps_and_tail() {
        use crate::labeled::record::LabelBucket;
        let buckets = [
            LabelBucket::default()
                .with_bucket_label_key(BucketLabelKey::from_raw(1))
                .with_edge_range(100, 10),
            LabelBucket::default()
                .with_bucket_label_key(BucketLabelKey::from_raw(2))
                .with_edge_range(150, 20),
        ];
        let intervals =
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::vertex_edge_span_retire_intervals(
                100, 100, &buckets,
            )
            .unwrap();
        let total: u64 = intervals.iter().map(|(_, len)| *len).sum();
        assert_eq!(total, 100);
        assert!(intervals.contains(&(110, 40)));
        assert!(intervals.contains(&(170, 30)));
    }

    /// Phase E gate: same workload as the span-release regression completes quickly.
    #[test]
    fn labeled_hub_33_labels_bounded_insert_time() {
        let started = std::time::Instant::now();
        build_mixed_label_hub(33, 50);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "33-label hub insert took {:?}",
            started.elapsed()
        );
    }

    /// Regression: 33rd label on a dense hub used to spend minutes in span release.
    #[test]
    fn mixed_label_hub_33_labels_span_release_regression() {
        build_mixed_label_hub(33, 50);
    }

    #[test]
    fn mixed_label_hub_parallel_edges_do_not_corrupt_overflow_log() {
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(1 << 20),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..10u16 {
            let label = BucketLabelKey::from_raw(1000 + label_idx);
            for edge_i in 0..24u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(dst),
                        },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
            }
        }
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn find_bucket_resolves_non_default_label_after_parallel_inserts() {
        let graph = test_graph();
        let label = BucketLabelKey::from_raw(42);
        for target in 0..8u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert!(graph.find_bucket_slot(&vertex, label).unwrap().is_some());
    }

    #[test]
    fn parallel_catalog_edges_on_high_index_vertex_stay_on_slab() {
        let graph = test_graph();
        for _ in 0..64 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(42);
        for target in 0..240u32 {
            graph
                .insert_edge(
                    hub,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(hub);
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(graph.iter_edges_for_label(hub, label).unwrap().len(), 240);
    }

    #[test]
    fn catalog_label_parallel_inserts_use_single_bucket_row() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(42);
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 0 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let bucket_only = graph.vertices().get(VertexId::from(0));
        assert!(!bucket_only.is_default_edge_labeled());
        for target in 1..24u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            24
        );
        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(0)).unwrap(),
            vec![road]
        );
    }

    #[test]
    fn retained_compaction_finished_matches_every_reported_move() {
        use super::super::test_support::{retained_compaction_fixture, retained_compaction_rows};
        use std::collections::BTreeMap;
        for width in [8, 33] {
            let graph = retained_compaction_fixture(width);
            let vid = VertexId::from(0);
            let source = graph
                .read_vertex_label_buckets(&graph.vertices().get(vid))
                .unwrap()[0];
            let mut expected = BTreeMap::new();
            for raw in [2, 3] {
                for (slot, target, value) in retained_compaction_rows(
                    &graph,
                    vid,
                    BucketLabelKey::from_raw(raw),
                    OutEdgeOrder::Ascending,
                ) {
                    expected.insert((raw, slot), (target, value));
                }
            }
            let peer = retained_compaction_rows(
                &graph,
                VertexId::from(1),
                BucketLabelKey::from_raw(2),
                OutEdgeOrder::Ascending,
            );
            let (mut resume, mut cursor, mut finished, mut moves_seen) = (0, 0, false, 0);
            for _ in 0..32 {
                let outcome = graph
                    .compact_vertex_edge_span_one_step_with_span_policy(
                        vid,
                        resume,
                        cursor,
                        &|_| EdgePlacementPolicy::Insertion,
                        SpanResolutionPolicy::SlackMayBeDropped,
                    )
                    .unwrap();
                let moves = match outcome {
                    VertexEdgeSpanCompactOneStep::EdgeMoved(moved) => {
                        cursor = moved.new_slot_index + 1;
                        vec![moved]
                    }
                    VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => {
                        resume = next;
                        cursor = 0;
                        Vec::new()
                    }
                    VertexEdgeSpanCompactOneStep::OverflowRewrite(moves) => {
                        assert!(
                            moves.len() <= 9,
                            "only the bounded source run may move in a batch"
                        );
                        assert!(
                            moves
                                .iter()
                                .all(|m| m.old_slot_index >= source.stored_slots())
                        );
                        resume = 0;
                        cursor = 0;
                        moves
                    }
                    VertexEdgeSpanCompactOneStep::Finished => {
                        finished = true;
                        Vec::new()
                    }
                };
                moves_seen += moves.len();
                for moved in moves {
                    let value = expected
                        .remove(&(moved.label_id.raw(), moved.old_slot_index))
                        .unwrap();
                    assert!(
                        expected
                            .insert((moved.label_id.raw(), moved.new_slot_index), value)
                            .is_none()
                    );
                }
                for raw in [2, 3] {
                    let exact: Vec<_> = expected
                        .iter()
                        .filter(|((label, _), _)| *label == raw)
                        .map(|((_, slot), (target, value))| (*slot, *target, value.clone()))
                        .collect();
                    assert_eq!(
                        retained_compaction_rows(
                            &graph,
                            vid,
                            BucketLabelKey::from_raw(raw),
                            OutEdgeOrder::Ascending
                        ),
                        exact,
                        "event replay, including Finished; width={width}, finished={finished}"
                    );
                    assert_eq!(
                        retained_compaction_rows(
                            &graph,
                            vid,
                            BucketLabelKey::from_raw(raw),
                            OutEdgeOrder::Descending
                        ),
                        exact.into_iter().rev().collect::<Vec<_>>()
                    );
                }
                assert_eq!(
                    retained_compaction_rows(
                        &graph,
                        VertexId::from(1),
                        BucketLabelKey::from_raw(2),
                        OutEdgeOrder::Ascending
                    ),
                    peer
                );
                if finished {
                    break;
                }
            }
            assert!(
                finished,
                "a bounded pass must complete, not silently remain pending"
            );
            assert_eq!(
                moves_seen, 3,
                "slab-only neighboring label still uses single-edge moves"
            );
            let after = graph
                .read_vertex_label_buckets(&graph.vertices().get(vid))
                .unwrap()[0];
            assert_eq!(after.edge_spill_run(), source.edge_spill_run());
            assert_eq!(after.stored_slots(), source.stored_slots());
            assert_eq!(after.values_spill_run(), source.values_spill_run());
            assert_eq!(graph.edges().header().num_edges, 18);
        }
    }

    #[test]
    fn incremental_vertex_edge_span_compact_clears_many_tombstones() {
        use super::VertexEdgeSpanCompactOneStep;

        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        for target in 1..=60u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        for target in 1..=55u32 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket_before = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(
            bucket_before
                .stored_slots()
                .saturating_sub(bucket_before.degree),
            55
        );
        assert_eq!(bucket_before.degree(), 5);

        let mut resume = 0u32;
        let mut edge_moves = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume, 0, &|_| {
                    EdgePlacementPolicy::Insertion
                })
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => edge_moves += 1,
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_) => break,
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        let bucket_after = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(
            edge_moves > 0 || bucket_after.stored_slots() == bucket_after.degree,
            "expected in-bucket compaction progress"
        );
        assert_eq!(bucket_after.stored_slots(), bucket_after.degree);
        assert_eq!(bucket_after.degree, 5);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            (56..=60)
                .rev()
                .map(|target| TestEdge { target })
                .collect::<Vec<_>>()
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn incremental_vertex_edge_span_compact_preserves_other_label_bucket() {
        use super::VertexEdgeSpanCompactOneStep;

        let graph = test_graph();
        let anchor = BucketLabelKey::from_raw(99);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                anchor,
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 1..=20u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        for target in 1..=17u32 {
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let anchor_slot = graph.find_bucket_slot(&vertex, anchor).unwrap().unwrap();
        let road_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        assert_eq!(
            road_bucket
                .stored_slots()
                .saturating_sub(road_bucket.degree),
            17
        );

        let mut resume = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume, 0, &|_| {
                    EdgePlacementPolicy::Insertion
                })
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => {}
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_)
                | VertexEdgeSpanCompactOneStep::Finished => {
                    break;
                }
            }
        }

        let anchor_bucket = graph.buckets().read_label_bucket_slot(anchor_slot).unwrap();
        assert_eq!(anchor_bucket.stored_slots(), anchor_bucket.degree);
        assert_eq!(anchor_bucket.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), anchor)
                .unwrap(),
            vec![TestEdge { target: 999 }]
        );

        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        assert_eq!(road_bucket.stored_slots(), road_bucket.degree);
        assert_eq!(road_bucket.degree, 3);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![
                TestEdge { target: 20 },
                TestEdge { target: 19 },
                TestEdge { target: 18 },
            ]
        );
    }

    #[test]
    fn label_bucket_accumulates_many_slab_tombstones_before_compact() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        let total = 202u32;
        for target in 1..=total {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();

        for target in 1..=200 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots().saturating_sub(bucket.degree), 200);
        assert_eq!(bucket.degree(), 2);

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots(), bucket.degree);
        assert_eq!(bucket.degree, 2);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 202 }, TestEdge { target: 201 }]
        );
    }

    #[test]
    fn label_bucket_store_vertex_segment_compacts_across_thirty_two_vertices() {
        let graph = test_graph();
        for _ in 1..32 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(31),
                BucketLabelKey::from_raw(3),
                TestEdge { target: 20 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(4),
                TestEdge { target: 30 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let first = graph.vertices().get(VertexId::from(0));
        let last = graph.vertices().get(VertexId::from(31));
        assert_eq!(first.degree(), 2);
        assert_eq!(last.degree(), 1);
        assert!(
            last.base_slot_start()
                >= first.base_slot_start().saturating_add(u64::from(
                    first
                        .label_bucket_descriptor_span()
                        .expect("hub vertex is in bucket mode"),
                ))
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 10 }, TestEdge { target: 30 }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(31)).unwrap(),
            vec![TestEdge { target: 20 }]
        );
    }

    fn live_edge_count_in_leaf(
        graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        vid: VertexId,
    ) -> u32 {
        let header = graph.edges().header();
        let seg = header.segment_size.max(1);
        let leaf = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::leaf_index_for_vid(
            vid,
            header.segment_size,
        );
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(graph.vertices().len());
        let mut total = 0u32;
        for vidx in start_vid..end_vid {
            for (_, targets) in materialized_labeled_edges(graph, VertexId::from(vidx)) {
                total = total.saturating_add(targets.len() as u32);
            }
        }
        total
    }

    #[test]
    fn vertex_edge_span_rewrite_weights_slack_by_label_degree() {
        // Slack is only taken when the current window already hosts it (a request that needs a
        // leaf relocation is a hint, not a requirement — `force_slack_grow` is best effort), so
        // a test about how spare room is *distributed* must run somewhere spare room exists.
        // Slack is only taken when the current window hosts it (a request that would need a
        // leaf relocation is a hint, not a requirement), so a test about how spare room is
        // *distributed* must run where spare room exists: `test_graph`'s 256 slots cannot host
        // this shape.
        let graph: LabeledLaraGraph<TestEdge, crate::VectorMemory> = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(1 << 20),
            BucketLabelKey::from_raw(1),
        )
        .expect("graph");
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let hot = BucketLabelKey::from_raw(2);
        let cold = BucketLabelKey::from_raw(3);
        for target in 0..64u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    hot,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .insert_edge(
                VertexId::from(0),
                cold,
                TestEdge { target: 900 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .rebalance_vertex_edge_span(VertexId::from(0), None, 0, true)
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let hot_slot = graph.find_bucket_slot(&vertex, hot).unwrap().unwrap();
        let cold_slot = graph.find_bucket_slot(&vertex, cold).unwrap().unwrap();
        let hot_index = hot_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let cold_index = cold_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let hot_bucket = graph.buckets().read_label_bucket_slot(hot_slot).unwrap();
        let cold_bucket = graph.buckets().read_label_bucket_slot(cold_slot).unwrap();
        let hot_capacity = graph
            .bucket_successor_start(&vertex, hot_index)
            .unwrap()
            .saturating_sub(hot_bucket.edge_start());
        let cold_capacity = graph
            .bucket_successor_start(&vertex, cold_index)
            .unwrap()
            .saturating_sub(cold_bucket.edge_start());

        assert!(hot_capacity > cold_capacity);
        assert!(hot_capacity > u64::from(hot_bucket.stored_slots()));
        assert!(cold_capacity >= u64::from(cold_bucket.stored_slots()));
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn labeled_proportional_slack_by_label_degree_after_slide() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let hot = BucketLabelKey::from_raw(2);
        let cold = BucketLabelKey::from_raw(3);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 0..64u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    hot,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                cold,
                TestEdge { target: 900 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph.rebalance_labeled_leaf_weighted_slide(vid).unwrap();

        let vertex = graph.vertices().get(vid);
        let hot_slot = graph.find_bucket_slot(&vertex, hot).unwrap().unwrap();
        let cold_slot = graph.find_bucket_slot(&vertex, cold).unwrap().unwrap();
        let hot_index = hot_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let cold_index = cold_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let hot_bucket = graph.buckets().read_label_bucket_slot(hot_slot).unwrap();
        let cold_bucket = graph.buckets().read_label_bucket_slot(cold_slot).unwrap();
        let hot_capacity = graph
            .bucket_successor_start(&vertex, hot_index)
            .unwrap()
            .saturating_sub(hot_bucket.edge_start());
        let cold_capacity = graph
            .bucket_successor_start(&vertex, cold_index)
            .unwrap()
            .saturating_sub(cold_bucket.edge_start());
        assert!(hot_capacity > cold_capacity);
        assert!(hot_capacity > u64::from(hot_bucket.stored_slots()));
        assert!(cold_capacity >= u64::from(cold_bucket.stored_slots()));
        graph
            .assert_labeled_buckets_within_leaf_physical(vid)
            .unwrap();
    }

    #[test]
    fn labeled_leaf_rebalance_rewrites_all_bucket_starts_in_leaf() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        let neighbor = VertexId::from(1);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label_a = BucketLabelKey::from_raw(10);
        let label_b = BucketLabelKey::from_raw(11);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                label_a,
                TestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                neighbor,
                label_b,
                TestEdge { target: 0 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                road,
                TestEdge { target: 2 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let before_hub = materialized_labeled_edges(&graph, hub);
        let before_neighbor = materialized_labeled_edges(&graph, neighbor);
        graph.rebalance_labeled_leaf_weighted_slide(hub).unwrap();
        assert_eq!(materialized_labeled_edges(&graph, hub), before_hub);
        assert_eq!(
            materialized_labeled_edges(&graph, neighbor),
            before_neighbor
        );
        graph
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
        graph
            .assert_labeled_buckets_within_leaf_physical(neighbor)
            .unwrap();
    }

    #[test]
    fn labeled_leaf_weighted_slide_preserves_total_live_edges() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 0..80u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for target in 0..20u32 {
            graph
                .remove_edge_matching(vid, road, |edge| edge.target == target)
                .unwrap();
        }
        let before = live_edge_count_in_leaf(&graph, vid);
        graph.rebalance_labeled_leaf_weighted_slide(vid).unwrap();
        let after = live_edge_count_in_leaf(&graph, vid);
        assert_eq!(before, after);
        assert!(after >= 60);
    }

    #[test]
    fn labeled_leaf_rebalance_does_not_release_span() {
        reset_labeled_leaf_release_test_metrics();
        let leaf_releases_before = labeled_leaf_physical_release_calls();
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 0..96u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph.rebalance_labeled_leaf_weighted_slide(vid).unwrap();
        assert_eq!(
            labeled_leaf_physical_release_calls().saturating_sub(leaf_releases_before),
            0
        );
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0
        );
    }

    #[test]
    fn labeled_segment_relocate_releases_single_footprint() {
        use super::super::leaf_arena::labeled_leaf_physical_block_len;
        reset_labeled_leaf_release_test_metrics();
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap().unwrap();
        let leaf_releases_before = labeled_leaf_physical_release_calls();
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        graph.relocate_labeled_leaf_physical_block(vid).unwrap();
        assert_eq!(
            labeled_leaf_physical_release_calls().saturating_sub(leaf_releases_before),
            1
        );
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0
        );
        // The old footprint must be reclaimed; it may coalesce with adjacent
        // free spans (promotion/tail spans free and merge), so assert coverage
        // rather than an exact standalone entry (coverage still catches leaks).
        let old_end = old_start.saturating_add(old_len);
        assert!(
            graph
                .edges()
                .free_span_store()
                .spans()
                .iter()
                .any(|span| span.start_slot <= old_start
                    && span.start_slot.saturating_add(span.len) >= old_end),
            "relocated old block [{old_start}, {old_end}) must be covered by free spans"
        );
        let counts = graph.leaf_segment_counts_for_vid(vid);
        assert!(counts.total as u64 > old_len);
    }

    #[test]
    fn labeled_rewrite_within_pinned_leaf_does_not_release_vertex_footprint() {
        reset_labeled_leaf_release_test_metrics();
        let graph = test_graph();
        let src = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        // ADR 0096 §3b: four inline seeds plus an explicit promotion so the
        // leaf pins with a real slab span for the in-pinned rewrite below.
        for target in [1u32, 2, 3, 4] {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        crate::labeled::graph::test_support::promote_bucket_to_slab(&graph, src, road);
        assert!(graph.labeled_leaf_physical_range(src).unwrap().is_some());
        graph
            .remove_edge_at_slot(src, road, 0)
            .unwrap()
            .expect("removed");
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        graph
            .rewrite_vertex_edge_span(src, None, 1, true, None)
            .unwrap();
        graph.compact_vertex_edge_span(src, 0).unwrap();
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0,
            "pinned-leaf rewrite keeps interior slack inside the leaf block"
        );
        graph.rebalance_labeled_leaf_weighted_slide(src).unwrap();
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0,
            "pinned-leaf slide must not peel per-vertex footprints"
        );
    }

    #[test]
    fn labeled_segment_relocate_does_not_call_vertex_span_release() {
        use super::super::leaf_arena::labeled_leaf_physical_block_len;
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0
        );
    }

    #[test]
    fn labeled_relocate_commit_order() {
        use super::super::leaf_arena::labeled_leaf_physical_block_len;
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap().unwrap();
        graph.relocate_labeled_leaf_physical_block(vid).unwrap();
        graph
            .assert_labeled_buckets_within_leaf_physical(vid)
            .unwrap();
        let (new_start, new_len) = graph.labeled_leaf_physical_range(vid).unwrap().unwrap();
        assert_ne!(new_start, old_start);
        assert!(new_len > old_len);
        // Coverage, not exact entry: the old footprint may coalesce with
        // adjacent free spans (see releases_single_footprint).
        let old_end = old_start.saturating_add(old_len);
        assert!(
            graph
                .edges()
                .free_span_store()
                .spans()
                .iter()
                .any(|span| span.start_slot <= old_start
                    && span.start_slot.saturating_add(span.len) >= old_end),
            "relocated old block [{old_start}, {old_end}) must be covered by free spans"
        );
        assert_eq!(materialized_labeled_edges(&graph, vid).len(), 2);
    }

    #[test]
    fn labeled_relocation_preserves_all_leaf_vertices() {
        use super::super::leaf_arena::labeled_leaf_physical_block_len;

        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        for (vid, offset) in [(VertexId::from(0), 0u32), (VertexId::from(1), 1u32)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    BucketLabelKey::from_raw(99),
                    TestEdge {
                        target: 999 + offset,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            for target in 0..block_len {
                graph
                    .insert_edge_skip_leaf_cascade(
                        vid,
                        road,
                        TestEdge {
                            target: target as u32 + offset * 1000,
                        },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }
        let before = [
            materialized_labeled_edges(&graph, VertexId::from(0)),
            materialized_labeled_edges(&graph, VertexId::from(1)),
        ];
        graph
            .relocate_labeled_leaf_physical_block(VertexId::from(0))
            .unwrap();

        assert_eq!(
            materialized_labeled_edges(&graph, VertexId::from(0)),
            before[0]
        );
        assert_eq!(
            materialized_labeled_edges(&graph, VertexId::from(1)),
            before[1]
        );
    }

    #[test]
    fn labeled_segment_slide_coalesces_adjacent_free() {
        use super::super::leaf_arena::labeled_leaf_physical_block_len;

        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        // ADR 0096 §4: pin before promotions so spans land in-block (unpinned
        // cover-extend would tail-grow live spans into the would-be adjacent
        // free ranges, breaking the void-both-sides coalescing premise).
        graph
            .ensure_labeled_leaf_block_pinned(vid, 16)
            .expect("pin before promotions");
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap().unwrap();
        let left_len = 6u64;
        let right_len = 5u64;
        assert!(
            old_start >= left_len,
            "leaf pin must leave room for a left-adjacent free span"
        );
        let left_start = old_start.saturating_sub(left_len);
        let right_start = old_start.saturating_add(old_len);
        // ADR 0096 §4: pins land at the allocation tail, so the right-adjacent
        // range may lie past elem_capacity (unallocated void, not releasable).
        // Grow the tail first so both sides are in-bounds void, matching the
        // scattered-pin geometry this coalescing premise needs.
        graph
            .edges()
            .set_elem_capacity(right_start.saturating_add(right_len))
            .expect("grow tail for right-adjacent free span");
        // Release only ranges not already free (promotion/relocate churn may
        // have freed them; re-releasing errors). Coverage preserves intent.
        let covered = |start: u64, len: u64| {
            let end = start.saturating_add(len);
            graph.edges().free_span_store().spans().iter().any(|span| {
                span.start_slot <= start && span.start_slot.saturating_add(span.len) >= end
            })
        };
        if !covered(left_start, left_len) {
            graph.edges().release_span(left_start, left_len).unwrap();
        }
        if !covered(right_start, right_len) {
            graph.edges().release_span(right_start, right_len).unwrap();
        }

        graph.relocate_labeled_leaf_physical_block(vid).unwrap();

        // Coalescing may absorb pre-existing adjacent free spans (promotion
        // churn frees around the block), so assert a single covering span
        // rather than exact bounds (single-span-ness IS the coalescing).
        let right_end = right_start.saturating_add(right_len);
        let spans = graph.edges().free_span_store().spans();
        assert_eq!(count_free_spans(&graph), 1);
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0].start_slot <= left_start
                && spans[0].start_slot.saturating_add(spans[0].len) >= right_end,
            "coalesced span must cover [{left_start}, {right_end})"
        );
        let (new_start, _) = graph.labeled_leaf_physical_range(vid).unwrap().unwrap();
        assert_ne!(new_start, old_start);
    }

    #[test]
    fn labeled_segment_relocate_reuses_free_span() {
        use super::super::leaf_arena::labeled_leaf_physical_block_len;
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        // ADR 0096 §4: single-edge buckets are born tiny (pinless); the
        // relocation below needs a pinned slab span. Promote directly.
        {
            let vertex = graph.vertices.get(vid);
            let (slot, bucket) = match graph
                .find_bucket(vid, &vertex, BucketLabelKey::from_raw(99))
                .unwrap()
            {
                BucketSearch::Found { slot, bucket } => (slot, bucket),
                _ => panic!("bucket must exist for promotion"),
            };
            graph.promote_tiny_to_slab(vid, slot, &bucket).unwrap();
        }
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap().unwrap();
        let adjacent = old_start.saturating_add(old_len);
        graph.edges().release_span(adjacent, old_len).unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph.relocate_labeled_leaf_physical_block(vid).unwrap();
        let (new_start, new_len) = graph.labeled_leaf_physical_range(vid).unwrap().unwrap();
        assert_eq!(new_start, old_start);
        assert!(new_len > old_len);
        assert!(
            graph
                .edges()
                .free_span_store()
                .free_span_starting_at(adjacent)
                .is_none(),
            "in-place leaf grow should consume the adjacent free span"
        );
    }

    #[test]
    fn compact_vertex_edge_span_shrinks_vertex_edge_span() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let road = BucketLabelKey::from_raw(2);
        for target in 0..80u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        for target in 0..72u32 {
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                .unwrap();
        }

        let before = graph.vertices().get(VertexId::from(0));
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            8
        );
        assert!(before.stored_slots > 8);

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        // Shrink packs exactly (promotion headroom is reclaimed, not retained).
        assert_eq!(after.stored_slots, 8);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            8
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn fold_growth_stays_mate_disjoint_across_span_growth() {
        // GAP-2026-09-20-005. One lobe of the leaf is a hub: the hub promotes as
        // a tiny bucket into the small gap its mates left, fills that span, then
        // the per-leaf overflow log folds ~170 rows into the span. The fold's
        // "the layout is unchanged, so publish the wider cover" branch used to
        // check only that the wider range fitted inside the leaf *block* — the
        // mates sitting inside the range were ignored, so the hub's cover walked
        // over them (`assert_no_labeled_leaf_mate_overlap` was the only detector
        // and is debug-only, so release builds ran on with a broken tiling).
        //
        // Wrong-implementation probe: with the block-only check restored this
        // test panics in the audit guard ("reserves up to N but vid M starts at
        // K"); with the fix the fold relocates the leaf block instead.
        const DEGREES: [u32; 16] = [
            8, 8, 8, 8, 64, 64, 64, 64, 64, 64, 64, 64, 512, 512, 512, 2048,
        ];
        let graph = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::new_with_segment_size(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::directed_from_index(1),
            16,
        )
        .unwrap();
        let label = BucketLabelKey::from_raw(2);
        for _ in 0..16 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        for vid_u in 0..16u32 {
            let vid = VertexId::from(vid_u);
            for target in 0..DEGREES[vid_u as usize] {
                graph
                    .insert_edge(
                        vid,
                        label,
                        TestEdge { target },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }

        // Every edge survives, and the leaf tiling is still exact.
        for vid_u in 0..16u32 {
            let vid = VertexId::from(vid_u);
            assert_eq!(
                graph.out_edges(vid).unwrap().len(),
                DEGREES[vid_u as usize] as usize,
                "vid {vid_u} lost edges across the fold"
            );
            graph.assert_no_labeled_leaf_mate_overlap(vid);
        }
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
        // A dense leaf may not be able to hold the hub at all; in that case the
        // documented recursion breaker tail-appends the span *outside* every leaf
        // block (pre-existing fallback, tracked separately). What the fold must
        // never do is land the span half-inside a block or across a mate — the
        // straddle case is what corrupted the tiling.
        let hub = VertexId::from(15);
        // The cover is the hub's slice, not `first base + stored_slots`: the slice is
        // derived from the starts and cannot drift when the fold moves a resident.
        let arena = crate::labeled::graph::leaf_arena::LeafArena::capture(&graph, hub)
            .unwrap()
            .expect("hub leaf stays pinned");
        let (slice_start, slice_end) = arena.slice(hub).expect("hub owns a slice after the fold");
        let (extent_start, extent_len) = arena.extent();
        assert!(
            slice_start >= extent_start && slice_end <= extent_start + extent_len,
            "hub slice {slice_start}..{slice_end} must stay inside its extent {extent_start}..{}",
            extent_start + extent_len
        );
        for (region_start, region_end) in arena.leaf_slice(hub).regions() {
            assert!(
                region_start >= slice_start && region_end <= slice_end,
                "hub region {region_start}..{region_end} must stay inside its slice {slice_start}..{slice_end}"
            );
        }
    }

    #[test]
    fn compact_vertex_edge_span_uses_edge_tombstone_contract() {
        let graph = flag_tombstone_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        // Four inline seeds plus an explicit promotion: the delete below must
        // leave a slab tombstone (K=4 keeps four edges inline, and the inline
        // tombstone path needs no compaction move).
        for target in [10, 11, 12, 13] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    FlagTombstoneEdge::live(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        crate::labeled::graph::test_support::promote_bucket_to_slab(
            &graph,
            VertexId::from(0),
            road,
        );
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| {
                *edge == FlagTombstoneEdge::live(10)
            })
            .unwrap();

        let moved = graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0, 0, &|_| {
                EdgePlacementPolicy::Insertion
            })
            .unwrap();

        assert_eq!(
            moved,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: road,
                old_slot_index: 1,
                new_slot_index: 0,
            })
        );
    }

    #[test]
    fn overflow_direct_unlink_reports_bounded_moves_before_tombstone_free_fold() {
        let graph = inline_property_test_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::directed_from_index(2);
        // Four inline seeds plus an explicit promotion: the delete below must
        // leave a slab tombstone whose fold reports bounded moves (K=4 keeps
        // four edges inline, and the inline-tombstone path needs no fold move).
        for target in [10, 11, 12, 13] {
            graph
                .insert_edge(
                    hub,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &[]),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        crate::labeled::graph::test_support::promote_bucket_to_slab(&graph, hub, road);
        let removal = graph
            .remove_edge_matching_with_move(hub, road, |edge| edge.target == 10)
            .unwrap();
        // ADR 0096 §4: post-promotion the removed edge is slab-resident, so the
        // delete tombstones in place (no log-ordinal renumbering moves); the
        // bounded move surfaces at fold below instead of at unlink time.
        assert_eq!(removal.unwrap().moves, vec![]);

        let rewritten = graph
            .compact_vertex_edge_span_one_step(hub, 0, 0, &|_| EdgePlacementPolicy::Insertion)
            .unwrap();

        // ADR 0096 §4 (continued): the fold compacts the tombstone with one
        // bounded move per step (11 into the hole); 12 follows on a later step.
        assert_eq!(
            rewritten,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: road,
                old_slot_index: 1,
                new_slot_index: 0,
            })
        );
        // Second step packs 12 into the remaining hole (bounded fold converges).
        graph
            .compact_vertex_edge_span_one_step(hub, 0, 0, &|_| EdgePlacementPolicy::Insertion)
            .unwrap();
        assert_eq!(
            graph
                .iter_edges_for_label(hub, road)
                .unwrap()
                .into_iter()
                .map(|edge| (edge.edge_slot_index_raw(), edge.target))
                .collect::<Vec<_>>(),
            vec![(3, 13), (1, 12), (0, 11)]
        );
    }

    #[test]
    fn edge_overflow_compaction_folds_both_planes_together() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(hub, road, 2)
            .unwrap();
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let peer = BucketLabelKey::directed_from_index(3);
        graph
            .ensure_label_bucket_inline_property_byte_width(hub, peer, 2)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                peer,
                InlinePropertyTestEdge::with_bytes(100, &100u16.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                road,
                InlinePropertyTestEdge::with_bytes(33, &33u16.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let read_bucket = || {
            let vertex = graph.vertices().get(hub);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let before = read_bucket();
        assert!(before.edge_run().is_some());
        assert!(before.values_run().is_some());

        // The span publish owns the edge overflow-log fold (ADR 0096 §5, "Span publish log
        // ownership"), so the step folds that suffix inside its rewrite and then carries on with
        // its own compaction. Which variant reports it is not what this test pins — the two
        // effects below are: the edge log is folded, and the inline property bytes log state is
        // left exactly as it was.
        graph
            .compact_vertex_edge_span_one_step(hub, 0, 0, &|_| EdgePlacementPolicy::Insertion)
            .unwrap();

        let after = read_bucket();
        assert_eq!(after.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        // ADR 0099 reverses what this fixture used to pin. The span publish's edge-log fold moves the
        // rows into the span, and the values plane is indexed by the same slots, so the fold has to
        // carry the blocks - leaving the values run behind would strand a block for every folded row.
        assert_eq!(after.values_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_eq!(
            after.inline_property_bytes_slab_slots(),
            after.stored_slots(),
            "the value prefix follows the edge prefix"
        );
        for slot in 0..after.degree() {
            let bytes = graph
                .read_bucket_inline_property_bytes_for_slot(&after, slot, None)
                .expect("every folded row keeps its value block");
            assert_eq!(bytes.len(), 2, "slot {slot}");
        }
    }

    #[test]
    fn compact_label_bucket_vertex_segment_preserves_rows() {
        let graph = test_graph();
        for label in 1..=6u16 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge {
                        target: label as u32,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let before = graph.vertices().get(VertexId::from(0));
        graph
            .compact_label_bucket_vertex_segment(VertexId::from(0))
            .unwrap();
        let after = graph.vertices().get(VertexId::from(0));
        assert_eq!(after.degree(), before.degree());
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap().len(), 6);
    }

    // --- ADR 0052 §7/§9: policy-aware compaction (unordered swap vs insertion left-pack) ---

    /// `[a, T, b, T, c]` with values `[va, vb, vc]`: two interior tombstones, degree 3.
    /// The first swap has `k_ord = 1 < m_ord = 2` and exercises the value rotation.
    fn slab_bucket_with_two_interior_tombstones() -> (
        LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory>,
        BucketLabelKey,
    ) {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 2u16)
            .unwrap();
        for target in 1..=5u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        graph
            .remove_edge_at_slot(VertexId::from(0), label, 1)
            .unwrap()
            .expect("slot 1 must be tombstoned");
        graph
            .remove_edge_at_slot(VertexId::from(0), label, 3)
            .unwrap()
            .expect("slot 3 must be tombstoned");
        (graph, label)
    }

    #[test]
    fn unordered_swap_compaction_moves_last_live_into_first_interior_tombstone() {
        let (graph, label) = slab_bucket_with_two_interior_tombstones();
        let moved = graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Unordered
            })
            .unwrap();
        assert_eq!(
            moved,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: label,
                old_slot_index: 4,
                new_slot_index: 1,
            }),
            "the last live edge must swap into the first interior tombstone (physical slot space)"
        );
        let edges = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            edges.iter().map(|edge| edge.target).collect::<Vec<_>>(),
            vec![1, 5, 3],
            "swap-compaction reorders: the last live edge lands in the first interior hole"
        );
        assert_eq!(
            edges
                .iter()
                .map(|edge| edge.value[..usize::from(edge.inline_property_len)].to_vec())
                .collect::<Vec<_>>(),
            vec![
                1u16.to_le_bytes().to_vec(),
                5u16.to_le_bytes().to_vec(),
                3u16.to_le_bytes().to_vec(),
            ],
            "inline property bytes must follow the swapped edge at its new live ordinal (ADR §9)"
        );
        // Drain the rest of the span: the next step finds no interior tombstone, finalizes
        // `stored_slots = degree`, and trims trailing tombstones.
        let mut resume = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume, 0, &|_| {
                    crate::labeled::graph::EdgePlacementPolicy::Unordered
                })
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => resume = 0,
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_) => resume = 0,
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots(), 3, "trailing tombstones trimmed");
        assert_eq!(bucket.degree(), 3);
        assert_eq!(
            bucket.inline_property_bytes_slab_slots(),
            3,
            "dense value span must stay degree-sized after the swap sequence"
        );
    }

    #[test]
    fn unordered_swap_compaction_k_equals_m_skips_value_rotation() {
        // [a, T, c]: the only interior tombstone's live ordinal equals degree - 1, so the
        // value window is one block and the rotation is a no-op; the edge still swaps.
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 2u16)
            .unwrap();
        for target in 1..=3u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        graph
            .remove_edge_at_slot(VertexId::from(0), label, 1)
            .unwrap()
            .expect("middle edge must be tombstoned");
        let moved = graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Unordered
            })
            .unwrap();
        assert_eq!(
            moved,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: label,
                old_slot_index: 2,
                new_slot_index: 1,
            })
        );
        let edges = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            edges.iter().map(|edge| edge.target).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            edges
                .iter()
                .map(|edge| edge.value[..usize::from(edge.inline_property_len)].to_vec())
                .collect::<Vec<_>>(),
            vec![1u16.to_le_bytes().to_vec(), 3u16.to_le_bytes().to_vec()],
            "values must stay aligned when the hole ordinal equals the source ordinal"
        );
    }

    #[test]
    fn unordered_swap_compaction_two_swaps_reorder_and_trim() {
        // [T, a, T, b, T, c]: two interior tombstones before live rows produce TWO swaps.
        // Swap 1 rotates the value window (k_ord = 0 < m_ord = 2); swap 2 has k_ord == m_ord.
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 2u16)
            .unwrap();
        for target in 1..=6u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        for slot in [0u32, 2, 4] {
            graph
                .remove_edge_at_slot(VertexId::from(0), label, slot)
                .unwrap()
                .expect("interior tombstone");
        }
        let first = graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Unordered
            })
            .unwrap();
        assert_eq!(
            first,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: label,
                old_slot_index: 5,
                new_slot_index: 0,
            })
        );
        let second = graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Unordered
            })
            .unwrap();
        assert_eq!(
            second,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: label,
                old_slot_index: 3,
                new_slot_index: 2,
            }),
            "second swap: k_ord == m_ord edge move without a value rotation"
        );
        let edges = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            edges.iter().map(|edge| edge.target).collect::<Vec<_>>(),
            vec![6, 2, 4],
            "two swaps reorder the bucket; trailing tombstones are trim candidates"
        );
        assert_eq!(
            edges
                .iter()
                .map(|edge| edge.value[..usize::from(edge.inline_property_len)].to_vec())
                .collect::<Vec<_>>(),
            vec![
                6u16.to_le_bytes().to_vec(),
                2u16.to_le_bytes().to_vec(),
                4u16.to_le_bytes().to_vec(),
            ],
            "values stay aligned across the two-swap sequence"
        );
    }

    #[test]
    fn insertion_bucket_compaction_keeps_left_pack_order() {
        // Same tombstone geometry, Insertion resolver: the order-preserving left-pack must
        // run and the original live order [1, 3, 5] must be preserved (no reordering).
        let (graph, label) = slab_bucket_with_two_interior_tombstones();
        let moved = graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Insertion
            })
            .unwrap();
        assert_eq!(
            moved,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: label,
                old_slot_index: 2,
                new_slot_index: 1,
            }),
            "Insertion compaction must keep the order-preserving left-pack"
        );
        let mut resume = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume, 0, &|_| {
                    crate::labeled::graph::EdgePlacementPolicy::Insertion
                })
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => resume = 0,
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_) => resume = 0,
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        assert_eq!(
            graph
                .out_edges(VertexId::from(0))
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![1, 3, 5],
            "Insertion compaction preserves bucket-local live order"
        );
    }

    #[test]
    fn unordered_swap_gate_asks_only_for_a_slab_bucket() {
        use crate::labeled::record::LabelBucket;
        // ADR 0099 withdrew the value clause: the swap used to shift a value window, which needed a
        // dense, run-free plane; a slot-addressed plane moves one block per row, so a bucket with a
        // value run or a sparse value span may swap like any other.
        let value_run = LabelBucket::default()
            .with_inline_property_byte_width(2)
            .with_degree_field(2)
            .with_stored_slots(4)
            .with_values_spill_run(7);
        assert!(
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::slab_bucket_allows_unordered_swap(
                &value_run
            ),
            "a value run no longer stops a swap"
        );
        let non_dense = LabelBucket::default()
            .with_inline_property_byte_width(2)
            .with_inline_property_bytes_slab_slots(2)
            .with_degree_field(3)
            .with_stored_slots(4);
        assert!(
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::slab_bucket_allows_unordered_swap(
                &non_dense
            ),
            "a sparse value span no longer stops a swap"
        );
        let edge_run = LabelBucket::default()
            .with_degree_field(2)
            .with_stored_slots(4)
            .with_edge_spill_run(9);
        assert!(
            !LabeledLaraGraph::<TestEdge, crate::VectorMemory>::slab_bucket_allows_unordered_swap(
                &edge_run
            ),
            "the move still needs the row to be in the span it swaps within"
        );
    }

    #[test]
    fn unordered_swap_step_forced_error_returns_before_mutation() {
        let (graph, label) = slab_bucket_with_two_interior_tombstones();
        crate::labeled::graph::force_next_compact_vertex_edge_span_step_error();
        graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Unordered
            })
            .expect_err("forced compaction step error must surface before any mutation");
        assert_eq!(
            graph
                .out_edges(VertexId::from(0))
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![1, 3, 5],
            "the pre-mutation forced error must leave the tombstone fixture untouched"
        );
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots(), 5);
        assert_eq!(bucket.degree(), 3);
    }

    /// Plan 0319 §Step 3 regression test: `bucket_span_region_len`
    /// returns the correct LEG region length for both slab-mode and
    /// tree-mode buckets.
    ///
    /// For a slab bucket: returns `stored_slots` (the contiguous
    /// edge-slab width).
    /// For a tree bucket at depth 1, stored=4096: returns
    /// `ceil(4096 / 1024) = 4` (the root region length, NOT 4096).
    /// For a tree bucket at depth 2, stored=1,048,576: returns
    /// `ceil(1,048,576 / 1024) / 1024 = 1` (the root region length,
    /// a single block_id pointing to the interior).
    #[test]
    fn tree_spans_owns_the_property_root_address() {
        let tree = crate::labeled::record::LabelBucket::default()
            .with_edge_range(100, 4096)
            .with_tree_mode(true)
            .with_inline_property_byte_width(8)
            .with_tree_mode_property_depth(1);
        let spans = super::TreeSpans::of(&tree).expect("tree bucket");
        let (edge_start, edge_len) = spans.edge_root();
        let (property_start, property_len) = spans.property_root();
        assert_eq!(edge_start, 100);
        assert_eq!(property_start, 100 + u64::from(edge_len));
        assert_eq!(spans.combined(), (100, edge_len + property_len));
        assert!(spans.validate(&tree).is_ok());
        // A recorded offset that disagrees with the derivation is the split-brain this type owns.
        let wrong = tree.with_inline_property_bytes_offset(999);
        assert!(spans.validate(&wrong).is_err());
        // Non-tree buckets have no root regions.
        let slab = crate::labeled::record::LabelBucket::default();
        assert!(super::TreeSpans::of(&slab).is_none());
    }

    #[test]
    fn bucket_span_region_len_returns_correct_region_per_mode() {
        use crate::labeled::bucket_label_key::BucketLabelKey;
        use crate::labeled::record::LabelBucket;

        // Slab-mode bucket: region == stored_slots.
        let slab_bucket = LabelBucket::try_from_parts(
            BucketLabelKey::directed_from_index(1),
            0,
            100, // degree
            200,
            0,
            0,
            0,
            -1,
            0,
        )
        .unwrap();
        assert_eq!(super::bucket_span_region_len(&slab_bucket), 200);

        // Tree-mode bucket at depth 1, stored = 4096: region == 4.
        let tree_d1_bucket = LabelBucket::try_from_parts(
            BucketLabelKey::directed_from_index(1),
            256, // edge_start (root region at slot 256)
            4096,
            4096,
            0,
            0,
            0,
            -1,
            0,
        )
        .unwrap()
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(1);
        assert_eq!(
            super::bucket_span_region_len(&tree_d1_bucket),
            4,
            "depth-1 root region for stored=4096 is 4 slots"
        );

        // Tree-mode bucket at depth 2, stored = 1,048,576: region == 1.
        // We can't construct this via try_from_parts directly because
        // the (log_head=-1, log_len=1) mismatch is validated before
        // with_tree_mode(true) sets the bit. Build it by starting from
        // a depth-1 bucket and patching the physical depth byte.
        let mut tree_d2_bucket = tree_d1_bucket;
        tree_d2_bucket = tree_d2_bucket.with_tree_mode_physical_depth(2);
        assert_eq!(
            super::bucket_span_region_len(&tree_d2_bucket),
            1,
            "depth-2 root region for stored=4096 (after manual deepen to d2) is 1 slot"
        );

        // ADR 0096 §5: tiny buckets hold zero slab slots regardless of degree.
        let tiny_bucket = LabelBucket::try_from_parts(
            BucketLabelKey::directed_from_index(1),
            512, // anchor (not storage)
            3,
            3,
            0,
            0,
            0,
            -1,
            0,
        )
        .unwrap()
        .try_enable_tiny_mode(4)
        .expect("enable")
        .with_tiny_target(0, 7)
        .with_tiny_target(1, 8)
        .with_tiny_target(2, 9);
        assert_eq!(super::bucket_span_region_len(&tiny_bucket), 0);
        assert_eq!(super::combined_span_region_len(&tiny_bucket), 0);
    }

    /// GAP-2026-09-17-001 regression (M1 shape): full-path single-bucket growth
    /// 0 → 8192 through `insert_edge` (impl + dense-check + cascade), the exact
    /// shape that trapped at the 5728th edge with
    /// `release_span(start=1063168, len=5728)` rejected as `OverlapPrevious`.
    /// The `release_vertex_edge_span_footprint` one-shot whole-cover release is
    /// gone: footprints retire as mode-aware bucket regions plus remainder, so
    /// tree covers never hand unowned ranges to the free store.
    ///
    /// Wrong-implementation probe: restoring the whole-cover `release_span`
    /// first attempt reintroduces the trap (this test fails at target=5727).
    /// Cost note: 8192 full-path inserts on `VectorMemory` complete in well
    /// under the 30s suite budget (prior art: `labeled_hub_33_labels_bounded_insert_time`).
    #[test]
    fn gap_tree_full_path_growth_past_5728_releases_only_owned_regions() {
        use crate::labeled::bucket_label_key::BucketLabelKey;
        use crate::labeled::graph::EdgePlacementPolicy;
        use crate::traits::CsrEdge;
        // 4-byte edge type exercises the production tree-mode path (B=1024,
        // T_PROMOTE=4096); the 10-byte bench edge never promotes.
        assert_eq!(TestEdge::BYTES, 4);
        // M1-shape capacity: the bench harness uses 1<<20 edge slots so the
        // 8192-edge growth never hits raw capacity (the trap under test is a
        // free-span overlap, not exhaustion). `test_graph` (256 slots) cannot
        // host this shape.
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(1 << 20),
            BucketLabelKey::from_raw(1),
        )
        .expect("graph");
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        for target in 0..8192u32 {
            graph
                .insert_edge(
                    vid,
                    label,
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|e| panic!("failed at target={target}: {e:?}"));
        }
        let vertex = graph.vertices().get(vid);
        let buckets = graph
            .read_vertex_label_buckets(&vertex)
            .expect("read buckets");
        assert_eq!(buckets.len(), 1);
        let bucket = buckets[0];
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.stored_slots(), 8192);
        assert_eq!(bucket.degree(), 8192);
        // Physical root region is 8 slots (ceil(8192/1024)), not 8192: the
        // cover/tree unit confusion this test guards against.
        assert_eq!(super::bucket_span_region_len(&bucket), 8);
        // Leaf PMA `actual` must hold across the whole growth/relocate chain:
        // every one of the 8192 rows is a tree row, so the leaf's live
        // edge-slab record count is exactly zero (ADR 0096 §5), and the audit
        // must agree after every relocate/slide/rebuild.
        let header = graph.edges().header();
        assert_eq!(
            graph
                .edges()
                .counts_store()
                .get(u64::from(header.segment_count))
                .actual,
            0,
            "8192 tree rows must contribute zero to leaf `actual`"
        );

        // tree rows are excluded, so this leaf's live slab-era records are the
        // only contribution (ADR 0096 §5).
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
        // Full adjacency survives every relocate in the growth chain
        // (`iter_edges_for_label` yields Descending; reverse for insertion order).
        let edges = graph.iter_edges_for_label(vid, label).expect("scan");
        assert_eq!(edges.len(), 8192);
        for (i, edge) in edges.iter().rev().enumerate() {
            assert_eq!(edge.target, i as u32, "adjacency mismatch at {i}");
        }
    }
    #[test]
    fn base_resolution_never_reports_a_width_it_did_not_obtain() {
        // Invariant I1: the width a resolution reports is what the caller publishes, so
        // it may only be the requested span (the resolver placed or reserved it) or the
        // span the vertex already holds (the fallback). Anything in between would be a
        // cover wider than its backing — the phantom cover the never-fail fallback used
        // to publish.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        for target in 0..64u32 {
            graph
                .insert_edge(
                    vid,
                    label,
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let cover_before = graph.vertices().get(vid).stored_slots;
        let requested = cover_before.saturating_add(100_000);
        if let Ok((_, width)) = graph.resolve_labeled_edge_base_for_rebalance(vid, requested) {
            // The width the caller will publish must be backed by the span the
            // vertex actually holds after the resolution. A relocation may have
            // grown that span (`cover_before` is only the pre-resolution value), so
            // the check is against the published cover, not the request.
            let backed = graph.vertices().get(vid).stored_slots;
            assert!(
                width <= backed,
                "resolution reported width {width} over a backing of {backed} (requested {requested}, was {cover_before})"
            );
        }
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }
    #[test]
    fn spanless_tiny_vertex_does_not_request_slack() {
        // Class C: a vertex whose buckets are all tiny owns zero slab slots (ADR 0096 §5),
        // so a slack request could only be a real span; when that span cannot be placed,
        // the slack tail has no base to report (`labeled_edge_base_from_first_bucket` needs
        // a non-tiny bucket) and the whole leaf-wide log recovery failed. The request must
        // not happen at all, so the vertex stays spanless even when slack is forced.
        let graph = test_graph();
        let vid = VertexId::from(0);
        graph
            .insert_edge(
                vid,
                BucketLabelKey::from_raw(2),
                TestEdge { target: 7 },
                EdgePlacementPolicy::Insertion,
            )
            .expect("tiny birth");
        assert_eq!(graph.vertices().get(vid).stored_slots, 0);
        graph
            .rebalance_vertex_edge_span(vid, None, 0, true)
            .expect("rebalance");
        assert_eq!(
            graph.vertices().get(vid).stored_slots,
            0,
            "a spanless vertex must stay spanless"
        );
    }
}
