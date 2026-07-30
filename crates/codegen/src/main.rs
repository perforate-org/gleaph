use gleaph_codegen::{PreparedManifest, generate_javascript, generate_typescript};
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
                "usage: gleaph-codegen --manifest <path> --target <typescript|javascript> [--output <path>]"
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
                    "usage: gleaph-codegen --manifest <path> --target <typescript|javascript> [--output <path>]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
        index += 2;
    }

    let manifest_path = manifest_path.ok_or("--manifest is required")?;
    let target = target.ok_or("--target is required")?;
    if !matches!(target.as_str(), "typescript" | "ts" | "javascript" | "js") {
        return Err(format!(
            "unsupported target {target:?}; expected typescript or javascript"
        ));
    }
    let input = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("read {}: {error}", manifest_path.display()))?;
    let manifest = PreparedManifest::from_json(&input).map_err(|error| error.to_string())?;
    let generated = match target.as_str() {
        "typescript" | "ts" => generate_typescript(&manifest),
        "javascript" | "js" => generate_javascript(&manifest),
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
