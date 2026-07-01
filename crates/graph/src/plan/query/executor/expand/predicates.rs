use std::collections::BTreeMap;

use crate::edge_payload_scalar_codec::encode_edge_payload_scalar;
use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;
use gleaph_gql_planner::plan::{
    EdgePayloadPredicate, EdgeVectorMetric as PlanEdgeVectorMetric, EdgeVectorPredicate, ScanValue,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgePayloadEncoding, EdgePayloadProfile};

use crate::plan::query::edge_payload_batch_kernel::PreparedEdgePayloadBatchKernel;
use crate::plan::query::edge_vector_kernel::{
    EdgeVectorMetric as KernelEdgeVectorMetric, PreparedEdgeVectorKernel,
};
use crate::plan::query::error::PlanQueryError;
use gleaph_graph_kernel::plan_exec::ResolvedLabelTable;
#[derive(Clone, Debug)]
pub(crate) struct PreparedEdgePayloadPredicate {
    pub(crate) kernel: PreparedEdgePayloadBatchKernel,
    pub(crate) op: CmpOp,
    pub(crate) expected: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedEdgeVectorThreshold {
    pub(crate) kernel: PreparedEdgeVectorKernel,
    metric: KernelEdgeVectorMetric,
    query: Vec<f32>,
    op: CmpOp,
    threshold: f32,
}

impl PreparedEdgeVectorThreshold {
    pub(crate) fn prepare(
        resolved_labels: Option<&ResolvedLabelTable>,
        label_id: EdgeLabelId,
        predicate: &EdgeVectorPredicate,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Option<Self>, PlanQueryError> {
        let profile =
            crate::edge_payload_schema::lookup_edge_payload_profile_with(resolved_labels, label_id);
        if profile.required_byte_width() == 0 {
            return Ok(None);
        }
        let EdgePayloadEncoding::VectorF32 { dims } = profile.encoding else {
            return Err(PlanQueryError::UnsupportedOp(
                "edge vector predicate for non-vector encodings",
            ));
        };
        let query = scan_value_to_f32_vector(&predicate.query, parameters)?;
        let threshold = scan_value_to_f32(&predicate.threshold, parameters)?;
        profile
            .validate()
            .map_err(|err| PlanQueryError::InvalidExpressionValue {
                expression: format!("edge vector payload profile: {err}"),
            })?;
        if usize::from(dims) != query.len() {
            return Err(PlanQueryError::InvalidExpressionValue {
                expression: "edge vector query dimension".into(),
            });
        }
        let Some(kernel) = PreparedEdgeVectorKernel::new(usize::from(dims)) else {
            return Ok(None);
        };
        Ok(Some(Self {
            kernel,
            metric: kernel_edge_vector_metric(predicate.metric),
            query,
            op: predicate.op,
            threshold,
        }))
    }

    pub(crate) fn collect_matching_indices(&self, payload_bytes: &[u8], out: &mut Vec<usize>) {
        match (self.metric, self.op) {
            (KernelEdgeVectorMetric::L2Squared, CmpOp::Lt) => {
                self.kernel.collect_l2_squared_upper_bound_indices(
                    payload_bytes,
                    &self.query,
                    self.threshold,
                    false,
                    out,
                )
            }
            (KernelEdgeVectorMetric::L2Squared, CmpOp::Le) => {
                self.kernel.collect_l2_squared_upper_bound_indices(
                    payload_bytes,
                    &self.query,
                    self.threshold,
                    true,
                    out,
                )
            }
            _ => self.kernel.collect_matching_indices(
                payload_bytes,
                &self.query,
                self.metric,
                self.threshold,
                |score, threshold| match self.op {
                    CmpOp::Lt => score < threshold,
                    CmpOp::Le => score <= threshold,
                    CmpOp::Gt => score > threshold,
                    CmpOp::Ge => score >= threshold,
                    CmpOp::Eq | CmpOp::Ne => false,
                },
                out,
            ),
        }
    }
}

fn kernel_edge_vector_metric(metric: PlanEdgeVectorMetric) -> KernelEdgeVectorMetric {
    match metric {
        PlanEdgeVectorMetric::Dot => KernelEdgeVectorMetric::Dot,
        PlanEdgeVectorMetric::L2Squared => KernelEdgeVectorMetric::L2Squared,
        PlanEdgeVectorMetric::CosineDistance => KernelEdgeVectorMetric::CosineDistance,
    }
}

impl PreparedEdgePayloadPredicate {
    pub(crate) fn prepare(
        resolved_labels: Option<&ResolvedLabelTable>,
        label_id: EdgeLabelId,
        predicate: &EdgePayloadPredicate,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Option<Self>, PlanQueryError> {
        let profile =
            crate::edge_payload_schema::lookup_edge_payload_profile_with(resolved_labels, label_id);
        if profile.required_byte_width() == 0 {
            return Ok(None);
        }
        let Some(expected) =
            scan_value_to_edge_payload_bytes(&profile, &predicate.value, parameters)?
        else {
            return Ok(None);
        };
        let kernel = PreparedEdgePayloadBatchKernel::new(profile.byte_width, profile.encoding);
        Ok(Some(Self {
            kernel,
            op: predicate.op,
            expected,
        }))
    }
}

fn scan_value_to_edge_payload_bytes(
    profile: &EdgePayloadProfile,
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let value = match scan_value {
        ScanValue::Literal(value) => value,
        ScanValue::Parameter(name) => {
            parameters
                .get(name.as_ref())
                .ok_or_else(|| PlanQueryError::MissingParameter {
                    name: name.to_string(),
                })?
        }
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    edge_payload_bytes_from_value(profile, value)
}

fn scan_value_to_f32_vector(
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<f32>, PlanQueryError> {
    let value = scan_value_to_value(scan_value, parameters)?;
    let Value::List(items) = value else {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: "edge vector query".into(),
        });
    };
    items
        .iter()
        .map(f32_from_value)
        .collect::<Result<Vec<_>, _>>()
}

fn scan_value_to_f32(
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<f32, PlanQueryError> {
    f32_from_value(scan_value_to_value(scan_value, parameters)?)
}

fn scan_value_to_value<'a>(
    scan_value: &'a ScanValue,
    parameters: &'a BTreeMap<String, Value>,
) -> Result<&'a Value, PlanQueryError> {
    match scan_value {
        ScanValue::Literal(value) => Ok(value),
        ScanValue::Parameter(name) => {
            parameters
                .get(name.as_ref())
                .ok_or_else(|| PlanQueryError::MissingParameter {
                    name: name.to_string(),
                })
        }
    }
}

fn edge_payload_bytes_from_value(
    profile: &EdgePayloadProfile,
    value: &Value,
) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let bytes = match &profile.encoding {
        EdgePayloadEncoding::RawU8
        | EdgePayloadEncoding::RawU16
        | EdgePayloadEncoding::RawU32
        | EdgePayloadEncoding::RawU64
        | EdgePayloadEncoding::RawI8
        | EdgePayloadEncoding::RawI16
        | EdgePayloadEncoding::RawI32
        | EdgePayloadEncoding::RawI64
        | EdgePayloadEncoding::RawU128
        | EdgePayloadEncoding::RawI128
        | EdgePayloadEncoding::F16
        | EdgePayloadEncoding::F32
        | EdgePayloadEncoding::F64
        | EdgePayloadEncoding::RawFixed32
        | EdgePayloadEncoding::RawFixed64 => {
            return encode_edge_payload_scalar(profile, value)
                .map(Some)
                .map_err(|err| PlanQueryError::InvalidExpressionValue {
                    expression: format!("edge payload scalar literal: {err}"),
                });
        }
        EdgePayloadEncoding::WeightRawU16 => {
            u16_from_value(value).map(|v| v.to_le_bytes().to_vec())?
        }
        EdgePayloadEncoding::WeightLinearU16 { .. }
        | EdgePayloadEncoding::WeightLogU16 { .. }
        | EdgePayloadEncoding::WeightBinary16 => {
            return Err(PlanQueryError::UnsupportedOp(
                "edge payload predicate for transformed weight encodings",
            ));
        }
        EdgePayloadEncoding::VectorF32 { .. } => {
            return Err(PlanQueryError::UnsupportedOp(
                "edge payload predicate for vector encodings",
            ));
        }
        EdgePayloadEncoding::RawBytes => match value {
            Value::Bytes(bytes) => bytes.clone(),
            _ => {
                return Err(PlanQueryError::InvalidExpressionValue {
                    expression: "opaque edge payload predicate literal".into(),
                });
            }
        },
    };
    if bytes.len() != usize::from(profile.required_byte_width()) {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: "edge payload predicate byte width".into(),
        });
    }
    Ok(Some(bytes))
}

fn u16_from_value(value: &Value) -> Result<u16, PlanQueryError> {
    let intermediate: u128 = match value {
        Value::Uint8(v) => u128::from(*v),
        Value::Uint16(v) => u128::from(*v),
        Value::Uint32(v) => u128::from(*v),
        Value::Uint64(v) => u128::from(*v),
        Value::Uint128(v) => *v,
        Value::Int8(v) => u128::try_from(*v).map_err(|_| invalid_u16_edge_payload())?,
        Value::Int16(v) => u128::try_from(*v).map_err(|_| invalid_u16_edge_payload())?,
        Value::Int32(v) => u128::try_from(*v).map_err(|_| invalid_u16_edge_payload())?,
        Value::Int64(v) => u128::try_from(*v).map_err(|_| invalid_u16_edge_payload())?,
        Value::Int128(v) => u128::try_from(*v).map_err(|_| invalid_u16_edge_payload())?,
        _ => return Err(invalid_u16_edge_payload()),
    };
    u16::try_from(intermediate).map_err(|_| invalid_u16_edge_payload())
}

fn invalid_u16_edge_payload() -> PlanQueryError {
    PlanQueryError::InvalidExpressionValue {
        expression: "u16 edge payload predicate literal".into(),
    }
}

fn f32_from_value(value: &Value) -> Result<f32, PlanQueryError> {
    match value {
        Value::Float16(v) => Ok(v.to_f32()),
        Value::Float32(v) => Ok(*v),
        Value::Float64(v) if *v >= f32::MIN as f64 && *v <= f32::MAX as f64 => Ok(*v as f32),
        _ => Err(PlanQueryError::InvalidExpressionValue {
            expression: "f32 edge payload predicate literal".into(),
        }),
    }
}
