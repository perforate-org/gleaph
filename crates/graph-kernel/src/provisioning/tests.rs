use super::{ProvisionableResourceKind, ProvisioningIntentKey};
use ic_stable_structures::Storable;

#[test]
fn provisionable_resource_kind_bytes_match_router_fixture() {
    assert_eq!(ProvisionableResourceKind::GraphShard.into_bytes(), vec![0]);
    assert_eq!(
        ProvisionableResourceKind::PropertyIndex.into_bytes(),
        vec![1]
    );
    assert_eq!(ProvisionableResourceKind::VectorIndex.into_bytes(), vec![2]);
    assert_eq!(
        ProvisionableResourceKind::from_bytes(vec![0].into()),
        ProvisionableResourceKind::GraphShard
    );
    assert_eq!(
        ProvisionableResourceKind::from_bytes(vec![1].into()),
        ProvisionableResourceKind::PropertyIndex
    );
    assert_eq!(
        ProvisionableResourceKind::from_bytes(vec![2].into()),
        ProvisionableResourceKind::VectorIndex
    );
}

#[test]
fn provisioning_intent_key_bytes_match_router_fixture() {
    let key = ProvisioningIntentKey {
        deployment_id: "dep-1".to_owned(),
        resource_kind: ProvisionableResourceKind::PropertyIndex,
        logical_resource_key: "shard-0".to_owned(),
    };
    let bytes = key.into_bytes();
    // Length-prefixed deployment_id, one-byte kind, length-prefixed logical_resource_key.
    // dep-1 = 5 chars -> 05 00 00 00 "dep-1" 01 09 00 00 00 "shard-0"
    let expected: Vec<u8> = {
        let mut out = Vec::new();
        out.extend_from_slice(&5u32.to_le_bytes());
        out.extend_from_slice(b"dep-1");
        out.push(1);
        out.extend_from_slice(&7u32.to_le_bytes());
        out.extend_from_slice(b"shard-0");
        out
    };
    assert_eq!(bytes, expected);

    let decoded = ProvisioningIntentKey::from_bytes(bytes.into());
    assert_eq!(decoded.deployment_id, "dep-1");
    assert_eq!(
        decoded.resource_kind,
        ProvisionableResourceKind::PropertyIndex
    );
    assert_eq!(decoded.logical_resource_key, "shard-0");
}
