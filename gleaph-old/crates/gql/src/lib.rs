//! GQL query engine for the Gleaph graph database.
//!
//! This crate provides a complete pipeline for parsing, validating, planning,
//! and executing a subset of the GQL graph query language against an in-memory
//! [`gleaph_pma::PmaGraph`].
//!
//! # Pipeline overview
//!
//! ```text
//! query string
//!   └─ lexer::tokenize
//!        └─ parser::parse_statement  → ast::Statement
//!             └─ validate::validate_statement
//!                  ├─ (queries) planner::build_plan → plan::PhysicalPlan
//!                  │              └─ executor::execute_plan → QueryResult
//!                  └─ (mutations) executor::execute_mutation → MutationResult
//! ```
//!
//! # Supported syntax
//!
//! - `MATCH (a:Label)-[e:REL]->(b:Label) WHERE … RETURN … ORDER BY … LIMIT n`
//! - `INSERT (:Label {prop: value})`
//! - `INSERT (:Label)-[:REL]->(:Label)`
//! - `MATCH … WHERE … DELETE <var>`
//!
//! See the crate README for a detailed grammar reference and example queries.

pub mod ast;
pub mod executor;
pub mod lexer;
pub mod param_inference;
pub mod parser;
pub mod plan;
pub mod planner;
pub mod semantic;
pub mod stats;
pub mod temporal;
pub mod type_check;
pub mod validate;
pub mod value;

pub use parser::{parse_statement, parse_statement_from_tokens};
pub use validate::validate_statement;
