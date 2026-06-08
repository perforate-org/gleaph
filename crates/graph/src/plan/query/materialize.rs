//! Schema-driven hydration of [`PlanQueryRow`] into GQL [`Value`] rows.

use std::collections::HashMap;

use gleaph_gql::Value;
use gleaph_gql_planner::{OutputBindingKind, OutputColumn, OutputSchema};

use super::error::PlanQueryError;
use super::executor::{PlanQueryRow, binding_to_value, path_binding_to_value};
use crate::facade::GraphStore;
use crate::plan::query::{PathBinding, PlanBinding};

/// Query result before GQL value hydration.
#[derive(Clone, Debug, Default)]
pub struct PlanQueryBindings {
    pub rows: Vec<PlanQueryRow>,
}

/// Per-query caches reused across many result rows (paths, etc.).
pub(crate) struct MaterializeCtx<'a> {
    store: &'a GraphStore,
    path_cache: HashMap<(usize, usize), Value>,
}

impl<'a> MaterializeCtx<'a> {
    pub(crate) fn new(store: &'a GraphStore) -> Self {
        Self {
            store,
            path_cache: HashMap::new(),
        }
    }

    fn materialize_path(&mut self, pb: &PathBinding) -> Value {
        let key = pb.materialize_cache_key();
        if let Some(cached) = self.path_cache.get(&key) {
            return cached.clone();
        }
        let value = path_binding_to_value(self.store, pb);
        self.path_cache.insert(key, value.clone());
        value
    }

    fn materialize_binding(
        &mut self,
        binding: &PlanBinding,
        kind: OutputBindingKind,
    ) -> Result<Value, PlanQueryError> {
        match (kind, binding) {
            (OutputBindingKind::Path, PlanBinding::Path(pb)) => Ok(self.materialize_path(pb)),
            (OutputBindingKind::Vertex, PlanBinding::Vertex(_))
            | (OutputBindingKind::Edge, PlanBinding::Edge(_))
            | (OutputBindingKind::RemoteVertex, PlanBinding::RemoteVertex(_))
            | (OutputBindingKind::Scalar, PlanBinding::Value(_))
            | (OutputBindingKind::Dynamic, _) => binding_to_value(self.store, None, binding),
            (_, actual) => binding_to_value(self.store, None, actual),
        }
    }
}

fn resolve_column_binding<'a>(
    row: &'a PlanQueryRow,
    column: &OutputColumn,
) -> Option<&'a PlanBinding> {
    row.get(column.name.as_ref()).or_else(|| {
        column
            .source_var
            .as_ref()
            .and_then(|var| row.get(var.as_ref()))
    })
}

fn single_path_output_column(schema: &OutputSchema) -> Option<&OutputColumn> {
    if schema.columns.len() != 1 || schema.columns[0].kind != OutputBindingKind::Path {
        return None;
    }
    Some(&schema.columns[0])
}

pub(crate) fn materialize_plan_rows(
    store: &GraphStore,
    rows: &[PlanQueryRow],
    schema: &OutputSchema,
) -> Result<Vec<std::collections::BTreeMap<String, Value>>, PlanQueryError> {
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = canbench_rs::bench_scope("plan_query_materialize_value_rows");

    let mut ctx = MaterializeCtx::new(store);
    let mut out = Vec::with_capacity(rows.len());

    if schema.hydrates_all_row_bindings() {
        for row in rows {
            out.push(super::executor::value_row(store, row)?);
        }
        return Ok(out);
    }

    if let Some(column) = single_path_output_column(schema) {
        let name = column.name.to_string();
        for row in rows {
            let value = match resolve_column_binding(row, column) {
                Some(PlanBinding::Path(pb)) => ctx.materialize_path(pb),
                Some(PlanBinding::PathGroup(paths)) => Value::List(
                    paths.iter().map(|pb| ctx.materialize_path(pb)).collect(),
                ),
                Some(binding) => ctx.materialize_binding(binding, column.kind)?,
                None => Value::Null,
            };
            out.push(std::collections::BTreeMap::from([(name.clone(), value)]));
        }
        return Ok(out);
    }

    for row in rows {
        let mut mapped = std::collections::BTreeMap::new();
        for column in &schema.columns {
            let value = match resolve_column_binding(row, column) {
                Some(binding) => ctx.materialize_binding(binding, column.kind)?,
                None => Value::Null,
            };
            mapped.insert(column.name.to_string(), value);
        }
        out.push(mapped);
    }
    Ok(out)
}

pub fn hydrate_plan_rows(
    store: &GraphStore,
    bindings: &PlanQueryBindings,
    schema: &OutputSchema,
) -> Result<super::executor::PlanQueryResult, PlanQueryError> {
    Ok(super::executor::PlanQueryResult {
        rows: materialize_plan_rows(store, &bindings.rows, schema)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::GraphStore;
    use gleaph_gql::Value;
    use gleaph_gql_planner::plan::Str;
    use gleaph_gql_planner::{OutputBindingKind, OutputColumn};

    #[test]
    fn hydrates_all_row_bindings_when_schema_empty() {
        let store = GraphStore::new();
        let v = store.insert_vertex().expect("vertex");
        let mut row = PlanQueryRow::new();
        row.insert("n".into(), PlanBinding::Vertex(v));
        let out = materialize_plan_rows(&store, &[row], &OutputSchema::default()).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].contains_key("n"));
    }

    #[test]
    fn materializes_named_vertex_column() {
        let store = GraphStore::new();
        let v = store.insert_vertex().expect("vertex");
        let mut row = PlanQueryRow::new();
        row.insert("n".into(), PlanBinding::Vertex(v));
        let schema = OutputSchema {
            columns: vec![OutputColumn {
                name: Str::from("n"),
                kind: OutputBindingKind::Vertex,
                source_var: Some(Str::from("n")),
            }],
        };
        let out = materialize_plan_rows(&store, &[row], &schema).unwrap();
        assert_eq!(out.len(), 1);
        match &out[0]["n"] {
            Value::Record(fields) => assert!(!fields.is_empty()),
            other => panic!("expected vertex record, got {other:?}"),
        }
    }

    #[test]
    fn missing_binding_yields_null_for_column() {
        let store = GraphStore::new();
        let row = PlanQueryRow::new();
        let schema = OutputSchema {
            columns: vec![OutputColumn {
                name: Str::from("missing"),
                kind: OutputBindingKind::Vertex,
                source_var: Some(Str::from("missing")),
            }],
        };
        let out = materialize_plan_rows(&store, &[row], &schema).unwrap();
        assert_eq!(out[0]["missing"], Value::Null);
    }

    #[test]
    fn resolves_column_via_source_var_alias() {
        let store = GraphStore::new();
        let v = store.insert_vertex().expect("vertex");
        let mut row = PlanQueryRow::new();
        row.insert("n".into(), PlanBinding::Vertex(v));
        let schema = OutputSchema {
            columns: vec![OutputColumn {
                name: Str::from("alias"),
                kind: OutputBindingKind::Vertex,
                source_var: Some(Str::from("n")),
            }],
        };
        let out = materialize_plan_rows(&store, &[row], &schema).unwrap();
        match &out[0]["alias"] {
            Value::Record(_) => {}
            other => panic!("expected vertex record via source_var, got {other:?}"),
        }
    }

    #[test]
    fn materializes_scalar_value_column() {
        let store = GraphStore::new();
        let mut row = PlanQueryRow::new();
        row.insert("x".into(), PlanBinding::Value(Value::Int64(42)));
        let schema = OutputSchema {
            columns: vec![OutputColumn {
                name: Str::from("x"),
                kind: OutputBindingKind::Scalar,
                source_var: Some(Str::from("x")),
            }],
        };
        let out = materialize_plan_rows(&store, &[row], &schema).unwrap();
        assert_eq!(out[0]["x"], Value::Int64(42));
    }
}
