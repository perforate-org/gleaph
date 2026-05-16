//! Candid-stable encoding of [`PlanQueryResult`] for inter-canister calls.
//!
//! Column values use [`gleaph_gql_ic::IcWireValue`] (same projection rules as
//! [`gleaph_gql_ic::wire`]). Rows are `Vec<(String, IcWireValue)>` in **sorted column name order**
//! (matching [`BTreeMap`] iteration on the executor side).
//!
//! For binding rows ([`super::query::PlanQueryRow`]) that still contain [`super::query::PlanBinding::Path`],
//! materialize with [`PlanQueryResult::try_from_plan_rows`](super::query::PlanQueryResult::try_from_plan_rows)
//! (or [`run_adhoc_gql`](crate::gql_run::run_adhoc_gql)) before converting to this wire shape.

use std::collections::BTreeMap;

use candid::CandidType;
use gleaph_gql::Value;
use gleaph_gql_ic::{IcWireValue, WireError};
use serde::{Deserialize, Serialize};

use super::query::PlanQueryResult;

/// One query result row as ordered `(column name, wire value)` pairs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct IcWirePlanQueryRow {
    pub columns: Vec<(String, IcWireValue)>,
}

impl IcWirePlanQueryRow {
    pub fn try_from_value_row(row: &BTreeMap<String, Value>) -> Result<Self, WireError> {
        let mut columns = Vec::with_capacity(row.len());
        for (k, v) in row.iter() {
            columns.push((k.clone(), IcWireValue::try_from_value(v)?));
        }
        Ok(Self { columns })
    }

    pub fn try_into_value_row(self) -> Result<BTreeMap<String, Value>, WireError> {
        let mut out = BTreeMap::new();
        for (k, wv) in self.columns {
            out.insert(k, wv.try_into_value()?);
        }
        Ok(out)
    }
}

/// Full query result for Candid inter-canister boundaries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct IcWirePlanQueryResult {
    pub rows: Vec<IcWirePlanQueryRow>,
}

impl IcWirePlanQueryResult {
    pub fn try_from_plan_query_result(result: &PlanQueryResult) -> Result<Self, WireError> {
        let rows = result
            .rows
            .iter()
            .map(IcWirePlanQueryRow::try_from_value_row)
            .collect::<Result<_, _>>()?;
        Ok(Self { rows })
    }

    pub fn try_into_plan_query_result(self) -> Result<PlanQueryResult, WireError> {
        let rows = self
            .rows
            .into_iter()
            .map(IcWirePlanQueryRow::try_into_value_row)
            .collect::<Result<_, _>>()?;
        Ok(PlanQueryResult { rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_query_wire_round_trips_value_semantics() {
        let original = PlanQueryResult {
            rows: vec![BTreeMap::from([
                ("n".into(), Value::Int64(7)),
                ("label".into(), Value::Text("ok".into())),
            ])],
        };
        let wire = IcWirePlanQueryResult::try_from_plan_query_result(&original).unwrap();
        let back = wire.try_into_plan_query_result().unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn plan_query_wire_round_trips_candid_blob() {
        let original = PlanQueryResult {
            rows: vec![BTreeMap::from([
                ("x".into(), Value::Uint64(42)),
                ("y".into(), Value::Null),
            ])],
        };
        let wire = IcWirePlanQueryResult::try_from_plan_query_result(&original).unwrap();
        let bytes = candid::encode_one(&wire).expect("encode");
        let decoded: IcWirePlanQueryResult = candid::decode_one(&bytes).expect("decode");
        assert_eq!(wire, decoded);
        let back = decoded.try_into_plan_query_result().unwrap();
        assert_eq!(original, back);
    }
}
