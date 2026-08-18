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
//! Graph canisters take GQL parameters as a single **compact-binary blob**
//! ([`encode_gql_params_blob`] / [`decode_gql_params_blob`]): one [`Value::Record`] on the wire.
//! [`wire::IcWireValue`] remains for Candid-structured tooling and tests where needed.

#![cfg_attr(test, feature(f128))]

pub use candid::Principal;
pub mod graph_registry;
pub mod plan_result_wire;
pub mod unique_key;
pub mod wire;

pub use gleaph_gql_ic_wire::{
    IcExtensionBinaryDecode, PRINCIPAL_EXTENSION_SORTABLE_DOMAIN, PrincipalValue,
    install_ic_extension_binary_decode_for_rkyv, principal_to_value, value_as_principal,
};
pub use plan_result_wire::{IcWirePlanQueryResult, IcWirePlanQueryRow};
pub use unique_key::{
    MAX_UNIQUE_ENCODED_VALUE_LEN, UniqueKeyOutcome, UniqueKeyRejection, encode_unique_value,
};
pub use wire::{
    IcWirePathElement, IcWireValue, WireError, decode_gql_params_blob, encode_gql_params_blob,
};

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ExtensionBinaryDecode;
    use gleaph_gql::value::cmp::compare_values;
    use gleaph_gql::value::{Value, ValueBinaryError};
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
