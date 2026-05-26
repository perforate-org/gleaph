//! GQL value type with extension support.
//!
//! The [`Value`] enum covers all GQL standard value types plus an
//! [`Extension`](Value::Extension) slot for platform-specific types.
//!
//! Extension binary encoding: **tag 33** (`u8` kind + `u32`-length payload), **tag 34**
//! (`u8` length + ≤255 bytes, no kind) when [`ExtensionValue::short_blob`] is set
//! (e.g. Internet Computer `Principal` via a platform extension crate).
//!
//! Fixed-size binary wire payloads (no backward compatibility):
//! - tag 7: `Int256` = `ethnum::I256` little-endian bytes (32 bytes)
//! - tag 13: `Uint256` = `ethnum::U256` little-endian bytes (32 bytes)
//! - tag 17: `Decimal` = `rust_decimal::Decimal::serialize()` bytes (16 bytes)
//! - tag 31: `Float128` bits (see `f128::to_bits()`) (16 bytes)
//! - tag 32: `Float256` little-endian bytes (32 bytes)

use std::any::Any;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::Hasher;
use std::str;

/// Error returned when one [`Value`] cannot be encoded to, or decoded from,
/// the rewrite-side binary byte format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueBinaryError {
    UnexpectedEof,
    InvalidTag(u8),
    InvalidUtf8,
    InvalidDecimal,
    InvalidInt256,
    InvalidUint256,
    /// Extension does not supply [`ExtensionValue::binary_payload`] and cannot be binary-encoded.
    InvalidExtensionType,
    /// Encoded bytes contain an extension tag but the decoder does not handle this kind or short blob.
    UnknownEncodedExtension,
    /// Extension payload bytes could not be parsed (decoder-specific, e.g. invalid length).
    InvalidExtensionPayload,
}

impl fmt::Display for ValueBinaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "unexpected end of binary value bytes"),
            Self::InvalidTag(tag) => write!(f, "invalid binary value tag: {tag}"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 in binary value bytes"),
            Self::InvalidDecimal => write!(f, "invalid decimal in binary value bytes"),
            Self::InvalidInt256 => write!(f, "invalid i256 in binary value bytes"),
            Self::InvalidUint256 => write!(f, "invalid u256 in binary value bytes"),
            Self::InvalidExtensionType => write!(f, "extension values are not binary-encodable"),
            Self::UnknownEncodedExtension => {
                write!(f, "unknown or unsupported extension wire type")
            }
            Self::InvalidExtensionPayload => write!(f, "invalid extension payload"),
        }
    }
}

impl std::error::Error for ValueBinaryError {}

// ──── ExtensionValue trait ────

/// Order-preserving key bytes for extension values that opt in to property-index ordering.
///
/// Within one `domain`, the lexicographic order of `bytes` must match [`ExtensionValue::cmp_ext`]
/// for all extension values that return that same domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionSortableKey<'a> {
    pub domain: Cow<'a, str>,
    pub bytes: Cow<'a, [u8]>,
}

/// Trait for user-defined value types that can be plugged into [`Value::Extension`].
///
/// Implementors must be `Clone`-able (via `clone_box`), equality comparable, and displayable.
/// Ordering and property-index keys are opt-in.
pub trait ExtensionValue: fmt::Debug + fmt::Display + Send + Sync {
    /// A short name identifying this extension type (e.g. `"Principal"`).
    fn type_name(&self) -> &str;

    /// Clone into a boxed trait object.
    fn clone_box(&self) -> Box<dyn ExtensionValue>;

    /// Equality comparison with another extension value.
    fn eq_ext(&self, other: &dyn ExtensionValue) -> bool;

    /// Ordering comparison with another extension value.
    ///
    /// Default: `None`, meaning this extension is not orderable for GQL comparisons.
    fn cmp_ext(&self, _other: &dyn ExtensionValue) -> Option<Ordering> {
        None
    }

    /// Order-preserving property-index key for extensions that opt in to sortable indexing.
    ///
    /// Default: `None`, meaning this extension is not supported by sortable property indexes.
    fn sortable_index_key(&self) -> Option<ExtensionSortableKey<'_>> {
        None
    }

    /// Downcast to concrete type.
    fn as_any(&self) -> &dyn Any;

    /// Opaque payload for [`Value::to_binary_bytes`] / property-store encoding.
    ///
    /// Default: [`ValueBinaryError::InvalidExtensionType`] (extensions that only exist at runtime).
    fn binary_payload(&self) -> Result<Cow<'_, [u8]>, ValueBinaryError> {
        Err(ValueBinaryError::InvalidExtensionType)
    }

    /// When [`Some`], and [`Self::short_blob`] is [`None`], binary encoding uses **tag 33**:
    /// `u8` kind + `u32`-length-prefixed payload from [`Self::binary_payload`].
    /// If both this and [`Self::short_blob`] are set, **short blob (tag 34) wins**.
    ///
    /// Kind IDs (0–255) are agreed with [`ExtensionBinaryDecode::decode_extension_compact`].
    fn compact_kind(&self) -> Option<u8> {
        None
    }

    /// When [`Some`], binary encoding uses **tag 34**: `u8` byte length (≤255) + raw bytes (no kind).
    ///
    /// Checked before [`Self::compact_kind`]. Platform extension crates may use this for `Principal`.
    fn short_blob(&self) -> Option<Cow<'_, [u8]>> {
        None
    }

    /// Mix bytes into `hasher` for join-key bucketing ([`crate::value_join_hash::hash_value_for_join`];
    /// runs after the outer discriminant tag `53` for [`Value::Extension`]).
    ///
    /// **Contract:** If [`Self::eq_ext`] returns `true` for two extensions, this method must produce
    /// identical writes (including [`Self::type_name`] when included by this implementation).
    ///
    /// Default: [`Self::type_name`] length + UTF-8 bytes, then [`Self::short_blob`] if present, otherwise
    /// [`Self::binary_payload`], otherwise an empty marker byte — aligned with wire preference for short blob.
    fn hash_join_key(&self, hasher: &mut dyn Hasher) {
        let tn = self.type_name();
        hasher.write_u64(tn.len() as u64);
        hasher.write(tn.as_bytes());
        if let Some(blob) = self.short_blob() {
            hasher.write_u8(1);
            let b = blob.as_ref();
            hasher.write_u64(b.len() as u64);
            hasher.write(b);
        } else {
            match self.binary_payload() {
                Ok(cow) => {
                    hasher.write_u8(1);
                    let bytes = cow.as_ref();
                    hasher.write_u64(bytes.len() as u64);
                    hasher.write(bytes);
                }
                Err(_) => hasher.write_u8(0),
            }
        }
    }
}

/// Decodes **tag 33** (compact kind) and **tag 34** (short blob) extension values.
pub trait ExtensionBinaryDecode {
    /// Tag **33**: `u8` kind + length-prefixed payload (`u32` length).
    fn decode_extension_compact(
        &self,
        _kind: u8,
        _payload: &[u8],
    ) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
        Err(ValueBinaryError::UnknownEncodedExtension)
    }

    /// Tag **34**: payload only (length was stored as the preceding `u8`).
    fn decode_extension_short_blob(
        &self,
        _payload: &[u8],
    ) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
        Err(ValueBinaryError::UnknownEncodedExtension)
    }
}

/// [`ExtensionBinaryDecode`] implementation that rejects every extension (used by [`Value::from_binary_bytes`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyExtensionBinaryDecode;

impl ExtensionBinaryDecode for DenyExtensionBinaryDecode {}

mod enum_;
mod impls;

pub use enum_::Value;
pub use impls::f128_is_finite;
