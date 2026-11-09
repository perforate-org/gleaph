//! Gleaph-specific integration between generic GQL parsing/planning and Graph/Router execution.
//!
//! This crate sits between portable GQL planning and Gleaph execution. It intentionally does not own
//! storage, canister calls, or generic GQL syntax. Its public modules are narrowly scoped:
//!
//! - `path_extension::GLEAPH_PATH_EXTENSION_HANDLER` translates Gleaph path-extension clauses
//!   (e.g. `GLEAPH COST`) into planner concepts consumed by Router and Graph planning.
//!
//! `gleaph_gql` and `gleaph_gql_planner` remain portable and gain no Router/Graph/ICP concepts.

pub mod path_extension;
