//! Adds the Glev SIMD kernel object to the canister's final wasm link.
//!
//! Cargo does not propagate a dependency's link arguments to a transitive dependent's final link,
//! so the canister depends on `glev-kernels-c` directly and forwards the object path it publishes
//! as `links` metadata.

fn main() {
    if std::env::var_os("CARGO_FEATURE_C_KERNEL").is_none() {
        return;
    }
    let Ok(object) = std::env::var("DEP_GLEV_KERNELS_C_OBJECT") else {
        return;
    };
    println!("cargo:rustc-link-arg={object}");
}
