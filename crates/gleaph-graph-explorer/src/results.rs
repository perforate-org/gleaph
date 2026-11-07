//! Query results as a console table.
//!
//! UI-neutral and pre-rendered. A virtualized host asks for [`ResultTable::row`] only for the rows
//! it mounts, so the table holds display strings instead of raw wire values: formatting per commit
//! would allocate every visible cell every frame, and a console re-commits often (each adopted
//! layout snapshot rebuilds the view).
//!
//! Rendering is GQL's own value display (`gleaph-gql-value`'s `Value`), which is the vocabulary the
//! console already speaks: text unquoted, bytes as `0x…`, lists bracketed, dates and times in ISO
//! form. A value the wire cannot project is shown as `<invalid>` rather than failing the table, and
//! a column missing from a row is `<missing>` rather than a lie such as `NULL`.

use gleaph_sdk::GqlQueryResult;

/// A query result flattened into columns and display strings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultTable {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl ResultTable {
    /// Build the table from a materialized query result.
    ///
    /// A result without rows (a mutation, an empty read) is an empty table, not an error; only a
    /// wire decode failure is an error, because that means the result cannot be shown at all.
    pub fn from_result(result: &GqlQueryResult) -> Result<Self, String> {
        let Some(wire_rows) = result
            .decode_rows()
            .map_err(|error| format!("decode result rows: {error}"))?
        else {
            return Ok(Self::default());
        };
        let columns: Vec<String> = wire_rows
            .rows
            .first()
            .map(|row| row.columns.iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default();
        let rows = wire_rows
            .rows
            .into_iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|column| {
                        row.columns
                            .iter()
                            .find(|(name, _)| name == column)
                            .map(|(_, value)| display(value.clone()))
                            .unwrap_or_else(|| "<missing>".to_owned())
                    })
                    .collect()
            })
            .collect();
        Ok(Self { columns, rows })
    }

    /// The column names, in result order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The number of result rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// One row's display strings, in column order.
    pub fn row(&self, index: usize) -> Option<&[String]> {
        self.rows.get(index).map(Vec::as_slice)
    }

    /// Whether the result had no columns and no rows.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.rows.is_empty()
    }
}

/// One wire value as the console shows it.
fn display(value: gleaph_sdk::GqlWireValue) -> String {
    match value.try_into_gql_value() {
        Ok(value) => value.to_string(),
        Err(_) => "<invalid>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_ic_wire::{GqlWireRow, GqlWireRows, encode_rows_blob};
    use gleaph_router_wire::GqlQueryResult;
    use gleaph_sdk::GqlWireValue;

    fn table(rows: Vec<Vec<(&str, GqlWireValue)>>) -> ResultTable {
        let wire = GqlWireRows {
            rows: rows
                .into_iter()
                .map(|columns| GqlWireRow {
                    columns: columns
                        .into_iter()
                        .map(|(name, value)| (name.to_owned(), value))
                        .collect(),
                })
                .collect(),
        };
        let blob = encode_rows_blob(&wire).expect("rows encode");
        let result = GqlQueryResult {
            row_count: wire.rows.len() as u64,
            rows_blob: Some(blob),
            phase: None,
            token: None,
            truncated: None,
        };
        ResultTable::from_result(&result).expect("table builds")
    }

    #[test]
    fn columns_and_rows_keep_result_order() {
        let table = table(vec![
            vec![
                ("name", GqlWireValue::Text("Alice".to_owned())),
                ("age", GqlWireValue::Int64(28)),
            ],
            vec![
                ("name", GqlWireValue::Text("Bob".to_owned())),
                ("age", GqlWireValue::Int64(41)),
            ],
        ]);
        assert_eq!(table.columns(), ["name", "age"]);
        assert_eq!(table.row_count(), 2);
        assert_eq!(table.row(0).unwrap(), ["Alice", "28"]);
        assert_eq!(table.row(1).unwrap(), ["Bob", "41"]);
        assert!(table.row(2).is_none());
    }

    #[test]
    fn values_render_in_the_gql_vocabulary() {
        let table = table(vec![vec![
            ("text", GqlWireValue::Text("plain".to_owned())),
            ("bytes", GqlWireValue::Bytes(vec![0xab, 0xcd])),
            (
                "list",
                GqlWireValue::List(vec![GqlWireValue::Int32(1), GqlWireValue::Int32(2)]),
            ),
            ("nothing", GqlWireValue::Null),
            ("flag", GqlWireValue::Bool(true)),
            ("ratio", GqlWireValue::Float64(0.5)),
        ]]);
        let row = table.row(0).unwrap();
        assert_eq!(row[0], "plain");
        assert_eq!(row[1], "0xabcd", "element ids read as hex");
        assert_eq!(row[2], "[1, 2]");
        assert_eq!(row[3], "NULL");
        assert_eq!(row[4], "true");
        assert_eq!(row[5], "0.5");
    }

    #[test]
    fn a_missing_column_is_marked_not_invented() {
        let table = table(vec![
            vec![("a", GqlWireValue::Int64(1)), ("b", GqlWireValue::Int64(2))],
            vec![("a", GqlWireValue::Int64(3))],
        ]);
        assert_eq!(table.row(1).unwrap(), ["3", "<missing>"]);
    }

    #[test]
    fn a_result_without_rows_is_an_empty_table() {
        let result = GqlQueryResult {
            row_count: 0,
            rows_blob: None,
            phase: None,
            token: None,
            truncated: None,
        };
        let table = ResultTable::from_result(&result).expect("no rows is not an error");
        assert!(table.is_empty());
        assert_eq!(table.row_count(), 0);
        assert!(table.columns().is_empty());
    }
}
