//! Unit tests for the `gql_extension!` proc macro (`define_gql_extension!`).
//!
//! These live in `gleaph-gql` — the layer that owns the macro contract — rather
//! than in `gleaph-gql-value`: the macro expansion hardcodes `::gleaph_gql::value::*`
//! paths, which resolve here via `extern crate self as gleaph_gql` but would point
//! at a second crate instance inside `gleaph-gql-value`'s own unit-test build.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;

use crate::value::cmp::compare_values;
use gleaph_gql_macros::define_gql_extension;
use gleaph_gql_value::{ExtensionValue, Value, ValueBinaryError};

#[derive(Debug, Clone)]
struct MacroTokenExt(Vec<u8>);

impl fmt::Display for MacroTokenExt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MacroTokenExt")
    }
}

fn decode_tt_compact(payload: &[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
    Ok(Box::new(MacroTokenExt(payload.to_vec())))
}

define_gql_extension! {
    prefix: "TT",
    types: [
        {
            rust_type: MacroTokenExt,
            type_name: "test.MacroToken",
            decoder: MacroTokenGqlDecode,
            eq: |this, other| this.0 == other.0,
            binary_payload: |this| Cow::Borrowed(this.0.as_slice()),
            compact: { 200u8 => decode_tt_compact },
        },
    ],
}

#[derive(Debug, Clone)]
struct MacroRankExt(u8);

impl fmt::Display for MacroRankExt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MacroRankExt({})", self.0)
    }
}

#[allow(dead_code)]
fn decode_rr_short(payload: &[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
    let b = *payload
        .first()
        .ok_or(ValueBinaryError::InvalidExtensionPayload)?;
    Ok(Box::new(MacroRankExt(b)))
}

define_gql_extension! {
    prefix: "RR",
    types: [
        {
            rust_type: MacroRankExt,
            type_name: "test.MacroRank",
            decoder: MacroRankGqlDecode,
            eq: |this, other| this.0 == other.0,
            cmp: |this, other| this.0.cmp(&other.0),
            sortable_index_key: {
                domain: "test.MacroRank/v1",
                bytes: |this| Cow::Owned(vec![this.0]),
            },
            short_blob: |this| Cow::Owned(vec![this.0]),
            short_blob_decode: decode_rr_short,
        },
    ],
}

#[derive(Debug, Clone)]
struct MacroCompactExt(Vec<u8>);

impl fmt::Display for MacroCompactExt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MacroCompactExt")
    }
}

define_gql_extension! {
    prefix: "MC",
    types: [
        {
            rust_type: MacroCompactExt,
            type_name: "test.MacroCompact",
            decoder: MacroCompactGqlDecode,
            eq: |this, other| this.0 == other.0,
            compact_kind: |_this| 200,
            binary_payload: |this| Cow::Borrowed(this.0.as_slice()),
            compact: { 200u8 => decode_tt_compact },
        },
    ],
}

#[test]
fn macro_defined_non_orderable_extension_is_incomparable() {
    let left = Value::Extension(Box::new(MacroTokenExt(vec![1])));
    let right = Value::Extension(Box::new(MacroTokenExt(vec![2])));
    assert_eq!(compare_values(&left, &right), None);

    let Value::Extension(left_ext) = &left else {
        panic!("expected extension");
    };
    let Value::Extension(right_ext) = &right else {
        panic!("expected extension");
    };
    assert!(!left_ext.eq_ext(right_ext.as_ref()));
    assert_eq!(left_ext.cmp_ext(right_ext.as_ref()), None);
    assert_eq!(left_ext.sortable_index_key(), None);
    assert_eq!(left_ext.type_name(), "TT.test.MacroToken");
}

#[test]
fn macro_defined_orderable_extension_supports_compare_and_sortable_key() {
    let left = Value::Extension(Box::new(MacroRankExt(1)));
    let right = Value::Extension(Box::new(MacroRankExt(2)));
    assert_eq!(compare_values(&left, &right), Some(Ordering::Less));

    let Value::Extension(left_ext) = &left else {
        panic!("expected extension");
    };
    let key = left_ext
        .sortable_index_key()
        .expect("macro sortable index key");
    assert_eq!(key.domain.as_ref(), "test.MacroRank/v1");
    assert_eq!(key.bytes.as_ref(), &[1]);
}

#[test]
fn macro_defined_extension_type_mismatch_is_not_equal_or_ordered() {
    let token = MacroTokenExt(vec![1]);
    let rank = MacroRankExt(1);
    assert!(!token.eq_ext(&rank));
    assert_eq!(rank.cmp_ext(&token), None);
}

#[test]
fn macro_defined_extension_binary_round_trips_with_decoder() {
    let value = Value::Extension(Box::new(MacroCompactExt(vec![1, 2, 3])));
    let bytes = value.to_binary_bytes().expect("encode");
    let back = Value::from_binary_bytes_with_extensions(&bytes, &MacroCompactGqlDecode::INSTANCE)
        .expect("decode");
    assert_eq!(
        back,
        Value::Extension(Box::new(MacroTokenExt(vec![1, 2, 3])))
    );
}
