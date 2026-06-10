//! Primary-store property value validation and persisted encoding.

use gleaph_gql::{Value, ValueBinaryError};
use gleaph_graph_kernel::entry::PropertyId;

/// Rejects reserved property id `0` used by both vertex and edge property stores.
pub(crate) fn ensure_property_id(property_id: PropertyId) -> Result<(), PropertyId> {
    if property_id.raw() == 0 {
        Err(property_id)
    } else {
        Ok(())
    }
}

/// Validates that `value` can be persisted with [`Value::to_binary_bytes`].
pub(crate) fn ensure_persistable(value: &Value) -> Result<(), ValueBinaryError> {
    value.to_binary_bytes().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;

    #[test]
    fn reserved_property_id_is_rejected() {
        assert!(ensure_property_id(PropertyId::default()).is_err());
        assert!(ensure_property_id(PropertyId::from_raw(1)).is_ok());
    }

    #[test]
    fn nan_float_is_persistable() {
        ensure_persistable(&Value::Float64(f64::NAN)).expect("NaN encodes for primary store");
    }
}
