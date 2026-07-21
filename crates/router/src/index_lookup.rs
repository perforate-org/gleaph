//! Async index client surface used by GQL dispatch and federation helpers.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingHit, IndexEqualSpec, IndexIntersectionRequest, IndexIntersectionResult,
    IndexLabelIntersectionRequest, IndexSubject, LabelIntersectionPageRequest,
    LabelLookupPageRequest, LabelLookupPageResult, LookupEdgeEqualPageRequest,
    LookupEqualBatchRequest, LookupEqualBatchResult, LookupEqualPageRequest,
    LookupIntersectionPageRequest, LookupPropertyIntersectionPageRequest,
    LookupValuePostingCountPageRequest, MAX_EQUALITY_INTERSECTION_ARMS, MAX_POSTING_PAGE_HITS,
    MAX_VALUE_POSTING_COUNT_PAGE_GROUPS, PostingHit, PostingHitPage, ValuePostingCount,
    ValuePostingCountPage,
};

use crate::facade::store::RouterStore;
use crate::federation::SeedHits;
use crate::index_client::RouterIndexClient;
use crate::seed::{IndexAnchor, SeedProbe};

/// Page size for paginated property / edge equality exports during seed routing. Bounds the
/// per-message materialization on the index canister (no full-bucket heap materialization).
const INDEX_LOOKUP_PAGE_LIMIT: u32 = MAX_POSTING_PAGE_HITS;

async fn collect_value_count_pages(
    client: &RouterIndexClient,
    property_id: u32,
    min_count: u64,
    vertex_filter_packed: Option<Vec<u64>>,
) -> Result<Vec<ValuePostingCount>, String> {
    let mut counts = Vec::new();
    let mut after = None;
    loop {
        let page: ValuePostingCountPage = client
            .count_postings_by_value_page(LookupValuePostingCountPageRequest {
                property_id,
                min_count,
                vertex_filter_packed: vertex_filter_packed.clone(),
                after,
                limit: MAX_VALUE_POSTING_COUNT_PAGE_GROUPS,
            })
            .await?;
        counts.extend(page.counts);
        if page.done {
            break;
        }
        after = page.next;
    }
    Ok(counts)
}

async fn collect_value_count_pages_for_label(
    client: &RouterIndexClient,
    property_id: u32,
    vertex_label_id: u32,
    min_count: u64,
) -> Result<Vec<ValuePostingCount>, String> {
    let mut counts = Vec::new();
    let mut after = None;
    loop {
        let page: ValuePostingCountPage = client
            .count_postings_by_value_for_label_page(
                LookupValuePostingCountPageRequest {
                    property_id,
                    min_count,
                    vertex_filter_packed: None,
                    after,
                    limit: MAX_VALUE_POSTING_COUNT_PAGE_GROUPS,
                },
                vertex_label_id,
            )
            .await?;
        counts.extend(page.counts);
        if page.done {
            break;
        }
        after = page.next;
    }
    Ok(counts)
}

async fn collect_property_intersection_pages(
    client: &RouterIndexClient,
    specs: Vec<IndexEqualSpec>,
) -> Result<IndexIntersectionResult, String> {
    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    let mut after = None;
    let mut shape = None;
    loop {
        let page = client
            .lookup_property_intersection_page(LookupPropertyIntersectionPageRequest {
                specs: specs.clone(),
                after,
                limit: INDEX_LOOKUP_PAGE_LIMIT,
            })
            .await?;
        match page.hits {
            IndexIntersectionResult::Vertices(hits) => {
                shape.get_or_insert(false);
                vertices.extend(hits);
            }
            IndexIntersectionResult::Edges(hits) => {
                shape.get_or_insert(true);
                edges.extend(hits);
            }
        }
        if page.done {
            break;
        }
        after = page.next;
    }
    Ok(if shape.unwrap_or(false) {
        IndexIntersectionResult::Edges(edges)
    } else {
        IndexIntersectionResult::Vertices(vertices)
    })
}

/// Collect all equality hits for `(property_id, value)` on one index canister by paging, so the
/// index never builds a full-bucket `Vec` in a single query message.
async fn collect_equal_hits_paged(
    client: &RouterIndexClient,
    property_id: u32,
    value: Vec<u8>,
) -> Result<Vec<PostingHit>, String> {
    let mut hits = Vec::new();
    let mut after = None;
    loop {
        let page = client
            .lookup_equal_page(LookupEqualPageRequest {
                property_id,
                value: value.clone(),
                after,
                limit: INDEX_LOOKUP_PAGE_LIMIT,
            })
            .await?;
        hits.extend(page.hits);
        if page.done {
            break;
        }
        after = page.next;
    }
    Ok(hits)
}

/// Collect all edge equality hits for `(property_id, value[, label_id])` on one index canister by
/// paging (no full-bucket heap materialization).
async fn collect_edge_equal_hits_paged(
    client: &RouterIndexClient,
    property_id: u32,
    value: Vec<u8>,
    label_id: Option<u16>,
) -> Result<Vec<EdgePostingHit>, String> {
    let mut hits = Vec::new();
    let mut after = None;
    loop {
        let page = client
            .lookup_edge_equal_page(LookupEdgeEqualPageRequest {
                property_id,
                value: value.clone(),
                label_id,
                after,
                limit: INDEX_LOOKUP_PAGE_LIMIT,
            })
            .await?;
        hits.extend(page.hits);
        if page.done {
            break;
        }
        after = page.next;
    }
    Ok(hits)
}

/// Maximum number of equality specs in a single `lookup_equal_batch` call. The encoded
/// `IndexEqualSpec::value` is the dominant payload contributor; 100 keeps the Candid message
/// comfortably under the 2 MiB inter-canister limit while amortizing fixed call cost.
const EQUAL_BATCH_MAX_SPECS: usize = 100;

/// Resolve a deduplicated set of equality `IndexAnchor::Equal` anchors in the fewest possible
/// index calls. Uses `lookup_equal_batch(after=None)` for the first page, then continues any
/// `done=false` bucket from its own `page.next`, and resumes a budget-truncated batch from the
/// returned `next` index with another `after=None` batch.
/// Batch equality lookup driver parameterized by caller closures so native unit tests can
/// inject responses without crossing the `ic_cdk::call` boundary.
async fn collect_equal_hits_batched_with_caller<F, Fut, G, Gf>(
    anchors: &[IndexAnchor],
    mut call_batch: F,
    mut call_page: G,
) -> Result<Vec<PostingHit>, String>
where
    F: FnMut(LookupEqualBatchRequest) -> Fut,
    Fut: Future<Output = Result<LookupEqualBatchResult, String>>,
    G: FnMut(LookupEqualPageRequest) -> Gf,
    Gf: Future<Output = Result<PostingHitPage, String>>,
{
    let mut pending: Vec<(IndexEqualSpec, (u32, Vec<u8>))> = Vec::new();
    for anchor in anchors {
        let IndexAnchor::Equal(probe) = anchor else {
            return Err("collect_equal_hits_batched requires Equal anchors".into());
        };
        let spec = IndexEqualSpec::vertex(probe.property_id, probe.payload_bytes.clone());
        let dedup_key = (probe.property_id, probe.payload_bytes.clone());
        if !pending.iter().any(|(_, k)| *k == dedup_key) {
            pending.push((spec, dedup_key));
        }
    }

    let mut all_hits: Vec<PostingHit> = Vec::new();
    let mut start = 0usize;
    while start < pending.len() {
        let end = (start + EQUAL_BATCH_MAX_SPECS).min(pending.len());
        let specs: Vec<IndexEqualSpec> =
            pending[start..end].iter().map(|(s, _)| s.clone()).collect();
        let result = call_batch(LookupEqualBatchRequest {
            specs,
            after: None,
            limit: INDEX_LOOKUP_PAGE_LIMIT,
        })
        .await?;

        match result.next {
            Some(next) if next as usize > result.pages.len() => {
                return Err(format!(
                    "lookup_equal_batch returned invalid next {} (pages={})",
                    next,
                    result.pages.len()
                ));
            }
            Some(next) if next as usize != result.pages.len() => {
                return Err(format!(
                    "lookup_equal_batch returned next={} but {} pages",
                    next,
                    result.pages.len()
                ));
            }
            _ => {}
        }
        if result.next.is_none() && result.pages.len() != end - start {
            return Err(format!(
                "lookup_equal_batch returned {} pages for {} specs",
                result.pages.len(),
                end - start
            ));
        }
        if let Some(next) = result.next {
            // specs [0..next) returned at least one page and the canister stopped before
            // spec next. Treat [next..end) as not yet processed; they will be handled in the
            // next outer iteration from `start + next`.
            // But first finish any done pages in [0..next).
            for (offset, page) in result.pages.iter().enumerate().take(next as usize) {
                all_hits.extend(page.hits.clone());
                if !page.done {
                    if page.next.is_none() {
                        return Err(format!(
                            "lookup_equal_batch returned done=false but next=None for spec {}",
                            start + offset
                        ));
                    }
                    let spec = pending[start + offset].0.clone();
                    let continued =
                        continue_equal_page_with_caller(&mut call_page, spec, page.next.clone())
                            .await?;
                    all_hits.extend(continued);
                }
            }
            start += next as usize;
            continue;
        }

        // No early stop: all pages returned.
        for (offset, page) in result.pages.iter().enumerate() {
            all_hits.extend(page.hits.clone());
            if !page.done {
                if page.next.is_none() {
                    return Err(format!(
                        "lookup_equal_batch returned done=false but next=None for spec {}",
                        start + offset
                    ));
                }
                let spec = pending[start + offset].0.clone();
                let continued =
                    continue_equal_page_with_caller(&mut call_page, spec, page.next.clone())
                        .await?;
                all_hits.extend(continued);
            }
        }
        start = end;
    }

    all_hits.sort_unstable_by_key(|h| (h.shard_id, h.vertex_id));
    all_hits.dedup_by_key(|h| (h.shard_id, h.vertex_id));
    Ok(all_hits)
}

pub(crate) async fn collect_equal_hits_batched(
    client: &RouterIndexClient,
    anchors: &[IndexAnchor],
) -> Result<Vec<PostingHit>, String> {
    collect_equal_hits_batched_with_caller(
        anchors,
        |req| client.lookup_equal_batch(req),
        |req| client.lookup_equal_page(req),
    )
    .await
}

async fn continue_equal_page_with_caller<G, Gf>(
    call_page: &mut G,
    spec: IndexEqualSpec,
    after: Option<gleaph_graph_kernel::index::PropertyPostingCursor>,
) -> Result<Vec<PostingHit>, String>
where
    G: FnMut(LookupEqualPageRequest) -> Gf,
    Gf: Future<Output = Result<PostingHitPage, String>>,
{
    let mut hits = Vec::new();
    let mut after = after;
    loop {
        let page: PostingHitPage = call_page(LookupEqualPageRequest {
            property_id: spec.property_id,
            value: spec.value.clone(),
            after,
            limit: INDEX_LOOKUP_PAGE_LIMIT,
        })
        .await?;
        hits.extend(page.hits);
        if page.done {
            break;
        }
        after = page.next;
    }
    Ok(hits)
}

async fn lookup_anchor_hits_for_index<I: IndexLookup + ?Sized>(
    index: &I,
    anchor: &IndexAnchor,
) -> Result<SeedHits, String> {
    match anchor {
        IndexAnchor::Equal(SeedProbe {
            property_id,
            payload_bytes,
            ..
        }) => Ok(SeedHits::Vertices(
            index
                .lookup_equal(*property_id, payload_bytes.clone())
                .await?,
        )),
        _ => Err("lookup_anchor_hits_for_index only supports equality anchors".into()),
    }
}

/// `true` when every arm targets a vertex property (the planner's `IndexIntersection` shape, which
/// is vertex-only). Edge / mixed intersection still uses the server-side `lookup_intersection`.
fn all_vertex_specs(specs: &[IndexEqualSpec]) -> bool {
    (2..=MAX_EQUALITY_INTERSECTION_ARMS).contains(&specs.len())
        && specs
            .iter()
            .all(|s| matches!(s.subject, IndexSubject::VertexProperty))
}

/// Streaming all-vertex intersection on one index canister via the server-side
/// [`RouterIndexClient::lookup_intersection_page`]: the index walks the first arm one page at a time
/// and sieves each page against the remaining arms in-heap, so no arm's full bucket is materialized
/// and the walk + sieve fold into a single inter-canister call per page (vs one call per arm per
/// page). Mirrors `collect_label_intersection_hits_for_shards` for labels.
async fn collect_vertex_intersection_hits_paged(
    client: &RouterIndexClient,
    specs: &[IndexEqualSpec],
) -> Result<Vec<PostingHit>, String> {
    let mut hits = Vec::new();
    let mut after = None;
    loop {
        let page = client
            .lookup_intersection_page(LookupIntersectionPageRequest {
                specs: specs.to_vec(),
                after,
                limit: INDEX_LOOKUP_PAGE_LIMIT,
            })
            .await?;
        hits.extend(page.hits);
        if page.done {
            break;
        }
        after = page.next;
    }
    Ok(hits)
}

/// Federated index reader: resolves one or more index canisters from shard registry (ADR 0010).
#[derive(Clone, Debug)]
pub struct RouterIndexLookup {
    targets: Vec<Principal>,
    shard_index: BTreeMap<ShardId, Principal>,
}

impl RouterIndexLookup {
    pub fn from_shards(
        graph_id: GraphId,
        shards: &[gleaph_graph_kernel::federation::ShardRegistryEntry],
    ) -> Result<Self, String> {
        let store = RouterStore::new();
        let targets = store
            .graph_index_lookup_targets(graph_id)
            .map_err(|e| e.to_string())?;
        let mut shard_index = BTreeMap::new();
        for entry in shards {
            let principal = store
                .graph_index_canister_for_shard(graph_id, entry.shard_id)
                .map_err(|e| e.to_string())?;
            shard_index.insert(entry.shard_id, principal);
        }
        Ok(Self {
            targets,
            shard_index,
        })
    }

    fn require_single_target(&self, operation: &str) -> Result<Principal, String> {
        match self.targets.as_slice() {
            [] => Err("no index canister registered for logical graph".into()),
            [principal] => Ok(*principal),
            _ => Err(format!(
                "{operation} requires a single index canister per logical graph"
            )),
        }
    }

    fn client_for_shard(&self, shard_id: ShardId) -> Result<RouterIndexClient, String> {
        let principal = self
            .shard_index
            .get(&shard_id)
            .copied()
            .ok_or_else(|| format!("shard {} is not registered", shard_id.raw()))?;
        Ok(RouterIndexClient::new(principal))
    }

    fn retain_live_vertex_hits(&self, hits: Vec<PostingHit>) -> Vec<PostingHit> {
        Self::filter_live_vertex_hits(&self.shard_index, hits)
    }

    fn retain_live_edge_hits(&self, hits: Vec<EdgePostingHit>) -> Vec<EdgePostingHit> {
        Self::filter_live_edge_hits(&self.shard_index, hits)
    }

    fn filter_live_vertex_hits(
        shard_index: &BTreeMap<ShardId, Principal>,
        hits: Vec<PostingHit>,
    ) -> Vec<PostingHit> {
        hits.into_iter()
            .filter(|hit| shard_index.contains_key(&hit.shard_id))
            .collect()
    }

    fn filter_live_edge_hits(
        shard_index: &BTreeMap<ShardId, Principal>,
        hits: Vec<EdgePostingHit>,
    ) -> Vec<EdgePostingHit> {
        hits.into_iter()
            .filter(|hit| shard_index.contains_key(&hit.shard_id))
            .collect()
    }
}

fn merge_posting_hits(mut hits: Vec<PostingHit>) -> Vec<PostingHit> {
    hits.sort_unstable_by_key(|hit| (hit.shard_id, hit.vertex_id));
    hits.dedup_by_key(|hit| (hit.shard_id, hit.vertex_id));
    hits
}

fn merge_edge_posting_hits(mut hits: Vec<EdgePostingHit>) -> Vec<EdgePostingHit> {
    hits.sort_unstable_by_key(|hit| {
        (
            hit.shard_id,
            hit.owner_vertex_id,
            hit.label_id,
            hit.slot_index,
        )
    });
    hits.dedup_by(|left, right| {
        left.shard_id == right.shard_id
            && left.owner_vertex_id == right.owner_vertex_id
            && left.label_id == right.label_id
            && left.slot_index == right.slot_index
    });
    hits
}

fn merge_value_posting_counts(counts: Vec<Vec<ValuePostingCount>>) -> Vec<ValuePostingCount> {
    let mut merged: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for group in counts {
        for entry in group {
            *merged.entry(entry.encoded_value).or_insert(0) += entry.count;
        }
    }
    merged
        .into_iter()
        .map(|(encoded_value, count)| ValuePostingCount {
            encoded_value,
            count,
        })
        .collect()
}

pub(crate) trait IndexLookup {
    fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>>;

    fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexIntersectionResult, String>> + '_>>;

    fn lookup_edge_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
        label_id: Option<u16>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>>;

    fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        vertex_filter_packed: Option<Vec<u64>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>>;

    /// Batch equality lookup for a set of `IndexAnchor::Equal` anchors. Default
    /// implementation fans out to per-anchor paged equality; `RouterIndexClient` overrides
    /// with a single `lookup_equal_batch` inter-canister call.
    fn lookup_equal_batch<'a>(
        &'a self,
        anchors: &'a [IndexAnchor],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + 'a>> {
        Box::pin(async move {
            let mut hits = Vec::new();
            for anchor in anchors {
                let SeedHits::Vertices(h) = lookup_anchor_hits_for_index(self, anchor).await?
                else {
                    return Err("batched equality lookup produced edge hits".into());
                };
                hits.extend(h);
            }
            hits.sort_unstable_by_key(|h| (h.shard_id, h.vertex_id));
            hits.dedup_by_key(|h| (h.shard_id, h.vertex_id));
            Ok(hits)
        })
    }

    fn lookup_label_page(
        &self,
        req: LabelLookupPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>>;

    fn lookup_label_intersection_page(
        &self,
        req: LabelIntersectionPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
        Box::pin(async move {
            let mut page = self
                .lookup_label_page(LabelLookupPageRequest {
                    vertex_label_id: req.walk_label_id,
                    shard_id: req.shard_id,
                    after: req.after,
                    limit: req.limit,
                })
                .await?;
            for label_id in req.sieve_label_ids {
                page.hits = self.filter_hits_by_label(label_id, page.hits).await?;
                if page.hits.is_empty() {
                    break;
                }
            }
            Ok(page)
        })
    }

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
        Box::pin(collect_equal_hits_paged(self, property_id, value))
    }

    fn lookup_equal_batch<'a>(
        &'a self,
        anchors: &'a [IndexAnchor],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + 'a>> {
        Box::pin(collect_equal_hits_batched(self, anchors))
    }

    fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexIntersectionResult, String>> + '_>> {
        Box::pin(async move {
            if all_vertex_specs(&req.specs) {
                let hits = collect_vertex_intersection_hits_paged(self, &req.specs).await?;
                return Ok(IndexIntersectionResult::Vertices(merge_posting_hits(hits)));
            }
            collect_property_intersection_pages(self, req.specs).await
        })
    }

    fn lookup_edge_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
        label_id: Option<u16>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
        Box::pin(collect_edge_equal_hits_paged(
            self,
            property_id,
            value,
            label_id,
        ))
    }

    fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        vertex_filter_packed: Option<Vec<u64>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
        Box::pin(collect_value_count_pages(
            self,
            property_id,
            min_count,
            vertex_filter_packed,
        ))
    }

    fn lookup_label_page(
        &self,
        req: LabelLookupPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
        Box::pin(self.lookup_label_page(req))
    }

    fn lookup_label_intersection_page(
        &self,
        req: LabelIntersectionPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
        Box::pin(self.lookup_label_intersection_page(req))
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
        Box::pin(collect_value_count_pages_for_label(
            self,
            property_id,
            vertex_label_id,
            min_count,
        ))
    }
}

impl IndexLookup for RouterIndexLookup {
    fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        let targets = self.targets.clone();
        Box::pin(async move {
            let mut merged = Vec::new();
            for principal in targets {
                merged.extend(
                    collect_equal_hits_paged(
                        &RouterIndexClient::new(principal),
                        property_id,
                        value.clone(),
                    )
                    .await?,
                );
            }
            Ok(merge_posting_hits(self.retain_live_vertex_hits(merged)))
        })
    }

    fn lookup_equal_batch<'a>(
        &'a self,
        anchors: &'a [IndexAnchor],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + 'a>> {
        let targets = self.targets.clone();
        let shard_index = self.shard_index.clone();
        Box::pin(async move {
            let mut merged = Vec::new();
            for principal in targets {
                merged.extend(
                    collect_equal_hits_batched(&RouterIndexClient::new(principal), anchors).await?,
                );
            }
            Ok(merge_posting_hits(Self::filter_live_vertex_hits(
                &shard_index,
                merged,
            )))
        })
    }

    fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexIntersectionResult, String>> + '_>> {
        let principal = match self.require_single_target("lookup_intersection") {
            Ok(principal) => principal,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        Box::pin(async move {
            let client = RouterIndexClient::new(principal);
            let result = collect_property_intersection_pages(&client, req.specs).await?;
            Ok(match result {
                IndexIntersectionResult::Vertices(hits) => IndexIntersectionResult::Vertices(
                    merge_posting_hits(self.retain_live_vertex_hits(hits)),
                ),
                IndexIntersectionResult::Edges(hits) => IndexIntersectionResult::Edges(
                    merge_edge_posting_hits(self.retain_live_edge_hits(hits)),
                ),
            })
        })
    }

    fn lookup_edge_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
        label_id: Option<u16>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
        let targets = self.targets.clone();
        Box::pin(async move {
            let mut merged = Vec::new();
            for principal in targets {
                merged.extend(
                    collect_edge_equal_hits_paged(
                        &RouterIndexClient::new(principal),
                        property_id,
                        value.clone(),
                        label_id,
                    )
                    .await?,
                );
            }
            Ok(merge_edge_posting_hits(self.retain_live_edge_hits(merged)))
        })
    }

    fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        vertex_filter_packed: Option<Vec<u64>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
        let targets = self.targets.clone();
        Box::pin(async move {
            let mut groups = Vec::with_capacity(targets.len());
            for principal in targets {
                groups.push(
                    collect_value_count_pages(
                        &RouterIndexClient::new(principal),
                        property_id,
                        min_count,
                        vertex_filter_packed.clone(),
                    )
                    .await?,
                );
            }
            Ok(merge_value_posting_counts(groups))
        })
    }

    fn lookup_label_page(
        &self,
        req: LabelLookupPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
        let shard_id = req.shard_id;
        let principal = match self.client_for_shard(shard_id) {
            Ok(client) => client.index_canister,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        Box::pin(async move {
            RouterIndexClient::new(principal)
                .lookup_label_page(req)
                .await
        })
    }

    fn lookup_label_intersection_page(
        &self,
        req: LabelIntersectionPageRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
        let principal = match self.client_for_shard(req.shard_id) {
            Ok(client) => client.index_canister,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        Box::pin(async move {
            RouterIndexClient::new(principal)
                .lookup_label_intersection_page(req)
                .await
        })
    }

    fn lookup_label_intersection(
        &self,
        req: IndexLabelIntersectionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        let principal = match self.require_single_target("lookup_label_intersection") {
            Ok(principal) => principal,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        Box::pin(async move {
            let hits = RouterIndexClient::new(principal)
                .lookup_label_intersection(req)
                .await?;
            Ok(merge_posting_hits(self.retain_live_vertex_hits(hits)))
        })
    }

    fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: Vec<PostingHit>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
        let shard_index = self.shard_index.clone();
        Box::pin(async move {
            let mut grouped: BTreeMap<Principal, Vec<PostingHit>> = BTreeMap::new();
            for hit in hits {
                let principal = shard_index
                    .get(&hit.shard_id)
                    .copied()
                    .ok_or_else(|| format!("shard {} is not registered", hit.shard_id.raw()))?;
                grouped.entry(principal).or_default().push(hit);
            }
            let mut merged = Vec::new();
            for (principal, group) in grouped {
                merged.extend(
                    RouterIndexClient::new(principal)
                        .filter_hits_by_label(vertex_label_id, group)
                        .await?,
                );
            }
            Ok(merge_posting_hits(merged))
        })
    }

    fn count_postings_by_value_for_label(
        &self,
        property_id: u32,
        vertex_label_id: u32,
        min_count: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
        let targets = self.targets.clone();
        Box::pin(async move {
            let mut groups = Vec::with_capacity(targets.len());
            for principal in targets {
                groups.push(
                    collect_value_count_pages_for_label(
                        &RouterIndexClient::new(principal),
                        property_id,
                        vertex_label_id,
                        min_count,
                    )
                    .await?,
                );
            }
            Ok(merge_value_posting_counts(groups))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::index::PropertyPostingCursor;

    fn equal_anchor(property_id: u32, payload: &[u8]) -> IndexAnchor {
        IndexAnchor::Equal(SeedProbe {
            variable: "v".into(),
            property: "p".into(),
            property_id,
            payload_bytes: payload.to_vec(),
        })
    }

    fn posting_hit(shard_id: u32, vertex_id: u32) -> PostingHit {
        PostingHit {
            shard_id: ShardId::new(shard_id),
            vertex_id,
        }
    }

    fn page(
        hits: Vec<PostingHit>,
        done: bool,
        next: Option<PropertyPostingCursor>,
    ) -> PostingHitPage {
        PostingHitPage { hits, next, done }
    }

    fn cursor(value: Vec<u8>, shard_id: u32, vertex_id: u32) -> PropertyPostingCursor {
        PropertyPostingCursor {
            value,
            shard_id: ShardId::new(shard_id),
            vertex_id,
        }
    }

    #[test]
    fn batched_equal_lookup_dedupes_duplicate_anchors() {
        futures::executor::block_on(async {
            let anchors = vec![
                equal_anchor(1, b"a"),
                equal_anchor(1, b"a"),
                equal_anchor(1, b"b"),
            ];
            let mut batch_calls = 0;
            let hits = collect_equal_hits_batched_with_caller(
                &anchors,
                |req| {
                    batch_calls += 1;
                    assert_eq!(req.specs.len(), 2);
                    async move {
                        Ok(LookupEqualBatchResult {
                            pages: vec![
                                page(vec![posting_hit(0, 1)], true, None),
                                page(vec![posting_hit(0, 2)], true, None),
                            ],
                            next: None,
                        })
                    }
                },
                |_req| async move { unreachable!("no continuation expected") },
            )
            .await
            .unwrap();
            assert_eq!(batch_calls, 1);
            assert_eq!(hits.len(), 2);
            assert!(hits.contains(&posting_hit(0, 1)));
            assert!(hits.contains(&posting_hit(0, 2)));
        });
    }

    #[test]
    fn batched_equal_lookup_does_not_dedup_same_payload_different_property() {
        futures::executor::block_on(async {
            let anchors = vec![equal_anchor(1, b"x"), equal_anchor(2, b"x")];
            let mut batch_calls = 0;
            let hits = collect_equal_hits_batched_with_caller(
                &anchors,
                |req| {
                    batch_calls += 1;
                    assert_eq!(req.specs.len(), 2);
                    async move {
                        Ok(LookupEqualBatchResult {
                            pages: vec![
                                page(vec![posting_hit(0, 1)], true, None),
                                page(vec![posting_hit(0, 2)], true, None),
                            ],
                            next: None,
                        })
                    }
                },
                |_req| async move { unreachable!("no continuation expected") },
            )
            .await
            .unwrap();
            assert_eq!(batch_calls, 1);
            assert_eq!(hits.len(), 2);
        });
    }

    #[test]
    fn batched_equal_lookup_resumes_from_budget_next() {
        futures::executor::block_on(async {
            let anchors = vec![
                equal_anchor(1, b"a"),
                equal_anchor(1, b"b"),
                equal_anchor(1, b"c"),
            ];
            let mut calls: Vec<Vec<u32>> = Vec::new();
            let hits = collect_equal_hits_batched_with_caller(
                &anchors,
                |req| {
                    let pids: Vec<u32> = req.specs.iter().map(|s| s.property_id).collect();
                    calls.push(pids);
                    async move {
                        if req.specs.len() == 3 {
                            Ok(LookupEqualBatchResult {
                                pages: vec![
                                    page(vec![posting_hit(0, 1)], true, None),
                                    page(vec![posting_hit(0, 2)], true, None),
                                ],
                                next: Some(2),
                            })
                        } else {
                            Ok(LookupEqualBatchResult {
                                pages: vec![page(vec![posting_hit(0, 3)], true, None)],
                                next: None,
                            })
                        }
                    }
                },
                |_req| async move { unreachable!("no continuation expected") },
            )
            .await
            .unwrap();
            assert_eq!(calls, vec![vec![1, 1, 1], vec![1]]);
            assert_eq!(hits.len(), 3);
        });
    }

    #[test]
    fn batched_equal_lookup_continues_non_done_page_with_own_cursor() {
        futures::executor::block_on(async {
            let anchors = vec![equal_anchor(1, b"a")];
            let mut batch_calls = 0;
            let mut page_calls = 0;
            let hits = collect_equal_hits_batched_with_caller(
                &anchors,
                |req| {
                    batch_calls += 1;
                    assert_eq!(req.specs.len(), 1);
                    async move {
                        Ok(LookupEqualBatchResult {
                            pages: vec![page(
                                vec![posting_hit(0, 1)],
                                false,
                                Some(cursor(b"a".to_vec(), 0, 1)),
                            )],
                            next: None,
                        })
                    }
                },
                |req| {
                    page_calls += 1;
                    assert_eq!(req.property_id, 1);
                    let after = req.after.as_ref().expect("continuation should have cursor");
                    assert_eq!(after.vertex_id, 1);
                    async move { Ok(page(vec![posting_hit(0, 2)], true, None)) }
                },
            )
            .await
            .unwrap();
            assert_eq!(batch_calls, 1);
            assert_eq!(page_calls, 1);
            assert_eq!(hits.len(), 2);
        });
    }

    #[test]
    fn batched_equal_lookup_rejects_page_count_mismatch() {
        futures::executor::block_on(async {
            let anchors = vec![equal_anchor(1, b"a"), equal_anchor(1, b"b")];
            let err = collect_equal_hits_batched_with_caller(
                &anchors,
                |_req| async move {
                    Ok(LookupEqualBatchResult {
                        pages: vec![page(vec![], true, None)],
                        next: None,
                    })
                },
                |_req| async move { unreachable!() },
            )
            .await
            .unwrap_err();
            assert!(err.contains("returned 1 pages for 2 specs"));
        });
    }

    #[test]
    fn batched_equal_lookup_rejects_invalid_next() {
        futures::executor::block_on(async {
            let anchors = vec![equal_anchor(1, b"a")];
            let err = collect_equal_hits_batched_with_caller(
                &anchors,
                |_req| async move {
                    Ok(LookupEqualBatchResult {
                        pages: vec![page(vec![], true, None)],
                        next: Some(5),
                    })
                },
                |_req| async move { unreachable!() },
            )
            .await
            .unwrap_err();
            assert!(err.contains("invalid next"));
        });
    }

    #[test]
    fn batched_equal_lookup_rejects_done_false_without_next() {
        futures::executor::block_on(async {
            let anchors = vec![equal_anchor(1, b"a")];
            let err = collect_equal_hits_batched_with_caller(
                &anchors,
                |_req| async move {
                    Ok(LookupEqualBatchResult {
                        pages: vec![page(vec![], false, None)],
                        next: None,
                    })
                },
                |_req| async move { unreachable!() },
            )
            .await
            .unwrap_err();
            assert!(err.contains("done=false but next=None"));
        });
    }

    #[test]
    fn merge_posting_hits_dedupes() {
        let hits = merge_posting_hits(vec![
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 1,
            },
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 1,
            },
            PostingHit {
                shard_id: ShardId::new(1),
                vertex_id: 2,
            },
        ]);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn merge_value_posting_counts_sums() {
        let merged = merge_value_posting_counts(vec![
            vec![ValuePostingCount {
                encoded_value: vec![1],
                count: 2,
            }],
            vec![ValuePostingCount {
                encoded_value: vec![1],
                count: 3,
            }],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].count, 5);
    }

    #[test]
    fn batched_vertex_lookup_filters_non_live_shards() {
        let mut shard_index = BTreeMap::new();
        shard_index.insert(ShardId::new(0), Principal::anonymous());
        let lookup = RouterIndexLookup {
            targets: Vec::new(),
            shard_index,
        };

        let hits = vec![RouterIndexLookup::filter_live_vertex_hits(
            &lookup.shard_index,
            vec![
                PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                PostingHit {
                    shard_id: ShardId::new(1),
                    vertex_id: 8,
                },
            ],
        )];

        assert_eq!(
            hits,
            vec![vec![PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 7,
            }]]
        );
    }

    #[test]
    fn batched_edge_lookup_filters_non_live_shards() {
        let mut shard_index = BTreeMap::new();
        shard_index.insert(ShardId::new(0), Principal::anonymous());
        let lookup = RouterIndexLookup {
            targets: Vec::new(),
            shard_index,
        };

        let hits = vec![RouterIndexLookup::filter_live_edge_hits(
            &lookup.shard_index,
            vec![
                EdgePostingHit {
                    shard_id: ShardId::new(0),
                    owner_vertex_id: 7,
                    label_id: 1,
                    slot_index: 2,
                },
                EdgePostingHit {
                    shard_id: ShardId::new(1),
                    owner_vertex_id: 8,
                    label_id: 1,
                    slot_index: 3,
                },
            ],
        )];

        assert_eq!(
            hits,
            vec![vec![EdgePostingHit {
                shard_id: ShardId::new(0),
                owner_vertex_id: 7,
                label_id: 1,
                slot_index: 2,
            }]]
        );
    }
}
