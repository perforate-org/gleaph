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

use std::any::Any;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;

pub use candid::Principal;
pub mod graph_registry;
pub mod wire;

pub use wire::{IcWirePathElement, IcWireValue, WireError, principal_to_value, value_as_principal};

use gleaph_gql::value::{ExtensionValue, Value, ValueBinaryError};

/// Global decoder instance (zero-sized): use with [`Value::from_binary_bytes_with_extensions`].
pub const IC_EXTENSION_BINARY_DECODER: IcExtensionBinaryDecode = IcExtensionBinaryDecode;

/// Name for logs / APIs (not written on the wire).
pub const PRINCIPAL_EXTENSION_TYPE_NAME: &str = "ic.Principal";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PrincipalValue(pub Principal);

impl fmt::Display for PrincipalValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl ExtensionValue for PrincipalValue {
    fn type_name(&self) -> &str {
        PRINCIPAL_EXTENSION_TYPE_NAME
    }

    fn clone_box(&self) -> Box<dyn ExtensionValue> {
        Box::new(self.clone())
    }

    fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
        other
            .as_any()
            .downcast_ref::<PrincipalValue>()
            .is_some_and(|o| self.0 == o.0)
    }

    fn cmp_ext(&self, other: &dyn ExtensionValue) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<PrincipalValue>()
            .map(|o| self.0.cmp(&o.0))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn binary_payload(&self) -> Result<Cow<'_, [u8]>, ValueBinaryError> {
        Ok(Cow::Borrowed(self.0.as_slice()))
    }

    fn short_blob(&self) -> Option<Cow<'_, [u8]>> {
        Some(Cow::Borrowed(self.0.as_slice()))
    }
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

fn decode_principal_payload(payload: &[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
    let p = Principal::try_from_slice(payload)
        .map_err(|_| ValueBinaryError::InvalidExtensionPayload)?;
    Ok(Box::new(PrincipalValue(p)))
}

gleaph_gql::extensions::declare_extension_types! {
    /// Decoder for IC extension values (currently `Principal` via tag 34 short blob).
    decoder: IcExtensionBinaryDecode;
    type_names: [PRINCIPAL_EXTENSION_TYPE_NAME, "PRINCIPAL"];
    short_blob: decode_principal_payload;
}

/// Registers [`IcExtensionBinaryDecode`] for deserializing extension values embedded in rkyv archives (e.g. AST or property [`Value`](Value) blobs).
///
/// Idempotent for the process: only the first successful [`gleaph_gql::try_install_global_rkyv_extension_binary_decode`] wins. Call during canister or service startup before loading rkyv data that may contain [`Principal`](Principal).
pub fn install_ic_extension_binary_decode_for_rkyv() {
    let _ =
        gleaph_gql::try_install_global_rkyv_extension_binary_decode(&IC_EXTENSION_BINARY_DECODER);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ExtensionBinaryDecode;

    #[test]
    fn principal_binary_roundtrip() {
        let p = Principal::from_text("aaaaa-aa").expect("management id");
        let v: Value = PrincipalValue(p).into();
        let bytes = v.to_binary_bytes().expect("encode");
        assert_eq!(bytes.first().copied(), Some(34));
        let back = Value::from_binary_bytes_with_extensions(&bytes, &IC_EXTENSION_BINARY_DECODER)
            .expect("decode");
        assert_eq!(back, v);

        let Value::Extension(ext) = &back else {
            panic!("expected extension");
        };
        let pv = ext
            .as_any()
            .downcast_ref::<PrincipalValue>()
            .expect("PrincipalValue");
        assert_eq!(pv.0, p);
    }

    #[test]
    fn ic_decoder_unknown_compact_kind() {
        let err = IcExtensionBinaryDecode
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
        let err = Value::from_binary_bytes_with_extensions(&legacy, &IC_EXTENSION_BINARY_DECODER)
            .expect_err("tag33 should be rejected");
        assert_eq!(err, ValueBinaryError::UnknownEncodedExtension);
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
