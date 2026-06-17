//! Async index client surface used by GQL dispatch and federation helpers.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingHit, IndexIntersectionRequest, IndexIntersectionResult,
    IndexLabelIntersectionRequest, LabelLookupPageRequest, LabelLookupPageResult, PostingHit,
    ValuePostingCount,
};

use crate::facade::store::RouterStore;
use crate::index_client::RouterIndexClient;

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
        hits.into_iter()
            .filter(|hit| self.shard_index.contains_key(&hit.shard_id))
            .collect()
    }

    fn retain_live_edge_hits(&self, hits: Vec<EdgePostingHit>) -> Vec<EdgePostingHit> {
        hits.into_iter()
            .filter(|hit| self.shard_index.contains_key(&hit.shard_id))
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
    ) -> Pin<Box<dyn Future<Output = Result<IndexIntersectionResult, String>> + '_>> {
        Box::pin(self.lookup_intersection(req))
    }

    fn lookup_edge_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
        label_id: Option<u16>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
        Box::pin(self.lookup_edge_equal(property_id, value, label_id))
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
                    RouterIndexClient::new(principal)
                        .lookup_equal(property_id, value.clone())
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
            let result = RouterIndexClient::new(principal)
                .lookup_intersection(req)
                .await?;
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
                    RouterIndexClient::new(principal)
                        .lookup_edge_equal(property_id, value.clone(), label_id)
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
                    RouterIndexClient::new(principal)
                        .count_postings_by_value(
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
                    RouterIndexClient::new(principal)
                        .count_postings_by_value_for_label(property_id, vertex_label_id, min_count)
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
}
