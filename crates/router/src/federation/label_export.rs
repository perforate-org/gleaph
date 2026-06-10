//! Shard-scoped, paginated label membership export for seed routing (ADR 0004 path A).

use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{LabelLookupPageRequest, PostingHit};

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
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
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

        fn lookup_label(
            &self,
            _vertex_label_id: u32,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
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
            _vertex_label_id: u32,
            hits: Vec<PostingHit>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(hits) })
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
            pages: Rc::new(RefCell::new(vec![
                LabelLookupPageResult {
                    hits: vec![PostingHit {
                        shard_id: 7,
                        vertex_id: 1,
                    }],
                    next: Some(LabelPostingCursor {
                        shard_id: 7,
                        vertex_id: 1,
                    }),
                    done: false,
                },
                LabelLookupPageResult {
                    hits: vec![PostingHit {
                        shard_id: 7,
                        vertex_id: 2,
                    }],
                    next: Some(LabelPostingCursor {
                        shard_id: 7,
                        vertex_id: 2,
                    }),
                    done: true,
                },
                LabelLookupPageResult {
                    hits: vec![PostingHit {
                        shard_id: 9,
                        vertex_id: 3,
                    }],
                    next: None,
                    done: true,
                },
            ])),
        };
        let hits = futures::executor::block_on(collect_label_hits_for_shards(&index, 1, &[7, 9]))
            .expect("collect");
        assert_eq!(hits.len(), 3);
    }
}
