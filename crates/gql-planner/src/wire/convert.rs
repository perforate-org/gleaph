//! Physical plan ↔ wire conversion (`GPL` bundle statement payloads).

mod decoder;
mod encoder;
mod helpers;
mod pools;
mod types;

pub use types::*;

use crate::plan::{PhysicalPlan, PlanAnnotations, PlanDiagnostics};

use decoder::Decoder;
use encoder::Encoder;

// ════════════════════════════════════════════════════════════════════════════════
// Wire root
// ════════════════════════════════════════════════════════════════════════════════

/// Rkyv wire image of a [`PhysicalPlan`] (ops + interned expression pools).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(
    serialize_bounds(
        __S: rkyv::ser::Writer + rkyv::ser::Allocator,
        __S::Error: rkyv::rancor::Source,
    ),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    bytecheck(bounds(
        __C: rkyv::validation::ArchiveContext,
        __C::Error: rkyv::rancor::Source,
    ))
)]
pub struct PhysicalPlanWire {
    #[rkyv(omit_bounds)]
    pub ops: Vec<PlanOpWire>,
    /// Rkyv [`Expr`] blobs (`gleaph-gql` `ast-rkyv-no-span`).
    pub expr_pool: Vec<Vec<u8>>,
    /// Rkyv [`LabelExpr`] blobs.
    pub label_expr_pool: Vec<Vec<u8>>,
    /// Rkyv [`OrderByClause`] blobs.
    pub order_by_pool: Vec<Vec<u8>>,
}

pub fn physical_plan_to_wire(plan: &PhysicalPlan) -> Result<PhysicalPlanWire, String> {
    let mut enc = Encoder::default();
    let ops = enc.encode_ops(&plan.ops)?;
    Ok(PhysicalPlanWire {
        ops,
        expr_pool: enc.expr_pool,
        label_expr_pool: enc.label_expr_pool,
        order_by_pool: enc.order_by_pool,
    })
}

pub fn physical_plan_from_wire(wire: &PhysicalPlanWire) -> Result<PhysicalPlan, String> {
    let dec = Decoder::new(wire);
    let ops = dec.decode_ops(&wire.ops)?;
    let mut annotations = PlanAnnotations::default();
    let mut ops = ops;
    crate::pushdown::apply_shortest_path_binding_pruning(&mut ops, &mut annotations);
    let output = crate::output_schema::derive_output_schema(&ops);
    let binding_layout = crate::binding_layout::derive_binding_layout(&ops);
    Ok(PhysicalPlan {
        ops,
        diagnostics: PlanDiagnostics::default(),
        annotations,
        output,
        binding_layout,
    })
}
