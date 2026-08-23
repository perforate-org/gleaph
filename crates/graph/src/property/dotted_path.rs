//! One canonical dotted-path resolver shared by every domain that reads leaves out of
//! [`Value::Record`] trees (edge inline struct encoding, vertex nested record indexing).

use gleaph_gql::Value;

/// Walks `path` (`a.b.c`) through [`Value::Record`] nodes.
///
/// A missing field or a non-record intermediate yields `None` — the shared absence rule:
/// shape drift is "no value", never an error (ADR 0073 §2).
pub(crate) fn record_value_at_dotted_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        let Value::Record(fields) = current else {
            return None;
        };
        current = &fields.iter().find(|(name, _)| name == part)?.1;
    }
    Some(current)
}

/// Classifies the resolved leaf of a declared nested path (ADR 0073 §5).
///
/// Container leaves (list/record/path/extension) stay unindexable: a declared path whose
/// leaf resolves to one yields no value rather than silently posting under the general
/// comparison-domain encoding.
pub(crate) fn nested_leaf_posting_value(value: &Value) -> Option<&Value> {
    match value {
        Value::List(_) | Value::Record(_) | Value::Path(_) | Value::Extension(_) => None,
        _ => Some(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_record() -> Value {
        Value::Record(vec![
            ("score".to_owned(), Value::Int64(30)),
            (
                "nested".to_owned(),
                Value::Record(vec![("deep".to_owned(), Value::Int64(7))]),
            ),
        ])
    }

    #[test]
    fn resolves_scalar_leaf_through_nested_records() {
        let record = stats_record();
        assert_eq!(
            record_value_at_dotted_path(&record, "score"),
            Some(&Value::Int64(30))
        );
        assert_eq!(
            record_value_at_dotted_path(&record, "nested.deep"),
            Some(&Value::Int64(7))
        );
    }

    #[test]
    fn missing_field_yields_no_value() {
        let record = stats_record();
        assert_eq!(record_value_at_dotted_path(&record, "absent"), None);
        assert_eq!(record_value_at_dotted_path(&record, "nested.absent"), None);
    }

    #[test]
    fn non_record_intermediate_yields_no_value() {
        let record = stats_record();
        assert_eq!(record_value_at_dotted_path(&record, "score.deep"), None);
    }
}
