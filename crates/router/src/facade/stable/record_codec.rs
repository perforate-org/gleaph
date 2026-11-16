//! Schema-less encoding for Router stable values that hot paths rewrite many times per call.
//!
//! Candid writes the full type table into every value and checks it on every decode. For
//! `RouterMutationRecord` that table is about 1.4 KiB of a 1.7 KiB value, so each progress
//! update paid for re-encoding and re-checking the schema rather than the data. This codec
//! writes only the data (bincode standard configuration: little-endian, varint integers).
//!
//! The encoding carries no schema, so a layout change to an encoded type needs fresh state, as
//! every other pre-production stable layout does. Decoding rejects trailing bytes, so a value
//! written for a different type cannot decode as a prefix of the expected one.

use serde::Serialize;
use serde::de::DeserializeOwned;

const CONFIG: bincode::config::Configuration = bincode::config::standard();

/// Encodes `value`. Panics only if serde cannot represent the value, which is a type defect.
pub(crate) fn encode<T: Serialize>(value: &T, what: &str) -> Vec<u8> {
    bincode::serde::encode_to_vec(value, CONFIG)
        .unwrap_or_else(|error| panic!("encode {what}: {error}"))
}

/// Decodes exactly one `T` from `bytes`. Panics on malformed or trailing bytes: stable memory
/// holds only values this module wrote, so either is corruption.
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8], what: &str) -> T {
    let (value, read) = bincode::serde::decode_from_slice(bytes, CONFIG)
        .unwrap_or_else(|error| panic!("decode {what}: {error}"));
    assert_eq!(read, bytes.len(), "decode {what}: trailing bytes");
    value
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

    #[test]
    fn a_value_round_trips() {
        let value = Probe {
            id: 7,
            name: "film".into(),
            tags: Some(vec![1, 2, 3]),
        };
        assert_eq!(decode::<Probe>(&encode(&value, "probe"), "probe"), value);
    }

    #[test]
    #[should_panic(expected = "decode probe: trailing bytes")]
    fn trailing_bytes_are_corruption() {
        let mut bytes = encode(
            &Probe {
                id: 1,
                name: String::new(),
                tags: None,
            },
            "probe",
        );
        bytes.push(0);
        let _ = decode::<Probe>(&bytes, "probe");
    }
}
