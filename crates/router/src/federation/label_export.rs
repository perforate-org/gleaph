//! Shard-scoped, paginated label membership export for seed routing (ADR 0004 path A).

use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    LabelIntersectionPageRequest, LabelLookupPageRequest, PostingHit,
};

use crate::index_lookup::IndexLookup;

/// Page size for graph-index `lookup_label_page` during seed collection.
pub const LABEL_SEED_EXPORT_PAGE_LIMIT: u32 = 10_000;

/// Collect all label postings for `vertex_label_id` by paging each registered shard.
pub async fn collect_label_hits_for_shards<I: IndexLookup + ?Sized>(
    index: &I,
    vertex_label_id: u32,
    shard_ids: &[ShardId],
) -> Result<Vec<PostingHit>, String> {
    let mut all = Vec::new();
    for &shard_id in shard_ids {
        let mut after = None;
        loop {
            let page = index
                .lookup_label_page(LabelLookupPageRequest {
                    vertex_label_id,
                    shard_id,
                    after,
                    limit: LABEL_SEED_EXPORT_PAGE_LIMIT,
                })
                .await?;
            all.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
    }
    Ok(all)
}

/// Collect multi-label intersection hits by paging one label bucket per shard and sieving
/// the rest with [`IndexLookup::filter_hits_by_label`] (ADR 0004 path D).
pub async fn collect_label_intersection_hits_for_shards<I: IndexLookup + ?Sized>(
    index: &I,
    vertex_label_ids: &[u32],
    shard_ids: &[ShardId],
) -> Result<Vec<PostingHit>, String> {
    if vertex_label_ids.len() < 2 {
        return Err("label intersection requires at least two labels".into());
    }
    let mut labels: Vec<u32> = vertex_label_ids.to_vec();
    labels.sort_unstable();
    labels.dedup();
    if labels.len() < 2 {
        return Err("label intersection requires at least two distinct labels".into());
    }
    let walk_label = labels[0];
    let sieve_labels = &labels[1..];
    let mut all = Vec::new();
    for &shard_id in shard_ids {
        let mut after = None;
        loop {
            let page = index
                .lookup_label_intersection_page(LabelIntersectionPageRequest {
                    walk_label_id: walk_label,
                    sieve_label_ids: sieve_labels.to_vec(),
                    shard_id,
                    after,
                    limit: LABEL_SEED_EXPORT_PAGE_LIMIT,
                })
                .await?;
            all.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;

    use gleaph_graph_kernel::index::{
        IndexIntersectionRequest, IndexLabelIntersectionRequest, LabelLookupPageRequest,
        LabelLookupPageResult, LabelPostingCursor, ValuePostingCount,
    };

    use super::*;
    use crate::index_lookup::IndexLookup;

    struct PageIndex {
        pages: Rc<RefCell<Vec<LabelLookupPageResult>>>,
        sieve_label: u32,
        sieve_members: Rc<RefCell<Vec<u32>>>,
    }

    impl IndexLookup for PageIndex {
        fn lookup_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn lookup_intersection(
            &self,
            _req: IndexIntersectionRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            gleaph_graph_kernel::index::IndexIntersectionResult,
                            String,
                        >,
                    > + '_,
            >,
        > {
            Box::pin(async {
                Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(Vec::new()))
            })
        }

        fn lookup_edge_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<gleaph_graph_kernel::index::EdgePostingHit>, String>>
                    + '_,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn count_postings_by_value(
            &self,
            _property_id: u32,
            _min_count: u64,
            _vertex_filter_packed: Option<Vec<u64>>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn lookup_label_intersection(
            &self,
            _req: IndexLabelIntersectionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn filter_hits_by_label(
            &self,
            vertex_label_id: u32,
            hits: Vec<PostingHit>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            let sieve_label = self.sieve_label;
            let members = self.sieve_members.clone();
            Box::pin(async move {
                if vertex_label_id != sieve_label {
                    return Ok(hits);
                }
                let keep = members.borrow();
                Ok(hits
                    .into_iter()
                    .filter(|hit| keep.contains(&hit.vertex_id))
                    .collect())
            })
        }

        fn count_postings_by_value_for_label(
            &self,
            _property_id: u32,
            _vertex_label_id: u32,
            _min_count: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn lookup_label_page(
            &self,
            _req: LabelLookupPageRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
            let page = self.pages.borrow_mut().remove(0);
            Box::pin(async move { Ok(page) })
        }
    }

    #[test]
    fn collect_label_hits_pages_each_shard() {
        let index = PageIndex {
            sieve_label: 2,
            sieve_members: Rc::new(RefCell::new(vec![1, 2])),
            pages: Rc::new(RefCell::new(vec![
                LabelLookupPageResult {
                    hits: vec![PostingHit {
                        shard_id: ShardId::new(0),
                        vertex_id: 1,
                    }],
                    next: Some(LabelPostingCursor {
                        shard_id: ShardId::new(0),
                        vertex_id: 1,
                    }),
                    done: false,
                },
                LabelLookupPageResult {
                    hits: vec![PostingHit {
                        shard_id: ShardId::new(0),
                        vertex_id: 2,
                    }],
                    next: Some(LabelPostingCursor {
                        shard_id: ShardId::new(0),
                        vertex_id: 2,
                    }),
                    done: true,
                },
                LabelLookupPageResult {
                    hits: vec![PostingHit {
                        shard_id: ShardId::new(1),
                        vertex_id: 3,
                    }],
                    next: None,
                    done: true,
                },
            ])),
        };
        let hits = futures::executor::block_on(collect_label_hits_for_shards(
            &index,
            1,
            &[ShardId::new(0), ShardId::new(1)],
        ))
        .expect("collect");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn collect_label_intersection_pages_and_sieves_other_labels() {
        let index = PageIndex {
            sieve_label: 2,
            sieve_members: Rc::new(RefCell::new(vec![1, 3])),
            pages: Rc::new(RefCell::new(vec![
                LabelLookupPageResult {
                    hits: vec![
                        PostingHit {
                            shard_id: ShardId::new(0),
                            vertex_id: 1,
                        },
                        PostingHit {
                            shard_id: ShardId::new(0),
                            vertex_id: 2,
                        },
                    ],
                    next: Some(LabelPostingCursor {
                        shard_id: ShardId::new(0),
                        vertex_id: 2,
                    }),
                    done: false,
                },
                LabelLookupPageResult {
                    hits: vec![PostingHit {
                        shard_id: ShardId::new(0),
                        vertex_id: 3,
                    }],
                    next: None,
                    done: true,
                },
            ])),
        };
        let hits = futures::executor::block_on(collect_label_intersection_hits_for_shards(
            &index,
            &[1, 2],
            &[ShardId::new(0)],
        ))
        .expect("collect intersection");
        assert_eq!(
            hits,
            vec![
                PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: 1,
                },
                PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: 3,
                },
            ]
        );
    }
}
