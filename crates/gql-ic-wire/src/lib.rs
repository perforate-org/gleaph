//! Lightweight Candid wire types for GQL query results.
//!
//! This crate intentionally contains no graph-kernel or planner dependency. It is suitable for
//! SDK and canister runtimes that need to decode Router response blobs.

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// A path element in a GQL result value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum GqlWirePathElement {
    /// Vertex identifier.
    Vertex(Vec<u8>),
    /// Edge identifier.
    Edge(Vec<u8>),
}

/// A scalar or composite value in the Candid GQL result wire format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub enum GqlWireValue {
    Null,
    Bool(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    Int256(String),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Uint128(u128),
    Uint256(String),
    Float16(u16),
    Float32(f32),
    Float64(f64),
    Float128(Vec<u8>),
    Float256(Vec<u8>),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(i32),
    Time(u64),
    LocalTime(u64),
    DateTime {
        seconds: i64,
        nanos: u32,
    },
    LocalDateTime {
        seconds: i64,
        nanos: u32,
    },
    ZonedDateTime {
        seconds: i64,
        nanos: u32,
        offset_seconds: i32,
    },
    ZonedTime {
        nanos: u64,
        offset_seconds: i32,
    },
    Duration {
        months: i32,
        nanos: i64,
    },
    Principal(Vec<u8>),
    ExtensionLeaf {
        type_name: String,
        payload: Vec<u8>,
    },
    ValueBinary(Vec<u8>),
    List(Vec<GqlWireValue>),
    Path(Vec<GqlWirePathElement>),
    Record(Vec<(String, GqlWireValue)>),
}

/// One result row as ordered column/value pairs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CandidType)]
pub struct GqlWireRow {
    /// Columns in wire order.
    pub columns: Vec<(String, GqlWireValue)>,
}

/// Materialized rows returned in a Router response blob.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CandidType)]
pub struct GqlWireRows {
    /// Rows in result order.
    pub rows: Vec<GqlWireRow>,
}

/// Error while decoding the Router rows blob.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GqlWireDecodeError {
    /// Candid payload was not a valid GQL rows envelope.
    #[error("invalid GQL rows Candid blob: {0}")]
    Candid(String),
}

/// Decode the opaque `rows_blob` carried by a Router response.
pub fn decode_rows_blob(bytes: &[u8]) -> Result<GqlWireRows, GqlWireDecodeError> {
    candid::decode_one(bytes).map_err(|error| GqlWireDecodeError::Candid(error.to_string()))
}

/// Encode rows into the Router response blob format.
pub fn encode_rows_blob(rows: &GqlWireRows) -> Result<Vec<u8>, GqlWireDecodeError> {
    candid::encode_one(rows).map_err(|error| GqlWireDecodeError::Candid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_blob_round_trips() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![("name".into(), GqlWireValue::Text("Ada".into()))],
            }],
        };
        let decoded = decode_rows_blob(&encode_rows_blob(&rows).unwrap()).unwrap();
        assert_eq!(decoded, rows);
    }
}
