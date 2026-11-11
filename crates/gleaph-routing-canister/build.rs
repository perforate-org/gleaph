//! Compiles the Clang Wasm SIMD kernels for the runtime's own wasm artifacts.
//!
//! The C source is owned by the `gleaph-kernels` project; this build step only invokes clang and
//! links the resulting object into the crate's final artifact (the place where `link-arg` applies).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../gleaph-kernels/crates/simd/c/gemv4.c");
    let target = std::env::var("TARGET").unwrap_or_default();
    let enabled = std::env::var("CARGO_FEATURE_C_KERNEL").is_ok() && target.starts_with("wasm32");
    if !enabled {
        return;
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let object = out_dir.join("gleaph_gemv.o");
    let clang = std::env::var("CLANG").unwrap_or_else(|_| "clang".into());
    let status = Command::new(&clang)
        .args(["--target=wasm32", "-msimd128", "-O3", "-c"])
        .arg("../../gleaph-kernels/crates/simd/c/gemv4.c")
        .arg("-o")
        .arg(&object)
        .status()
        .expect("failed to run clang");
    assert!(status.success(), "clang failed to build the wasm kernels");
    println!("cargo:rustc-link-arg={}", object.display());
}
