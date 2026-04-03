use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gleaph_gql::Value;
use gleaph_gql_executor::{
    ExecutionError, ExecutionResultExt, OutputRow, ProcedureInvocation, ProcedureRegistry,
};
use gleaph_graph_kernel::GraphRead;

#[derive(Debug, Default)]
pub struct StandardProcedureRegistry;

impl ProcedureRegistry for StandardProcedureRegistry {
    fn call(
        &self,
        graph: &dyn GraphRead,
        invocation: &ProcedureInvocation,
    ) -> ExecutionResultExt<Vec<OutputRow>> {
        let procedure = invocation
            .name
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect::<Vec<_>>();
        match procedure.as_slice() {
            [db, labels] if db == "db" && labels == "labels" => db_labels(graph, invocation),
            [db, labels] if db == "db" && labels == "nodelabels" => {
                db_node_labels(graph, invocation)
            }
            [db, rel_types] if db == "db" && rel_types == "relationshiptypes" => {
                db_relationship_types(graph, invocation)
            }
            [db, prop_keys] if db == "db" && prop_keys == "propertykeys" => {
                db_property_keys(graph, invocation)
            }
            _ => Err(ExecutionError::UnsupportedPlanOp(
                "CallProcedure.unknown_procedure",
            )),
        }
    }
}

pub fn standard_procedure_registry() -> Arc<dyn ProcedureRegistry> {
    Arc::new(StandardProcedureRegistry)
}

pub struct DelegatingProcedureRegistry {
    primary: Arc<dyn ProcedureRegistry>,
    fallback: Arc<dyn ProcedureRegistry>,
}

impl DelegatingProcedureRegistry {
    pub fn new(primary: Arc<dyn ProcedureRegistry>, fallback: Arc<dyn ProcedureRegistry>) -> Self {
        Self { primary, fallback }
    }
}

impl ProcedureRegistry for DelegatingProcedureRegistry {
    fn call(
        &self,
        graph: &dyn GraphRead,
        invocation: &ProcedureInvocation,
    ) -> ExecutionResultExt<Vec<OutputRow>> {
        match self.primary.call(graph, invocation) {
            Err(ExecutionError::UnsupportedPlanOp("CallProcedure.unknown_procedure")) => {
                self.fallback.call(graph, invocation)
            }
            other => other,
        }
    }
}

pub fn delegated_procedure_registry(
    primary: Arc<dyn ProcedureRegistry>,
) -> Arc<dyn ProcedureRegistry> {
    Arc::new(DelegatingProcedureRegistry::new(
        primary,
        standard_procedure_registry(),
    ))
}

fn ensure_no_args(invocation: &ProcedureInvocation) -> ExecutionResultExt<()> {
    if invocation.args.is_empty() {
        Ok(())
    } else {
        Err(ExecutionError::TypeMismatch(
            "procedure does not accept arguments",
        ))
    }
}

fn collect_node_labels(graph: &dyn GraphRead) -> Result<BTreeSet<String>, ExecutionError> {
    let mut labels = BTreeSet::new();
    for node in graph.scan_nodes(None)? {
        for label in node.labels {
            labels.insert(label);
        }
    }
    Ok(labels)
}

fn collect_edges(
    graph: &dyn GraphRead,
) -> Result<BTreeMap<u64, gleaph_graph_kernel::EdgeRecord>, ExecutionError> {
    let mut edges = BTreeMap::new();
    for edge in graph.scan_all_edges()? {
        edges.entry(edge.id).or_insert(edge);
    }
    Ok(edges)
}

fn db_labels(
    graph: &dyn GraphRead,
    invocation: &ProcedureInvocation,
) -> ExecutionResultExt<Vec<OutputRow>> {
    ensure_no_args(invocation)?;
    let labels = collect_node_labels(graph)?;
    Ok(labels
        .into_iter()
        .map(|label| {
            BTreeMap::from([
                ("label".to_owned(), Value::Text(label.clone())),
                ("lbl".to_owned(), Value::Text(label)),
            ])
        })
        .collect())
}

fn db_node_labels(
    graph: &dyn GraphRead,
    invocation: &ProcedureInvocation,
) -> ExecutionResultExt<Vec<OutputRow>> {
    ensure_no_args(invocation)?;
    let labels = collect_node_labels(graph)?;
    Ok(labels
        .into_iter()
        .map(|label| BTreeMap::from([("label".to_owned(), Value::Text(label))]))
        .collect())
}

fn db_relationship_types(
    graph: &dyn GraphRead,
    invocation: &ProcedureInvocation,
) -> ExecutionResultExt<Vec<OutputRow>> {
    ensure_no_args(invocation)?;
    let mut labels = BTreeSet::new();
    for (_id, edge) in collect_edges(graph)? {
        if let Some(label) = edge.label {
            labels.insert(label);
        }
    }
    Ok(labels
        .into_iter()
        .map(|label| BTreeMap::from([("relationshipType".to_owned(), Value::Text(label))]))
        .collect())
}

fn db_property_keys(
    graph: &dyn GraphRead,
    invocation: &ProcedureInvocation,
) -> ExecutionResultExt<Vec<OutputRow>> {
    ensure_no_args(invocation)?;
    let keys = graph.all_property_key_names()?;
    Ok(keys
        .into_iter()
        .map(|key| BTreeMap::from([("propertyKey".to_owned(), Value::Text(key))]))
        .collect())
}
