//! Physical plan wire format (`GPL` + version byte) for router → graph execution.

mod bundle;
mod convert;

pub use bundle::{
    PLAN_WIRE_MAGIC, PLAN_WIRE_VERSION, PlanBundleError, decode_plan_bundle,
    decode_plan_bundle_to_block, encode_block_plans, encode_statement_plans,
};
pub use convert::{physical_plan_from_wire, physical_plan_to_wire};
