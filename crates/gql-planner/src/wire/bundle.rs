//! `GPL` plan bundle header and statement list encoding.
//!
//! Header layout (little-endian after magic):
//! - bytes `[0..3]`: magic `GPL`
//! - byte `3`: format version
//! - bytes `[4..6]`: flags (`requires_write_path` in bit 0)
//! - bytes `[6..10]`: statement count (`u32`)
//! - bytes `[10..12]`: reserved (must be zero; first length prefix starts at offset 12)
//! - per statement: optional zero padding, then `u32` payload length + rkyv [`PhysicalPlanWire`]
//!   (length prefix at offset ≡ 4 mod 8 so the rkyv payload starts 8-byte aligned)
//!
//! On wasm32, decode copies the bundle into an owned buffer before walking statement payloads.
//! Each rkyv payload is deserialized through [`gleaph_gql::rkyv_from_wire_bytes`].

use gleaph_gql::ast::StatementBlock;
use gleaph_gql::rkyv_from_wire_bytes;

use crate::PlanBuildOptions;
use crate::plan::PhysicalPlan;
use crate::planner::build_block_plan_with_schema;
use crate::stats::GraphStats;
use gleaph_gql::type_check::NoSchema;

use super::convert::{physical_plan_from_wire, physical_plan_to_wire};

/// Three-byte wire magic; version lives in the fourth header byte.
pub const PLAN_WIRE_MAGIC: [u8; 3] = *b"GPL";
pub const PLAN_WIRE_VERSION: u8 = 1;

const HEADER_LEN: usize = 12;
/// Rkyv archived [`super::convert::PhysicalPlanWire`] payloads require 8-byte alignment.
const STATEMENT_PAYLOAD_ALIGN: usize = 8;
/// Length prefix sits 4 bytes before the aligned payload.
const STATEMENT_LEN_PREFIX_MOD: usize = 4;

fn pad_for_next_statement_len(out: &mut Vec<u8>) {
    let rem = out.len() % STATEMENT_PAYLOAD_ALIGN;
    let pad = if rem <= STATEMENT_LEN_PREFIX_MOD {
        STATEMENT_LEN_PREFIX_MOD - rem
    } else {
        STATEMENT_PAYLOAD_ALIGN - rem + STATEMENT_LEN_PREFIX_MOD
    };
    if pad > 0 {
        out.extend(std::iter::repeat_n(0u8, pad));
    }
}

fn advance_to_next_statement_len(bytes: &[u8], offset: &mut usize) -> Result<(), PlanBundleError> {
    while *offset % STATEMENT_PAYLOAD_ALIGN != STATEMENT_LEN_PREFIX_MOD {
        if *offset >= bytes.len() {
            return Err(PlanBundleError::Truncated);
        }
        if bytes[*offset] != 0 {
            return Err(PlanBundleError::Wire(format!(
                "unexpected non-zero statement alignment padding at offset {}",
                *offset
            )));
        }
        *offset += 1;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PlanBundleError {
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported version {0}")]
    UnsupportedVersion(u8),
    #[error("truncated bundle")]
    Truncated,
    #[error("wire error: {0}")]
    Wire(String),
}

/// Encode a statement block's plans into a single GPL wire blob.
pub fn encode_block_plans(
    plans: &[PhysicalPlan],
    requires_write_path: bool,
) -> Result<Vec<u8>, PlanBundleError> {
    encode_statement_plans(plans, requires_write_path)
}

pub fn encode_statement_plans(
    plans: &[PhysicalPlan],
    requires_write_path: bool,
) -> Result<Vec<u8>, PlanBundleError> {
    let mut out = Vec::new();
    out.extend_from_slice(&PLAN_WIRE_MAGIC);
    out.push(PLAN_WIRE_VERSION);
    let flags: u16 = u16::from(requires_write_path);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(plans.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 2]); // reserved (must be zero)
    for (i, plan) in plans.iter().enumerate() {
        if i > 0 {
            pad_for_next_statement_len(&mut out);
        }
        let wire = physical_plan_to_wire(plan).map_err(PlanBundleError::Wire)?;
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&wire)
            .map_err(|e| PlanBundleError::Wire(e.to_string()))?
            .into_vec();
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

pub fn decode_plan_bundle(bytes: &[u8]) -> Result<(bool, Vec<PhysicalPlan>), PlanBundleError> {
    #[cfg(target_family = "wasm")]
    {
        // Caller slices may be short-lived or unaligned on wasm32.
        let bytes = bytes.to_vec();
        decode_plan_bundle_owned(&bytes)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        decode_plan_bundle_owned(bytes)
    }
}

fn decode_plan_bundle_owned(bytes: &[u8]) -> Result<(bool, Vec<PhysicalPlan>), PlanBundleError> {
    if bytes.len() < HEADER_LEN {
        return Err(PlanBundleError::Truncated);
    }
    if bytes[0..3] != PLAN_WIRE_MAGIC {
        return Err(PlanBundleError::BadMagic);
    }
    let version = bytes[3];
    if version != PLAN_WIRE_VERSION {
        return Err(PlanBundleError::UnsupportedVersion(version));
    }
    let requires_write_path = bytes[4] != 0 || bytes[5] != 0;
    let stmt_count = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]) as usize;
    if bytes[10] != 0 || bytes[11] != 0 {
        return Err(PlanBundleError::Wire(
            "header reserved bytes must be zero".into(),
        ));
    }
    let mut offset = HEADER_LEN;
    // Each statement is at least a 4-byte length prefix, so the declared
    // `stmt_count` cannot legitimately exceed `bytes.len() / 4`. Clamp the
    // pre-allocation to that bound: a malformed header otherwise requests an
    // arbitrarily large allocation (memory-amplification DoS) before any
    // statement is read.
    let mut plans = Vec::with_capacity(stmt_count.min(bytes.len() / 4));
    for i in 0..stmt_count {
        if i > 0 {
            advance_to_next_statement_len(bytes, &mut offset)?;
        }
        // `checked_add` guards against `usize` overflow on 32-bit (wasm32):
        // a near-`u32::MAX` length would otherwise wrap past the bounds check
        // and panic on the slice range.
        let len_end = offset.checked_add(4).ok_or(PlanBundleError::Truncated)?;
        if len_end > bytes.len() {
            return Err(PlanBundleError::Truncated);
        }
        let len = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        offset = len_end;
        let payload_end = offset.checked_add(len).ok_or(PlanBundleError::Truncated)?;
        if payload_end > bytes.len() {
            return Err(PlanBundleError::Truncated);
        }
        let slice = &bytes[offset..payload_end];
        offset = payload_end;
        let wire: super::convert::PhysicalPlanWire =
            rkyv_from_wire_bytes(slice).map_err(PlanBundleError::Wire)?;
        plans.push(physical_plan_from_wire(&wire).map_err(PlanBundleError::Wire)?);
    }
    Ok((requires_write_path, plans))
}

/// Rebuild a statement block's plans from a bundle (for tests).
pub fn decode_plan_bundle_to_block(
    bytes: &[u8],
    block: &StatementBlock,
    options: PlanBuildOptions<'_>,
    stats: &dyn GraphStats,
) -> Result<Vec<PhysicalPlan>, PlanBundleError> {
    let (requires_write, plans) = decode_plan_bundle(bytes)?;
    let _ = options;
    let expected = build_block_plan_with_schema(block, Some(stats), &NoSchema)
        .map_err(|e| PlanBundleError::Wire(e.to_string()))?;
    let expected_write = expected.has_dml();
    if requires_write != expected_write {
        return Err(PlanBundleError::Wire("requires_write_path mismatch".into()));
    }
    if plans.len() != 1 {
        return Err(PlanBundleError::Wire(format!(
            "statement count {} vs 1 (merged block plan)",
            plans.len()
        )));
    }
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanOp;

    fn minimal_read_plan() -> PhysicalPlan {
        PhysicalPlan {
            ops: vec![PlanOp::NodeScan {
                variable: "n".into(),
                label: None,
                property_projection: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn header_starts_with_magic_and_version_byte() {
        let blob = encode_statement_plans(&[minimal_read_plan()], false).expect("encode");
        assert_eq!(&blob[0..3], &PLAN_WIRE_MAGIC);
        assert_eq!(blob[3], PLAN_WIRE_VERSION);
        assert!(blob.len() >= HEADER_LEN);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut blob = encode_statement_plans(&[minimal_read_plan()], false).expect("encode");
        blob[0] = b'X';
        assert!(matches!(
            decode_plan_bundle(&blob),
            Err(PlanBundleError::BadMagic)
        ));
    }

    #[test]
    fn rejects_unsupported_version_byte() {
        let mut blob = encode_statement_plans(&[minimal_read_plan()], false).expect("encode");
        blob[3] = 99;
        assert!(matches!(
            decode_plan_bundle(&blob),
            Err(PlanBundleError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn rejects_truncated_header() {
        let blob = encode_statement_plans(&[minimal_read_plan()], false).expect("encode");
        assert!(matches!(
            decode_plan_bundle(&blob[..HEADER_LEN - 1]),
            Err(PlanBundleError::Truncated)
        ));
    }

    #[test]
    fn rejects_huge_statement_count_without_oom() {
        // Header declares u32::MAX statements but carries no payloads. Must fail
        // gracefully rather than pre-allocating billions of entries.
        let mut blob = Vec::new();
        blob.extend_from_slice(&PLAN_WIRE_MAGIC);
        blob.push(PLAN_WIRE_VERSION);
        blob.extend_from_slice(&0u16.to_le_bytes()); // flags
        blob.extend_from_slice(&u32::MAX.to_le_bytes()); // stmt_count
        blob.extend_from_slice(&[0u8; 2]); // reserved
        assert!(matches!(
            decode_plan_bundle(&blob),
            Err(PlanBundleError::Truncated)
        ));
    }

    #[test]
    fn rejects_huge_payload_length_without_overflow() {
        // One statement whose declared payload length is u32::MAX; the bounds
        // check must reject it instead of overflowing the offset on wasm32.
        let mut blob = Vec::new();
        blob.extend_from_slice(&PLAN_WIRE_MAGIC);
        blob.push(PLAN_WIRE_VERSION);
        blob.extend_from_slice(&0u16.to_le_bytes()); // flags
        blob.extend_from_slice(&1u32.to_le_bytes()); // stmt_count = 1
        blob.extend_from_slice(&[0u8; 2]); // reserved
        blob.extend_from_slice(&u32::MAX.to_le_bytes()); // payload len
        assert!(matches!(
            decode_plan_bundle(&blob),
            Err(PlanBundleError::Truncated)
        ));
    }

    #[test]
    fn multi_statement_bundle_round_trips() {
        use gleaph_gql::Value;
        use gleaph_gql::ast::{CmpOp, Expr, ExprKind};

        use crate::plan::{ProjectColumn, ScanValue};

        let index_plan = PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "age".into(),
                value: ScanValue::Literal(Value::Int64(5)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".into())),
                    alias: Some("n".into()),
                }],
                distinct: false,
            },
        ]);
        let tail_plan = PhysicalPlan::from_ops(vec![PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::new(ExprKind::Literal(Value::Int64(1))),
                alias: Some("x".into()),
            }],
            distinct: false,
        }]);
        let blob = encode_statement_plans(&[index_plan.clone(), tail_plan.clone()], false)
            .expect("encode");
        let (_, decoded) = decode_plan_bundle(&blob).expect("decode multi-statement bundle");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].ops.len(), index_plan.ops.len());
        assert_eq!(decoded[1].ops.len(), tail_plan.ops.len());
    }

    #[test]
    fn decode_plan_bundle_accepts_unaligned_subslice() {
        let blob = encode_statement_plans(&[minimal_read_plan()], false).expect("encode");
        let mut prefixed = vec![0u8];
        prefixed.extend_from_slice(&blob);
        let (_, plans) =
            decode_plan_bundle(&prefixed[1..]).expect("decode from unaligned subslice");
        assert_eq!(plans.len(), 1);
    }
}
