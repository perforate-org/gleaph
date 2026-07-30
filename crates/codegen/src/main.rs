//! Command-line entrypoint for generating prepared-query client and canister adapters.

use gleaph_codegen::{
    generate_javascript, generate_motoko, generate_rust, generate_rust_canister,
    generate_typescript, parse_manifest,
};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gleaph-codegen: {message}");
            eprintln!(
                "usage: gleaph-codegen --manifest <path> --target <typescript|javascript|rust|rust-canister|motoko> [--output <path>]"
            );
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut manifest_path = None;
    let mut target = None;
    let mut output = None;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let value = || {
            args.get(index + 1)
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--manifest" => manifest_path = Some(PathBuf::from(value()?.clone())),
            "--target" => target = Some(value()?.clone()),
            "--output" => output = Some(PathBuf::from(value()?.clone())),
            "-h" | "--help" => {
                println!(
                    "usage: gleaph-codegen --manifest <path> --target <typescript|javascript|rust|rust-canister|motoko> [--output <path>]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
        index += 2;
    }

    let manifest_path = manifest_path.ok_or("--manifest is required")?;
    let target = target.ok_or("--target is required")?;
    if !matches!(
        target.as_str(),
        "typescript"
            | "ts"
            | "javascript"
            | "js"
            | "rust"
            | "rs"
            | "rust-canister"
            | "motoko"
            | "mo"
    ) {
        return Err(format!(
            "unsupported target {target:?}; expected typescript, javascript, rust, rust-canister, or motoko"
        ));
    }
    let input = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("read {}: {error}", manifest_path.display()))?;
    let manifest = parse_manifest(&input).map_err(|error| error.to_string())?;
    let generated = match target.as_str() {
        "typescript" | "ts" => generate_typescript(&manifest),
        "javascript" | "js" => generate_javascript(&manifest),
        "rust" | "rs" => generate_rust(&manifest),
        "rust-canister" => generate_rust_canister(&manifest),
        "motoko" | "mo" => generate_motoko(&manifest),
        _ => unreachable!("target was validated above"),
    }
    .map_err(|error| error.to_string())?;
    if let Some(output) = output {
        fs::write(&output, generated)
            .map_err(|error| format!("write {}: {error}", output.display()))?;
    } else {
        print!("{generated}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMPORARY_OUTPUT_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_output_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let id = NEXT_TEMPORARY_OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gleaph-codegen-cli-{}-{nonce}-{id}.ts",
            std::process::id()
        ))
    }

    #[test]
    fn generates_typescript_alias_to_explicit_output() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/typescript-basic");
        let manifest = fixture_dir.join("manifest.json");
        let output = temporary_output_path();

        run(vec![
            "--manifest".into(),
            manifest.to_string_lossy().into_owned(),
            "--target".into(),
            "ts".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
        ])
        .expect("CLI should generate the TypeScript fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("generated.ts"))
            .expect("TypeScript fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn generates_typescript_imports_required_by_advanced_fixture() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/typescript-advanced");
        let output = temporary_output_path();

        run(vec![
            "--manifest".into(),
            fixture_dir
                .join("manifest.json")
                .to_string_lossy()
                .into_owned(),
            "--target".into(),
            "typescript".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
        ])
        .expect("CLI should generate the advanced TypeScript fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("generated.ts"))
            .expect("advanced TypeScript fixture should exist");
        assert_eq!(generated, expected);
        assert!(generated.contains("PreparedSortSpec"));
        assert!(generated.contains("ApiPathElement"));
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn generates_rust_client_fixture() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/rust-client-basic");
        let output = temporary_output_path();

        run(vec![
            "--manifest".into(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/typescript-basic/manifest.json")
                .to_string_lossy()
                .into_owned(),
            "--target".into(),
            "rust".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
        ])
        .expect("CLI should generate the Rust client fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("src/lib.rs"))
            .expect("Rust client fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn generates_motoko_fixture() {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/motoko-basic");
        let output = temporary_output_path();

        run(vec![
            "--manifest".into(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/typescript-basic/manifest.json")
                .to_string_lossy()
                .into_owned(),
            "--target".into(),
            "motoko".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
        ])
        .expect("CLI should generate the Motoko fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("src/generated.mo"))
            .expect("Motoko fixture should exist");
        assert_eq!(generated, expected);
        assert!(generated.contains("module {"));
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn rejects_unknown_target() {
        let error = run(vec![
            "--manifest".into(),
            "manifest.json".into(),
            "--target".into(),
            "swift".into(),
        ])
        .expect_err("unknown targets must fail before reading the manifest");

        assert!(error.contains("unsupported target \"swift\""));
    }
}
