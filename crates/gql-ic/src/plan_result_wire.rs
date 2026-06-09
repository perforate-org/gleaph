//! Candid-stable query result rows for router ↔ graph federation merge.
//!
//! Column values use [`IcWireValue`]. Each row is `(column name, value)` pairs in sorted name order.

use std::collections::BTreeMap;

use candid::CandidType;
use gleaph_gql::Value;
use serde::{Deserialize, Serialize};

use crate::wire::{IcWireValue, WireError};

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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CandidType)]
pub struct IcWirePlanQueryResult {
    pub rows: Vec<IcWirePlanQueryRow>,
}

impl IcWirePlanQueryResult {
    pub fn try_from_value_rows(rows: &[BTreeMap<String, Value>]) -> Result<Self, WireError> {
        rows.iter()
            .map(IcWirePlanQueryRow::try_from_value_row)
            .collect::<Result<Vec<_>, _>>()
            .map(|rows| Self { rows })
    }

    pub fn try_into_value_rows(self) -> Result<Vec<BTreeMap<String, Value>>, WireError> {
        self.rows
            .into_iter()
            .map(IcWirePlanQueryRow::try_into_value_row)
            .collect()
    }

    pub fn encode_blob(&self) -> Result<Vec<u8>, WireError> {
        candid::encode_one(self).map_err(|e| WireError::Candid(e.to_string()))
    }

    pub fn decode_blob(blob: &[u8]) -> Result<Self, WireError> {
        candid::decode_one(blob).map_err(|e| WireError::Candid(e.to_string()))
    }

    /// Concatenate independent shard-local row batches (union merge).
    pub fn merge_batch_blobs(left: &[u8], right: &[u8]) -> Result<Vec<u8>, WireError> {
        let mut merged = Self::decode_blob(left)?;
        merged.rows.extend(Self::decode_blob(right)?.rows);
        merged.encode_blob()
    }

    pub fn merge_optional_batch_blobs(
        acc: Option<Vec<u8>>,
        next: Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, WireError> {
        match (acc, next) {
            (None, None) => Ok(None),
            (Some(blob), None) | (None, Some(blob)) => Ok(Some(blob)),
            (Some(left), Some(right)) => Ok(Some(Self::merge_batch_blobs(&left, &right)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_batch_blobs_concatenates_rows() {
        let left = IcWirePlanQueryResult {
            rows: vec![IcWirePlanQueryRow {
                columns: vec![("n".into(), IcWireValue::Int64(1))],
            }],
        };
        let right = IcWirePlanQueryResult {
            rows: vec![IcWirePlanQueryRow {
                columns: vec![("n".into(), IcWireValue::Int64(2))],
            }],
        };
        let merged = IcWirePlanQueryResult::decode_blob(
            &IcWirePlanQueryResult::merge_batch_blobs(
                &left.encode_blob().unwrap(),
                &right.encode_blob().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(merged.rows.len(), 2);
    }
}
