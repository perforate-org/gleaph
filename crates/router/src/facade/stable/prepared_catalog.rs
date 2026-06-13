//! Stable prepared-query catalog (ADR 0007 region 29).

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::entry::GraphId;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use super::ROUTER_PREPARED_PLANS;
use crate::state::RouterError;

/// Stable map key: logical graph + prepared query name.
#[derive(
    CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub(crate) struct PreparedPlanKey {
    pub graph_id: GraphId,
    pub name: String,
}

impl PreparedPlanKey {
    pub fn new(graph_id: GraphId, name: impl Into<String>) -> Self {
        Self {
            graph_id,
            name: name.into(),
        }
    }
}

impl Storable for PreparedPlanKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode PreparedPlanKey"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode PreparedPlanKey")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode PreparedPlanKey")
    }
}

/// Version 1 prepared plan payload (wire plan blob + execution classification).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PreparedPlanRecordV1 {
    pub plan_blob: Vec<u8>,
    pub requires_write_path: bool,
}

/// Versioned prepared plan record for stable storage and upgrade-safe evolution.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum PreparedPlanRecord {
    V1(PreparedPlanRecordV1),
}

impl PreparedPlanRecord {
    pub fn from_v1(record: PreparedPlanRecordV1) -> Self {
        Self::V1(record)
    }

    pub fn as_v1(&self) -> Result<&PreparedPlanRecordV1, RouterError> {
        match self {
            PreparedPlanRecord::V1(v1) => Ok(v1),
        }
    }
}

impl Storable for PreparedPlanRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode PreparedPlanRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode PreparedPlanRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode PreparedPlanRecord")
    }
}

pub(crate) fn insert_prepared_plan(key: PreparedPlanKey, record: PreparedPlanRecord) {
    ROUTER_PREPARED_PLANS.with_borrow_mut(|map| {
        map.insert(key, record);
    });
}

pub(crate) fn remove_prepared_plan(key: &PreparedPlanKey) {
    ROUTER_PREPARED_PLANS.with_borrow_mut(|map| {
        map.remove(key);
    });
}

pub(crate) fn get_prepared_plan(key: &PreparedPlanKey) -> Option<PreparedPlanRecord> {
    ROUTER_PREPARED_PLANS.with_borrow(|map| map.get(key))
}

pub(crate) fn contains_prepared_plan(key: &PreparedPlanKey) -> bool {
    ROUTER_PREPARED_PLANS.with_borrow(|map| map.contains_key(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_plan_key_orders_by_graph_then_name() {
        let a = PreparedPlanKey::new(GraphId::from_raw(1), "b");
        let b = PreparedPlanKey::new(GraphId::from_raw(1), "c");
        let c = PreparedPlanKey::new(GraphId::from_raw(2), "a");
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn prepared_plan_record_v1_round_trips_through_storable() {
        let record = PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
            plan_blob: vec![1, 2, 3],
            requires_write_path: true,
        });
        let bytes = record.clone().into_bytes();
        let decoded = PreparedPlanRecord::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, record);
        assert!(decoded.as_v1().expect("v1").requires_write_path);
    }
}
