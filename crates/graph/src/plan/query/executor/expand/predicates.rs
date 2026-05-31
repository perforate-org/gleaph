use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;
use gleaph_gql_planner::plan::{
    EdgeValuePredicate, EdgeVectorMetric as PlanEdgeVectorMetric, EdgeVectorPredicate, ScanValue,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeValueEncoding, EdgeValueProfile};
use half::f16;

use crate::facade::GraphStore;
use crate::plan::query::edge_value_batch_kernel::PreparedEdgeValueBatchKernel;
use crate::plan::query::edge_vector_kernel::{
    EdgeVectorMetric as KernelEdgeVectorMetric, PreparedEdgeVectorKernel,
};
use crate::plan::query::error::PlanQueryError;
#[derive(Clone, Debug)]
pub(crate) struct PreparedEdgeValuePredicate {
    pub(crate) kernel: PreparedEdgeValueBatchKernel,
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
        store: &GraphStore,
        label_id: EdgeLabelId,
        predicate: &EdgeVectorPredicate,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Option<Self>, PlanQueryError> {
        let Some(profile) = store.edge_label_value_profile(label_id) else {
            return Ok(None);
        };
        let EdgeValueEncoding::VectorF32 { dims } = profile.encoding else {
            return Err(PlanQueryError::UnsupportedOp(
                "edge vector predicate for non-vector encodings",
            ));
        };
        let query = scan_value_to_f32_vector(&predicate.query, parameters)?;
        let threshold = scan_value_to_f32(&predicate.threshold, parameters)?;
        profile
            .validate()
            .map_err(|err| PlanQueryError::InvalidExpressionValue {
                expression: format!("edge vector value profile: {err}"),
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

    pub(crate) fn collect_matching_indices(&self, value_bytes: &[u8], out: &mut Vec<usize>) {
        match (self.metric, self.op) {
            (KernelEdgeVectorMetric::L2Squared, CmpOp::Lt) => {
                self.kernel.collect_l2_squared_upper_bound_indices(
                    value_bytes,
                    &self.query,
                    self.threshold,
                    false,
                    out,
                )
            }
            (KernelEdgeVectorMetric::L2Squared, CmpOp::Le) => {
                self.kernel.collect_l2_squared_upper_bound_indices(
                    value_bytes,
                    &self.query,
                    self.threshold,
                    true,
                    out,
                )
            }
            _ => self.kernel.collect_matching_indices(
                value_bytes,
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

impl PreparedEdgeValuePredicate {
    pub(crate) fn prepare(
        store: &GraphStore,
        label_id: EdgeLabelId,
        predicate: &EdgeValuePredicate,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Option<Self>, PlanQueryError> {
        let Some(profile) = store.edge_label_value_profile(label_id) else {
            return Ok(None);
        };
        if profile.required_byte_width() == 0 {
            return Ok(None);
        }
        let Some(expected) =
            scan_value_to_edge_value_bytes(&profile, &predicate.value, parameters)?
        else {
            return Ok(None);
        };
        let kernel = PreparedEdgeValueBatchKernel::new(profile.byte_width, profile.encoding);
        Ok(Some(Self {
            kernel,
            op: predicate.op,
            expected,
        }))
    }
}

fn scan_value_to_edge_value_bytes(
    profile: &EdgeValueProfile,
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
    edge_value_bytes_from_value(profile, value)
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

fn edge_value_bytes_from_value(
    profile: &EdgeValueProfile,
    value: &Value,
) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let bytes = match &profile.encoding {
        EdgeValueEncoding::RawU8 => u8_from_value(value).map(|v| vec![v])?,
        EdgeValueEncoding::RawU16 | EdgeValueEncoding::WeightRawU16 => {
            u16_from_value(value).map(|v| v.to_le_bytes().to_vec())?
        }
        EdgeValueEncoding::RawU32 => u32_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawU64 => u64_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawI8 => i8_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawI16 => i16_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawI32 => i32_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawI64 => i64_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawU128 => u128_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawI128 => i128_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::F16 => {
            f32_from_value(value).map(|v| f16::from_f32(v).to_le_bytes().to_vec())?
        }
        EdgeValueEncoding::F32 => f32_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::F64 => f64_from_value(value).map(|v| v.to_le_bytes().to_vec())?,
        EdgeValueEncoding::RawFixed32 => fixed_bytes_from_value(value, 32)?,
        EdgeValueEncoding::RawFixed64 => fixed_bytes_from_value(value, 64)?,
        EdgeValueEncoding::WeightLinearU16 { .. }
        | EdgeValueEncoding::WeightLogU16 { .. }
        | EdgeValueEncoding::WeightBinary16 => {
            return Err(PlanQueryError::UnsupportedOp(
                "edge value predicate for transformed weight encodings",
            ));
        }
        EdgeValueEncoding::VectorF32 { .. } => {
            return Err(PlanQueryError::UnsupportedOp(
                "edge value predicate for vector encodings",
            ));
        }
        EdgeValueEncoding::RawBytes => match value {
            Value::Bytes(bytes) => bytes.clone(),
            _ => {
                return Err(PlanQueryError::InvalidExpressionValue {
                    expression: "opaque edge value predicate literal".into(),
                });
            }
        },
    };
    if bytes.len() != usize::from(profile.required_byte_width()) {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: "edge value predicate byte width".into(),
        });
    }
    Ok(Some(bytes))
}

fn fixed_bytes_from_value(value: &Value, expected_len: usize) -> Result<Vec<u8>, PlanQueryError> {
    match value {
        Value::Bytes(bytes) if bytes.len() == expected_len => Ok(bytes.clone()),
        _ => Err(PlanQueryError::InvalidExpressionValue {
            expression: "fixed-width edge value predicate literal".into(),
        }),
    }
}

fn u8_from_value(value: &Value) -> Result<u8, PlanQueryError> {
    unsigned_from_value(value).and_then(|v| {
        u8::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "u8 edge value predicate literal".into(),
        })
    })
}

fn u16_from_value(value: &Value) -> Result<u16, PlanQueryError> {
    unsigned_from_value(value).and_then(|v| {
        u16::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "u16 edge value predicate literal".into(),
        })
    })
}

fn u32_from_value(value: &Value) -> Result<u32, PlanQueryError> {
    unsigned_from_value(value).and_then(|v| {
        u32::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "u32 edge value predicate literal".into(),
        })
    })
}

fn u64_from_value(value: &Value) -> Result<u64, PlanQueryError> {
    unsigned_from_value(value).and_then(|v| {
        u64::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "u64 edge value predicate literal".into(),
        })
    })
}

fn u128_from_value(value: &Value) -> Result<u128, PlanQueryError> {
    unsigned_from_value(value)
}

fn i8_from_value(value: &Value) -> Result<i8, PlanQueryError> {
    signed_from_value(value).and_then(|v| {
        i8::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "i8 edge value predicate literal".into(),
        })
    })
}

fn i16_from_value(value: &Value) -> Result<i16, PlanQueryError> {
    signed_from_value(value).and_then(|v| {
        i16::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "i16 edge value predicate literal".into(),
        })
    })
}

fn i32_from_value(value: &Value) -> Result<i32, PlanQueryError> {
    signed_from_value(value).and_then(|v| {
        i32::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "i32 edge value predicate literal".into(),
        })
    })
}

fn i64_from_value(value: &Value) -> Result<i64, PlanQueryError> {
    signed_from_value(value).and_then(|v| {
        i64::try_from(v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "i64 edge value predicate literal".into(),
        })
    })
}

fn i128_from_value(value: &Value) -> Result<i128, PlanQueryError> {
    signed_from_value(value)
}

fn unsigned_from_value(value: &Value) -> Result<u128, PlanQueryError> {
    match value {
        Value::Uint8(v) => Ok(u128::from(*v)),
        Value::Uint16(v) => Ok(u128::from(*v)),
        Value::Uint32(v) => Ok(u128::from(*v)),
        Value::Uint64(v) => Ok(u128::from(*v)),
        Value::Uint128(v) => Ok(*v),
        Value::Int8(v) => u128::try_from(*v).map_err(|_| invalid_unsigned_edge_value()),
        Value::Int16(v) => u128::try_from(*v).map_err(|_| invalid_unsigned_edge_value()),
        Value::Int32(v) => u128::try_from(*v).map_err(|_| invalid_unsigned_edge_value()),
        Value::Int64(v) => u128::try_from(*v).map_err(|_| invalid_unsigned_edge_value()),
        Value::Int128(v) => u128::try_from(*v).map_err(|_| invalid_unsigned_edge_value()),
        _ => Err(invalid_unsigned_edge_value()),
    }
}

fn signed_from_value(value: &Value) -> Result<i128, PlanQueryError> {
    match value {
        Value::Int8(v) => Ok(i128::from(*v)),
        Value::Int16(v) => Ok(i128::from(*v)),
        Value::Int32(v) => Ok(i128::from(*v)),
        Value::Int64(v) => Ok(i128::from(*v)),
        Value::Int128(v) => Ok(*v),
        Value::Uint8(v) => Ok(i128::from(*v)),
        Value::Uint16(v) => Ok(i128::from(*v)),
        Value::Uint32(v) => Ok(i128::from(*v)),
        Value::Uint64(v) => Ok(i128::from(*v)),
        Value::Uint128(v) => i128::try_from(*v).map_err(|_| invalid_signed_edge_value()),
        _ => Err(invalid_signed_edge_value()),
    }
}

fn f32_from_value(value: &Value) -> Result<f32, PlanQueryError> {
    match value {
        Value::Float16(v) => Ok(v.to_f32()),
        Value::Float32(v) => Ok(*v),
        Value::Float64(v) if *v >= f32::MIN as f64 && *v <= f32::MAX as f64 => Ok(*v as f32),
        _ => Err(PlanQueryError::InvalidExpressionValue {
            expression: "f32 edge value predicate literal".into(),
        }),
    }
}

fn f64_from_value(value: &Value) -> Result<f64, PlanQueryError> {
    match value {
        Value::Float16(v) => Ok(f64::from(v.to_f32())),
        Value::Float32(v) => Ok(f64::from(*v)),
        Value::Float64(v) => Ok(*v),
        _ => Err(PlanQueryError::InvalidExpressionValue {
            expression: "f64 edge value predicate literal".into(),
        }),
    }
}

fn invalid_unsigned_edge_value() -> PlanQueryError {
    PlanQueryError::InvalidExpressionValue {
        expression: "unsigned edge value predicate literal".into(),
    }
}

fn invalid_signed_edge_value() -> PlanQueryError {
    PlanQueryError::InvalidExpressionValue {
        expression: "signed edge value predicate literal".into(),
    }
}
