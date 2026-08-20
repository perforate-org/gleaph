//! Rust application-client output profile for prepared-query adapters.
//!
//! Generated code binds typed prepared operations to the `gleaph-sdk` application client: a
//! `Prepared` marker type and a `PreparedExt` extension trait implemented for
//! `GleaphClient<Prepared>` wrap the Router's `prepared_query` / `prepared_mutate` endpoints over
//! the `ic-agent` transport.
//!
//! This profile is the Rust application-client binding of [`super::shared`]; the shared renderer
//! owns the generated constructs and this module selects the `gleaph_sdk` runtime profile.

use super::shared::{RuntimeProfile, generate_rust_prepared};
use crate::{ManifestError, PreparedManifest, SemanticType};

/// The `gleaph_sdk` runtime profile: no Candid row derives because application clients only
/// deserialize locally.
pub const SDK_PROFILE: RuntimeProfile = RuntimeProfile {
    path: "gleaph_sdk",
    candid_row_derive: false,
};

/// Generate Rust client declarations and a `PreparedExt` operations trait.
pub fn generate_rust(manifest: &PreparedManifest) -> Result<String, ManifestError> {
    generate_rust_prepared(manifest, SDK_PROFILE)
}

pub(crate) fn rust_field(name: &str) -> String {
    let mut result = String::new();
    for (index, c) in name.chars().enumerate() {
        if c.is_ascii_alphanumeric() || c == '_' {
            if index == 0 && c.is_ascii_digit() {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push('_');
        }
    }
    if result.is_empty() {
        "value".to_string()
    } else if matches!(
        result.as_str(),
        "type" | "match" | "ref" | "self" | "Self" | "crate"
    ) {
        format!("r#{result}")
    } else {
        result
    }
}

/// Plain scalar Rust type used by the shared renderer's fallback arm.
///
/// Exotic, temporal, path, and record types are bound through the runtime row/param wrappers in
/// [`super::shared`], so this fallback only reaches the primitive scalars.
pub(crate) fn rust_type(semantic_type: &SemanticType) -> String {
    match semantic_type {
        SemanticType::Null => "()".to_string(),
        SemanticType::Bool => "bool".to_string(),
        SemanticType::Int8 => "i8".to_string(),
        SemanticType::Int16 => "i16".to_string(),
        SemanticType::Int32 => "i32".to_string(),
        SemanticType::Int64 => "i64".to_string(),
        SemanticType::Uint8 => "u8".to_string(),
        SemanticType::Uint16 => "u16".to_string(),
        SemanticType::Uint32 => "u32".to_string(),
        SemanticType::Uint64 => "u64".to_string(),
        SemanticType::Int128 => "i128".to_string(),
        SemanticType::Uint128 => "u128".to_string(),
        SemanticType::Float32 => "f32".to_string(),
        SemanticType::Float64 => "f64".to_string(),
        SemanticType::Text => "String".to_string(),
        SemanticType::Bytes => "Vec<u8>".to_string(),
        _ => {
            unreachable!("exotic types are bound through the runtime row/param wrappers")
        }
    }
}
