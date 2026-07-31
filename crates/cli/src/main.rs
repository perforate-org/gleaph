//! The top-level Gleaph command-line interface.

use std::env;
use std::process::ExitCode;

const USAGE: &str = "usage: gleaph <codegen> [gleaph-codegen options]";

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gleaph: {message}");
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("a command is required".into());
    };

    match command {
        "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        "codegen" => {
            args.remove(0);
            gleaph_codegen::run_cli(args)
        }
        other => Err(format!("unknown command {other:?}")),
    }
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
        std::env::temp_dir().join(format!("gleaph-cli-{}-{nonce}-{id}.ts", std::process::id()))
    }

    #[test]
    fn codegen_subcommand_uses_the_shared_codegen_cli() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../codegen/fixtures/typescript-basic");
        let output = temporary_output_path();

        run(vec![
            "codegen".into(),
            "--manifest".into(),
            fixture_dir
                .join("manifest.json")
                .to_string_lossy()
                .into_owned(),
            "--target".into(),
            "ts".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
        ])
        .expect("codegen subcommand should generate the fixture");

        let generated = fs::read_to_string(&output).expect("CLI should write the output file");
        let expected = fs::read_to_string(fixture_dir.join("generated.ts"))
            .expect("TypeScript fixture should exist");
        assert_eq!(generated, expected);
        fs::remove_file(output).expect("temporary output should be removable");
    }

    #[test]
    fn rejects_unknown_top_level_command() {
        let error = run(vec!["deploy".into()]).expect_err("unknown commands must fail");
        assert_eq!(error, "unknown command \"deploy\"");
    }
}
