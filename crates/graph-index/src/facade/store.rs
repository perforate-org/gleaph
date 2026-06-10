//! Stateless facade over stable index storage ([`super::stable`]).
//!
//! Mirrors [`gleaph_graph::facade::GraphStore`]: coordination methods delegate to
//! `thread_local` [`RefCell`]s wrapping stable [`ic_stable_structures`] collections.

use super::stable::{
    INDEX_ADMINS, INDEX_LABEL_POSTINGS, INDEX_POSTINGS, INDEX_ROUTER, INDEX_SHARD_OWNERS,
};
use crate::init::IndexInitArgs;
use crate::key::PostingKey;
use crate::label_key::LabelPostingKey;
use crate::label_range::label_posting_bucket;
use crate::posting_range::{posting_key_half_open_range, property_posting_bucket};
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest, ValuePostingCount};

/// Default cap on groups returned by [`IndexStore::count_postings_by_value`].
pub const DEFAULT_COUNT_POSTINGS_MAX_GROUPS: usize = 10_000;

/// Stateless facade over index stable structures initialized in [`super::stable`].
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStore;

impl IndexStore {
    pub const fn new() -> Self {
        Self
    }

    /// Clears admins, shard owners, postings; seeds admins and router principal from init args.
    pub fn init_from_args(&self, args: &IndexInitArgs) {
        INDEX_ADMINS.with_borrow_mut(|admins| {
            admins.clear();
            for p in &args.controllers {
                admins.insert(*p);
            }
        });
        INDEX_SHARD_OWNERS.with_borrow_mut(|shards| shards.clear_new());
        INDEX_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_LABEL_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_ROUTER.with_borrow_mut(|router| {
            router.set(args.router_canister);
        });
    }

    pub fn bootstrap_admins(&self, principals: &[Principal]) {
        INDEX_ADMINS.with_borrow_mut(|admins| {
            for p in principals {
                admins.insert(*p);
            }
        });
    }

    fn assert_router_caller(&self, caller: Principal) -> Result<(), IndexError> {
        let router = INDEX_ROUTER.with_borrow(|r| *r.get());
        if caller != router {
            return Err(IndexError::NotAuthorized);
        }
        Ok(())
    }

    pub fn admin_set_shard_owner(
        &self,
        caller: Principal,
        shard_id: ShardId,
        owner_principal: Principal,
    ) -> Result<(), IndexError> {
        self.assert_router_caller(caller)?;
        if owner_principal == Principal::anonymous() {
            return Err(IndexError::InvalidPrincipalInRegistry);
        }
        let existing = INDEX_SHARD_OWNERS.with_borrow(|shards| shards.get(&shard_id));
        if let Some(p) = existing {
            if p != owner_principal {
                return Err(IndexError::ShardAlreadyRegistered);
            }
            return Ok(());
        }
        INDEX_SHARD_OWNERS.with_borrow_mut(|shards| {
            shards.insert(shard_id, owner_principal);
        });
        Ok(())
    }

    pub fn admin_clear_shard_owner(
        &self,
        caller: Principal,
        shard_id: ShardId,
    ) -> Result<(), IndexError> {
        self.assert_router_caller(caller)?;
        INDEX_SHARD_OWNERS.with_borrow_mut(|shards| {
            shards.remove(&shard_id);
        });
        Ok(())
    }

    pub fn posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        let key = PostingKey {
            property_id,
            value,
            shard_id,
            vertex_id,
        };
        let registered = INDEX_SHARD_OWNERS.with_borrow(|shards| shards.get(&shard_id));
        let Some(reg) = registered else {
            return Err(IndexError::UnknownShard);
        };
        if caller != reg {
            return Err(IndexError::WrongShardOwner);
        }
        INDEX_POSTINGS.with_borrow_mut(|postings| {
            postings.insert(key);
        });
        Ok(())
    }

    pub fn posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        let key = PostingKey {
            property_id,
            value,
            shard_id,
            vertex_id,
        };
        let registered = INDEX_SHARD_OWNERS.with_borrow(|shards| shards.get(&shard_id));
        let Some(reg) = registered else {
            return Err(IndexError::UnknownShard);
        };
        if caller != reg {
            return Err(IndexError::WrongShardOwner);
        }
        INDEX_POSTINGS.with_borrow_mut(|postings| {
            postings.remove(&key);
        });
        Ok(())
    }

    pub fn label_posting_insert(
        &self,
        caller: Principal,
        shard_id: ShardId,
        vertex_label_id: u32,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        let key = LabelPostingKey {
            vertex_label_id,
            shard_id,
            vertex_id,
        };
        let registered = INDEX_SHARD_OWNERS.with_borrow(|shards| shards.get(&shard_id));
        let Some(reg) = registered else {
            return Err(IndexError::UnknownShard);
        };
        if caller != reg {
            return Err(IndexError::WrongShardOwner);
        }
        INDEX_LABEL_POSTINGS.with_borrow_mut(|postings| {
            postings.insert(key);
        });
        Ok(())
    }

    pub fn label_posting_remove(
        &self,
        caller: Principal,
        shard_id: ShardId,
        vertex_label_id: u32,
        vertex_id: u32,
    ) -> Result<(), IndexError> {
        let key = LabelPostingKey {
            vertex_label_id,
            shard_id,
            vertex_id,
        };
        let registered = INDEX_SHARD_OWNERS.with_borrow(|shards| shards.get(&shard_id));
        let Some(reg) = registered else {
            return Err(IndexError::UnknownShard);
        };
        if caller != reg {
            return Err(IndexError::WrongShardOwner);
        }
        INDEX_LABEL_POSTINGS.with_borrow_mut(|postings| {
            postings.remove(&key);
        });
        Ok(())
    }

    pub fn lookup_label(&self, vertex_label_id: u32) -> Vec<PostingHit> {
        let lo = LabelPostingKey::prefix_lower(vertex_label_id);
        let hi = LabelPostingKey::prefix_upper(vertex_label_id);
        INDEX_LABEL_POSTINGS.with_borrow(|postings| {
            postings
                .range(lo..=hi)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        })
    }

    pub fn lookup_equal(&self, property_id: u32, value: &[u8]) -> Vec<PostingHit> {
        let lo = PostingKey::prefix_lower(property_id, value);
        let hi = PostingKey::prefix_upper(property_id, value);
        INDEX_POSTINGS.with_borrow(|postings| {
            postings
                .range(lo..=hi)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        })
    }

    pub fn lookup_intersection(
        &self,
        req: &gleaph_graph_kernel::index::IndexIntersectionRequest,
    ) -> Vec<PostingHit> {
        if req.specs.len() < 2 {
            return Vec::new();
        }
        let mut sets: Vec<std::collections::HashSet<u64>> = Vec::with_capacity(req.specs.len());
        for spec in &req.specs {
            let lo = PostingKey::prefix_lower(spec.property_id, &spec.value);
            let hi = PostingKey::prefix_upper(spec.property_id, &spec.value);
            let mut set = std::collections::HashSet::new();
            INDEX_POSTINGS.with_borrow(|postings| {
                for key in postings.range(lo..=hi) {
                    let packed = (u64::from(key.shard_id) << 32) | u64::from(key.vertex_id);
                    set.insert(packed);
                }
            });
            sets.push(set);
        }
        sets.sort_by_key(|set| set.len());
        let mut intersection = sets[0].clone();
        for set in sets.iter().skip(1) {
            intersection = intersection.intersection(set).copied().collect();
            if intersection.is_empty() {
                return Vec::new();
            }
        }
        intersection
            .into_iter()
            .map(|packed| PostingHit {
                shard_id: (packed >> 32) as u32,
                vertex_id: (packed & 0xFFFF_FFFF) as u32,
            })
            .collect()
    }

    /// Intersect label membership across at least two `vertex_label_id` buckets.
    pub fn lookup_label_intersection(&self, vertex_label_ids: &[u32]) -> Vec<PostingHit> {
        if vertex_label_ids.len() < 2 {
            return Vec::new();
        }
        let mut sets: Vec<std::collections::HashSet<u64>> =
            Vec::with_capacity(vertex_label_ids.len());
        for &label_id in vertex_label_ids {
            let lo = LabelPostingKey::prefix_lower(label_id);
            let hi = LabelPostingKey::prefix_upper(label_id);
            let mut set = std::collections::HashSet::new();
            INDEX_LABEL_POSTINGS.with_borrow(|postings| {
                for key in postings.range(lo..=hi) {
                    let packed = pack_posting_vertex(key.shard_id, key.vertex_id);
                    set.insert(packed);
                }
            });
            sets.push(set);
        }
        sets.sort_by_key(|set| set.len());
        let mut intersection = sets[0].clone();
        for set in sets.iter().skip(1) {
            intersection = intersection.intersection(set).copied().collect();
            if intersection.is_empty() {
                return Vec::new();
            }
        }
        intersection
            .into_iter()
            .map(|packed| PostingHit {
                shard_id: (packed >> 32) as u32,
                vertex_id: (packed & 0xFFFF_FFFF) as u32,
            })
            .collect()
    }

    /// Walk the property bucket and return `(encoded_value, global_count)` groups with `count >= min_count`.
    ///
    /// When `vertex_filter` is set, only postings whose `(shard_id, vertex_id)` appear in the set
    /// are counted (packed as `(shard_id as u64) << 32 | vertex_id as u64`).
    pub fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        max_groups: usize,
        vertex_filter: Option<&std::collections::HashSet<u64>>,
    ) -> Vec<ValuePostingCount> {
        let Some((low, high)) = property_posting_bucket(property_id) else {
            return Vec::new();
        };
        let max_groups = max_groups.max(1);
        let mut out = Vec::new();
        let mut current_value: Option<Vec<u8>> = None;
        let mut current_count: u64 = 0;

        let flush = |value: Vec<u8>, count: u64, out: &mut Vec<ValuePostingCount>| {
            if count >= min_count {
                out.push(ValuePostingCount {
                    encoded_value: value,
                    count,
                });
            }
        };

        INDEX_POSTINGS.with_borrow(|postings| {
            for key in postings.range(low..high) {
                if let Some(filter) = vertex_filter {
                    let packed = pack_posting_vertex(key.shard_id, key.vertex_id);
                    if !filter.contains(&packed) {
                        continue;
                    }
                }
                match current_value.as_ref() {
                    None => {
                        current_value = Some(key.value.clone());
                        current_count = 1;
                    }
                    Some(value) if value == &key.value => {
                        current_count = current_count.saturating_add(1);
                    }
                    Some(value) => {
                        flush(value.clone(), current_count, &mut out);
                        if out.len() >= max_groups {
                            return;
                        }
                        current_value = Some(key.value.clone());
                        current_count = 1;
                    }
                }
            }
        });

        if let Some(value) = current_value
            && out.len() < max_groups
        {
            flush(value, current_count, &mut out);
        }
        out
    }

    /// Keep only hits whose `(shard_id, vertex_id)` has a label posting for `vertex_label_id`.
    pub fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: &[PostingHit],
    ) -> Vec<PostingHit> {
        INDEX_LABEL_POSTINGS.with_borrow(|labels| {
            hits.iter()
                .copied()
                .filter(|hit| {
                    labels.contains(&LabelPostingKey {
                        vertex_label_id,
                        shard_id: hit.shard_id,
                        vertex_id: hit.vertex_id,
                    })
                })
                .collect()
        })
    }

    /// Walk one property bucket and count groups whose postings belong to `vertex_label_id`.
    pub fn count_postings_by_value_for_label(
        &self,
        property_id: u32,
        vertex_label_id: u32,
        min_count: u64,
        max_groups: usize,
    ) -> Vec<ValuePostingCount> {
        let Some((low, high)) = property_posting_bucket(property_id) else {
            return Vec::new();
        };
        let max_groups = max_groups.max(1);
        let mut out = Vec::new();
        let mut current_value: Option<Vec<u8>> = None;
        let mut current_count: u64 = 0;

        let flush = |value: Vec<u8>, count: u64, out: &mut Vec<ValuePostingCount>| {
            if count >= min_count {
                out.push(ValuePostingCount {
                    encoded_value: value,
                    count,
                });
            }
        };

        INDEX_POSTINGS.with_borrow(|postings| {
            INDEX_LABEL_POSTINGS.with_borrow(|labels| {
                for key in postings.range(low..high) {
                    if !labels.contains(&LabelPostingKey {
                        vertex_label_id,
                        shard_id: key.shard_id,
                        vertex_id: key.vertex_id,
                    }) {
                        continue;
                    }
                    match current_value.as_ref() {
                        None => {
                            current_value = Some(key.value.clone());
                            current_count = 1;
                        }
                        Some(value) if value == &key.value => {
                            current_count = current_count.saturating_add(1);
                        }
                        Some(value) => {
                            flush(value.clone(), current_count, &mut out);
                            if out.len() >= max_groups {
                                return;
                            }
                            current_value = Some(key.value.clone());
                            current_count = 1;
                        }
                    }
                }
            });
        });

        if let Some(value) = current_value
            && out.len() < max_groups
        {
            flush(value, current_count, &mut out);
        }
        out
    }

    /// Half-open `[low, high)` scan over postings for `property_id` using encoded-value [`PostingRangeRequest`].
    pub fn lookup_range(&self, property_id: u32, req: &PostingRangeRequest) -> Vec<PostingHit> {
        let Some((low, high)) = posting_key_half_open_range(property_id, req) else {
            return Vec::new();
        };
        if low >= high {
            return Vec::new();
        }
        INDEX_POSTINGS.with_borrow(|postings| {
            postings
                .range(low..high)
                .map(|k| PostingHit {
                    shard_id: k.shard_id,
                    vertex_id: k.vertex_id,
                })
                .collect()
        })
    }
}

pub(crate) fn pack_posting_vertex(shard_id: ShardId, vertex_id: u32) -> u64 {
    (u64::from(shard_id) << 32) | u64::from(vertex_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IndexError;
    use candid::Principal;
    use gleaph_gql::{Value, value_to_index_key_bytes};
    use gleaph_gql_ic::PrincipalValue;

    fn index_key(value: gleaph_gql::Value) -> Vec<u8> {
        value_to_index_key_bytes(&value).unwrap().unwrap()
    }

    fn test_router() -> Principal {
        Principal::from_slice(&[9])
    }

    fn init_test_store(store: &IndexStore) -> Principal {
        let router = test_router();
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
            router_canister: router,
        });
        router
    }

    fn register_shard_owner(
        store: &IndexStore,
        router: Principal,
        shard_id: u32,
        owner: Principal,
    ) {
        store
            .admin_set_shard_owner(router, shard_id, owner)
            .expect("set shard owner");
    }

    #[test]
    fn count_postings_by_value_groups_across_shards() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_a = Principal::from_slice(&[1]);
        let shard_b = Principal::from_slice(&[2]);
        register_shard_owner(&store, router, 7, shard_a);
        register_shard_owner(&store, router, 9, shard_b);

        let property_id = 42;
        let us = index_key(Value::Text("US".into()));
        let uk = index_key(Value::Text("UK".into()));
        for (shard, owner, vid) in [
            (7, shard_a, 1),
            (7, shard_a, 2),
            (9, shard_b, 3),
            (7, shard_a, 4),
        ] {
            store
                .posting_insert(owner, shard, property_id, us.clone(), vid)
                .expect("insert us");
        }
        store
            .posting_insert(shard_a, 7, property_id, uk.clone(), 5)
            .expect("insert uk");

        let counts = store.count_postings_by_value(property_id, 2, 100, None);
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].encoded_value, us);
        assert_eq!(counts[0].count, 4);

        let all = store.count_postings_by_value(property_id, 1, 100, None);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn count_postings_by_value_respects_vertex_filter() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_a = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_a);

        let property_id = 42;
        let us = index_key(Value::Text("US".into()));
        let uk = index_key(Value::Text("UK".into()));
        store
            .posting_insert(shard_a, 7, property_id, us.clone(), 1)
            .expect("us");
        store
            .posting_insert(shard_a, 7, property_id, us.clone(), 2)
            .expect("us");
        store
            .posting_insert(shard_a, 7, property_id, uk.clone(), 3)
            .expect("uk");

        let mut filter = std::collections::HashSet::new();
        filter.insert(pack_posting_vertex(7, 1));
        let counts = store.count_postings_by_value(property_id, 1, 100, Some(&filter));
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].encoded_value, us);
        assert_eq!(counts[0].count, 1);
    }

    #[test]
    fn insert_and_lookup_equal() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        store
            .posting_insert(shard_principal, 7, 42, b"v".to_vec(), 100)
            .expect("insert");

        let hits = store.lookup_equal(42, b"v");
        assert_eq!(
            hits,
            vec![PostingHit {
                shard_id: 7,
                vertex_id: 100
            }]
        );
    }

    #[test]
    fn insert_and_lookup_equal_principal_value_index_key() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        let p = Principal::from_text("aaaaa-aa").expect("management id");
        let key = index_key(Value::from(PrincipalValue(p)));

        store
            .posting_insert(shard_principal, 7, 42, key.clone(), 100)
            .expect("insert");

        let hits = store.lookup_equal(42, &key);
        assert_eq!(
            hits,
            vec![PostingHit {
                shard_id: 7,
                vertex_id: 100
            }]
        );
    }

    #[test]
    fn lookup_range_ge_and_lt_use_encoded_lex_order() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        for (vid, val) in [
            (100u32, vec![1u8]),
            (200u32, vec![2u8]),
            (300u32, vec![3u8]),
        ] {
            store
                .posting_insert(shard_principal, 7, 42, val, vid)
                .expect("insert");
        }

        let mut ge2: Vec<u32> = store
            .lookup_range(42, &PostingRangeRequest::Ge(vec![2]))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        ge2.sort_unstable();
        assert_eq!(ge2, vec![200, 300]);

        let mut lt2: Vec<u32> = store
            .lookup_range(42, &PostingRangeRequest::Lt(vec![2]))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        lt2.sort_unstable();
        assert_eq!(lt2, vec![100]);
    }

    #[test]
    fn lookup_range_respects_sortable_value_key_boundaries() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        for (vid, value) in [
            (10u32, gleaph_gql::Value::Int64(-1)),
            (20u32, gleaph_gql::Value::Uint8(0)),
            (30u32, gleaph_gql::Value::Int16(5)),
            (40u32, gleaph_gql::Value::Uint64(9)),
        ] {
            store
                .posting_insert(shard_principal, 7, 42, index_key(value), vid)
                .expect("insert");
        }

        let five = index_key(gleaph_gql::Value::Uint8(5));
        let mut ge5: Vec<u32> = store
            .lookup_range(42, &PostingRangeRequest::Ge(five.clone()))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        ge5.sort_unstable();
        assert_eq!(ge5, vec![30, 40]);

        let mut lt5: Vec<u32> = store
            .lookup_range(42, &PostingRangeRequest::Lt(five))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        lt5.sort_unstable();
        assert_eq!(lt5, vec![10, 20]);
    }

    #[test]
    fn lookup_range_text_prefix_boundaries_are_exact() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        for (vid, value) in [
            (1u32, gleaph_gql::Value::Text("a".into())),
            (2u32, gleaph_gql::Value::Text("a\0".into())),
            (3u32, gleaph_gql::Value::Text("aa".into())),
        ] {
            store
                .posting_insert(shard_principal, 7, 77, index_key(value), vid)
                .expect("insert");
        }

        let a = index_key(gleaph_gql::Value::Text("a".into()));
        assert_eq!(store.lookup_equal(77, &a)[0].vertex_id, 1);

        let mut gt_a: Vec<u32> = store
            .lookup_range(77, &PostingRangeRequest::Gt(a))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        gt_a.sort_unstable();
        assert_eq!(gt_a, vec![2, 3]);
    }

    #[test]
    fn lookup_range_respects_list_value_key_boundaries() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        let values = [
            (10u32, gleaph_gql::Value::List(vec![])),
            (
                20u32,
                gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(1)]),
            ),
            (
                30u32,
                gleaph_gql::Value::List(vec![
                    gleaph_gql::Value::Int64(1),
                    gleaph_gql::Value::Int64(2),
                ]),
            ),
            (
                40u32,
                gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(2)]),
            ),
        ];
        for (vid, value) in values {
            store
                .posting_insert(shard_principal, 7, 88, index_key(value), vid)
                .expect("insert");
        }

        let one = index_key(gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(1)]));
        let two = index_key(gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(2)]));

        let mut ge_one: Vec<u32> = store
            .lookup_range(88, &PostingRangeRequest::Ge(one))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        ge_one.sort_unstable();
        assert_eq!(ge_one, vec![20, 30, 40]);

        let mut lt_two: Vec<u32> = store
            .lookup_range(88, &PostingRangeRequest::Lt(two))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        lt_two.sort_unstable();
        assert_eq!(lt_two, vec![10, 20, 30]);
    }

    #[test]
    fn lookup_range_respects_record_value_key_boundaries() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        for (vid, value) in [
            (
                10u32,
                gleaph_gql::Value::Record(vec![("a".into(), gleaph_gql::Value::Int64(1))]),
            ),
            (
                20u32,
                gleaph_gql::Value::Record(vec![("a".into(), gleaph_gql::Value::Int64(2))]),
            ),
            (
                30u32,
                gleaph_gql::Value::Record(vec![("b".into(), gleaph_gql::Value::Int64(1))]),
            ),
        ] {
            store
                .posting_insert(shard_principal, 7, 99, index_key(value), vid)
                .expect("insert");
        }

        let same_key = index_key(gleaph_gql::Value::Record(vec![
            ("b".into(), gleaph_gql::Value::Int64(2)),
            ("a".into(), gleaph_gql::Value::Int64(1)),
        ]));
        assert_eq!(
            same_key,
            index_key(gleaph_gql::Value::Record(vec![
                ("a".into(), gleaph_gql::Value::Int64(1)),
                ("b".into(), gleaph_gql::Value::Int64(2)),
            ]))
        );

        let bound = index_key(gleaph_gql::Value::Record(vec![(
            "a".into(),
            gleaph_gql::Value::Int64(2),
        )]));
        let mut ge_bound: Vec<u32> = store
            .lookup_range(99, &PostingRangeRequest::Ge(bound))
            .into_iter()
            .map(|h| h.vertex_id)
            .collect();
        ge_bound.sort_unstable();
        assert_eq!(ge_bound, vec![20, 30]);
    }

    #[test]
    fn admin_set_shard_owner_idempotent_same_principal() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard = Principal::from_slice(&[2]);
        store
            .admin_set_shard_owner(router, 1, shard)
            .expect("first register");
        store
            .admin_set_shard_owner(router, 1, shard)
            .expect("idempotent re-register");
    }

    #[test]
    fn admin_set_shard_owner_rejects_principal_change() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let a = Principal::self_authenticating([1u8; 32]);
        let b = Principal::self_authenticating([2u8; 32]);
        store.admin_set_shard_owner(router, 9, a).unwrap();
        assert_eq!(
            store.admin_set_shard_owner(router, 9, b),
            Err(IndexError::ShardAlreadyRegistered)
        );
    }

    #[test]
    fn admin_set_shard_owner_rejects_anonymous_owner() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        assert_eq!(
            store.admin_set_shard_owner(router, 3, Principal::anonymous()),
            Err(IndexError::InvalidPrincipalInRegistry)
        );
    }

    #[test]
    fn admin_set_shard_owner_rejects_non_router_caller() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let other = Principal::from_slice(&[8]);
        assert_eq!(
            store.admin_set_shard_owner(other, 1, Principal::from_slice(&[1])),
            Err(IndexError::NotAuthorized)
        );
        let _ = router;
    }

    #[test]
    fn lookup_intersection_returns_vertices_in_all_specs() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        store
            .posting_insert(shard_principal, 7, 1, b"alice".to_vec(), 10)
            .expect("uid alice v10");
        store
            .posting_insert(shard_principal, 7, 1, b"alice".to_vec(), 20)
            .expect("uid alice v20");
        store
            .posting_insert(shard_principal, 7, 2, b"a@b.c".to_vec(), 20)
            .expect("email v20");
        store
            .posting_insert(shard_principal, 7, 2, b"a@b.c".to_vec(), 30)
            .expect("email v30");

        let hits =
            store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
                specs: vec![
                    gleaph_graph_kernel::index::IndexEqualSpec {
                        property_id: 1,
                        value: b"alice".to_vec(),
                    },
                    gleaph_graph_kernel::index::IndexEqualSpec {
                        property_id: 2,
                        value: b"a@b.c".to_vec(),
                    },
                ],
            });
        assert_eq!(
            hits,
            vec![PostingHit {
                shard_id: 7,
                vertex_id: 20
            }]
        );
    }

    #[test]
    fn lookup_intersection_empty_when_disjoint() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        store
            .posting_insert(shard_principal, 7, 1, b"alice".to_vec(), 10)
            .expect("uid");
        store
            .posting_insert(shard_principal, 7, 2, b"bob".to_vec(), 20)
            .expect("email");

        let hits =
            store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
                specs: vec![
                    gleaph_graph_kernel::index::IndexEqualSpec {
                        property_id: 1,
                        value: b"alice".to_vec(),
                    },
                    gleaph_graph_kernel::index::IndexEqualSpec {
                        property_id: 2,
                        value: b"bob".to_vec(),
                    },
                ],
            });
        assert!(hits.is_empty());
    }

    #[test]
    fn lookup_intersection_requires_two_specs() {
        let store = IndexStore::new();
        let hits =
            store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
                specs: vec![gleaph_graph_kernel::index::IndexEqualSpec {
                    property_id: 1,
                    value: b"x".to_vec(),
                }],
            });
        assert!(hits.is_empty());
    }

    #[test]
    fn insert_and_lookup_label() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        store
            .label_posting_insert(shard_principal, 7, 3, 100)
            .expect("insert");
        store
            .label_posting_insert(shard_principal, 7, 3, 200)
            .expect("insert");
        store
            .label_posting_insert(shard_principal, 7, 4, 300)
            .expect("other label");

        let hits = store.lookup_label(3);
        assert_eq!(hits.len(), 2);
        assert!(hits.contains(&PostingHit {
            shard_id: 7,
            vertex_id: 100
        }));
        assert!(hits.contains(&PostingHit {
            shard_id: 7,
            vertex_id: 200
        }));
        assert!(store.lookup_label(4).contains(&PostingHit {
            shard_id: 7,
            vertex_id: 300
        }));
    }

    #[test]
    fn label_posting_remove() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        store
            .label_posting_insert(shard_principal, 7, 1, 10)
            .expect("insert");
        store
            .label_posting_remove(shard_principal, 7, 1, 10)
            .expect("remove");
        assert!(store.lookup_label(1).is_empty());
    }

    #[test]
    fn filter_hits_by_label_keeps_members_only() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        store
            .label_posting_insert(shard_principal, 7, 2, 10)
            .expect("label");
        store
            .label_posting_insert(shard_principal, 7, 2, 30)
            .expect("label");

        let hits = vec![
            PostingHit {
                shard_id: 7,
                vertex_id: 10,
            },
            PostingHit {
                shard_id: 7,
                vertex_id: 20,
            },
            PostingHit {
                shard_id: 7,
                vertex_id: 30,
            },
        ];
        let filtered = store.filter_hits_by_label(2, &hits);
        assert_eq!(
            filtered,
            vec![
                PostingHit {
                    shard_id: 7,
                    vertex_id: 10
                },
                PostingHit {
                    shard_id: 7,
                    vertex_id: 30
                },
            ]
        );
    }

    #[test]
    fn lookup_label_intersection_returns_common_vertices() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        for vid in [10u32, 20, 30] {
            store
                .label_posting_insert(shard_principal, 7, 1, vid)
                .expect("L1");
            store
                .label_posting_insert(shard_principal, 7, 2, vid)
                .expect("L2");
        }
        store
            .label_posting_insert(shard_principal, 7, 1, 40)
            .expect("L1 only");

        let hits = store.lookup_label_intersection(&[1, 2]);
        assert_eq!(hits.len(), 3);
        assert!(hits.contains(&PostingHit {
            shard_id: 7,
            vertex_id: 10
        }));
        assert!(!hits.iter().any(|hit| hit.vertex_id == 40));
    }

    #[test]
    fn count_postings_by_value_for_label_sieves_by_label() {
        let store = IndexStore::new();
        let router = init_test_store(&store);
        let shard_principal = Principal::from_slice(&[1]);
        register_shard_owner(&store, router, 7, shard_principal);

        let property_id = 42;
        let us = index_key(Value::Text("US".into()));
        let uk = index_key(Value::Text("UK".into()));
        for vid in [1, 2, 3] {
            store
                .posting_insert(shard_principal, 7, property_id, us.clone(), vid)
                .expect("us");
            store
                .label_posting_insert(shard_principal, 7, 5, vid)
                .expect("person");
        }
        store
            .posting_insert(shard_principal, 7, property_id, uk.clone(), 4)
            .expect("uk unlabeled");

        let counts = store.count_postings_by_value_for_label(property_id, 5, 1, 100);
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].encoded_value, us);
        assert_eq!(counts[0].count, 3);
    }
}
