use super::{DeploymentId, LogicalResource, ProvisioningIntentKey};
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

    let inference = LogicalResource::Inference;
    assert_eq!(inference.into_bytes(), vec![5, 0, 0, 0, 0]);
    assert_eq!(
        LogicalResource::from_bytes(vec![5, 0, 0, 0, 0].into()),
        inference
    );
}

#[test]
fn provisioning_intent_key_bytes_match_router_fixture() {
    let deployment = DeploymentId::new(candid::Principal::from_slice(&[1, 2, 3]));
    let key = ProvisioningIntentKey {
        deployment_id: deployment,
        logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
    };
    let bytes = key.into_bytes();
    // Length-prefixed principal bytes, then fixed 5-byte LogicalResource.
    let expected: Vec<u8> = {
        let mut out = Vec::new();
        out.extend_from_slice(&[3, 1, 2, 3]);
        out.extend_from_slice(&[0, 0, 0, 0, 0]);
        out
    };
    assert_eq!(bytes, expected);

    let decoded = ProvisioningIntentKey::from_bytes(bytes.into());
    assert_eq!(decoded.deployment_id, deployment);
    assert_eq!(
        decoded.logical_resource,
        LogicalResource::GraphShard(ShardId::new(0))
    );
}

#[test]
fn deployment_id_candid_and_stable_bytes_wrap_a_principal() {
    use candid::{Encode, Principal};
    let principal = Principal::from_slice(&[7, 3, 9]);
    let deployment = DeploymentId::new(principal);
    assert_eq!(
        Encode!(&deployment).expect("encode deployment"),
        Encode!(&principal).expect("encode principal")
    );
    let bytes = deployment.into_bytes();
    assert_eq!(bytes, vec![3, 7, 3, 9]);
    assert_eq!(DeploymentId::from_bytes(bytes.into()), deployment);
    assert_eq!(
        DeploymentId::parse("2vxsx-fae").expect("parse"),
        DeploymentId::new(Principal::anonymous())
    );
}
