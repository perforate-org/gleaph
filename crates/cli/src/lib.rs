//! Library API for loading prepared-query JSON and generating clients.

pub mod canister;
pub mod codegen;

pub use canister::{
    ensure_input_xor_canister, fetch_prepared_queries_from_canister, parse_canister_id,
};
pub use codegen::rust_lang::generate_rs;
pub use codegen::typescript::{generate_dts, generate_js, generate_ts};
pub use codegen::{
    JsParamStyle, Lang, load_prepared_queries_from_json, load_prepared_queries_from_path,
    resolve_output_path, write_codegen_output,
};
