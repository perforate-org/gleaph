//! Sortable property-index keys for equality and range postings.

use gleaph_gql::{Value, value_to_index_key_bytes};

/// Whether a property value participates in equality or range index postings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PropertyIndexability {
    /// Value has a sortable index key.
    Indexable,
    /// Value is absent from indexes (currently only [`Value::Null`]).
    Absent,
    /// Value is stored in the primary property map but has no sortable index key.
    NotIndexable,
}

/// Classifies index participation for `value`.
pub(crate) fn property_indexability(value: &Value) -> PropertyIndexability {
    match value_to_index_key_bytes(value) {
        Ok(Some(_)) => PropertyIndexability::Indexable,
        Ok(None) => PropertyIndexability::Absent,
        Err(_) => PropertyIndexability::NotIndexable,
    }
}

/// Returns the sortable index key for `value` when the value is indexable.
pub(crate) fn sortable_index_key(value: &Value) -> Option<Vec<u8>> {
    match property_indexability(value) {
        PropertyIndexability::Indexable => value_to_index_key_bytes(value).ok().flatten(),
        PropertyIndexability::Absent | PropertyIndexability::NotIndexable => None,
    }
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

    #[test]
    fn non_finite_float_is_not_indexable() {
        assert_eq!(
            property_indexability(&Value::Float64(f64::NAN)),
            PropertyIndexability::NotIndexable
        );
        assert_eq!(sortable_index_key(&Value::Float64(f64::NAN)), None);
    }
}
