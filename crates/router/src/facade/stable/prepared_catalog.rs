//! Stable prepared-query catalog (ADR 0007 region 29).
//!
//! Fresh-state contract (ADR 0074 slice 3): `PreparedPlanRecordV1` was destructively
//! redefined to carry the statically extracted requirement set. There is no prior-version
//! variant, migration constructor, or decode fallback; rows written before this format
//! fail to decode (fresh router state is required).

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_prepared_api::PreparedOperation;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use super::ROUTER_PREPARED_PLANS;
use crate::authz::RequirementSet;

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

/// Version 1 prepared query payload (ADR 0074 slice 3 format).
///
/// The query source is the durable source of truth. Parsed ASTs and compiled plans are rebuilt in
/// the Router heap after an upgrade and are intentionally not part of this stable record.
/// `required_privileges` is the static data-plane requirement set extracted at registration
/// by the same walker plan-time enforcement uses (`crate::authz`); it gates invariant-7
/// publication and is the primary checked artifact at execution.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PreparedPlanRecordV1 {
    /// Target graph bound at registration from the hidden source (ADR 0063 §2).
    pub graph_id: GraphId,
    pub query: String,
    /// Optional operation metadata exposed by the prepared catalog.
    pub metadata: Option<PreparedOperation>,
    /// Statically extracted data-plane requirements of `query` (ADR 0074 §4).
    pub required_privileges: RequirementSet,
}

/// Versioned prepared plan record for stable storage and upgrade-safe evolution.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum PreparedPlanRecord {
    V1(PreparedPlanRecordV1),
}

impl PreparedPlanRecord {
    pub fn as_v1(&self) -> &PreparedPlanRecordV1 {
        match self {
            PreparedPlanRecord::V1(v1) => v1,
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
        // Fail closed on any non-current payload (including pre-slice-3 rows without
        // `required_privileges`): there is no decode fallback; fresh state is required.
        Decode!(bytes.as_ref(), Self).expect("decode PreparedPlanRecord (fresh-state format)")
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

    fn record_with_requirements() -> PreparedPlanRecord {
        PreparedPlanRecord::V1(PreparedPlanRecordV1 {
            graph_id: GraphId::from_raw(1),
            query: "MATCH (n) RETURN n".into(),
            metadata: None,
            required_privileges: RequirementSet::default(),
        })
    }

    #[test]
    fn prepared_plan_record_v1_round_trips_through_storable() {
        let record = record_with_requirements();
        let bytes = record.clone().into_bytes();
        let decoded = PreparedPlanRecord::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, record);
        assert_eq!(decoded.as_v1().query, "MATCH (n) RETURN n");
        assert_eq!(decoded.as_v1().graph_id, GraphId::from_raw(1));
        assert_eq!(
            decoded.as_v1().required_privileges,
            RequirementSet::default()
        );
    }

    #[test]
    fn prepared_plan_record_v1_round_trips_metadata() {
        let record = PreparedPlanRecord::V1(PreparedPlanRecordV1 {
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
            required_privileges: RequirementSet::default(),
        });
        let bytes = record.clone().into_bytes();
        let decoded = PreparedPlanRecord::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.as_v1().metadata.as_ref().unwrap().name,
            "find-users"
        );
    }

    /// The exact pre-slice-3 payload shape (`{graph_id, query, metadata}`, no
    /// requirement set), encoded with candid like the old record stored it.
    #[derive(candid::CandidType, serde::Serialize, serde::Deserialize)]
    struct SupersededPreparedPlanRecordV1 {
        graph_id: GraphId,
        query: String,
        metadata: Option<PreparedOperation>,
    }

    #[test]
    fn superseded_record_shape_fails_decode_without_fallback() {
        let old_bytes = Encode!(&SupersededPreparedPlanRecordV1 {
            graph_id: GraphId::from_raw(3),
            query: "old shape".into(),
            metadata: None,
        })
        .expect("encode superseded shape");
        let result =
            std::panic::catch_unwind(|| PreparedPlanRecord::from_bytes(Cow::Owned(old_bytes)));
        assert!(
            result.is_err(),
            "pre-slice-3 rows must fail loudly (fresh state required), never decode"
        );
    }
}
