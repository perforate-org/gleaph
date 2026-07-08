use std::rc::Rc;

use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;

use crate::plan::{
    ConditionalScanCandidate, EdgeInlineValuePredicate, EdgeInlineVectorPredicate, EdgeLabelRef,
    EdgeVectorMetric, IndexScanSpec, NodeLabelRef, RemovePlanItem, ScanValue, ShortestMode, Str,
    VarLenSpec, YieldColumn,
};

use rkyv::rancor;

use super::types::{
    ConditionalScanCandidateWire, EdgeInlineValuePredicateWire, EdgeInlineVectorPredicateWire,
    IndexScanSpecWire, RemovePlanItemWire, ScanValueWire, ShortestModeWire, VarLenSpecWire,
    YieldColumnWire,
};

pub(super) fn rc_str(s: &str) -> Str {
    s.into()
}

pub(super) fn opt_str_opt(s: &Option<Str>) -> Option<String> {
    s.as_ref().map(|x| x.to_string())
}

pub(super) fn opt_rc_str(s: &Option<String>) -> Option<Str> {
    s.as_ref().map(|x| x.as_str().into())
}

pub(super) fn opt_node_label_str(s: &Option<NodeLabelRef>) -> Option<String> {
    s.as_ref().map(|x| x.to_string())
}

pub(super) fn opt_edge_label_str(s: &Option<EdgeLabelRef>) -> Option<String> {
    s.as_ref().map(|x| x.to_string())
}

pub(super) fn opt_node_label(s: &Option<String>) -> Option<NodeLabelRef> {
    s.as_ref().map(|x| NodeLabelRef::from(x.as_str()))
}

pub(super) fn opt_edge_label(s: &Option<String>) -> Option<EdgeLabelRef> {
    s.as_ref().map(|x| EdgeLabelRef::from(x.as_str()))
}

pub(super) fn vec_str(v: &[Str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub(super) fn vec_rc_str(v: &[String]) -> Vec<Str> {
    v.iter().map(|s| s.as_str().into()).collect()
}

pub(super) fn vec_node_labels(v: &[NodeLabelRef]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub(super) fn vec_edge_labels(v: &[EdgeLabelRef]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub(super) fn decode_node_labels(v: &[String]) -> Vec<NodeLabelRef> {
    v.iter().map(|s| NodeLabelRef::from(s.as_str())).collect()
}

pub(super) fn decode_edge_labels(v: &[String]) -> Vec<EdgeLabelRef> {
    v.iter().map(|s| EdgeLabelRef::from(s.as_str())).collect()
}

pub(super) fn opt_str_slice(s: &Option<Rc<[Str]>>) -> Option<Vec<String>> {
    s.as_ref()
        .map(|rc| rc.iter().map(|x| x.to_string()).collect())
}

pub(super) fn decode_str_slice(s: &Option<Vec<String>>) -> Option<Rc<[Str]>> {
    s.as_ref().map(|names| {
        names
            .iter()
            .map(|n| n.as_str().into())
            .collect::<Vec<Str>>()
            .into()
    })
}

pub(super) fn encode_scan_value(v: &ScanValue) -> Result<ScanValueWire, String> {
    Ok(match v {
        ScanValue::Literal(lit) => ScanValueWire::Literal(rkyv_encode_value(lit)?),
        ScanValue::Parameter(p) => ScanValueWire::Parameter(p.to_string()),
    })
}

pub(super) fn decode_scan_value(v: &ScanValueWire) -> Result<ScanValue, String> {
    Ok(match v {
        ScanValueWire::Literal(bytes) => ScanValue::Literal(rkyv_decode_value(bytes)?),
        ScanValueWire::Parameter(p) => ScanValue::Parameter(p.as_str().into()),
    })
}

pub(super) fn encode_edge_inline_value_predicate(
    v: &Option<EdgeInlineValuePredicate>,
) -> Result<Option<EdgeInlineValuePredicateWire>, String> {
    v.as_ref()
        .map(|pred| {
            Ok(EdgeInlineValuePredicateWire {
                op: cmp_op_to_wire(pred.op),
                value: encode_scan_value(&pred.value)?,
            })
        })
        .transpose()
}

pub(super) fn decode_edge_inline_value_predicate(
    v: &Option<EdgeInlineValuePredicateWire>,
) -> Result<Option<EdgeInlineValuePredicate>, String> {
    v.as_ref()
        .map(|pred| {
            Ok(EdgeInlineValuePredicate {
                op: cmp_op_from_wire(pred.op)?,
                value: decode_scan_value(&pred.value)?,
            })
        })
        .transpose()
}

pub(super) fn encode_edge_inline_vector_predicate(
    v: &Option<EdgeInlineVectorPredicate>,
) -> Result<Option<EdgeInlineVectorPredicateWire>, String> {
    v.as_ref()
        .map(|pred| {
            Ok(EdgeInlineVectorPredicateWire {
                metric: edge_vector_metric_to_wire(pred.metric),
                query: encode_scan_value(&pred.query)?,
                op: cmp_op_to_wire(pred.op),
                threshold: encode_scan_value(&pred.threshold)?,
            })
        })
        .transpose()
}

pub(super) fn decode_edge_inline_vector_predicate(
    v: &Option<EdgeInlineVectorPredicateWire>,
) -> Result<Option<EdgeInlineVectorPredicate>, String> {
    v.as_ref()
        .map(|pred| {
            Ok(EdgeInlineVectorPredicate {
                metric: edge_vector_metric_from_wire(pred.metric)?,
                query: decode_scan_value(&pred.query)?,
                op: cmp_op_from_wire(pred.op)?,
                threshold: decode_scan_value(&pred.threshold)?,
            })
        })
        .transpose()
}

pub(super) fn edge_vector_metric_to_wire(metric: EdgeVectorMetric) -> u8 {
    match metric {
        EdgeVectorMetric::Dot => 0,
        EdgeVectorMetric::L2Squared => 1,
        EdgeVectorMetric::CosineDistance => 2,
    }
}

pub(super) fn edge_vector_metric_from_wire(metric: u8) -> Result<EdgeVectorMetric, String> {
    match metric {
        0 => Ok(EdgeVectorMetric::Dot),
        1 => Ok(EdgeVectorMetric::L2Squared),
        2 => Ok(EdgeVectorMetric::CosineDistance),
        other => Err(format!("invalid edge inline vector metric tag {other}")),
    }
}

pub(super) fn cmp_op_to_wire(op: CmpOp) -> u8 {
    match op {
        CmpOp::Eq => 0,
        CmpOp::Ne => 1,
        CmpOp::Lt => 2,
        CmpOp::Le => 3,
        CmpOp::Gt => 4,
        CmpOp::Ge => 5,
    }
}

pub(super) fn cmp_op_from_wire(op: u8) -> Result<CmpOp, String> {
    match op {
        0 => Ok(CmpOp::Eq),
        1 => Ok(CmpOp::Ne),
        2 => Ok(CmpOp::Lt),
        3 => Ok(CmpOp::Le),
        4 => Ok(CmpOp::Gt),
        5 => Ok(CmpOp::Ge),
        _ => Err(format!(
            "invalid edge inline value predicate comparison op {op}"
        )),
    }
}

pub(super) fn rkyv_encode_value(value: &Value) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

pub(super) fn rkyv_decode_value(bytes: &[u8]) -> Result<Value, String> {
    gleaph_gql::rkyv_from_wire_bytes(bytes)
}

pub(super) fn encode_indexed_edge_equality(
    eq: &Option<(Str, ScanValue)>,
) -> Result<Option<(String, ScanValueWire)>, String> {
    match eq {
        None => Ok(None),
        Some((prop, val)) => Ok(Some((prop.to_string(), encode_scan_value(val)?))),
    }
}

pub(super) fn decode_indexed_edge_equality(
    eq: &Option<(String, ScanValueWire)>,
) -> Result<Option<(Str, ScanValue)>, String> {
    match eq {
        None => Ok(None),
        Some((prop, val)) => Ok(Some((prop.as_str().into(), decode_scan_value(val)?))),
    }
}

pub(super) fn encode_conditional_candidate(
    c: &ConditionalScanCandidate,
) -> ConditionalScanCandidateWire {
    ConditionalScanCandidateWire {
        param_name: c.param_name.to_string(),
        property: c.property.to_string(),
        variable: c.variable.to_string(),
        cmp: c.cmp,
    }
}

pub(super) fn decode_conditional_candidate(
    c: &ConditionalScanCandidateWire,
) -> ConditionalScanCandidate {
    ConditionalScanCandidate {
        param_name: c.param_name.as_str().into(),
        property: c.property.as_str().into(),
        variable: c.variable.as_str().into(),
        cmp: c.cmp,
    }
}

pub(super) fn encode_index_scan_spec(s: &IndexScanSpec) -> Result<IndexScanSpecWire, String> {
    Ok(IndexScanSpecWire {
        property: s.property.to_string(),
        value: encode_scan_value(&s.value)?,
        cmp: s.cmp,
    })
}

pub(super) fn decode_index_scan_spec(s: &IndexScanSpecWire) -> Result<IndexScanSpec, String> {
    Ok(IndexScanSpec {
        property: s.property.as_str().into(),
        value: decode_scan_value(&s.value)?,
        cmp: s.cmp,
    })
}

pub(super) fn encode_yield_column(c: &YieldColumn) -> YieldColumnWire {
    YieldColumnWire {
        name: c.name.to_string(),
        alias: opt_str_opt(&c.alias),
    }
}

pub(super) fn decode_yield_column(c: &YieldColumnWire) -> YieldColumn {
    YieldColumn {
        name: c.name.as_str().into(),
        alias: opt_rc_str(&c.alias),
    }
}

pub(super) fn encode_remove_item(item: &RemovePlanItem) -> RemovePlanItemWire {
    match item {
        RemovePlanItem::Property { variable, property } => RemovePlanItemWire::Property {
            variable: variable.to_string(),
            property: property.to_string(),
        },
        RemovePlanItem::Label { variable, label } => RemovePlanItemWire::Label {
            variable: variable.to_string(),
            label: label.to_string(),
        },
    }
}

pub(super) fn decode_remove_item(item: &RemovePlanItemWire) -> RemovePlanItem {
    match item {
        RemovePlanItemWire::Property { variable, property } => RemovePlanItem::Property {
            variable: rc_str(variable),
            property: rc_str(property),
        },
        RemovePlanItemWire::Label { variable, label } => RemovePlanItem::Label {
            variable: rc_str(variable),
            label: NodeLabelRef::from(label.as_str()),
        },
    }
}

pub(super) fn var_len_to_wire(v: VarLenSpec) -> VarLenSpecWire {
    VarLenSpecWire {
        min: v.min,
        max: v.max,
    }
}

pub(super) fn var_len_from_wire(v: VarLenSpecWire) -> VarLenSpec {
    VarLenSpec {
        min: v.min,
        max: v.max,
    }
}

pub(super) fn shortest_mode_to_wire(m: ShortestMode) -> ShortestModeWire {
    match m {
        ShortestMode::AnyShortest => ShortestModeWire::AnyShortest,
        ShortestMode::AllShortest => ShortestModeWire::AllShortest,
        ShortestMode::ShortestK(k) => ShortestModeWire::ShortestK(k),
        ShortestMode::ShortestKGroup(k) => ShortestModeWire::ShortestKGroup(k),
    }
}

pub(super) fn shortest_mode_from_wire(m: ShortestModeWire) -> ShortestMode {
    match m {
        ShortestModeWire::AnyShortest => ShortestMode::AnyShortest,
        ShortestModeWire::AllShortest => ShortestMode::AllShortest,
        ShortestModeWire::ShortestK(k) => ShortestMode::ShortestK(k),
        ShortestModeWire::ShortestKGroup(k) => ShortestMode::ShortestKGroup(k),
    }
}
