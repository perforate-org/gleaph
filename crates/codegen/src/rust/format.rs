//! Formatting policy and external formatter integration for generated Rust source.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Formatting policy for generated Rust source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RustFormatMode {
    /// Use `rustfmt` when available and otherwise keep the built-in rendering.
    Auto,
    /// Require `rustfmt` and fail if it cannot format the generated source.
    Rustfmt,
    /// Do not invoke an external formatter.
    Never,
}

/// Built-in layout defaults mirrored from the Rustfmt defaults used by generated Rust.
///
/// The renderers own syntax-aware decisions; this module owns the shared thresholds and
/// whitespace policy so Rust client and canister profiles do not drift.
pub const MAX_WIDTH: usize = 100;
pub const FN_CALL_WIDTH: usize = 60;
pub const CHAIN_WIDTH: usize = 60;
pub const ARRAY_WIDTH: usize = 60;

/// Return whether a generated construct exceeds a built-in Rustfmt-style width threshold.
pub(crate) fn exceeds_width(value: &str, width: usize) -> bool {
    value.chars().count() > width
}

pub(crate) fn exceeds_default_width(value: &str) -> bool {
    exceeds_width(value, MAX_WIDTH)
}

impl RustFormatMode {
    /// Parse a command-line formatting mode.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "rustfmt" => Ok(Self::Rustfmt),
            "never" => Ok(Self::Never),
            _ => Err(format!(
                "unsupported Rust format mode {value:?}; expected auto, rustfmt, or never"
            )),
        }
    }
}

/// Apply the built-in Rust formatting layer and, when requested, `rustfmt`.
pub fn format_rust(
    source: String,
    mode: RustFormatMode,
    output_path: Option<&Path>,
) -> Result<String, String> {
    let source = normalize_rust_source(source);
    if mode == RustFormatMode::Never {
        return Ok(source);
    }

    match run_rustfmt(&source, output_path) {
        Ok(formatted) => Ok(formatted),
        Err(RustfmtError::Unavailable(_)) if mode == RustFormatMode::Auto => Ok(source),
        Err(error) => Err(error.to_string()),
    }
}

fn normalize_rust_source(mut source: String) -> String {
    source = source.replace("\r\n", "\n");
    source = source
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    if !source.ends_with('\n') {
        source.push('\n');
    }
    source
}

#[derive(Debug)]
enum RustfmtError {
    Unavailable(String),
    Failed(String),
}

impl std::fmt::Display for RustfmtError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "{error}"),
            Self::Failed(error) => write!(formatter, "{error}"),
        }
    }
}

fn run_rustfmt(source: &str, output_path: Option<&Path>) -> Result<String, RustfmtError> {
    let mut command = Command::new("rustfmt");
    command
        .arg("--edition")
        .arg("2024")
        .arg("--emit")
        .arg("stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(output_path) = output_path
        && let Some(parent) = output_path.parent()
    {
        command.current_dir(parent);
    }

    let mut child = command
        .spawn()
        .map_err(|error| RustfmtError::Unavailable(format!("failed to start rustfmt: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| RustfmtError::Failed("rustfmt stdin was not available".to_string()))?
        .write_all(source.as_bytes())
        .map_err(|error| RustfmtError::Failed(format!("write source to rustfmt: {error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| RustfmtError::Failed(format!("wait for rustfmt: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RustfmtError::Failed(format!(
            "rustfmt failed: {}",
            stderr.trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| RustfmtError::Failed(format!("decode rustfmt output: {error}")))
}
