//! Network endpoint resolution and PEM identity handling.
//!
//! Conventions are intentionally identical to the dev CLI's `RemoteTransport::connect`
//! (`crates/cli/src/remote.rs`): `ic` targets mainnet without a root-key fetch, `local`
//! targets the default gateway with an automatic root-key fetch, and a custom http(s) URL is
//! accepted only together with an explicit root-key fetch. One persona difference by design:
//! this tool takes a raw PEM path instead of the dev identity store, because governance key
//! material must not live in developer-tool conventions (ADR 0087 §Problem).

/// Mainnet API endpoint (same constant as the dev CLI).
pub const DEFAULT_IC_URL: &str = "https://icp-api.io";
/// Local replica gateway (same constant as the dev CLI).
pub const DEFAULT_LOCAL_URL: &str = "http://localhost:8000";

/// Resolve a network selector to its endpoint URL and whether the IC root key must be fetched
/// before the first call. Mirrors `crates/cli/src/remote.rs::resolve_network`.
pub fn resolve_network(network: &str) -> Result<(String, bool), String> {
    match network {
        "ic" => Ok((DEFAULT_IC_URL.to_owned(), false)),
        "local" => Ok((DEFAULT_LOCAL_URL.to_owned(), true)),
        url if url.starts_with("http://") || url.starts_with("https://") => {
            Ok((url.to_owned(), true))
        }
        other => Err(format!(
            "unknown network {other:?}; expected \"ic\", \"local\", or an http(s) URL"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_resolution_matches_dev_cli_conventions() {
        let (url, fetch) = resolve_network("ic").expect("ic");
        assert_eq!(url, DEFAULT_IC_URL);
        assert!(!fetch, "mainnet must not fetch the root key");

        let (url, fetch) = resolve_network("local").expect("local");
        assert_eq!(url, DEFAULT_LOCAL_URL);
        assert!(fetch, "local replicas always need the root key");

        let (url, fetch) = resolve_network("https://example.invalid").expect("custom https");
        assert_eq!(url, "https://example.invalid");
        assert!(fetch);

        let error = resolve_network("somewhere").expect_err("unknown name");
        assert!(error.contains("unknown network"), "got: {error}");
    }
}
