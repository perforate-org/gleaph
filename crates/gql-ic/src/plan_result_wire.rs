//! Candid-stable query result rows for router ↔ graph federation merge.
//!
//! The row containers are single-sourced in [`gleaph_gql_ic_wire`] and re-exported here under the
//! historical `IcWirePlanQuery*` names so the execution side (router/graph) and the SDK share one
//! row wire shape.

pub use gleaph_gql_ic_wire::{
    GqlWireDecodeError as WireError, GqlWireRow as IcWirePlanQueryRow,
    GqlWireRows as IcWirePlanQueryResult,
};
