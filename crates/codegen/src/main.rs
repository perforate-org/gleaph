use std::env;
use std::process::ExitCode;

use gleaph_codegen::run_cli as run;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gleaph-codegen: {message}");
            eprintln!(
                "usage: gleaph-codegen (--manifest <path> | --canister <principal> --graph <name>) --target <typescript|javascript|rust|rust-canister|motoko> [--output <path>] [--format rust=<auto|rustfmt|never>] [-n <ic|local|url>] [--identity <pem>] [--fetch-root-key]"
            );
            ExitCode::FAILURE
        }
    }
}
