//! Build graph / router / index WASM for PocketIC tests and fetch the PocketIC server binary.

use flate2::read::GzDecoder;
use std::fs::{self, File};
use std::io::copy;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=Cargo.toml");

    let pocket_ic_bin = ensure_pocket_ic_binary(&manifest_dir);
    println!("cargo:rustc-env=POCKET_IC_BIN={}", pocket_ic_bin.display());

    build_wasm(&manifest_dir);
}

fn ensure_pocket_ic_binary(manifest_dir: &Path) -> PathBuf {
    let version = pocket_ic_version_from_manifest(manifest_dir);
    let bin_dir = manifest_dir.join(".pocket-ic");
    let bin_path = bin_dir.join("pocket-ic");

    if bin_path.is_file() && validate_pocket_ic_binary(&bin_path, &version).is_ok() {
        println!("cargo:rerun-if-changed={}", bin_path.display());
        return bin_path;
    }

    fs::create_dir_all(&bin_dir).expect("create .pocket-ic directory");
    let (arch, os) = pocket_ic_platform();
    let url = format!(
        "https://github.com/dfinity/pocketic/releases/download/{version}/pocket-ic-{arch}-{os}.gz"
    );
    eprintln!("downloading PocketIC {version} ({arch}-{os}) from {url}");

    let mut response = ureq::get(&url)
        .call()
        .unwrap_or_else(|e| panic!("download PocketIC from {url}: {e}"));
    let gz_bytes = response
        .body_mut()
        .read_to_vec()
        .unwrap_or_else(|e| panic!("read PocketIC download from {url}: {e}"));

    let tmp_path = bin_dir.join("pocket-ic.download");
    {
        let mut decoder = GzDecoder::new(&gz_bytes[..]);
        let mut out = File::create(&tmp_path)
            .unwrap_or_else(|e| panic!("create {}: {e}", tmp_path.display()));
        copy(&mut decoder, &mut out).unwrap_or_else(|e| panic!("gunzip PocketIC binary: {e}"));
    }
    fs::rename(&tmp_path, &bin_path).unwrap_or_else(|e| panic!("install pocket-ic binary: {e}"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod pocket-ic binary: {e}"));
    }

    validate_pocket_ic_binary(&bin_path, &version)
        .unwrap_or_else(|e| panic!("validate pocket-ic binary at {}: {e}", bin_path.display()));

    println!("cargo:rerun-if-changed={}", bin_path.display());
    bin_path
}

fn pocket_ic_version_from_manifest(manifest_dir: &Path) -> String {
    let cargo_toml = fs::read_to_string(manifest_dir.join("Cargo.toml"))
        .expect("read crates/pocket-ic-tests/Cargo.toml");
    for line in cargo_toml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pocket-ic") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let version = rest.trim().trim_matches('"').trim_matches('\'').to_string();
                if !version.is_empty() {
                    return version;
                }
            }
        }
    }
    panic!("pocket-ic dependency version not found in crates/pocket-ic-tests/Cargo.toml");
}

fn pocket_ic_platform() -> (&'static str, &'static str) {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return ("arm64", "darwin");
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return ("x86_64", "darwin");
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return ("arm64", "linux");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return ("x86_64", "linux");
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
    )))]
    compile_error!("pocket-ic-tests only supports macOS and Linux (x86_64 or aarch64)");
}

fn validate_pocket_ic_binary(path: &Path, version: &str) -> Result<(), String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|e| format!("run --version: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pocket-ic --version failed: status {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim_end();
    let expected = format!("pocket-ic-server {version}");
    if line == expected {
        return Ok(());
    }
    if line.starts_with("pocket-ic-server ")
        && line
            .strip_prefix("pocket-ic-server ")
            .is_some_and(|v| v == version || v.starts_with(&format!("{version}.")))
    {
        return Ok(());
    }
    Err(format!("unexpected pocket-ic --version output: {line:?}"))
}

fn build_wasm(manifest_dir: &Path) {
    let meta = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .expect("cargo metadata");
    let root = meta.workspace_root;
    let target_dir = root.join("target");
    let wasm_target = "wasm32-unknown-unknown";

    let mut build_args = vec![
        "build",
        "--release",
        "-p",
        "gleaph-router",
        "-p",
        "gleaph-graph-index",
        "--target",
        wasm_target,
        "--features",
        "gleaph-router/pocket-ic-e2e",
    ];
    if std::env::var("POCKET_IC_BUILD_GRAPH").is_ok() {
        build_args.extend([
            "-p",
            "gleaph-graph",
            "--features",
            "gleaph-graph/pocket-ic-e2e",
        ]);
        println!(
            "cargo:rerun-if-changed={}",
            manifest_dir.join("../graph/src").display()
        );
    }
    let status = Command::new("cargo")
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(build_args)
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "wasm build for pocket-ic tests failed");

    let wasm_dir = target_dir.join(wasm_target).join("release");
    set_wasm_env(
        "ROUTER_WASM",
        wasm_dir.join("gleaph_router.wasm").into_std_path_buf(),
    );
    set_wasm_env(
        "INDEX_WASM",
        wasm_dir.join("gleaph_graph_index.wasm").into_std_path_buf(),
    );
    if std::env::var("POCKET_IC_BUILD_GRAPH").is_ok() {
        set_wasm_env(
            "GRAPH_WASM",
            wasm_dir.join("gleaph_graph.wasm").into_std_path_buf(),
        );
    }
}

fn set_wasm_env(var: &str, path: PathBuf) {
    assert!(path.is_file(), "missing wasm artifact: {}", path.display());
    println!("cargo:rustc-env={var}={}", path.display());
    println!("cargo:rerun-if-changed={}", path.display());
}
