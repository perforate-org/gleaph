use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let out_file = out_dir.join("gleaph_graph.wasm");
    let target = env::var("TARGET").unwrap_or_default();

    let profile_wasm = workspace_root.join(format!(
        "target/wasm32-unknown-unknown/{profile}/gleaph_graph.wasm"
    ));
    let fallback_profile = if profile == "release" {
        "debug"
    } else {
        "release"
    };
    let fallback_wasm = workspace_root.join(format!(
        "target/wasm32-unknown-unknown/{fallback_profile}/gleaph_graph.wasm"
    ));
    let candidates = [
        workspace_root.join("gleaph_graph.wasm"),
        profile_wasm,
        fallback_wasm,
    ];
    for path in &candidates {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let wasm = match candidates.iter().find_map(|p| fs::read(p).ok()) {
        Some(bytes) => bytes,
        None if target == "wasm32-unknown-unknown" => {
            panic!(
                "missing embedded gleaph_graph.wasm for registry canister build; build `gleaph-graph` for wasm first (searched: {})",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        None => Vec::new(),
    };
    fs::write(out_file, wasm).expect("write embedded graph wasm");
}
