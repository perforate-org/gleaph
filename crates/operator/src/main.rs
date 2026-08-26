//! Thin binary shell: parse the command line, drive one async runtime, map failures to the
//! process exit code. All behavior lives in the `gleaph_operator` library.

use std::process::ExitCode;

use clap::Parser;

use gleaph_operator::{cli::Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: create async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
