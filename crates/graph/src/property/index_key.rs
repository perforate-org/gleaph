//! Sortable property-index keys for equality and range postings.

use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_graph_kernel::index::validate_index_value_key_bytes;

/// Returns the sortable index key for `value` when the value is indexable: not null, and within
/// the index's key bounds. Encoding a number normalizes its decimal digits, so the key is encoded
/// once: a second encoding to classify the value first cost about 10K instructions a value
/// (`index_export_edge_inline_page_256`).
pub(crate) fn sortable_index_key(value: &Value) -> Option<Vec<u8>> {
    value_to_index_key_bytes(value)
        .ok()
        .flatten()
        .filter(|key| validate_index_value_key_bytes(key).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    #[test]
    fn null_values_are_not_indexable() {
        assert_eq!(sortable_index_key(&Value::Null), None);
    }

    #[test]
    fn int64_values_encode_to_sortable_keys() {
        let key = sortable_index_key(&Value::Int64(42)).expect("indexable");
        assert!(!key.is_empty());
    }

    #[test]
    fn non_finite_float_is_not_indexable() {
        assert_eq!(sortable_index_key(&Value::Float64(f64::NAN)), None);
    }

    fn bytes_index_key_of_len(len: usize) -> Vec<u8> {
        assert!(len >= 3);
        value_to_index_key_bytes(&Value::Bytes(vec![1u8; len - 3]))
            .expect("encode")
            .expect("non-null")
    }

    #[test]
    fn oversized_sortable_key_is_not_indexable() {
        let at_limit = bytes_index_key_of_len(MAX_INDEX_VALUE_KEY_BYTES);
        assert_eq!(at_limit.len(), MAX_INDEX_VALUE_KEY_BYTES);
        assert_eq!(
            sortable_index_key(&Value::Bytes(vec![1u8; MAX_INDEX_VALUE_KEY_BYTES - 3])),
            Some(at_limit)
        );
        assert_eq!(
            sortable_index_key(&Value::Bytes(vec![1u8; MAX_INDEX_VALUE_KEY_BYTES - 2])),
            None
        );
    }
}
