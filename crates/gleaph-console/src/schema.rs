//! The graph's label vocabulary and result shape, as the console reads it.
//!
//! In production this is the schema the console loads from the graph; for the prototype it is the
//! embedded fixture so the label colours, the legend and the result columns all come from data
//! rather than from constants compiled into the UI.

use serde::Deserialize;

/// One result column the schema declares.
#[derive(Clone, Debug, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    /// The unit shown beside the name, when the column is a measurement.
    pub unit: Option<String>,
}

impl ResultColumn {
    /// The header text: the unit belongs with the name, not in every cell.
    pub fn header(&self) -> String {
        match &self.unit {
            Some(unit) => format!("{} ({unit})", self.name),
            None => self.name.clone(),
        }
    }
}

/// The schema facts the console draws from.
#[derive(Clone, Debug, Deserialize)]
pub struct Schema {
    pub node_labels: Vec<String>,
    pub edge_labels: Vec<String>,
    pub result_columns: Vec<ResultColumn>,
}

impl Schema {
    /// The embedded fixture, standing in for the graph's own schema until the client is wired.
    pub fn fixture() -> Self {
        serde_json::from_str(include_str!("../schema.json")).expect("the schema fixture parses")
    }
}
