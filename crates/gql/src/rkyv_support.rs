//! rkyv helpers for [`crate::Value::Extension`] (wire bytes) and related `with` types.
//!
//! Deserializing [`Value::Extension`] through rkyv requires a registered
//! [`crate::ExtensionBinaryDecode`] implementation (typically supplied by a platform extension crate):
//! call [`try_install_global_rkyv_extension_binary_decode`] at process startup, or
//! [`RkyvExtensionDecodeScopeGuard`] for thread-local overrides in tests.

use std::cell::Cell;
use std::sync::OnceLock;

use rkyv::bytecheck::CheckBytes;
use rkyv::de::Pool;
use rkyv::rancor::{Fallible, Source, Strategy};
use rkyv::util::AlignedVec;
use rkyv::validation::Validator;
use rkyv::validation::archive::ArchiveValidator;
use rkyv::validation::shared::SharedValidator;
use rkyv::vec::{ArchivedVec, VecResolver};
use rkyv::with::{ArchiveWith, DeserializeWith, SerializeWith};
use rkyv::{Archive, Deserialize, Place, Serialize};

use thiserror::Error;

use crate::Value;
use crate::ValueBinaryError;
use crate::value::{DenyExtensionBinaryDecode, ExtensionBinaryDecode, ExtensionValue};

/// Returned when [`try_install_global_rkyv_extension_binary_decode`] is called after a decoder was already installed (first wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("global rkyv extension binary decode hook is already installed")]
pub struct GlobalRkyvExtensionDecodeAlreadyInstalled;

thread_local! {
    static RKYV_EXT_DECODE_OVERRIDE: Cell<Option<&'static (dyn ExtensionBinaryDecode + Sync)>> =
        const { Cell::new(None) };
}

static RKYV_EXT_DECODE_GLOBAL: OnceLock<&'static (dyn ExtensionBinaryDecode + Sync)> =
    OnceLock::new();

static DENY_RKYV: DenyExtensionBinaryDecode = DenyExtensionBinaryDecode;

fn effective_rkyv_extension_binary_decode() -> &'static (dyn ExtensionBinaryDecode + Sync) {
    if let Some(d) = RKYV_EXT_DECODE_OVERRIDE.with(|c| c.get()) {
        return d;
    }
    RKYV_EXT_DECODE_GLOBAL.get().copied().unwrap_or(&DENY_RKYV)
}

/// Registers a process-wide [`ExtensionBinaryDecode`] for rkyv [`ExtensionBinaryWire`] deserialization.
///
/// Returns `Ok(())` if this call installed the decoder, or `Err(`[`GlobalRkyvExtensionDecodeAlreadyInstalled`]`)` if a decoder was already set (first wins).
pub fn try_install_global_rkyv_extension_binary_decode(
    decoder: &'static (dyn ExtensionBinaryDecode + Sync),
) -> Result<(), GlobalRkyvExtensionDecodeAlreadyInstalled> {
    RKYV_EXT_DECODE_GLOBAL
        .set(decoder)
        .map_err(|_| GlobalRkyvExtensionDecodeAlreadyInstalled)
}

/// Thread-local override for rkyv extension decoding (e.g. unit tests). Restores the previous override on drop.
pub struct RkyvExtensionDecodeScopeGuard {
    previous: Option<&'static (dyn ExtensionBinaryDecode + Sync)>,
}

impl RkyvExtensionDecodeScopeGuard {
    pub fn set(decoder: &'static (dyn ExtensionBinaryDecode + Sync)) -> Self {
        let previous = RKYV_EXT_DECODE_OVERRIDE.with(|c| c.replace(Some(decoder)));
        Self { previous }
    }
}

impl Drop for RkyvExtensionDecodeScopeGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        RKYV_EXT_DECODE_OVERRIDE.with(|c| c.set(prev));
    }
}

#[cfg(feature = "ast-rkyv-no-span")]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(remote = half::f16)]
pub(crate) struct F16Def(#[rkyv(getter = f16_to_bits)] u16);

#[cfg(feature = "ast-rkyv-no-span")]
fn f16_to_bits(x: &half::f16) -> u16 {
    x.to_bits()
}

#[cfg(feature = "ast-rkyv-no-span")]
impl From<F16Def> for half::f16 {
    fn from(F16Def(bits): F16Def) -> Self {
        half::f16::from_bits(bits)
    }
}

#[cfg(all(feature = "ast-rkyv-no-span", feature = "f128"))]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(remote = f128)]
pub(crate) struct F128Def(#[rkyv(getter = f128_to_bits)] u128);

#[cfg(all(feature = "ast-rkyv-no-span", feature = "f128"))]
fn f128_to_bits(x: &f128) -> u128 {
    x.to_bits()
}

#[cfg(all(feature = "ast-rkyv-no-span", feature = "f128"))]
impl From<F128Def> for f128 {
    fn from(F128Def(bits): F128Def) -> Self {
        f128::from_bits(bits)
    }
}

#[cfg(all(feature = "ast-rkyv-no-span", feature = "f256"))]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(remote = f256::f256)]
pub(crate) struct F256Def(#[rkyv(getter = f256_to_bytes)] [u8; 32]);

#[cfg(all(feature = "ast-rkyv-no-span", feature = "f256"))]
fn f256_to_bytes(x: &f256::f256) -> [u8; 32] {
    x.to_le_bytes()
}

#[cfg(all(feature = "ast-rkyv-no-span", feature = "f256"))]
impl From<F256Def> for f256::f256 {
    fn from(F256Def(bytes): F256Def) -> Self {
        f256::f256::from_le_bytes(bytes)
    }
}

/// Archives [`Value::Extension`] as the same byte layout as [`Value::to_binary_bytes`].
///
/// Deserialization uses [`Value::from_binary_bytes_with_extensions`] with the decoder from
/// [`try_install_global_rkyv_extension_binary_decode`] and/or [`RkyvExtensionDecodeScopeGuard`];
/// if none is set, extensions are rejected like [`Value::from_binary_bytes`].
#[derive(Debug)]
pub struct ExtensionBinaryWire;

impl ArchiveWith<Box<dyn ExtensionValue>> for ExtensionBinaryWire {
    type Archived = ArchivedVec<u8>;
    type Resolver = VecResolver;

    fn resolve_with(
        field: &Box<dyn ExtensionValue>,
        resolver: Self::Resolver,
        out: Place<Self::Archived>,
    ) {
        let bytes = Value::Extension(field.as_ref().clone_box())
            .to_binary_bytes()
            .expect("ExtensionBinaryWire::resolve_with: bytes must match serialize_with");
        Archive::resolve(&bytes, resolver, out);
    }
}

impl<S: Fallible + ?Sized> SerializeWith<Box<dyn ExtensionValue>, S> for ExtensionBinaryWire
where
    Vec<u8>: Serialize<S>,
    S::Error: Source,
{
    fn serialize_with(
        field: &Box<dyn ExtensionValue>,
        serializer: &mut S,
    ) -> Result<Self::Resolver, S::Error> {
        let bytes = Value::Extension(field.as_ref().clone_box())
            .to_binary_bytes()
            .map_err(extension_serialize_error::<S::Error>)?;
        bytes.serialize(serializer)
    }
}

impl<D: Fallible + ?Sized> DeserializeWith<ArchivedVec<u8>, Box<dyn ExtensionValue>, D>
    for ExtensionBinaryWire
where
    ArchivedVec<u8>: Deserialize<Vec<u8>, D>,
    D::Error: Source,
{
    fn deserialize_with(
        field: &ArchivedVec<u8>,
        deserializer: &mut D,
    ) -> Result<Box<dyn ExtensionValue>, D::Error> {
        let bytes: Vec<u8> = field.deserialize(deserializer)?;
        match Value::from_binary_bytes_with_extensions(
            &bytes,
            effective_rkyv_extension_binary_decode(),
        ) {
            Ok(Value::Extension(ext)) => Ok(ext),
            Ok(_) => Err(D::Error::new(ExtensionDeserializeWrongVariant)),
            Err(e) => Err(D::Error::new(e)),
        }
    }
}

fn extension_serialize_error<E: Source>(e: ValueBinaryError) -> E {
    E::new(e)
}

#[derive(Debug)]
struct ExtensionDeserializeWrongVariant;

impl std::fmt::Display for ExtensionDeserializeWrongVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "expected Extension value in rkyv wire bytes")
    }
}

impl std::error::Error for ExtensionDeserializeWrongVariant {}

/// Alignment required for [`rkyv::from_bytes`] on archived GQL wire types.
pub const RKYV_WIRE_ALIGN: usize = 16;

fn rkyv_wire_error(context: &'static str, err: rkyv::rancor::Error) -> String {
    let detail = err.to_string();
    if detail.is_empty()
        || detail.contains("failed without error information")
        || detail.contains("enable debug assertions")
    {
        format!("{context}: rkyv deserialize failed (enable `rkyv/alloc` for details)")
    } else {
        format!("{context}: {detail}")
    }
}

/// Returns whether `bytes` satisfies rkyv root alignment for `T::Archived` at [`rkyv::api::root_position`].
#[cfg(not(target_family = "wasm"))]
fn wire_root_is_aligned<T: Archive>(bytes: &[u8]) -> bool
where
    T::Archived: rkyv::Portable,
{
    use core::mem::{align_of, size_of};

    let pos = rkyv::api::root_position::<T::Archived>(bytes.len());
    let root_size = size_of::<T::Archived>();
    if pos.saturating_add(root_size) > bytes.len() {
        return false;
    }
    let align = align_of::<T::Archived>();
    if align <= 1 {
        return true;
    }
    let addr = bytes.as_ptr().cast::<u8>() as usize + pos;
    addr & (align - 1) == 0
}

/// Deserialize an archived GQL value when the caller guarantees root alignment.
///
/// # Alignment contract
///
/// `bytes` must satisfy rkyv checked [`rkyv::from_bytes`] requirements: the archived root for
/// `T::Archived` at [`rkyv::api::root_position`] must be aligned for
/// `align_of::<T::Archived>()`. Typical satisfied cases:
///
/// - Output of [`rkyv::to_bytes`] stored in `Vec` / [`AlignedVec`]
/// - A sub-slice of a buffer you already copied into aligned storage at the storage boundary
///
/// This function never copies. For unaligned subslices or buffers of unknown provenance, use
/// [`rkyv_from_wire_bytes`] instead.
pub fn rkyv_from_aligned_bytes<T>(bytes: &[u8]) -> Result<T, String>
where
    T: Archive,
    T::Archived: rkyv::Portable,
    for<'a> T::Archived:
        CheckBytes<Strategy<Validator<ArchiveValidator<'a>, SharedValidator>, rkyv::rancor::Error>>,
    T::Archived: Deserialize<T, Strategy<Pool, rkyv::rancor::Error>>,
{
    if bytes.is_empty() {
        return Err("rkyv wire bytes are empty".into());
    }
    rkyv::from_bytes::<T, rkyv::rancor::Error>(bytes)
        .map_err(|e| rkyv_wire_error(std::any::type_name::<T>(), e))
}

/// Deserialize an archived GQL wire value from possibly unaligned bytes.
///
/// On native targets, delegates to [`rkyv_from_aligned_bytes`] when the buffer is already
/// root-aligned (typical for `rkyv::to_bytes` output in `Vec`). On wasm32 and on misaligned
/// slices, copies into an [`AlignedVec`] first.
pub fn rkyv_from_wire_bytes<T>(bytes: &[u8]) -> Result<T, String>
where
    T: Archive,
    T::Archived: rkyv::Portable,
    for<'a> T::Archived:
        CheckBytes<Strategy<Validator<ArchiveValidator<'a>, SharedValidator>, rkyv::rancor::Error>>,
    T::Archived: Deserialize<T, Strategy<Pool, rkyv::rancor::Error>>,
{
    if bytes.is_empty() {
        return Err("rkyv wire bytes are empty".into());
    }
    #[cfg(not(target_family = "wasm"))]
    if wire_root_is_aligned::<T>(bytes) {
        return rkyv_from_aligned_bytes(bytes);
    }
    let mut aligned = AlignedVec::<RKYV_WIRE_ALIGN>::with_capacity(bytes.len());
    aligned.extend_from_slice(bytes);
    rkyv_from_aligned_bytes(aligned.as_ref())
}

#[cfg(test)]
mod wire_deserialize_tests {
    use super::*;
    use crate::ast::Expr;

    #[test]
    fn rkyv_from_aligned_bytes_accepts_to_bytes_vec() {
        let expr = Expr::var("n");
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&expr)
            .expect("encode")
            .into_vec();
        let decoded = rkyv_from_aligned_bytes::<Expr>(&bytes).expect("decode aligned expr wire");
        assert_eq!(decoded, expr);
    }

    #[test]
    fn rkyv_from_wire_bytes_accepts_to_bytes_vec() {
        let expr = Expr::var("n");
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&expr)
            .expect("encode")
            .into_vec();
        let decoded = rkyv_from_wire_bytes::<Expr>(&bytes).expect("decode aligned expr wire");
        assert_eq!(decoded, expr);
    }

    #[test]
    fn rkyv_from_wire_bytes_accepts_unaligned_subslice() {
        let expr = Expr::var("n");
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&expr)
            .expect("encode")
            .into_vec();
        let mut buf = vec![0u8];
        buf.extend_from_slice(&bytes);
        let decoded = rkyv_from_wire_bytes::<Expr>(&buf[1..]).expect("decode unaligned expr wire");
        assert_eq!(decoded, expr);
    }
}
