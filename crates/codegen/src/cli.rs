//! Command-line implementation shared by `gleaph-codegen` and `gleaph codegen`.

use candid::{Decode, Encode, IDLArgs, IDLValue, Principal};
use ic_agent::identity::Secp256k1Identity;
use std::fs;
use std::path::PathBuf;

use crate::{
    RustFormatMode, format_rust, generate_javascript, generate_motoko, generate_rust,
    generate_rust_canister, generate_typescript, parse_manifest,
};

const DEFAULT_IC_URL: &str = "https://icp-api.io";
const DEFAULT_LOCAL_URL: &str = "http://localhost:8000";

/// Run the `gleaph-codegen` command with the supplied arguments.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let mut manifest_path = None;
    let mut canister = None;
    let mut graph = None;
    let mut network = "ic".to_string();
    let mut fetch_root_key = false;
    let mut identity_path = None;
    let mut target = None;
    let mut output = None;
    let mut rust_format = RustFormatMode::Auto;
    let mut format_targets = std::collections::BTreeSet::new();
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
            "--canister" => canister = Some(value()?.clone()),
            "--graph" => graph = Some(value()?.clone()),
            "-n" | "--network" => network = value()?.clone(),
            "--identity" => identity_path = Some(PathBuf::from(value()?.clone())),
            "--fetch-root-key" => {
                fetch_root_key = true;
                index += 1;
                continue;
            }
            "--target" => target = Some(value()?.clone()),
            "--output" => output = Some(PathBuf::from(value()?.clone())),
            "--format" => {
                let format = value()?;
                let (language, mode) = format.split_once('=').ok_or_else(|| {
                    format!("invalid format {format:?}; expected rust=<auto|rustfmt|never>")
                })?;
                if language != "rust" {
                    return Err(format!(
                        "unsupported format language {language:?}; expected rust"
                    ));
                }
                if !format_targets.insert(language.to_string()) {
                    return Err("duplicate format target \"rust\"".into());
                }
                rust_format = RustFormatMode::parse(mode)?;
            }
            "-h" | "--help" => {
                println!(
                    "usage: gleaph-codegen (--manifest <path> | --canister <principal> --graph <name>) --target <typescript|javascript|rust|rust-canister|motoko> [--output <path>] [--format rust=<auto|rustfmt|never>] [-n <ic|local|url>] [--identity <pem>] [--fetch-root-key]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
        index += 2;
    }

    if manifest_path.is_some() && canister.is_some() {
        return Err("--manifest and --canister are mutually exclusive".into());
    }
    if canister.is_some() != graph.is_some() {
        return Err("--canister and --graph must be provided together".into());
    }
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
    let input = if let Some(manifest_path) = manifest_path {
        fs::read_to_string(&manifest_path)
            .map_err(|error| format!("read {}: {error}", manifest_path.display()))?
    } else if let Some(canister) = canister {
        tokio::runtime::Runtime::new()
            .map_err(|error| format!("create async runtime: {error}"))?
            .block_on(fetch_manifest(
                &canister,
                &graph.expect("graph was validated"),
                &network,
                fetch_root_key,
                identity_path.as_deref(),
            ))?
    } else {
        return Err("one manifest source is required: --manifest or --canister/--graph".into());
    };
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
    let generated = if matches!(target.as_str(), "rust" | "rs" | "rust-canister") {
        format_rust(generated, rust_format, output.as_deref())?
    } else {
        generated
    };
    if let Some(output) = output {
        fs::write(&output, generated)
            .map_err(|error| format!("write {}: {error}", output.display()))?;
    } else {
        print!("{generated}");
    }
    Ok(())
}

async fn fetch_manifest(
    canister: &str,
    graph: &str,
    network: &str,
    fetch_root_key_flag: bool,
    identity_path: Option<&std::path::Path>,
) -> Result<String, String> {
    let (url, fetch_root_key) = resolve_network(network, fetch_root_key_flag)?;
    let canister_id = Principal::from_text(canister)
        .map_err(|error| format!("invalid canister principal {canister:?}: {error}"))?;
    let agent = if let Some(identity_path) = identity_path {
        let identity = Secp256k1Identity::from_pem_file(identity_path)
            .map_err(|error| format!("read identity {}: {error}", identity_path.display()))?;
        ic_agent::Agent::builder()
            .with_url(url)
            .with_identity(identity)
            .build()
            .map_err(|error| format!("create IC agent: {error}"))?
    } else {
        ic_agent::Agent::builder()
            .with_url(url)
            .build()
            .map_err(|error| format!("create IC agent: {error}"))?
    };
    if fetch_root_key {
        agent
            .fetch_root_key()
            .await
            .map_err(|error| format!("fetch IC root key: {error}"))?;
    }
    let args =
        Encode!(&graph.to_string()).map_err(|error| format!("encode graph name: {error}"))?;
    let response = agent
        .query(&canister_id, "prepared_manifest")
        .with_arg(args)
        .call()
        .await
        .map_err(|error| format!("query prepared_manifest: {error}"))?;
    decode_manifest_response(&response)
}

fn resolve_network(network: &str, fetch_root_key_flag: bool) -> Result<(&str, bool), String> {
    match network {
        "ic" => Ok((DEFAULT_IC_URL, false)),
        "local" => Ok((DEFAULT_LOCAL_URL, true)),
        url if url.starts_with("http://") || url.starts_with("https://") => {
            if !fetch_root_key_flag {
                return Err(
                    "a custom network URL requires --fetch-root-key (icp-cli --root-key fetch equivalent)"
                        .into(),
                );
            }
            Ok((url, true))
        }
        other => Err(format!(
            "unknown network {other:?}; expected \"ic\", \"local\", or an http(s) URL"
        )),
    }
}

fn decode_manifest_response(response: &[u8]) -> Result<String, String> {
    let args = IDLArgs::from_bytes(response)
        .map_err(|error| format!("decode prepared_manifest response: {error}"))?;
    let Some(IDLValue::Variant(result)) = args.args.first() else {
        return Err("decode prepared_manifest response: expected Result variant".into());
    };
    let value = &result.0.val;
    if result.0.id.get_id() != candid::idl_hash("Ok") {
        return Err(format!("Router rejected prepared_manifest: {value:?}"));
    }
    let payload = IDLArgs::new(std::slice::from_ref(value))
        .to_bytes()
        .map_err(|error| format!("decode prepared_manifest payload: {error}"))?;
    let manifest = Decode!(&payload, gleaph_prepared_api::PreparedManifest)
        .map_err(|error| format!("decode prepared_manifest payload: {error}"))?;
    serde_json::to_string(&manifest)
        .map_err(|error| format!("serialize prepared manifest: {error}"))
}

#[cfg(test)]
mod tests {
    use super::run;
    use candid::Encode;
    use gleaph_prepared_api::{GraphIdentity, MANIFEST_VERSION, PreparedManifest};
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
            "--format".into(),
            "rust=never".into(),
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
    fn rejects_duplicate_format_targets() {
        let error = run(vec![
            "--manifest".into(),
            "manifest.json".into(),
            "--target".into(),
            "rust".into(),
            "--format".into(),
            "rust=never".into(),
            "--format".into(),
            "rust=auto".into(),
        ])
        .expect_err("duplicate format targets must fail");

        assert_eq!(error, "duplicate format target \"rust\"");
    }

    #[test]
    fn rejects_unknown_rust_format_mode() {
        let error = run(vec![
            "--manifest".into(),
            "manifest.json".into(),
            "--target".into(),
            "rust".into(),
            "--format".into(),
            "rust=prettyplease".into(),
        ])
        .expect_err("unknown Rust format modes must fail");

        assert!(error.contains("expected auto, rustfmt, or never"));
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

    #[test]
    fn rejects_incomplete_remote_source() {
        let error = run(vec![
            "--canister".into(),
            "aaaaa-aa".into(),
            "--target".into(),
            "typescript".into(),
        ])
        .expect_err("remote source requires a graph name");

        assert_eq!(error, "--canister and --graph must be provided together");
    }

    #[test]
    fn rejects_multiple_manifest_sources() {
        let error = run(vec![
            "--manifest".into(),
            "manifest.json".into(),
            "--canister".into(),
            "aaaaa-aa".into(),
            "--graph".into(),
            "default".into(),
            "--target".into(),
            "typescript".into(),
        ])
        .expect_err("manifest sources must be mutually exclusive");

        assert_eq!(error, "--manifest and --canister are mutually exclusive");
    }

    #[test]
    fn decodes_router_manifest_response() {
        let manifest = PreparedManifest {
            manifest_version: MANIFEST_VERSION,
            graph: GraphIdentity {
                id: "default".into(),
                name: None,
            },
            operations: Vec::new(),
        };
        let response = Encode!(&Result::<PreparedManifest, String>::Ok(manifest))
            .expect("manifest response should encode");

        let json = super::decode_manifest_response(&response).expect("manifest should decode");
        assert!(json.contains("\"manifest_version\":1"));
        assert!(json.contains("\"id\":\"default\""));
    }

    #[test]
    fn resolves_icp_cli_network_names() {
        assert_eq!(
            super::resolve_network("ic", false).unwrap(),
            (super::DEFAULT_IC_URL, false)
        );
        assert_eq!(
            super::resolve_network("local", false).unwrap(),
            (super::DEFAULT_LOCAL_URL, true)
        );
    }

    #[test]
    fn custom_network_url_requires_root_key_fetch() {
        let error = super::resolve_network("http://127.0.0.1:8000", false)
            .expect_err("custom networks must opt into fetched root keys");
        assert!(error.contains("requires --fetch-root-key"));
    }
}
