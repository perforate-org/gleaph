//! rkyv helpers for [`crate::Value::Extension`] (wire bytes) and related `with` types.

use rkyv::rancor::{Fallible, Source};
use rkyv::vec::{ArchivedVec, VecResolver};
use rkyv::with::{ArchiveWith, DeserializeWith, SerializeWith};
use rkyv::{Archive, Deserialize, Place, Serialize};

use crate::Value;
use crate::ValueBinaryError;
use crate::value::ExtensionValue;

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
/// Deserialization uses [`Value::from_binary_bytes`], which rejects extension tags unless a
/// custom [`crate::ExtensionBinaryDecode`] path is added in the future.
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
        match Value::from_binary_bytes(&bytes) {
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
