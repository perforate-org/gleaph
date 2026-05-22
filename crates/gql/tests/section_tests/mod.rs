//! AST verification tests organized by GQL section.
//!
//! Three-level nesting mirrors GQL exactly:
//!
//!   mod s06 {                        // section
//!       mod gql_program {            //   rule name
//!           fn alternative_..() {}   //     each production alternative
//!       }
//!   }

pub mod s06;
pub mod s07;
pub mod s08;
pub mod s09;
pub mod s10;
pub mod s11;
pub mod s12;
pub mod s13;
pub mod s14;
pub mod s15;
pub mod s16;
pub mod s17;
pub mod s18;
pub mod s19;
pub mod s20;
pub mod s21;

use gleaph_gql::ast::*;
use gleaph_gql::parser;

pub fn p(input: &str) -> GqlProgram {
    parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"))
}

pub fn ta(prog: &GqlProgram) -> &TransactionActivity {
    prog.transaction_activity.as_ref().unwrap()
}

pub fn body(prog: &GqlProgram) -> &StatementBlock {
    ta(prog).body.as_ref().unwrap()
}
pub mod grammar_smoke;
