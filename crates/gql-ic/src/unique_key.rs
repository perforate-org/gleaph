//! Canonical `encoded_value` for cross-shard uniqueness reservations (ADR 0030).
//!
//! The reservation key is `(graph_id, constraint_id, encoded_value)`. `encoded_value` must be a
//! **total, injective function of GQL value equality** for the constrained property's type — two
//! values share a key *iff* GQL considers them equal — otherwise the uniqueness gate either
//! false-rejects distinct values or false-admits equal ones.
//!
//! Rather than invent a second value encoding, this reuses the single source of truth,
//! [`gleaph_gql::value_to_index_key_bytes`] (the order-preserving property-index key), which already
//! guarantees the equality-injective contract: GQL-equal numbers across widths/types collapse to
//! identical bytes, strings are byte-exact canonical UTF-8 with no implicit normalization, and
//! non-finite floats are rejected. This module adds only the reservation-specific framing ADR 0030
//! requires on top of that encoding:
//!
//! - **NULL / missing → no claim.** SQL-style: a `NULL` constrained property reserves nothing and
//!   multiple `NULL`s coexist. (Only the *top-level* value; a `NULL` nested inside a list/record is
//!   part of a concrete, claimable value.)
//! - **NaN / non-finite → rejected.** `NaN ≠ NaN` in GQL, so it has no stable key identity and
//!   cannot participate in a uniqueness constraint.
//! - **Unsupported → rejected.** A value with no canonical key (e.g. a non-orderable extension).
//! - **Over-length → rejected.** The encoding is bounded by [`MAX_UNIQUE_ENCODED_VALUE_LEN`]; an
//!   over-long value is rejected rather than hashed (hashing would reintroduce collision risk).

use gleaph_gql::value::Value;
use gleaph_gql::{ValueIndexKeyError, value_to_index_key_bytes};

/// Maximum length, in bytes, of a canonical `encoded_value` admitted into a uniqueness
/// reservation. The reservation key embeds these bytes, so this is the bound the Router reservation
/// map (ADR 0030 slice 3) enforces on its key; the encoder and the key share this single constant.
/// A value whose canonical encoding exceeds it is rejected (no hashing — see the module docs).
pub const MAX_UNIQUE_ENCODED_VALUE_LEN: usize = 2048;

/// Why a value cannot become a uniqueness reservation key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UniqueKeyRejection {
    /// A non-finite float (`NaN`/`±∞`) has no stable equality identity.
    NonFinite,
    /// The value type has no canonical, order-preserving key encoding.
    Unsupported,
    /// The canonical encoding exceeds [`MAX_UNIQUE_ENCODED_VALUE_LEN`].
    TooLong { len: usize, max: usize },
}

/// Outcome of canonicalizing a constrained property value into an `encoded_value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UniqueKeyOutcome {
    /// The value makes a uniqueness claim keyed by these canonical bytes.
    Claim(Vec<u8>),
    /// The value (top-level `NULL`/missing) makes no claim; it is not reservable.
    NoClaim,
    /// The value cannot be a uniqueness key.
    Rejected(UniqueKeyRejection),
}

/// Canonicalizes a constrained property value into its reservation `encoded_value`.
///
/// See the module docs for the full contract. The returned [`UniqueKeyOutcome::Claim`] bytes are a
/// total, injective function of GQL value equality.
pub fn encode_unique_value(value: &Value) -> UniqueKeyOutcome {
    match value_to_index_key_bytes(value) {
        Ok(None) => UniqueKeyOutcome::NoClaim,
        Ok(Some(bytes)) => {
            if bytes.len() > MAX_UNIQUE_ENCODED_VALUE_LEN {
                UniqueKeyOutcome::Rejected(UniqueKeyRejection::TooLong {
                    len: bytes.len(),
                    max: MAX_UNIQUE_ENCODED_VALUE_LEN,
                })
            } else {
                UniqueKeyOutcome::Claim(bytes)
            }
        }
        Err(ValueIndexKeyError::NonFiniteFloat) => {
            UniqueKeyOutcome::Rejected(UniqueKeyRejection::NonFinite)
        }
        Err(ValueIndexKeyError::UnsupportedValue) => {
            UniqueKeyOutcome::Rejected(UniqueKeyRejection::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::types::Decimal;
    use gleaph_gql::value::ExtensionValue;
    use std::any::Any;
    use std::fmt;

    /// An extension value that opts out of sortable indexing, so it has no canonical key.
    #[derive(Clone, Debug)]
    struct NonOrderableExt;

    impl fmt::Display for NonOrderableExt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "NonOrderableExt")
        }
    }

    impl ExtensionValue for NonOrderableExt {
        fn type_name(&self) -> &str {
            "NonOrderableExt"
        }
        fn clone_box(&self) -> Box<dyn ExtensionValue> {
            Box::new(self.clone())
        }
        fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
            other.as_any().downcast_ref::<Self>().is_some()
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn claim(value: &Value) -> Vec<u8> {
        match encode_unique_value(value) {
            UniqueKeyOutcome::Claim(bytes) => bytes,
            other => panic!("expected Claim, got {other:?}"),
        }
    }

    fn decimal(text: &str) -> Value {
        Value::Decimal(Decimal::parse(text).expect("decimal"))
    }

    #[test]
    fn gql_equal_numbers_share_one_key_across_types_and_widths() {
        let five = [
            Value::Int64(5),
            Value::Uint8(5),
            Value::Int32(5),
            decimal("5.0"),
            decimal("5.00"),
            Value::Float64(5.0),
            Value::Float32(5.0),
        ];
        let keys: Vec<_> = five.iter().map(claim).collect();
        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "GQL-equal numbers must share one encoded_value: {keys:?}"
        );
    }

    #[test]
    fn distinct_values_get_distinct_keys() {
        assert_ne!(claim(&Value::Int64(5)), claim(&Value::Int64(6)));
        assert_ne!(
            claim(&Value::Text("a".into())),
            claim(&Value::Text("b".into()))
        );
        // Exact decimal 0.1 and binary float 0.1 are not GQL-equal, so they must not collide.
        assert_ne!(claim(&decimal("0.1")), claim(&Value::Float64(0.1)));
    }

    #[test]
    fn signed_zero_and_negative_zero_collapse() {
        assert_eq!(claim(&Value::Float64(0.0)), claim(&Value::Float64(-0.0)));
        assert_eq!(claim(&Value::Float64(0.0)), claim(&Value::Int64(0)));
    }

    #[test]
    fn top_level_null_makes_no_claim() {
        assert_eq!(encode_unique_value(&Value::Null), UniqueKeyOutcome::NoClaim);
    }

    #[test]
    fn nested_null_is_a_concrete_claimable_value() {
        // A list containing NULL is a concrete value, not a "missing property".
        assert!(matches!(
            encode_unique_value(&Value::List(vec![Value::Null])),
            UniqueKeyOutcome::Claim(_)
        ));
    }

    #[test]
    fn nan_and_infinity_are_rejected() {
        assert_eq!(
            encode_unique_value(&Value::Float64(f64::NAN)),
            UniqueKeyOutcome::Rejected(UniqueKeyRejection::NonFinite)
        );
        assert_eq!(
            encode_unique_value(&Value::Float64(f64::INFINITY)),
            UniqueKeyOutcome::Rejected(UniqueKeyRejection::NonFinite)
        );
        assert_eq!(
            encode_unique_value(&Value::Float64(f64::NEG_INFINITY)),
            UniqueKeyOutcome::Rejected(UniqueKeyRejection::NonFinite)
        );
    }

    #[test]
    fn strings_are_byte_exact_without_unicode_normalization() {
        // "é" as a single precomposed scalar (U+00E9) vs. "e" + combining acute (U+0065 U+0301):
        // canonically NFC-equal, but byte-distinct. The first cut does no normalization, so they
        // are distinct uniqueness keys.
        let precomposed = Value::Text("\u{00E9}".into());
        let decomposed = Value::Text("e\u{0301}".into());
        assert_ne!(claim(&precomposed), claim(&decomposed));
    }

    #[test]
    fn encoded_length_exactly_at_bound_is_admitted_one_past_is_rejected() {
        // A NUL-free ASCII text key encodes as: 1-byte tag + body (passed through unescaped) +
        // 2-byte terminator, so encoded_len == body_len + 3. This lets us hit the bound exactly.
        let at_bound = Value::Text("a".repeat(MAX_UNIQUE_ENCODED_VALUE_LEN - 3));
        match encode_unique_value(&at_bound) {
            UniqueKeyOutcome::Claim(bytes) => {
                assert_eq!(
                    bytes.len(),
                    MAX_UNIQUE_ENCODED_VALUE_LEN,
                    "body MAX-3 must encode to exactly MAX bytes"
                );
            }
            other => panic!("value encoding to exactly MAX must be a Claim, got {other:?}"),
        }

        let one_past = Value::Text("a".repeat(MAX_UNIQUE_ENCODED_VALUE_LEN - 2));
        match encode_unique_value(&one_past) {
            UniqueKeyOutcome::Rejected(UniqueKeyRejection::TooLong { len, max }) => {
                assert_eq!(len, MAX_UNIQUE_ENCODED_VALUE_LEN + 1);
                assert_eq!(max, MAX_UNIQUE_ENCODED_VALUE_LEN);
            }
            other => panic!("value encoding to MAX+1 must be TooLong, got {other:?}"),
        }
    }

    #[test]
    fn value_under_the_bound_is_admitted() {
        assert!(matches!(
            encode_unique_value(&Value::Text("user@example.com".into())),
            UniqueKeyOutcome::Claim(_)
        ));
    }

    #[test]
    fn unsupported_value_is_rejected() {
        // An extension with no `sortable_index_key` has no canonical key encoding.
        let value = Value::Extension(Box::new(NonOrderableExt));
        assert_eq!(
            encode_unique_value(&value),
            UniqueKeyOutcome::Rejected(UniqueKeyRejection::Unsupported)
        );
    }

    #[test]
    fn claim_bytes_are_deterministic() {
        let value = Value::Text("stable@example.com".into());
        assert_eq!(claim(&value), claim(&value));
    }
}
