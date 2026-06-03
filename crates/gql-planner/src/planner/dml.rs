use gleaph_gql::ast::*;
use gleaph_gql::type_check::BindingKind;

use crate::plan::*;

pub(super) fn plan_insert(
    insert_stmt: &InsertStatement,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    for pattern in &insert_stmt.patterns {
        let mut prev_node_var: Option<String> = None;

        for (i, element) in pattern.elements.iter().enumerate() {
            match element {
                InsertElement::Node(node) => {
                    let var = node
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("__insert_n{}", i));
                    let props: Vec<PropertyAssignment> = node
                        .properties
                        .iter()
                        .map(|p| PropertyAssignment {
                            name: p.name.clone().into(),
                            value: p.value.clone(),
                        })
                        .collect();
                    ops.push(PlanOp::InsertVertex {
                        variable: Some(Str::from(var.as_str())),
                        labels: node
                            .labels
                            .iter()
                            .map(|s| NodeLabelRef::from(s.as_str()))
                            .collect(),
                        properties: props,
                    });
                    prev_node_var = Some(var);
                }
                InsertElement::Edge(edge) => {
                    let var = edge
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("__insert_e{}", i));
                    let src = prev_node_var.clone().unwrap_or_default();
                    // Lookahead for destination node.
                    let dst = pattern.elements[i + 1..]
                        .iter()
                        .find_map(|e| match e {
                            InsertElement::Node(n) => Some(
                                n.variable
                                    .clone()
                                    .unwrap_or_else(|| format!("__insert_n{}", i + 1)),
                            ),
                            _ => None,
                        })
                        .unwrap_or_else(|| format!("__insert_dst_{}", i));
                    let props: Vec<PropertyAssignment> = edge
                        .properties
                        .iter()
                        .map(|p| PropertyAssignment {
                            name: p.name.clone().into(),
                            value: p.value.clone(),
                        })
                        .collect();
                    ops.push(PlanOp::InsertEdge {
                        variable: Some(Str::from(var.as_str())),
                        src: Str::from(src.as_str()),
                        dst: Str::from(dst.as_str()),
                        direction: edge.direction,
                        labels: edge
                            .labels
                            .iter()
                            .map(|s| EdgeLabelRef::from(s.as_str()))
                            .collect(),
                        properties: props,
                    });
                    prev_node_var = None;
                }
            }
        }
    }
}

pub(super) fn plan_set(
    set_stmt: &SetStatement,
    _binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    let items: Vec<SetPlanItem> = set_stmt
        .items
        .iter()
        .map(|item| match item {
            SetItem::Property {
                span: _,
                variable,
                property,
                value,
            } => SetPlanItem::Property {
                variable: variable.clone().into(),
                property: property.clone().into(),
                value: value.clone(),
            },
            SetItem::AllProperties {
                span: _,
                variable,
                value,
            } => SetPlanItem::AllProperties {
                variable: variable.clone().into(),
                value: value.clone(),
            },
            SetItem::Label {
                span: _,
                variable,
                label,
                ..
            } => SetPlanItem::Label {
                variable: variable.clone().into(),
                label: label.clone().into(),
            },
        })
        .collect();

    ops.push(PlanOp::SetProperties { items });
}

pub(super) fn plan_remove(
    remove_stmt: &RemoveStatement,
    _binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    let items: Vec<RemovePlanItem> = remove_stmt
        .items
        .iter()
        .map(|item| match item {
            RemoveItem::Property {
                span: _,
                variable,
                property,
            } => RemovePlanItem::Property {
                variable: variable.clone().into(),
                property: property.clone().into(),
            },
            RemoveItem::Label {
                span: _,
                variable,
                label,
                ..
            } => RemovePlanItem::Label {
                variable: variable.clone().into(),
                label: label.clone().into(),
            },
        })
        .collect();

    ops.push(PlanOp::RemoveProperties { items });
}

pub(super) fn plan_delete(
    delete_stmt: &DeleteStatement,
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    for item in &delete_stmt.items {
        let variable: Str = match &item.kind {
            ExprKind::Variable(v) => Str::from(v.as_str()),
            _ => Str::from(format!("{:?}", item.kind).as_str()),
        };

        match binding_kinds
            .get(variable.as_ref())
            .copied()
            .unwrap_or(BindingKind::Unknown)
        {
            BindingKind::Edge => {
                ops.push(PlanOp::DeleteEdge { variable });
            }
            BindingKind::Node | BindingKind::Unknown | BindingKind::Path | BindingKind::Value => {
                match delete_stmt.detach {
                    DeleteDetach::Detach => {
                        ops.push(PlanOp::DetachDeleteVertex { variable });
                    }
                    DeleteDetach::NoDetach | DeleteDetach::Unspecified => {
                        ops.push(PlanOp::DeleteVertex { variable });
                    }
                }
            }
        }
    }
}
