//! [`candid::Principal`] as [`gleaph_gql::Value::Extension`].
//!
//! Depends on `candid` only here — [`gleaph_gql`] stays free of IC crates.
//!
//! ## Binary wire encoding
//!
//! **Encode:** **tag 34** — `u8` length + principal bytes ([`Principal::as_slice`], max 29).
//!
//! **Decode:** tag **34** only (short blob).
//!
//! ## Candid / canister boundary
//!
//! Use [`wire::IcWireValue`] for lossless conversion to and from [`gleaph_gql::Value`]
//! (including Principals and opaque extension leaves).

#![cfg_attr(test, feature(f128))]

use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;

pub use candid::Principal;
pub mod graph_registry;
pub mod wire;

pub use wire::{IcWirePathElement, IcWireValue, WireError, principal_to_value, value_as_principal};

use gleaph_gql::extensions::gql_extension;
use gleaph_gql::value::{ExtensionValue, Value, ValueBinaryError};

/// Sortable-index domain for [`PrincipalValue`] (not the GQL extension type string).
pub const PRINCIPAL_EXTENSION_SORTABLE_DOMAIN: &str = "Pr";

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PrincipalValue(pub Principal);

impl Deref for PrincipalValue {
    type Target = Principal;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for PrincipalValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

fn decode_principal_payload(payload: &[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
    let p = Principal::try_from_slice(payload)
        .map_err(|_| ValueBinaryError::InvalidExtensionPayload)?;
    Ok(Box::new(PrincipalValue(p)))
}

gql_extension! {
    prefix: "IC",
    types: [
        {
            rust_type: PrincipalValue,
            type_name: "PRINCIPAL",
            decoder: IcExtensionBinaryDecode,
            eq: |this, other| this.0 == other.0,
            cmp: |this, other| this.0.cmp(&other.0),
            sortable_index_key: {
                domain: PRINCIPAL_EXTENSION_SORTABLE_DOMAIN,
                bytes: |this| Cow::Borrowed(this.0.as_slice()),
            },
            binary_payload: |this| Cow::Borrowed(this.0.as_slice()),
            short_blob: |this| Cow::Borrowed(this.0.as_slice()),
            short_blob_decode: decode_principal_payload,
        },
    ],
}

impl From<Principal> for PrincipalValue {
    fn from(value: Principal) -> Self {
        Self(value)
    }
}

impl From<PrincipalValue> for Value {
    fn from(value: PrincipalValue) -> Self {
        Value::Extension(Box::new(value))
    }
}

/// Registers [`IcExtensionBinaryDecode`] for deserializing extension values embedded in rkyv archives (e.g. AST or property [`Value`](Value) blobs).
///
/// Idempotent for the process: only the first successful [`gleaph_gql::try_install_global_rkyv_extension_binary_decode`] wins. Call during canister or service startup before loading rkyv data that may contain [`Principal`](Principal).
pub fn install_ic_extension_binary_decode_for_rkyv() {
    let _ = gleaph_gql::try_install_global_rkyv_extension_binary_decode(
        &IcExtensionBinaryDecode::INSTANCE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ExtensionBinaryDecode;
    use gleaph_gql::value_cmp::compare_values;
    use gleaph_gql::value_to_index_key_bytes;

    #[test]
    fn principal_binary_roundtrip() {
        let p = Principal::from_text("aaaaa-aa").expect("management id");
        let v: Value = PrincipalValue(p).into();
        let bytes = v.to_binary_bytes().expect("encode");
        assert_eq!(bytes.first().copied(), Some(34));
        let back =
            Value::from_binary_bytes_with_extensions(&bytes, &IcExtensionBinaryDecode::INSTANCE)
                .expect("decode");
        assert_eq!(back, v);

        let Value::Extension(ext) = &back else {
            panic!("expected extension");
        };
        assert_eq!(ext.type_name(), "IC.PRINCIPAL");
        let pv = ext
            .as_any()
            .downcast_ref::<PrincipalValue>()
            .expect("PrincipalValue");
        assert_eq!(pv.0, p);
    }

    #[test]
    fn ic_decoder_unknown_compact_kind() {
        let err = IcExtensionBinaryDecode::INSTANCE
            .decode_extension_compact(255, &[])
            .unwrap_err();
        assert_eq!(err, ValueBinaryError::UnknownEncodedExtension);
    }

    #[test]
    fn principal_rejects_tag33_compact_payload() {
        let p = Principal::from_text("aaaaa-aa").expect("management id");
        let pl = p.as_slice();
        let mut legacy = vec![33u8, 1u8];
        legacy.extend_from_slice(&(pl.len() as u32).to_le_bytes());
        legacy.extend_from_slice(pl);
        let err =
            Value::from_binary_bytes_with_extensions(&legacy, &IcExtensionBinaryDecode::INSTANCE)
                .expect_err("tag33 should be rejected");
        assert_eq!(err, ValueBinaryError::UnknownEncodedExtension);
    }

    #[test]
    fn principal_compare_values_uses_principal_ordering() {
        let left = Principal::self_authenticating([1u8; 32]);
        let right = Principal::self_authenticating([2u8; 32]);
        let expected = left.cmp(&right);

        assert_eq!(
            compare_values(
                &Value::from(PrincipalValue(left)),
                &Value::from(PrincipalValue(right))
            ),
            Some(expected)
        );
    }

    #[test]
    fn principal_sortable_index_key_order_matches_compare_values() {
        let left = Principal::self_authenticating([1u8; 32]);
        let right = Principal::self_authenticating([2u8; 32]);
        let left_value = Value::from(PrincipalValue(left));
        let right_value = Value::from(PrincipalValue(right));
        let left_key = value_to_index_key_bytes(&left_value).unwrap().unwrap();
        let right_key = value_to_index_key_bytes(&right_value).unwrap().unwrap();

        assert_eq!(left.as_slice().cmp(right.as_slice()), left.cmp(&right));
        assert_eq!(left_key.cmp(&right_key), left.cmp(&right));
        assert_eq!(
            compare_values(&left_value, &right_value),
            Some(left.cmp(&right))
        );
    }

    #[test]
    fn principal_rkyv_roundtrips_with_global_decode_hook() {
        install_ic_extension_binary_decode_for_rkyv();
        let p = Principal::from_text("aaaaa-aa").expect("management id");
        let v: Value = PrincipalValue(p).into();
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&v).expect("to_bytes");
        let back: Value =
            rkyv::from_bytes::<Value, rkyv::rancor::Error>(&bytes).expect("from_bytes");
        assert_eq!(back, v);
    }
}
