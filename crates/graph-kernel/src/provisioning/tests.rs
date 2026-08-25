use super::{LogicalResource, ProvisioningIntentKey};
use crate::federation::{IndexClusterId, ShardId, TextIndexId, VectorIndexId};
use ic_stable_structures::Storable;

#[test]
fn logical_resource_bytes_roundtrip() {
    let shard = LogicalResource::GraphShard(ShardId::new(0));
    assert_eq!(shard.into_bytes(), vec![0, 0, 0, 0, 0]);
    assert_eq!(
        LogicalResource::from_bytes(vec![0, 0, 0, 0, 0].into()),
        shard
    );

    let index = LogicalResource::PropertyIndex(IndexClusterId::new(3));
    assert_eq!(index.into_bytes(), vec![1, 3, 0, 0, 0]);
    assert_eq!(
        LogicalResource::from_bytes(vec![1, 3, 0, 0, 0].into()),
        index
    );

    let vector = LogicalResource::VectorIndex(VectorIndexId::new(9));
    assert_eq!(vector.into_bytes(), vec![2, 9, 0, 0, 0]);
    assert_eq!(
        LogicalResource::from_bytes(vec![2, 9, 0, 0, 0].into()),
        vector
    );

    let router = LogicalResource::Router;
    assert_eq!(router.into_bytes(), vec![3, 0, 0, 0, 0]);
    assert_eq!(
        LogicalResource::from_bytes(vec![3, 0, 0, 0, 0].into()),
        router
    );

    let text = LogicalResource::TextIndex(TextIndexId::new(7));
    assert_eq!(text.into_bytes(), vec![4, 7, 0, 0, 0]);
    assert_eq!(
        LogicalResource::from_bytes(vec![4, 7, 0, 0, 0].into()),
        text
    );
}

#[test]
fn provisioning_intent_key_bytes_match_router_fixture() {
    let key = ProvisioningIntentKey {
        deployment_id: "dep-1".to_owned(),
        logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
    };
    let bytes = key.into_bytes();
    // Length-prefixed deployment_id, then fixed 5-byte LogicalResource.
    // dep-1 = 5 chars -> 05 00 00 00 "dep-1" 00 00 00 00 00
    let expected: Vec<u8> = {
        let mut out = Vec::new();
        out.extend_from_slice(&5u32.to_le_bytes());
        out.extend_from_slice(b"dep-1");
        out.extend_from_slice(&[0, 0, 0, 0, 0]);
        out
    };
    assert_eq!(bytes, expected);

    let decoded = ProvisioningIntentKey::from_bytes(bytes.into());
    assert_eq!(decoded.deployment_id, "dep-1");
    assert_eq!(
        decoded.logical_resource,
        LogicalResource::GraphShard(ShardId::new(0))
    );
}
