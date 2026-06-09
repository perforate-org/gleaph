//! Candid-stable encoding of [`PlanQueryResult`] for inter-canister calls.
//!
//! Wire shapes live in [`gleaph_gql_ic::plan_result_wire`]. Rows must be fully materialized
//! [`Value`] maps before conversion (no lazy [`PlanBinding::Path`] in binding rows).

pub use gleaph_gql_ic::{IcWirePlanQueryResult, IcWirePlanQueryRow, WireError};

use super::query::PlanQueryResult;

pub fn ic_wire_from_plan_query_result(
    result: &PlanQueryResult,
) -> Result<IcWirePlanQueryResult, WireError> {
    IcWirePlanQueryResult::try_from_value_rows(&result.rows)
}

pub fn plan_query_result_from_ic_wire(
    wire: IcWirePlanQueryResult,
) -> Result<PlanQueryResult, WireError> {
    Ok(PlanQueryResult {
        rows: wire.try_into_value_rows()?,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use gleaph_gql::Value;
    use gleaph_gql_ic::IcWirePlanQueryResult;

    use super::*;

    #[test]
    fn plan_query_wire_round_trips_value_semantics() {
        let original = PlanQueryResult {
            rows: vec![BTreeMap::from([
                ("n".into(), Value::Int64(7)),
                ("label".into(), Value::Text("ok".into())),
            ])],
        };
        let wire = ic_wire_from_plan_query_result(&original).unwrap();
        let back = plan_query_result_from_ic_wire(wire).unwrap();
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
        let wire = ic_wire_from_plan_query_result(&original).unwrap();
        let bytes = wire.encode_blob().expect("encode");
        let decoded = IcWirePlanQueryResult::decode_blob(&bytes).expect("decode");
        assert_eq!(wire, decoded);
        let back = plan_query_result_from_ic_wire(decoded).unwrap();
        assert_eq!(original, back);
    }
}
