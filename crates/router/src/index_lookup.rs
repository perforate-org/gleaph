//! Async index client surface used by GQL dispatch and federation helpers.

use std::future::Future;
use std::pin::Pin;

use gleaph_graph_kernel::index::{
    IndexIntersectionRequest, IndexLabelIntersectionRequest, LabelLookupPageRequest,
    LabelLookupPageResult, PostingHit, ValuePostingCount,
};

use crate::index_client::RouterIndexClient;

pub(crate) trait IndexLookup {
    fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>>;

    fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>>;

    fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        vertex_filter_packed: Option<Vec<u64>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>>;

    fn lookup_label_page(
        &self,
        req: LabelLookupPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>>;

    #[expect(
        dead_code,
        reason = "IndexLookup trait surface for label intersection fast paths"
    )]
    fn lookup_label_intersection(
        &self,
        req: IndexLabelIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>>;

    fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: Vec<PostingHit>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>>;

    fn count_postings_by_value_for_label(
        &self,
        property_id: u32,
        vertex_label_id: u32,
        min_count: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>>;
}

impl IndexLookup for RouterIndexClient {
    fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        Box::pin(self.lookup_equal(property_id, value))
    }

    fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        Box::pin(self.lookup_intersection(req))
    }

    fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        vertex_filter_packed: Option<Vec<u64>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
        Box::pin(self.count_postings_by_value(property_id, min_count, vertex_filter_packed))
    }

    fn lookup_label_page(
        &self,
        req: LabelLookupPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
        Box::pin(self.lookup_label_page(req))
    }

    fn lookup_label_intersection(
        &self,
        req: IndexLabelIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        Box::pin(self.lookup_label_intersection(req))
    }

    fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: Vec<PostingHit>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        Box::pin(self.filter_hits_by_label(vertex_label_id, hits))
    }

    fn count_postings_by_value_for_label(
        &self,
        property_id: u32,
        vertex_label_id: u32,
        min_count: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
        Box::pin(self.count_postings_by_value_for_label(property_id, vertex_label_id, min_count))
    }
}
