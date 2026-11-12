//! Install the inference canister's tokenizer through the Router (ADR 0113/0114).
//!
//! The Router owns the deployment inference canister and is its controller. This tool uploads the
//! pinned tokenizer in Router-sized chunks, checks the fingerprint that the canister reports, and
//! optionally enables the graph. The fixed-intent neural pack no longer exists; the tokenizer is
//! required for exact question-token quota counts and the general route's policy inputs.

use std::path::{Path, PathBuf};

use candid::{CandidType, Decode, Encode, Principal, types::value::IDLValue};
use clap::Parser;
use gleaph_ingress_client::IcIngress;
use sha2::{Digest, Sha256};

/// Maximum chunk accepted by the Router admin methods (1.9 MB; the IC message cap is 2 MiB).
const MAX_CHUNK_BYTES: usize = 1_900_000;

#[derive(Debug, Parser)]
#[command(
    name = "gleaph-inference-install",
    about = "Install the inference canister tokenizer through the Router (ADR 0113)"
)]
struct Args {
    /// Router canister principal (the inference canister's governance controller).
    #[arg(long, value_name = "PRINCIPAL")]
    router: String,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short, long, default_value = "local", value_name = "NETWORK")]
    network: String,
    /// PEM identity with controller authority on the Router; anonymous when omitted.
    #[arg(long, value_name = "PATH")]
    identity: Option<PathBuf>,
    /// HuggingFace tokenizer JSON (`tokenizer.json`).
    #[arg(long, value_name = "PATH")]
    tokenizer: PathBuf,
    /// Expected SHA-256 of the tokenizer, from the pinned release metadata.
    #[arg(long, value_name = "HEX")]
    tokenizer_sha256: String,
    /// Graph to enable after both assets verify; omit to install only.
    #[arg(long, value_name = "NAME")]
    graph: Option<String>,
    /// Upload chunk size in bytes (at most 1,900,000).
    #[arg(long, default_value_t = MAX_CHUNK_BYTES, value_name = "BYTES")]
    chunk_bytes: usize,
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn chunk_ranges(len: usize, chunk_bytes: usize) -> Result<Vec<(u64, usize, usize)>, String> {
    if chunk_bytes == 0 || chunk_bytes > MAX_CHUNK_BYTES {
        return Err(format!(
            "--chunk-bytes must be between 1 and {MAX_CHUNK_BYTES}"
        ));
    }
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while offset < len {
        let end = (offset + chunk_bytes).min(len);
        ranges.push((offset as u64, offset, end));
        offset = end;
    }
    Ok(ranges)
}

async fn update_two_args(
    ingress: &IcIngress,
    router: Principal,
    method: &str,
    offset: u64,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let encoded = Encode!(&offset, &bytes).map_err(|error| format!("{method} encode: {error}"))?;
    let reply = ingress
        .update_raw(router, method, encoded)
        .await
        .map_err(|error| format!("{method} call: {error}"))?;
    let result: Result<(), IDLValue> = Decode!(&reply, Result<(), IDLValue>)
        .map_err(|error| format!("{method} decode: {error}"))?;
    result.map_err(|error| format!("{method} rejected: {error:?}"))
}

async fn update_result<A, T>(
    ingress: &IcIngress,
    router: Principal,
    method: &str,
    args: &A,
) -> Result<T, String>
where
    A: CandidType,
    T: CandidType + serde::de::DeserializeOwned,
{
    let result = ingress
        .update_result::<A, T, IDLValue>(router, method, args)
        .await
        .map_err(|error| format!("{method} call: {error}"))?;
    result.map_err(|error| format!("{method} rejected: {error:?}"))
}

async fn upload(
    ingress: &IcIngress,
    router: Principal,
    method: &str,
    bytes: &[u8],
    chunk_bytes: usize,
    label: &str,
) -> Result<(), String> {
    let ranges = chunk_ranges(bytes.len(), chunk_bytes)?;
    let total = ranges.len();
    for (index, (offset, start, end)) in ranges.into_iter().enumerate() {
        update_two_args(ingress, router, method, offset, bytes[start..end].to_vec()).await?;
        println!(
            "{label}: chunk {}/{total} uploaded ({} bytes at offset {offset})",
            index + 1,
            end - start
        );
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: create async runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let router = Principal::from_text(&args.router)
        .map_err(|error| format!("invalid --router principal: {error}"))?;
    let tokenizer = read(&args.tokenizer)?;
    if tokenizer.is_empty() {
        return Err("tokenizer is empty".to_owned());
    }
    // Checksums are the local source of truth for what this invocation uploads; the Router
    // verifies the tokenizer fingerprint against the same digest before returning.
    let tokenizer_sha = sha256_hex(&tokenizer);
    if tokenizer_sha != args.tokenizer_sha256.to_ascii_lowercase() {
        return Err(format!(
            "tokenizer sha256 mismatch: file {tokenizer_sha}, expected {}",
            args.tokenizer_sha256
        ));
    }
    println!(
        "tokenizer: {} bytes sha256={tokenizer_sha}",
        tokenizer.len()
    );

    let ingress = IcIngress::connect(&args.network, args.identity.as_deref())
        .await
        .map_err(|error| format!("connect: {error}"))?;
    // `admin_enable_graph_inference` provisions and binds the canister (idempotent). Bind first,
    // install second: until the tokenizer verifies, every general route call fails closed.
    if let Some(graph) = &args.graph {
        let enabled: Principal =
            update_result(&ingress, router, "admin_enable_graph_inference", graph).await?;
        println!("graph {graph}: inference canister bound at {enabled}");
    }

    upload(
        &ingress,
        router,
        "admin_install_tokenizer_chunk",
        &tokenizer,
        args.chunk_bytes,
        "tokenizer",
    )
    .await?;
    let fingerprint = update_result::<Option<String>, gleaph_routing_wire::TokenizerFingerprint>(
        &ingress,
        router,
        "admin_install_tokenizer_finish",
        &Some(tokenizer_sha.clone()),
    )
    .await?;
    if fingerprint.bytes != tokenizer.len() as u64 || fingerprint.sha256.len() != 32 {
        return Err(format!(
            "installed tokenizer fingerprint is inconsistent: {:?}",
            fingerprint
        ));
    }
    println!(
        "tokenizer: installed and fingerprint verified ({} bytes)",
        fingerprint.bytes
    );

    if args.graph.is_some() {
        println!("graph: inference enabled");
    } else {
        println!("graph: not enabled (pass --graph to enable)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_the_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn chunk_ranges_cover_every_byte_without_overlap() {
        let ranges = chunk_ranges(10, 4).expect("ranges");
        assert_eq!(ranges, vec![(0, 0, 4), (4, 4, 8), (8, 8, 10)]);
        assert_eq!(chunk_ranges(0, 4).expect("empty"), Vec::new());
        assert!(chunk_ranges(10, 0).is_err());
        assert!(chunk_ranges(10, MAX_CHUNK_BYTES + 1).is_err());
    }
}
