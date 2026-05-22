//! §20 — Value expressions and functions.
//!
//! Sub-sections:
//!   20.1  — Arithmetic
//!   20.2  — Value expression primary
//!   20.3  — Value specification (literals)
//!   20.4  — Dynamic parameter specification
//!   20.5  — LET value expression
//!   20.6  — Value query expression
//!   20.7  — CASE expression
//!   20.8  — CAST specification
//!   20.9  — Aggregate functions
//!   20.10 — Element ID & property reference
//!   20.14 — Constructed values (path, list, record)
//!   20.20 — Boolean value expression
//!   20.21 — String functions, numeric functions
//!   20.27 — Datetime functions

pub mod s20_01;
pub mod s20_02;
pub mod s20_03;
pub mod s20_04;
pub mod s20_05;
pub mod s20_06;
pub mod s20_07;
pub mod s20_08;
pub mod s20_09;
pub mod s20_10;
pub mod s20_14;
pub mod s20_20;
pub mod s20_21;
pub mod s20_27;

pub mod expr_constructors;
pub mod expr_functions;
pub mod expr_literals;
pub mod expr_misc;
