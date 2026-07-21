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
    LookupEqualPageRequest, LookupIntersectionPageRequest, LookupPropertyIntersectionPageRequest,
    LookupValuePostingCountPageRequest, MAX_EQUALITY_INTERSECTION_ARMS, MAX_POSTING_PAGE_HITS,
    MAX_VALUE_POSTING_COUNT_PAGE_GROUPS, PostingHit, ValuePostingCount, ValuePostingCountPage,
};

use crate::facade::store::RouterStore;
use crate::index_client::RouterIndexClient;

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
