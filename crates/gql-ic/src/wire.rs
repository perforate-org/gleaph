//! Candid-oriented wire values: lossless bridge to [`gleaph_gql::Value`].
//!
//! The wire value enum, path element, and error type are single-sourced in
//! [`gleaph_gql_ic_wire`] and re-exported from the crate root. This module additionally owns
//! the compact-binary GQL **parameter** blob helpers, which are a gql-ic concern.
//!
//! ## Canister GQL parameters (preferred)
//!
//! Pass a single [`Vec<u8>`] at the IC boundary: [`encode_gql_params_blob`] /
//! [`decode_gql_params_blob`] — one compact-binary [`Value::Record`] (same codec as
//! [`Value::to_binary_bytes`]), not a Candid-deep wire value tree.

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql_ic_wire::GqlWireDecodeError;

/// Encode GQL named parameters for the graph canister: one compact-binary [`Value::Record`].
#[inline]
pub fn encode_gql_params_blob(fields: Vec<(String, Value)>) -> Result<Vec<u8>, GqlWireDecodeError> {
    Value::Record(fields).to_binary_bytes().map_err(Into::into)
}

/// Decode [`encode_gql_params_blob`] output into a parameter map. Empty input yields an empty map.
pub fn decode_gql_params_blob(bytes: &[u8]) -> Result<BTreeMap<String, Value>, GqlWireDecodeError> {
    if bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let v = Value::from_binary_bytes_with_extensions(bytes, ic_extension_decode())?;
    match v {
        Value::Record(fields) => Ok(fields.into_iter().collect()),
        _ => Err(GqlWireDecodeError::ParamsTopLevelNotRecord),
    }
}

/// Decode extension / value compact blobs using the default IC decoder (Principal, …).
#[inline]
pub fn ic_extension_decode() -> &'static IcExtensionBinaryDecode {
    &IcExtensionBinaryDecode::INSTANCE
}

use crate::IcExtensionBinaryDecode;
