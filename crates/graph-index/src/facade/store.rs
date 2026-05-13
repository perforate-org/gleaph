//! Stateless facade over stable index storage ([`super::stable`]).
//!
//! Mirrors [`gleaph_graph::facade::GraphStore`]: coordination methods delegate to
//! `thread_local` [`RefCell`]s wrapping stable [`ic_stable_structures`] collections.

use super::stable::{INDEX_ADMINS, INDEX_POSTINGS, INDEX_SHARDS};
use crate::init::IndexInitArgs;
use crate::key::PostingKey;
use crate::posting_range::posting_key_half_open_range;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};

/// Stateless facade over index stable structures initialized in [`super::stable`].
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStore;

impl IndexStore {
    pub const fn new() -> Self {
        Self
    }

    /// Clears admins, shard registry, and postings; seeds admins from init args (canister [`init`]).
    pub fn init_from_args(&self, args: &IndexInitArgs) {
        INDEX_ADMINS.with_borrow_mut(|admins| {
            admins.clear();
            for p in &args.controllers {
                admins.insert(*p);
            }
        });
        INDEX_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        INDEX_POSTINGS.with_borrow_mut(|postings| postings.clear());
    }

    pub fn bootstrap_admins(&self, principals: &[Principal]) {
        INDEX_ADMINS.with_borrow_mut(|admins| {
            for p in principals {
                admins.insert(*p);
            }
        });
    }

    pub fn admin_register_shard(
        &self,
        caller: Principal,
        shard_id: u64,
        shard_principal: Principal,
    ) -> Result<(), IndexError> {
        let authorized = INDEX_ADMINS.with_borrow(|admins| admins.contains(&caller));
        if !authorized {
            return Err(IndexError::NotAuthorized);
        }
        if shard_principal == Principal::anonymous() {
            return Err(IndexError::InvalidPrincipalInRegistry);
        }
        let existing = INDEX_SHARDS.with_borrow(|shards| shards.get(&shard_id));
        if let Some(p) = existing {
            if p != shard_principal {
                return Err(IndexError::ShardAlreadyRegistered);
            }
            return Ok(());
        }
        INDEX_SHARDS.with_borrow_mut(|shards| {
            shards.insert(shard_id, shard_principal);
        });
        Ok(())
    }

    pub fn posting_insert(
        &self,
        caller: Principal,
        shard_id: u64,
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
        let registered = INDEX_SHARDS.with_borrow(|shards| shards.get(&shard_id));
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
        shard_id: u64,
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
        let registered = INDEX_SHARDS.with_borrow(|shards| shards.get(&shard_id));
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

    pub fn resolve_shard_principal(&self, shard_id: u64) -> Option<Principal> {
        INDEX_SHARDS.with_borrow(|shards| shards.get(&shard_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IndexError;
    use candid::Principal;
    use gleaph_gql::value_to_index_key_bytes;

    fn index_key(value: gleaph_gql::Value) -> Vec<u8> {
        value_to_index_key_bytes(&value).unwrap().unwrap()
    }

    #[test]
    fn insert_and_lookup_equal() {
        let store = IndexStore::new();
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let shard_principal = Principal::from_slice(&[1]);
        store
            .admin_register_shard(admin, 7, shard_principal)
            .expect("register shard");

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
    fn lookup_range_ge_and_lt_use_encoded_lex_order() {
        let store = IndexStore::new();
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let shard_principal = Principal::from_slice(&[1]);
        store
            .admin_register_shard(admin, 7, shard_principal)
            .expect("register shard");

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
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let shard_principal = Principal::from_slice(&[1]);
        store
            .admin_register_shard(admin, 7, shard_principal)
            .expect("register shard");

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
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let shard_principal = Principal::from_slice(&[1]);
        store
            .admin_register_shard(admin, 7, shard_principal)
            .expect("register shard");

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
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let shard_principal = Principal::from_slice(&[1]);
        store
            .admin_register_shard(admin, 7, shard_principal)
            .expect("register shard");

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
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let shard_principal = Principal::from_slice(&[1]);
        store
            .admin_register_shard(admin, 7, shard_principal)
            .expect("register shard");

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
    fn admin_register_shard_idempotent_same_principal() {
        let store = IndexStore::new();
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let shard = Principal::from_slice(&[2]);
        store
            .admin_register_shard(admin, 1, shard)
            .expect("first register");
        store
            .admin_register_shard(admin, 1, shard)
            .expect("idempotent re-register");
    }

    #[test]
    fn admin_register_shard_rejects_principal_change() {
        let store = IndexStore::new();
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        let a = Principal::self_authenticating([1u8; 32]);
        let b = Principal::self_authenticating([2u8; 32]);
        store.admin_register_shard(admin, 9, a).unwrap();
        assert_eq!(
            store.admin_register_shard(admin, 9, b),
            Err(IndexError::ShardAlreadyRegistered)
        );
    }

    #[test]
    fn admin_register_shard_rejects_anonymous_shard_principal() {
        let store = IndexStore::new();
        store.init_from_args(&IndexInitArgs {
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_admins(&[admin]);
        assert_eq!(
            store.admin_register_shard(admin, 3, Principal::anonymous()),
            Err(IndexError::InvalidPrincipalInRegistry)
        );
    }
}
