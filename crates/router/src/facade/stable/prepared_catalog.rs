//! Stable prepared-query catalog (ADR 0007 region 29).

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_prepared_api::PreparedOperation;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use super::ROUTER_PREPARED_PLANS;
use crate::state::RouterError;

/// Stable map key: Router-global prepared query name (ADR 0063).
///
/// The target graph is not part of the key; it is bound at registration and stored on
/// [`PreparedPlanRecordV1::graph_id`]. Name-collision avoidance is the operator's responsibility.
#[derive(
    CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub(crate) struct PreparedPlanKey {
    pub name: String,
}

impl PreparedPlanKey {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
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

/// Version 1 prepared query payload.
///
/// The query source is the durable source of truth. Parsed ASTs and compiled plans are rebuilt in
/// the Router heap after an upgrade and are intentionally not part of this stable record.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PreparedPlanRecordV1 {
    /// Target graph bound at registration from the hidden source (ADR 0063 §2).
    pub graph_id: GraphId,
    pub query: String,
    /// Optional operation metadata exposed by the prepared catalog.
    pub metadata: Option<PreparedOperation>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_prepared_api::{OperationKind, ResultSchema};

    #[test]
    fn prepared_plan_key_orders_by_name() {
        let a = PreparedPlanKey::new("a");
        let b = PreparedPlanKey::new("b");
        let c = PreparedPlanKey::new("c");
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn prepared_plan_record_v1_round_trips_through_storable() {
        let record = PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
            graph_id: GraphId::from_raw(1),
            query: "MATCH (n) RETURN n".into(),
            metadata: None,
        });
        let bytes = record.clone().into_bytes();
        let decoded = PreparedPlanRecord::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, record);
        assert_eq!(decoded.as_v1().expect("v1").query, "MATCH (n) RETURN n");
        assert_eq!(decoded.as_v1().expect("v1").graph_id, GraphId::from_raw(1));
    }

    #[test]
    fn prepared_plan_record_v1_round_trips_metadata() {
        let record = PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
            graph_id: GraphId::from_raw(2),
            query: "MATCH (n) RETURN n".into(),
            metadata: Some(PreparedOperation {
                name: "find-users".into(),
                description: None,
                kind: OperationKind::Query,
                parameters: Vec::new(),
                result: ResultSchema {
                    columns: Vec::new(),
                },
                supports_consistency: true,
                supports_idempotency: false,
                allowed_sorts: Vec::new(),
            }),
        });
        let bytes = record.clone().into_bytes();
        let decoded = PreparedPlanRecord::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.as_v1().expect("v1").metadata.as_ref().unwrap().name,
            "find-users"
        );
    }
}
