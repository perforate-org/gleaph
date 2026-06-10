//! Sortable property-index keys for equality and range postings.

use gleaph_gql::{Value, value_to_index_key_bytes};

/// Returns the sortable index key for `value` when the value is indexable.
///
/// `None` means the value is absent from indexes: nulls, non-finite floats, and
/// values with no sortable encoding.
pub(crate) fn sortable_index_key(value: &Value) -> Option<Vec<u8>> {
    value_to_index_key_bytes(value).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;

    #[test]
    fn null_values_are_not_indexable() {
        assert_eq!(sortable_index_key(&Value::Null), None);
    }

    #[test]
    fn int64_values_encode_to_sortable_keys() {
        let key = sortable_index_key(&Value::Int64(42)).expect("indexable");
        assert!(!key.is_empty());
    }
}
