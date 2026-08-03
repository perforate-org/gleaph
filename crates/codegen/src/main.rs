use clap::Parser;
use std::process::ExitCode;

use gleaph_codegen::{CodegenArgs, run};

const USAGE: &str = "usage: gleaph-codegen (--manifest <path> | --canister <principal> --graph <name>) --target <typescript|javascript|rust|rust-canister|motoko> [--output <path>] [--format rust=<auto|rustfmt|never>] [-n <ic|local|url>] [--identity <pem>] [--fetch-root-key]";

#[derive(Debug, Parser)]
#[command(
    name = "gleaph-codegen",
    about = "Generate typed prepared-query clients and adapters"
)]
struct Cli {
    #[command(flatten)]
    args: CodegenArgs,
}

fn main() -> ExitCode {
    match run(Cli::parse().args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("gleaph-codegen: {error}");
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
