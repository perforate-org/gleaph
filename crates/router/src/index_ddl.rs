//! Router's import facade for the shared Gleaph index-DDL parser.
//!
//! The parser and statement types are owned by `gleaph-index-ddl` so migration tooling and Router
//! cannot drift. Router keeps this narrow module as its internal import boundary.

pub(crate) use gleaph_index_ddl::{
    IndexDdlStatement, IndexTarget, TextIndexDdlStatement, VectorIndexDdlStatement,
    VectorIndexTarget, try_parse, try_parse_text, try_parse_vector,
};
