//! The result table lives in its own crate so a host that does not build GPUI (the Rune console)
//! can read query results without pulling the explorer's UI stack.

pub use gleaph_result_table::ResultTable;
