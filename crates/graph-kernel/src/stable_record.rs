//! Stable values stored as a layout byte and a bincode body.
//!
//! Candid writes a value's whole type table into every stored value and checks it on every
//! decode: hundreds of thousands to millions of instructions for a record of a few fields
//! (Router `unique_try_reserve_256`, `bulk_load_update_progress_64_rows`). Bincode, in its
//! standard configuration (little-endian, varint integers), writes only the data.
//!
//! The body carries no schema, so the first byte names its [`RecordLayout`] (ADR 0135: one
//! current layout, 1). A reader turns it into the enum and rejects any other, and decoding
//! rejects trailing bytes, so a value written for another type does not read as a prefix of
//! the expected one. A type whose layout changes after launch takes a reader of its own for the
//! older layout and stops using [`bincode_storable`].

use serde::Serialize;
use serde::de::DeserializeOwned;

#[doc(hidden)]
pub use ic_stable_structures::Storable;
#[doc(hidden)]
pub use ic_stable_structures::storable::Bound;

const CONFIG: bincode::config::Configuration = bincode::config::standard();

/// The layouts a stored record's first byte names: one current layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordLayout {
    /// The value's bincode body.
    V1 = 1,
}

impl RecordLayout {
    /// The layout writers write.
    pub const CURRENT: Self = Self::V1;
}

impl TryFrom<u8> for RecordLayout {
    /// The byte, naming no layout.
    type Error = u8;

    fn try_from(byte: u8) -> Result<Self, u8> {
        match byte {
            1 => Ok(Self::V1),
            other => Err(other),
        }
    }
}

/// `value` in the current layout. Panics only if serde cannot represent the value, which is a
/// type defect.
pub fn encode<T: Serialize>(value: &T, what: &str) -> Vec<u8> {
    let body = bincode::serde::encode_to_vec(value, CONFIG)
        .unwrap_or_else(|error| panic!("encode {what}: {error}"));
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(RecordLayout::CURRENT as u8);
    out.extend_from_slice(&body);
    out
}

/// The one `T` that `bytes` hold. Panics on another layout, malformed or trailing bytes: stable
/// memory holds only values this module wrote, so each is corruption or a stale layout that
/// needs fresh state.
pub fn decode<T: DeserializeOwned>(bytes: &[u8], what: &str) -> T {
    let (&layout, body) = bytes
        .split_first()
        .unwrap_or_else(|| panic!("decode {what}: missing layout"));
    match RecordLayout::try_from(layout)
        .unwrap_or_else(|layout| panic!("decode {what}: unknown layout {layout}"))
    {
        RecordLayout::V1 => {
            let (value, read) = bincode::serde::decode_from_slice(body, CONFIG)
                .unwrap_or_else(|error| panic!("decode {what}: {error}"));
            assert_eq!(read, body.len(), "decode {what}: trailing bytes");
            value
        }
    }
}

/// Implements `Storable` for a serde type through [`encode`] and [`decode`], unbounded.
#[macro_export]
macro_rules! bincode_storable {
    ($ty:ty, $what:literal) => {
        impl $crate::stable_record::Storable for $ty {
            const BOUND: $crate::stable_record::Bound = $crate::stable_record::Bound::Unbounded;

            fn to_bytes(&self) -> ::std::borrow::Cow<'_, [u8]> {
                ::std::borrow::Cow::Owned($crate::stable_record::encode(self, $what))
            }

            fn into_bytes(self) -> ::std::vec::Vec<u8> {
                $crate::stable_record::encode(&self, $what)
            }

            fn from_bytes(bytes: ::std::borrow::Cow<'_, [u8]>) -> Self {
                $crate::stable_record::decode(bytes.as_ref(), $what)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Probe {
        id: u64,
        name: String,
        tags: Option<Vec<u8>>,
    }

    fn probe() -> Probe {
        Probe {
            id: 7,
            name: "film".into(),
            tags: Some(vec![1, 2, 3]),
        }
    }

    #[test]
    fn a_value_round_trips_behind_its_layout() {
        let bytes = encode(&probe(), "probe");
        assert_eq!(bytes.first(), Some(&(RecordLayout::CURRENT as u8)));
        assert_eq!(decode::<Probe>(&bytes, "probe"), probe());
    }

    #[test]
    #[should_panic(expected = "decode probe: unknown layout 2")]
    fn another_layout_is_rejected() {
        let mut bytes = encode(&probe(), "probe");
        bytes[0] = RecordLayout::CURRENT as u8 + 1;
        let _ = decode::<Probe>(&bytes, "probe");
    }

    #[test]
    #[should_panic(expected = "decode probe: trailing bytes")]
    fn trailing_bytes_are_corruption() {
        let mut bytes = encode(&probe(), "probe");
        bytes.push(0);
        let _ = decode::<Probe>(&bytes, "probe");
    }
}
