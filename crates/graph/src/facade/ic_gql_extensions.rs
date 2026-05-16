//! Internet Computer integration for [`gleaph_gql::Value`] extensions (e.g. [`Principal`](candid::Principal)).
//!
//! Call [`init_ic_gql_extensions`] once when bringing up stable storage so rkyv archives containing
//! [`Value::Extension`](gleaph_gql::Value::Extension) decode correctly, and so extension type names
//! are available for allowlists / diagnostics.

use std::sync::OnceLock;

static IC_EXTENSION_TYPE_NAMES: OnceLock<Vec<String>> = OnceLock::new();

/// Type names declared by [`gleaph_gql_ic::IcExtensionBinaryDecode`] (e.g. `IC.PRINCIPAL`), computed once.
pub fn ic_extension_type_names() -> &'static [String] {
    IC_EXTENSION_TYPE_NAMES.get_or_init(|| {
        let mut out = Vec::new();
        gleaph_gql_ic::IcExtensionBinaryDecode::for_each_extension_type(|name| {
            out.push(name.to_string());
        });
        out
    })
}

/// Installs the process-wide rkyv extension decode hook and warms [`ic_extension_type_names`].
pub fn init_ic_gql_extensions() {
    gleaph_gql_ic::install_ic_extension_binary_decode_for_rkyv();
    let _ = ic_extension_type_names();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_ic::Principal;
    use gleaph_gql_ic::{IcWireValue, principal_to_value};

    #[test]
    fn exposes_ic_principal_extension_names() {
        init_ic_gql_extensions();
        let names = ic_extension_type_names();
        assert!(names.iter().any(|n| n == "IC.PRINCIPAL"), "{names:?}");
    }

    #[test]
    fn wire_principal_round_trips_after_extension_init() {
        init_ic_gql_extensions();
        let p = Principal::from_text("aaaaa-aa").expect("principal");
        let v = principal_to_value(p);
        let wire = IcWireValue::try_from_value(&v).expect("to wire");
        assert_eq!(wire.try_into_value().expect("from wire"), v);
    }
}
